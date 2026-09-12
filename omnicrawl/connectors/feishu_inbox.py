"""飞书入站持久队列与跨重启去重。

参考 openclaw 飞书插件的 "inbound durability" 设计，为本地单 Agent 连接器
提供"先持久化、后处理"的入站语义：

* ``FeishuInbox.enqueue``：事件先落盘（pending 日志），再交回调用方分发；
  若进程在任务处理中途退出，重启后 ``recover`` 会回放尚未完成的事件，
  避免消息在重启窗口丢失。
* 去重键默认使用飞书 ``message_id``（24 小时窗口，跨进程/重启生效）；
  ``dedupe_key`` 可额外传入重投递指纹（同一逻辑消息在新连接重投时会携带
  新的 message_id，指纹用于兜底抑制，例如文本的 sender+chat+create_time
  +内容哈希）。
* 文件损坏或不可写时自动降级为纯内存模式：进程内仍去重，只是不再跨重启
  持久化；所有写失败都只记录日志，绝不阻断消息接收。

存储布局（每个目录一个队列实例，互不干扰）：:

    <root>/
      pending.jsonl   # 已接收但尚未确认完成的事件（追加写）
      done.jsonl      # 已完成的去重键（追加写 + 启动时紧凑化）
      state.json      # 元数据（下一序号、内存已加载水位）

本模块不依赖 lark-oapi，可单独测试。所有公共方法线程安全。
"""

from __future__ import annotations

import json
import logging
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterator

LOGGER = logging.getLogger(__name__)

# 去重/待办记录保留窗口。openclaw 使用 24h/1 万条，本地同样采用 24h，避免
# 重连窗口内的重投递被当作新消息；超过窗口的旧键自然失效。
DEFAULT_DEDUP_TTL_SECONDS = 24 * 60 * 60
# 单文件最大记录数；超过后启动紧凑化，避免日志无限增长。
DEFAULT_MAX_RECORDS = 10_000
# 每次 compact 后保留窗口内的最新键（防止去重表无限膨胀）。
DEFAULT_COMPACT_KEEP = 2_000

# pending/done 行版本；未来变更格式时据此迁移或丢弃。
_RECORD_VERSION = 1


@dataclass(frozen=True)
class InboxRecord:
    """入队后等待确认完成的一条事件。"""

    seq: int
    event_id: str
    dedupe_key: str
    payload: dict[str, Any]
    created_at: float
    version: int = _RECORD_VERSION


@dataclass
class FeishuInbox:
    """线程安全的持久入站队列。

    参数:
        root: 队列目录。为 None 时使用纯内存模式（不落盘）。
        dedup_ttl_seconds: 去重键保留窗口。
        max_records: 超过该数量后启动时紧凑化。
        compact_keep: 紧凑化后保留的最新去重键数量。
    """

    root: Path | None = None
    dedup_ttl_seconds: float = DEFAULT_DEDUP_TTL_SECONDS
    max_records: int = DEFAULT_MAX_RECORDS
    compact_keep: int = DEFAULT_COMPACT_KEEP
    now: Callable[[], float] = time.time

    _lock: threading.RLock = field(default_factory=threading.RLock, init=False)
    _pending: list[InboxRecord] = field(default_factory=list, init=False)
    _done: dict[str, float] = field(default_factory=dict, init=False)  # key -> timestamp
    _seq: int = field(default=0, init=False)
    _memory_only: bool = field(default=False, init=False)
    _pending_fp: Any = field(default=None, init=False)
    _done_fp: Any = field(default=None, init=False)
    _dirty_done: bool = field(default=False, init=False)

    # ------------------------------------------------------------------
    # 生命周期
    # ------------------------------------------------------------------

    def __post_init__(self) -> None:
        if self.root is None:
            self._memory_only = True
            return
        self.root = Path(self.root)
        try:
            self.root.mkdir(parents=True, exist_ok=True)
            # 打开文件句柄时失败（只读目录/权限）→ 降级内存。
            self._pending_fp = (self.root / "pending.jsonl").open("a", encoding="utf-8")
            self._done_fp = (self.root / "done.jsonl").open("a", encoding="utf-8")
        except OSError:
            LOGGER.warning("飞书入站队列无法写 %s，降级为纯内存模式", self.root, exc_info=True)
            self._memory_only = True
            self._pending_fp = None
            self._done_fp = None
            return
        self._load_state()

    def _load_state(self) -> None:
        """启动时恢复：读取 state、pending 与 done，剔除过期/损坏行。"""

        state_path = self.root / "state.json"
        try:
            state = json.loads(state_path.read_text(encoding="utf-8"))
            self._seq = int(state.get("seq", 0) or 0)
        except (OSError, ValueError):
            self._seq = 0

        now = self.now()
        # 恢复 pending：损坏行跳过；仅保留窗口内记录，避免陈旧任务在长时间停机
        # 后突然被执行。
        recovered: list[InboxRecord] = []
        for row in self._iter_json_lines(self.root / "pending.jsonl"):
            record = self._parse_record(row)
            if record is None:
                continue
            if now - record.created_at > self.dedup_ttl_seconds:
                continue
            recovered.append(record)
        # 同一事件可能出现多条 pending（重连期间重复入队但尚未确认），只保留
        # 每个 dedupe_key 最新一条。
        by_key: dict[str, InboxRecord] = {}
        for record in recovered:
            by_key[record.dedupe_key] = record
        self._pending = sorted(by_key.values(), key=lambda r: r.seq)
        if self._pending:
            self._seq = max(self._seq, self._pending[-1].seq)

        # 恢复 done 去重表。
        done: dict[str, float] = {}
        for row in self._iter_json_lines(self.root / "done.jsonl"):
            try:
                obj = json.loads(row)
                key = str(obj.get("key") or "").strip()
                ts = float(obj.get("ts") or 0)
            except (ValueError, TypeError):
                continue
            if key and now - ts <= self.dedup_ttl_seconds:
                done[key] = max(done.get(key, 0), ts)
        self._done = done

        if len(self._done) >= self.max_records:
            self._compact(now)

    def _iter_json_lines(self, path: Path) -> Iterator[str]:
        try:
            with path.open(encoding="utf-8") as handle:
                for line in handle:
                    line = line.strip()
                    if line:
                        yield line
        except OSError:
            return

    def _parse_record(self, row: str) -> InboxRecord | None:
        try:
            obj = json.loads(row)
            seq = int(obj.get("seq") or 0)
            event_id = str(obj.get("event_id") or "")
            dedupe_key = str(obj.get("dedupe_key") or "")
            payload = obj.get("payload")
            created_at = float(obj.get("created_at") or 0)
            version = int(obj.get("version") or 0)
        except (ValueError, TypeError):
            return None
        if not dedupe_key or not event_id or version != _RECORD_VERSION or not isinstance(payload, dict):
            return None
        return InboxRecord(seq, event_id, dedupe_key, payload, created_at, version)

    # ------------------------------------------------------------------
    # 入队与确认
    # ------------------------------------------------------------------

    def enqueue(
        self,
        *,
        event_id: str,
        dedupe_key: str | None = None,
        payload: dict[str, Any] | None = None,
    ) -> bool:
        """先落盘再登记内存；返回是否应处理（False = 重复/空键）。"""

        event_id = str(event_id or "").strip()
        if not event_id:
            return False
        key = str(dedupe_key or "").strip() or event_id
        payload = dict(payload or {})
        now = self.now()
        with self._lock:
            if self._seen_recent(key, now):
                return False
            seq = self._seq + 1
            self._seq = seq
            record = InboxRecord(seq, event_id, key, payload, now)
            self._pending.append(record)
            self._done[key] = now
            if not self._memory_only:
                self._append_record(self._pending_fp, record)
                self._append_key(self._done_fp, key, now)
            return True

    def _append_record(self, fp: Any, record: InboxRecord) -> None:
        try:
            fp.write(
                json.dumps(
                    {
                        "version": record.version,
                        "seq": record.seq,
                        "event_id": record.event_id,
                        "dedupe_key": record.dedupe_key,
                        "created_at": record.created_at,
                        "payload": record.payload,
                    },
                    ensure_ascii=False,
                    separators=(",", ":"),
                )
                + "\n"
            )
            fp.flush()
        except OSError:
            LOGGER.warning("飞书入站队列写 pending 失败，降级为纯内存模式", exc_info=True)
            self._memory_only = True
            try:
                self._pending_fp.close()
            except Exception:  # noqa: BLE001
                pass
            self._pending_fp = None
            self._done_fp = None

    def _append_key(self, fp: Any, key: str, ts: float) -> None:
        try:
            fp.write(
                json.dumps({"key": key, "ts": ts}, ensure_ascii=False, separators=(",", ":"))
                + "\n"
            )
            fp.flush()
        except OSError:
            LOGGER.warning("飞书入站队列写 done 失败，降级为纯内存模式", exc_info=True)
            self._memory_only = True
            try:
                self._done_fp.close()
            except Exception:  # noqa: BLE001
                pass
            self._done_fp = None
            self._pending_fp = None

    def confirm(self, dedupe_key: str) -> None:
        """任务处理完成（成功或失败都算终结），从 pending 移除。"""

        key = str(dedupe_key or "").strip()
        if not key:
            return
        with self._lock:
            before = len(self._pending)
            self._pending = [r for r in self._pending if r.dedupe_key != key]
            # done 键已在 enqueue 时登记，无需重复追加；只是从待办移除。
            if not self._memory_only and len(self._pending) != before:
                self._rewrite_pending()

    def _rewrite_pending(self) -> None:
        """把当前内存 pending 全量重写到磁盘，保持磁盘与确认状态一致。

        JSONL 是追加写，删除某条已完成记录需要重写文件；确认频率低（每个
        任务一次），成本可接受。写失败时降级内存模式并继续。
        """

        if self._memory_only or self._pending_fp is None or self.root is None:
            return
        try:
            self._pending_fp.close()
            path = self.root / "pending.jsonl"
            tmp_path = path.with_suffix(".jsonl.tmp")
            with tmp_path.open("w", encoding="utf-8") as handle:
                for record in self._pending:
                    handle.write(
                        json.dumps(
                            {
                                "version": record.version,
                                "seq": record.seq,
                                "event_id": record.event_id,
                                "dedupe_key": record.dedupe_key,
                                "created_at": record.created_at,
                                "payload": record.payload,
                            },
                            ensure_ascii=False,
                            separators=(",", ":"),
                        )
                        + "\n"
                    )
            tmp_path.replace(path)
            self._pending_fp = path.open("a", encoding="utf-8")
        except OSError:
            LOGGER.warning("飞书入站队列重写 pending 失败，降级为纯内存模式", exc_info=True)
            self._memory_only = True
            try:
                if self._pending_fp is not None:
                    self._pending_fp.close()
            except Exception:  # noqa: BLE001
                pass
            self._pending_fp = None
            self._done_fp = None

    def recover(self) -> list[InboxRecord]:
        """返回尚未确认的待办事件（按入队顺序）；调用方负责重新分发。"""

        with self._lock:
            return list(self._pending)

    # ------------------------------------------------------------------
    # 查询
    # ------------------------------------------------------------------

    def _seen_recent(self, key: str, now: float) -> bool:
        ts = self._done.get(key)
        return ts is not None and (now - ts) <= self.dedup_ttl_seconds

    def is_duplicate(self, dedupe_key: str) -> bool:
        key = str(dedupe_key or "").strip()
        if not key:
            return False
        with self._lock:
            return self._seen_recent(key, self.now())

    @property
    def pending_count(self) -> int:
        with self._lock:
            return len(self._pending)

    @property
    def memory_only(self) -> bool:
        return self._memory_only

    # ------------------------------------------------------------------
    # 内部工具
    # ------------------------------------------------------------------

    def _compact(self, now: float | None = None) -> None:
        """把 done 表按最新 ts 保留 compact_keep 条，重写 done.jsonl。"""

        if self._memory_only or self._done_fp is None:
            return
        now = now or self.now()
        try:
            # 先关掉当前追加句柄，重写后重新打开。
            self._done_fp.close()
            path = self.root / "done.jsonl"
            keep = sorted(self._done.items(), key=lambda item: item[1], reverse=True)[
                : self.compact_keep
            ]
            self._done = dict(keep)
            tmp_path = path.with_suffix(".jsonl.tmp")
            with tmp_path.open("w", encoding="utf-8") as handle:
                for key, ts in self._done.items():
                    handle.write(json.dumps({"key": key, "ts": ts}, ensure_ascii=False) + "\n")
            tmp_path.replace(path)
            self._done_fp = path.open("a", encoding="utf-8")
        except OSError:
            LOGGER.warning("飞书入站队列紧凑化失败", exc_info=True)

    def close(self) -> None:
        """关闭文件句柄并持久化元数据（若仍可写）。"""

        with self._lock:
            if self._memory_only:
                return
            try:
                if self._pending_fp is not None:
                    self._pending_fp.close()
                if self._done_fp is not None:
                    self._done_fp.close()
            except OSError:
                LOGGER.warning("关闭飞书入站队列文件失败", exc_info=True)
            if not self._memory_only and self.root is not None:
                try:
                    (self.root / "state.json").write_text(
                        json.dumps({"seq": self._seq}, ensure_ascii=False),
                        encoding="utf-8",
                    )
                except OSError:
                    LOGGER.warning("写飞书入站队列元数据失败", exc_info=True)
            self._pending_fp = None
            self._done_fp = None
