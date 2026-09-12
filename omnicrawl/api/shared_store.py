"""API 多 worker（``uvicorn workers>1``）下的跨进程运行状态存储。

问题背景：uvicorn 的 ``workers`` 走 SO_REUSEPORT，由内核按连接轮询，**不保证
粘性**。客户端 ``POST /runs`` 落在 worker A 之后，后续的 ``GET /runs/{id}``、
``GET /runs/{id}/events``、``POST .../confirmations/...`` 都可能落到 worker B。
只要 Run 元数据、事件流与人工决策还留在进程内存里，多 worker 就会表现为
随机 404、事件流断流、审批打不出去。

本模块把"跨进程可见的那一层"抽成共享存储，用标准库 ``sqlite3`` 实现，不引入
新依赖、不需要 Redis 等外部服务：

* ``runs`` / ``events`` / ``subagent_events`` / ``task_sources``：**所有者进程
  写入、任意进程读取**。每个 run 只有一个所有者（即 start_run 所在的 worker），
  事件 id 由该所有者单调分配，因此无需跨进程竞争即可保持有序。
* ``confirmations`` / ``questions`` / ``decisions``：**任意进程写入、所有者进程
  消费**。审批与提问的状态必须对所有 worker 可见才能给出正确的 404/409；
  投票结果通过 ``decisions`` 投递给正在阻塞等待的所有者。

并发模型：每个进程一份连接（``check_same_thread=False``）+ 进程内 ``RLock``
串行化写操作；跨进程用 WAL + ``busy_timeout`` 做多读单写。所有 SQL 都是短事务，
调用方位于同步端点或 ``asyncio.to_thread`` 中，不会阻塞事件循环。

单进程（``workers=1``，默认）不使用本模块：``AgentAPIService`` 直接持有内存
``RunState``，行为与历史版本完全一致。
"""

from __future__ import annotations

import json
import logging
import sqlite3
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Sequence

from ..config.core.runtime import user_config_dir
from .models import ACTIVE_RUN_STATUSES, RunEvent


LOGGER = logging.getLogger(__name__)

DB_FILENAME_TEMPLATE = "api-runs-{host}-{port}.sqlite3"
BUSY_TIMEOUT_MS = 5000

# 决策种类：取消 / 审批 / 提问。target_id 为对应 confirmation_id 或 question_id，
# cancel 用空串。所有者按 run_id 消费并幂等应用。
DECISION_CANCEL = "cancel"
DECISION_CONFIRM = "confirm"
DECISION_ANSWER = "answer"

_SCHEMA = (
    """
    CREATE TABLE IF NOT EXISTS runs (
        run_id      TEXT PRIMARY KEY,
        session_id  TEXT NOT NULL,
        message     TEXT NOT NULL,
        status      TEXT NOT NULL,
        created_at  REAL NOT NULL,
        updated_at  REAL NOT NULL,
        result      TEXT NOT NULL DEFAULT '',
        error       TEXT NOT NULL DEFAULT '',
        todo_items  TEXT NOT NULL DEFAULT '[]',
        owner_pid   INTEGER NOT NULL
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS events (
        run_id   TEXT NOT NULL,
        event_id INTEGER NOT NULL,
        name     TEXT NOT NULL,
        data     TEXT NOT NULL,
        PRIMARY KEY (run_id, event_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS subagent_events (
        session_id TEXT NOT NULL,
        event_id   INTEGER NOT NULL,
        name       TEXT NOT NULL,
        data       TEXT NOT NULL,
        PRIMARY KEY (session_id, event_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS task_sources (
        task_id    TEXT PRIMARY KEY,
        run_id     TEXT NOT NULL,
        session_id TEXT NOT NULL
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS subagent_counters (
        session_id TEXT PRIMARY KEY,
        next_id    INTEGER NOT NULL
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS confirmations (
        confirmation_id TEXT PRIMARY KEY,
        run_id          TEXT NOT NULL,
        tool_name       TEXT NOT NULL,
        arguments       TEXT NOT NULL,
        created_at      REAL NOT NULL,
        resolved        INTEGER NOT NULL DEFAULT 0,
        decision        INTEGER
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS questions (
        question_id TEXT PRIMARY KEY,
        run_id      TEXT NOT NULL,
        kind        TEXT NOT NULL,
        question    TEXT NOT NULL,
        options     TEXT NOT NULL DEFAULT '[]',
        created_at  REAL NOT NULL,
        resolved    INTEGER NOT NULL DEFAULT 0,
        answer      TEXT
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS decisions (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id     TEXT NOT NULL,
        kind       TEXT NOT NULL,
        target_id  TEXT NOT NULL,
        payload    TEXT NOT NULL DEFAULT '{}',
        created_at REAL NOT NULL
    )
    """,
    "CREATE INDEX IF NOT EXISTS idx_events_run ON events (run_id, event_id)",
    "CREATE INDEX IF NOT EXISTS idx_subagent_events_session ON subagent_events (session_id, event_id)",
    "CREATE INDEX IF NOT EXISTS idx_decisions_run ON decisions (run_id, seq)",
    "CREATE INDEX IF NOT EXISTS idx_runs_updated ON runs (updated_at)",
)


class SharedStoreError(RuntimeError):
    """共享运行状态存储不可用（建库、加锁或读写失败）。"""


@dataclass(frozen=True)
class RunRecord:
    """``runs`` 表的一行；由服务层还原成 ``RunState`` 供路由读取。"""

    run_id: str
    session_id: str
    message: str
    status: str
    created_at: float
    updated_at: float
    result: str
    error: str
    todo_items: tuple[dict[str, Any], ...]
    owner_pid: int


@dataclass(frozen=True)
class Decision:
    """任意 worker 投递给所有者的一条人工决策。"""

    seq: int
    kind: str
    target_id: str
    payload: dict[str, Any]


def api_run_store_path(host: str, port: int) -> Path:
    """共享库路径：按 host:port 区分，不同 API 实例互不串状态。"""

    safe_host = "".join(char if char.isalnum() or char in ".-" else "-" for char in host)
    return user_config_dir() / DB_FILENAME_TEMPLATE.format(host=safe_host, port=int(port))


class SharedRunStore:
    """跨进程运行状态存储（SQLite / WAL）。"""

    def __init__(
        self,
        path: Path,
        *,
        max_events_per_run: int = 2000,
        max_retained_runs: int = 100,
    ) -> None:
        self.path = Path(path)
        self.max_events_per_run = max(10, int(max_events_per_run))
        self.max_retained_runs = max(1, int(max_retained_runs))
        self._lock = threading.RLock()
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            # isolation_level=None：关闭 sqlite3 的隐式事务，由本模块显式
            # BEGIN IMMEDIATE 控制"读-改-写"的原子性。
            self._connection = sqlite3.connect(
                str(self.path),
                timeout=BUSY_TIMEOUT_MS / 1000.0,
                check_same_thread=False,
                isolation_level=None,
            )
            self._connection.row_factory = sqlite3.Row
            self._connection.execute(f"PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}")
            self._connection.execute("PRAGMA journal_mode = WAL")
            self._connection.execute("PRAGMA synchronous = NORMAL")
            with self._lock:
                for statement in _SCHEMA:
                    self._connection.execute(statement)
        except (sqlite3.Error, OSError) as exc:
            raise SharedStoreError(f"无法打开 API 共享状态存储：{self.path}，{exc}") from exc

    # ---- 生命周期 ---------------------------------------------------------

    def close(self) -> None:
        with self._lock:
            try:
                self._connection.close()
            except sqlite3.Error:  # pragma: no cover - 关闭失败不影响退出
                LOGGER.warning("关闭 API 共享状态存储失败：%s", self.path)

    # ---- 内部工具 ---------------------------------------------------------

    def _execute(self, sql: str, parameters: Sequence[Any] = ()) -> sqlite3.Cursor:
        with self._lock:
            try:
                return self._connection.execute(sql, tuple(parameters))
            except sqlite3.Error as exc:
                raise SharedStoreError(f"共享状态存储写入失败：{exc}") from exc

    def _query(self, sql: str, parameters: Sequence[Any] = ()) -> list[sqlite3.Row]:
        with self._lock:
            try:
                return list(self._connection.execute(sql, tuple(parameters)).fetchall())
            except sqlite3.Error as exc:
                raise SharedStoreError(f"共享状态存储读取失败：{exc}") from exc

    def _write_transaction(self, action: Callable[[], Any]) -> Any:
        """在 BEGIN IMMEDIATE 内执行 action；跨进程写互斥由 SQLite 保证。"""

        with self._lock:
            try:
                self._connection.execute("BEGIN IMMEDIATE")
            except sqlite3.Error as exc:
                raise SharedStoreError(f"共享状态存储加锁失败：{exc}") from exc
            try:
                result = action()
            except BaseException:
                self._connection.execute("ROLLBACK")
                raise
            self._connection.execute("COMMIT")
            return result

    # ---- runs -------------------------------------------------------------

    def create_run(
        self,
        run_id: str,
        *,
        message: str,
        session_id: str,
        owner_pid: int,
    ) -> str | None:
        """原子创建 Run；已有活动 Run 时返回其 ID，不插入新任务。"""

        def action() -> str | None:
            placeholders = ", ".join("?" for _ in ACTIVE_RUN_STATUSES)
            row = self._connection.execute(
                f"SELECT run_id FROM runs WHERE status IN ({placeholders}) "
                "ORDER BY updated_at DESC LIMIT 1",
                tuple(sorted(ACTIVE_RUN_STATUSES)),
            ).fetchone()
            if row is not None:
                return str(row["run_id"])
            now = _now()
            self._connection.execute(
                "INSERT OR REPLACE INTO runs "
                "(run_id, session_id, message, status, created_at, updated_at, result, error, "
                " todo_items, owner_pid) "
                "VALUES (?, ?, ?, 'pending', ?, ?, '', '', '[]', ?)",
                (run_id, session_id, message, now, now, int(owner_pid)),
            )
            return None

        active_run_id = self._write_transaction(action)
        if active_run_id is None:
            self.prune_runs()
        return active_run_id

    def get_run(self, run_id: str) -> RunRecord | None:
        rows = self._query("SELECT * FROM runs WHERE run_id = ?", (run_id,))
        return _record(rows[0]) if rows else None

    def latest_run(self) -> RunRecord | None:
        rows = self._query("SELECT * FROM runs ORDER BY updated_at DESC LIMIT 1")
        return _record(rows[0]) if rows else None

    def update_run(
        self,
        run_id: str,
        *,
        status: str | None = None,
        result: str | None = None,
        error: str | None = None,
        todo_items: Sequence[dict[str, Any]] | None = None,
    ) -> None:
        assignments = ["updated_at = ?"]
        parameters: list[Any] = [_now()]
        if status is not None:
            assignments.append("status = ?")
            parameters.append(status)
        if result is not None:
            assignments.append("result = ?")
            parameters.append(result)
        if error is not None:
            assignments.append("error = ?")
            parameters.append(error)
        if todo_items is not None:
            assignments.append("todo_items = ?")
            parameters.append(json.dumps(list(todo_items), ensure_ascii=False))
        parameters.append(run_id)
        self._execute(
            f"UPDATE runs SET {', '.join(assignments)} WHERE run_id = ?",
            parameters,
        )

    def request_cancel(self, run_id: str) -> None:
        """跨进程取消：投递一条 cancel 决策给所有者。

        不在这里改写 status：真正的终态由正在跑该 run 的所有者置位，避免引入
        ``ACTIVE_RUN_STATUSES`` / ``TERMINAL_RUN_STATUSES`` 之外的状态值让
        SSE 循环与任务收敛逻辑判断失真。客户端会在所有者响应后就看到终态。
        """

        self.push_decision(run_id, DECISION_CANCEL, "", {})

    def prune_runs(self) -> None:
        """只保留最近若干个已结束的 run，并连带清理其事件与人工决策。"""

        placeholders = ", ".join("?" for _ in ACTIVE_RUN_STATUSES)
        rows = self._query(
            f"SELECT run_id FROM runs WHERE status NOT IN ({placeholders}) "
            "ORDER BY updated_at DESC",
            tuple(sorted(ACTIVE_RUN_STATUSES)),
        )
        stale = [row["run_id"] for row in rows[self.max_retained_runs :]]
        for run_id in stale:
            self._write_transaction(lambda run_id=run_id: self._delete_run(run_id))

    def _delete_run(self, run_id: str) -> None:
        for table in ("events", "decisions", "confirmations", "questions", "task_sources"):
            self._connection.execute(f"DELETE FROM {table} WHERE run_id = ?", (run_id,))
        self._connection.execute("DELETE FROM runs WHERE run_id = ?", (run_id,))

    def reconcile_orphan_runs(self, pid_alive: Callable[[int], bool]) -> int:
        """把所有者进程已消失的活动 run 标记为失败，避免客户端无限等待。

        worker 崩溃后它的内存 RunState 随之消失，若不收敛状态，SSE 客户端会
        一直挂在 ``running`` 上轮询。返回被收敛的 run 数量。
        """

        placeholders = ", ".join("?" for _ in ACTIVE_RUN_STATUSES)
        rows = self._query(
            f"SELECT run_id, owner_pid FROM runs WHERE status IN ({placeholders})",
            tuple(sorted(ACTIVE_RUN_STATUSES)),
        )
        orphans = [row["run_id"] for row in rows if not pid_alive(int(row["owner_pid"]))]
        for run_id in orphans:
            self.update_run(
                run_id,
                status="failed",
                error="拥有该生成任务的 API worker 已退出。",
            )
        if orphans:
            LOGGER.warning("收敛了 %d 个所有进程已消失的 API 生成任务。", len(orphans))
        return len(orphans)

    # ---- run 事件 ---------------------------------------------------------

    def append_event(self, run_id: str, event_id: int, name: str, data: dict[str, Any]) -> None:
        self._execute(
            "INSERT OR REPLACE INTO events (run_id, event_id, name, data) VALUES (?, ?, ?, ?)",
            (run_id, int(event_id), name, _dumps(data)),
        )
        # 保留窗口与内存实现一致：超出部分从最老的开始丢弃。
        self._execute(
            "DELETE FROM events WHERE run_id = ? AND event_id <= ?",
            (run_id, int(event_id) - self.max_events_per_run),
        )

    def events_after(self, run_id: str, last_event_id: int) -> list[RunEvent]:
        rows = self._query(
            "SELECT event_id, name, data FROM events WHERE run_id = ? AND event_id > ? "
            "ORDER BY event_id",
            (run_id, int(last_event_id)),
        )
        return [_event(row) for row in rows]

    def has_events_after(self, run_id: str, last_event_id: int) -> bool:
        """廉价的存在性检查：SSE 等待循环不需要把事件全部取回。"""

        rows = self._query(
            "SELECT 1 FROM events WHERE run_id = ? AND event_id > ? LIMIT 1",
            (run_id, int(last_event_id)),
        )
        return bool(rows)

    def earliest_event_id(self, run_id: str) -> int | None:
        rows = self._query("SELECT MIN(event_id) AS first_id FROM events WHERE run_id = ?", (run_id,))
        return rows[0]["first_id"] if rows and rows[0]["first_id"] is not None else None

    # ---- Session 级后台任务事件 -------------------------------------------

    def append_subagent_event(
        self,
        session_id: str,
        event_id: int,
        name: str,
        data: dict[str, Any],
    ) -> None:
        self._execute(
            "INSERT OR REPLACE INTO subagent_events (session_id, event_id, name, data) "
            "VALUES (?, ?, ?, ?)",
            (session_id, int(event_id), name, _dumps(data)),
        )
        self._execute(
            "DELETE FROM subagent_events WHERE session_id = ? AND event_id <= ?",
            (session_id, int(event_id) - self.max_events_per_run),
        )

    def allocate_subagent_event_id(self, session_id: str) -> int:
        """分配（并预留）下一个 Session 事件 id。

        用独立计数器而不是 ``MAX(event_id)+1``，原因有两个：

        * 不同 worker 可能为同一 Session 追加事件，``MAX+1`` 会让并发调用拿到
          同一个 id，而这里在 BEGIN IMMEDIATE 内"取值 + 自增"，天然互斥；
        * 事件受保留窗口裁剪，一旦某 Session 的旧事件全被删掉，``MAX+1`` 会
          从 1 重新开始，破坏客户端游标依赖的单调性。计数器持久保存。
        """

        def action() -> int:
            self._connection.execute(
                "INSERT OR IGNORE INTO subagent_counters (session_id, next_id) VALUES (?, 1)",
                (session_id,),
            )
            row = self._connection.execute(
                "SELECT next_id FROM subagent_counters WHERE session_id = ?",
                (session_id,),
            ).fetchone()
            allocated = int(row["next_id"])
            self._connection.execute(
                "UPDATE subagent_counters SET next_id = next_id + 1 WHERE session_id = ?",
                (session_id,),
            )
            return allocated

        return int(self._write_transaction(action))

    def subagent_events_after(self, session_id: str, last_event_id: int) -> list[RunEvent]:
        rows = self._query(
            "SELECT event_id, name, data FROM subagent_events "
            "WHERE session_id = ? AND event_id > ? ORDER BY event_id",
            (session_id, int(last_event_id)),
        )
        return [_event(row) for row in rows]

    def has_subagent_events_after(self, session_id: str, last_event_id: int) -> bool:
        rows = self._query(
            "SELECT 1 FROM subagent_events WHERE session_id = ? AND event_id > ? LIMIT 1",
            (session_id, int(last_event_id)),
        )
        return bool(rows)

    def earliest_subagent_event_id(self, session_id: str) -> int | None:
        rows = self._query(
            "SELECT MIN(event_id) AS first_id FROM subagent_events WHERE session_id = ?",
            (session_id,),
        )
        return rows[0]["first_id"] if rows and rows[0]["first_id"] is not None else None

    # ---- 后台任务来源 -----------------------------------------------------

    def record_task_source(self, task_id: str, run_id: str, session_id: str) -> None:
        self._execute(
            "INSERT OR REPLACE INTO task_sources (task_id, run_id, session_id) VALUES (?, ?, ?)",
            (task_id, run_id, session_id),
        )

    def task_source(self, task_id: str) -> tuple[str, str] | None:
        rows = self._query(
            "SELECT run_id, session_id FROM task_sources WHERE task_id = ?",
            (task_id,),
        )
        return (rows[0]["run_id"], rows[0]["session_id"]) if rows else None

    def drop_task_source(self, task_id: str) -> None:
        self._execute("DELETE FROM task_sources WHERE task_id = ?", (task_id,))

    # ---- 审批与提问 -------------------------------------------------------

    def register_confirmation(
        self,
        confirmation_id: str,
        *,
        run_id: str,
        tool_name: str,
        arguments: dict[str, Any],
    ) -> None:
        self._execute(
            "INSERT OR REPLACE INTO confirmations "
            "(confirmation_id, run_id, tool_name, arguments, created_at, resolved, decision) "
            "VALUES (?, ?, ?, ?, ?, 0, NULL)",
            (confirmation_id, run_id, tool_name, _dumps(arguments), _now()),
        )

    def confirmation(self, confirmation_id: str) -> sqlite3.Row | None:
        rows = self._query(
            "SELECT * FROM confirmations WHERE confirmation_id = ?",
            (confirmation_id,),
        )
        return rows[0] if rows else None

    def resolve_confirmation(self, confirmation_id: str, approved: bool) -> bool:
        """原子地抢占一个审批的终态；已被处理时返回 False。"""

        def action() -> bool:
            cursor = self._connection.execute(
                "UPDATE confirmations SET resolved = 1, decision = ? "
                "WHERE confirmation_id = ? AND resolved = 0",
                (1 if approved else 0, confirmation_id),
            )
            return cursor.rowcount == 1

        return bool(self._write_transaction(action))

    def unresolved_confirmation_ids(self, run_id: str) -> list[str]:
        """该 run 尚未定终态的审批 id（取消时需要一并置为拒绝）。"""

        rows = self._query(
            "SELECT confirmation_id FROM confirmations WHERE run_id = ? AND resolved = 0",
            (run_id,),
        )
        return [str(row["confirmation_id"]) for row in rows]

    def unresolved_question_ids(self, run_id: str) -> list[str]:
        rows = self._query(
            "SELECT question_id FROM questions WHERE run_id = ? AND resolved = 0",
            (run_id,),
        )
        return [str(row["question_id"]) for row in rows]

    def register_question(
        self,
        question_id: str,
        *,
        run_id: str,
        kind: str,
        question: str,
        options: Sequence[str],
    ) -> None:
        self._execute(
            "INSERT OR REPLACE INTO questions "
            "(question_id, run_id, kind, question, options, created_at, resolved, answer) "
            "VALUES (?, ?, ?, ?, ?, ?, 0, NULL)",
            (
                question_id,
                run_id,
                kind,
                question,
                json.dumps(list(options), ensure_ascii=False),
                _now(),
            ),
        )

    def question(self, question_id: str) -> sqlite3.Row | None:
        rows = self._query("SELECT * FROM questions WHERE question_id = ?", (question_id,))
        return rows[0] if rows else None

    def resolve_question(self, question_id: str, answer: str | None) -> bool:
        """原子地抢占一个提问的终态；已被处理时返回 False。"""

        def action() -> bool:
            cursor = self._connection.execute(
                "UPDATE questions SET resolved = 1, answer = ? "
                "WHERE question_id = ? AND resolved = 0",
                (answer, question_id),
            )
            return cursor.rowcount == 1

        return bool(self._write_transaction(action))

    # ---- 决策投递 ---------------------------------------------------------

    def push_decision(
        self,
        run_id: str,
        kind: str,
        target_id: str,
        payload: dict[str, Any] | None = None,
    ) -> None:
        self._execute(
            "INSERT INTO decisions (run_id, kind, target_id, payload, created_at) "
            "VALUES (?, ?, ?, ?, ?)",
            (run_id, kind, target_id, _dumps(payload or {}), _now()),
        )

    def take_decisions(self, run_id: str) -> list[Decision]:
        """取出并删除该 run 的待处理决策；只有所有者会调用，不会互相抢。"""

        def action() -> list[Decision]:
            rows = self._connection.execute(
                "SELECT seq, kind, target_id, payload FROM decisions "
                "WHERE run_id = ? ORDER BY seq",
                (run_id,),
            ).fetchall()
            if rows:
                self._connection.execute(
                    "DELETE FROM decisions WHERE run_id = ? AND seq <= ?",
                    (run_id, max(int(row["seq"]) for row in rows)),
                )
            return [
                Decision(
                    seq=int(row["seq"]),
                    kind=str(row["kind"]),
                    target_id=str(row["target_id"]),
                    payload=_loads(row["payload"]),
                )
                for row in rows
            ]

        return list(self._write_transaction(action))


def _now() -> float:
    return time.time()


def _dumps(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, separators=(",", ":"))


def _loads(raw: Any) -> dict[str, Any]:
    try:
        value = json.loads(raw) if raw else {}
    except (TypeError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def _event(row: sqlite3.Row) -> RunEvent:
    return RunEvent(id=int(row["event_id"]), event=str(row["name"]), data=_loads(row["data"]))


def _record(row: sqlite3.Row) -> RunRecord:
    try:
        todo_items = tuple(json.loads(row["todo_items"]))
    except (TypeError, ValueError):
        todo_items = ()
    return RunRecord(
        run_id=str(row["run_id"]),
        session_id=str(row["session_id"]),
        message=str(row["message"]),
        status=str(row["status"]),
        created_at=float(row["created_at"]),
        updated_at=float(row["updated_at"]),
        result=str(row["result"]),
        error=str(row["error"]),
        todo_items=tuple(item for item in todo_items if isinstance(item, dict)),
        owner_pid=int(row["owner_pid"]),
    )


__all__ = [
    "BUSY_TIMEOUT_MS",
    "DECISION_ANSWER",
    "DECISION_CANCEL",
    "DECISION_CONFIRM",
    "Decision",
    "RunRecord",
    "SharedRunStore",
    "SharedStoreError",
    "api_run_store_path",
]
