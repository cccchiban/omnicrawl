"""Telegram Bot 远程接入：让用户通过 Telegram 远程操作 OmniCrawl Agent（harness）。

设计说明：
- 零新增第三方依赖：直接用 requests 调用 Telegram Bot HTTP API，通过
  getUpdates 长轮询接收消息（无需公网 webhook），避免引入 python-telegram-bot。
- 安全白名单：只有 allowed_user_ids 中的 Telegram 用户才能操作；未授权消息
  直接忽略且不回复，避免向无关用户泄露 Bot 存在与任何信息。
- 单活动执行：同一时刻只运行一个 Agent 任务（与本地 API 服务保持一致），
  防止多线程同时驱动同一个 LocalToolAgent 造成会话状态错乱。
- 工具确认：Agent 在 manual/review 审批模式下对敏感工具（bash/powershell）
  调用前会请求确认；本模块把确认请求转发到 Telegram 聊天，等待用户回复
  /approve 或 /reject，超时默认拒绝（保证无人值守时不自动放行危险命令）。
- 任务取消：/cancel 置位取消事件，通过 run_stream 的 cancel_check 回调中断
  模型请求与工具执行循环。
- 流式输出：最终回答以打字机效果流式显示（编辑同一条消息）；思考内容
  （/thinking on 开启）、工具调用、状态提示各自独立成消息，互不合并。
  任务失败/取消时已流式输出的部分会保留（追加中断标记），不覆盖丢失。
- 跨进程同步：TUI 中切换的推理强度（/reasoning）与工作区（/workspace）
  都会写回 config.toml；本模块每次任务开始前重读并应用到当前 Agent，
  保证 TUI ↔ tg bot 的推理强度、审批模式与工作区一致。tg bot 的 /workspace 与 /approval 切换
  同样持久化，TUI/Telegram 重启后生效。审批默认自动审查（review），
  Telegram 远程不支持完全自动（auto），仅限本地 TUI 配置。
- 文件接收：图片/文档/视频/语音/音频等文件消息会下载并按类型分类存入
  工作区 .agent_tmp 的 images/videos/scripts/code/files/audio 子目录，
  随后把“已收到文件：位于 xxx”作为任务文本交给 Agent 处理；
  caption 作为补充说明一并附带。文件下载/保存在后台线程执行，
  不阻塞轮询线程（下载期间 /cancel 等命令仍可响应）。

运行方式：
    python -m omnicrawl.connectors.telegram

配置（环境变量优先级高于 config.toml 的 [telegram] 段）：
    TELEGRAM_BOT_TOKEN          Bot Token（必填，由 @BotFather 创建）
    TELEGRAM_ALLOWED_USER_IDS   允许操作的用户 ID，逗号分隔（必填，安全白名单）
    TELEGRAM_CONFIRM_TIMEOUT    工具确认超时秒数（默认 300）

支持的命令：
    /start     使用说明
    /status    harness 状态（工作区、会话、是否忙碌）
    /session   当前会话 ID
    /reset     开启新会话（清空对话历史）
    /cancel    取消当前任务
    /thinking  查看/切换思考内容显示（on|off，默认关闭）
    /workspace 查看/切换工作区（切换会持久化并同步到 TUI）
    /plan      启用主 Agent 计划模式
    /approve   批准当前等待确认的工具调用
    /reject    拒绝当前等待确认的工具调用
    其他文本   作为任务发送给 OmniCrawl Agent 执行
    图片/文件 自动下载分类存入 .agent_tmp，再交给 Agent 处理（含语音）
"""

from __future__ import annotations

import json
import logging
import os
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import requests

from omnicrawl.state.session_artifacts import redact_sensitive_text, redact_sensitive_values


LOGGER = logging.getLogger(__name__)

# Telegram Bot API 地址模板：{token} 由实例持有。
API_BASE = "https://api.telegram.org/bot{token}"

# sendMessage 单条消息上限 4096 字符，留 96 字符余量避免边界触发错误。
MAX_MESSAGE_LEN = 4000

# getUpdates 长轮询等待秒数（Telegram 上限 50）。
POLLING_TIMEOUT = 25

# 流式编辑最小间隔（秒）：Telegram 对同一条消息的编辑频率有限制，
# 约 1 次/秒；低于此间隔的 delta 合并到下一次编辑，避免 429。
STREAM_EDIT_INTERVAL = 0.9

# close() 等待任务线程退出的最大秒数：
# 先置取消事件并释放挂起确认，再等任务线程自行退出，避免
# 任务线程在 Agent 已关闭后仍继续使用它。
CLOSE_TASK_JOIN_TIMEOUT = 10.0


# 文件分类规则：扩展名 -> .agent_tmp 子目录（与默认分类子目录一致）。
_FILE_CATEGORY_RULES: tuple[tuple[str, frozenset[str]], ...] = (
    ("images", frozenset({".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg", ".tif", ".tiff"})),
    ("videos", frozenset({".mp4", ".mov", ".avi", ".mkv", ".webm", ".flv", ".wmv"})),
    ("audio", frozenset({".mp3", ".wav", ".flac", ".m4a", ".aac", ".ogg", ".oga", ".opus", ".wma", ".mid", ".midi"})),
    ("scripts", frozenset({".py", ".js", ".mjs", ".ts", ".sh", ".ps1", ".bat", ".cmd", ".rb", ".lua"})),
    ("code", frozenset({".c", ".cpp", ".h", ".hpp", ".java", ".go", ".rs", ".cs", ".json", ".toml", ".yaml", ".yml", ".xml", ".html", ".css", ".sql", ".md", ".ini", ".cfg", ".csv"})),
)

# Telegram 文件下载地址模板（与 getFile 返回的 file_path 拼接）。
FILE_DOWNLOAD_BASE = "https://api.telegram.org/file/bot{token}/{file_path}"


class TelegramAPIError(RuntimeError):
    """Telegram Bot API 调用失败。

    retryable=False 表示认证类错误（Token 无效等），重试无意义，应直接退出。
    """

    def __init__(self, message: str, *, retryable: bool = True, code: int | None = None) -> None:
        super().__init__(message)
        self.retryable = retryable
        self.code = code


class TaskCancelled(RuntimeError):
    """用户通过 Telegram 主动取消当前任务时抛出的内部中断信号。"""


@dataclass
class _PendingConfirm:
    """一次等待用户审批的工具调用请求。"""

    tool_name: str
    arguments: dict[str, Any]
    chat_id: int
    user_id: int
    event: threading.Event = field(default_factory=threading.Event)
    decision: bool = False
    created_at: float = field(default_factory=time.time)


@dataclass
class _ActiveTask:
    """当前正在后台执行的 Agent 任务。"""

    chat_id: int
    user_id: int
    text: str
    cancel_event: threading.Event = field(default_factory=threading.Event)
    thread: threading.Thread | None = None
    started_at: float = field(default_factory=time.time)


class TelegramAgentBot:
    """通过 Telegram 远程驱动单个 OmniCrawl Agent 的轮询服务。"""

    def __init__(
        self,
        token: str,
        allowed_user_ids: list[int] | tuple[int, ...] | set[int],
        *,
        confirm_timeout_seconds: float = 300.0,
        agent: Any | None = None,
    ) -> None:
        if not token.strip():
            raise ValueError("缺少 Telegram Bot Token。")
        if not allowed_user_ids:
            raise ValueError("allowed_user_ids 不能为空：至少需要一个授权用户 ID。")
        self._token = token.strip()
        self._allowed = set(int(uid) for uid in allowed_user_ids)
        self._confirm_timeout = max(1.0, float(confirm_timeout_seconds))
        self._api_base = API_BASE.format(token=self._token)
        self._lock = threading.RLock()
        self._active_task: _ActiveTask | None = None
        self._pending_confirm: _PendingConfirm | None = None
        self._stopped = threading.Event()
        self._next_offset = 0
        self._agent = agent
        self._confirm_handler_bound = False
        # 思考内容显示开关：默认关闭（/thinking on|off 切换）。
        self._show_thinking = False
        # 上次同步运行时配置时 config.toml 的 mtime（纳秒）；
        # 未变则跳过重读，减少每个任务的配置文件读取开销。
        self._config_mtime: int | None = None

    # ------------------------------------------------------------------
    # Agent 获取（延迟 import，测试时可注入 mock agent 而不依赖 FastAPI）
    # ------------------------------------------------------------------

    def _ensure_agent(self) -> Any:
        """按需创建 Agent；首次调用后复用同一实例。

        confirm handler 对注入的 agent 也必须绑定：外部（测试/复用）传入
        agent 实例时同样需要把工具确认转发到 Telegram，否则 manual 审批
        模式下敏感工具会静默通过默认终端确认逻辑。

        轮询线程、任务线程与文件下载线程可能并发调用本方法，创建与
        confirm handler 绑定必须在锁内完成，避免重复创建 Agent。
        """

        with self._lock:
            if self._agent is None:
                from omnicrawl.api.app import create_default_agent

                self._agent = create_default_agent()
            if not self._confirm_handler_bound:
                self._agent.set_confirm_handler(self._confirm_tool_call)
                self._confirm_handler_bound = True
            return self._agent

    # ------------------------------------------------------------------
    # 公共控制
    # ------------------------------------------------------------------

    def run_forever(self) -> None:
        """启动 getUpdates 长轮询主循环，直到 close() 或认证失败。"""

        self._ensure_agent()
        # 启动即同步：TUI 上次持久化的推理强度/工作区立即生效。
        self._sync_runtime_config()
        LOGGER.info("Telegram Bot 已启动（polling），允许用户：%s", sorted(self._allowed))
        backoff = 1.0
        while not self._stopped.is_set():
            try:
                updates = self._get_updates()
                backoff = 1.0
                for update in updates:
                    try:
                        self._handle_update(update)
                    except Exception:  # noqa: BLE001 - 单条消息失败不阻断轮询
                        LOGGER.exception("处理 Telegram 更新失败：%s", update.get("update_id"))
            except TelegramAPIError as exc:
                if not exc.retryable:
                    LOGGER.error("Telegram API 不可恢复错误，停止轮询：%s", exc)
                    raise
                LOGGER.warning("Telegram API 错误，%.0f 秒后重试：%s", backoff, exc)
                if self._stopped.wait(backoff):
                    break
                backoff = min(backoff * 2, 30.0)
            except Exception:  # noqa: BLE001 - 轮询循环必须健壮
                LOGGER.exception("Telegram 轮询异常，%.0f 秒后重试", backoff)
                if self._stopped.wait(backoff):
                    break
                backoff = min(backoff * 2, 30.0)
        LOGGER.info("Telegram Bot 已停止。")

    def close(self) -> None:
        """停止轮询并回收 Agent 资源。

        先置停止/取消事件并释放挂起的工具确认（否则确认回调可能阻塞
        到超时），再等待任务线程自行退出，最后才关闭 Agent，避免任务
        线程在 Agent 已关闭后继续使用它。等待设上限，不阻塞退出。
        """

        self._stopped.set()
        with self._lock:
            task = self._active_task
            pending = self._pending_confirm
        if task is not None and task.cancel_event is not None:
            task.cancel_event.set()
        if pending is not None:
            # 按拒绝释放挂起的确认，让任务线程不再阻塞在等待上。
            pending.decision = False
            pending.event.set()
        if task is not None and task.thread is not None and task.thread.is_alive():
            task.thread.join(timeout=CLOSE_TASK_JOIN_TIMEOUT)
            if task.thread.is_alive():
                LOGGER.warning("任务线程在 %ss 内未退出，将继续关闭 Agent。", CLOSE_TASK_JOIN_TIMEOUT)
        if self._agent is not None:
            try:
                self._agent.close()
            except Exception:  # noqa: BLE001 - 关闭失败不应阻断退出
                LOGGER.exception("关闭 Agent 失败")

    # ------------------------------------------------------------------
    # Telegram Bot API 底层
    # ------------------------------------------------------------------

    def _api(
        self,
        method: str,
        *,
        params: dict[str, Any] | None = None,
        data: dict[str, Any] | None = None,
        timeout: float = 30.0,
    ) -> dict[str, Any]:
        """调用 Telegram Bot API，统一错误分类。

        返回 payload 中的 result 由调用方自行提取；非 ok 响应统一抛
        TelegramAPIError，401 标记为不可重试。
        """

        url = f"{self._api_base}/{method}"
        try:
            response = requests.post(url, params=params, data=data, timeout=timeout)
        except requests.RequestException as exc:
            raise TelegramAPIError(f"Telegram API 网络请求失败：{exc}") from exc
        try:
            payload = response.json()
        except ValueError as exc:
            raise TelegramAPIError(
                f"Telegram API 返回非 JSON（HTTP {response.status_code}）"
            ) from exc
        if not isinstance(payload, dict) or payload.get("ok") is not True:
            error_code = payload.get("error_code") if isinstance(payload, dict) else None
            description = payload.get("description", "未知错误") if isinstance(payload, dict) else "未知错误"
            retryable = error_code not in (401, 400, 404)
            raise TelegramAPIError(
                f"Telegram API 错误 {error_code}：{description}",
                retryable=retryable,
                code=error_code,
            )
        return payload

    def _get_updates(self) -> list[dict[str, Any]]:
        """长轮询拉取增量更新；收到即推进 offset，处理失败也不丢消息。"""

        payload = self._api(
            "getUpdates",
            params={
                "timeout": POLLING_TIMEOUT,
                "offset": self._next_offset,
                "allowed_updates": ["message"],
            },
            timeout=POLLING_TIMEOUT + 10,
        )
        updates = payload.get("result") or []
        result: list[dict[str, Any]] = []
        for update in updates:
            update_id = int(update.get("update_id", 0))
            self._next_offset = max(self._next_offset, update_id + 1)
            result.append(update)
        return result

    def _send_message(self, chat_id: int, text: str) -> None:
        """向聊天发送文本，自动按 Telegram 单条长度上限分段。"""

        if not text:
            return
        for part in self._split_message(text):
            try:
                self._api("sendMessage", data={"chat_id": chat_id, "text": part})
            except TelegramAPIError as exc:
                # 发送失败不能中断主循环：记录后继续处理后续消息。
                LOGGER.warning("发送 Telegram 消息失败（chat=%s）：%s", chat_id, exc)

    @staticmethod
    def _split_message(text: str, limit: int = MAX_MESSAGE_LEN) -> list[str]:
        """按行优先、字符兜底把长文本切成不超过 limit 的分段。

        - 分段内保留原有换行，不会把相邻两行粘连成一行；
        - 行边界处切分时在段尾保留换行分隔符，下一段从新行开始；
        - 单行超长时硬切，切点插入换行分隔符（占下一段 1 字符预算），
          避免切出的连续无分隔大段难以阅读。
        """

        if len(text) <= limit:
            return [text]
        parts: list[str] = []
        current = ""
        for line in text.split("\n"):
            sep = "\n" if current else ""
            if len(current) + len(sep) + len(line) > limit:
                if current:
                    # 段尾保留行间换行，避免两段消息边界处行被粘连。
                    parts.append(current + "\n")
                current = line
            else:
                current = f"{current}{sep}{line}"
        if current:
            parts.append(current)
        # 单行超长（或段内仍超长）的硬切兜底：切点插入换行分隔符。
        final: list[str] = []
        for part in parts:
            while len(part) > limit:
                final.append(part[:limit])
                part = "\n" + part[limit:]
            final.append(part)
        return final

    # ------------------------------------------------------------------
    # 更新分发
    # ------------------------------------------------------------------

    def _handle_update(self, update: dict[str, Any]) -> None:
        message = update.get("message") or {}
        chat = message.get("chat") or {}
        from_user = message.get("from") or {}
        chat_id = chat.get("id")
        user_id = from_user.get("id")
        text = str(message.get("text", "") or "").strip()
        if not chat_id or not user_id:
            return
        if not self._is_allowed(user_id):
            # 未授权用户：不回复、不提示，仅留日志。
            LOGGER.warning("拒绝未授权用户 %s 的操作（chat=%s）", user_id, chat_id)
            return
        if text:
            self._dispatch(chat_id, user_id, text)
            return
        # 无文本消息：尝试按文件消息处理（图片/文档/视频等），
        # 既无文本也无文件时静默忽略。
        self._handle_file_message(chat_id, user_id, message)

    def _is_allowed(self, user_id: Any) -> bool:
        try:
            return int(user_id) in self._allowed
        except (TypeError, ValueError):
            return False

    def _dispatch(self, chat_id: int, user_id: int, text: str) -> None:
        """分发命令或普通任务文本。

        基础命令（/start /status /session /reset /cancel /approve /reject）
        由本模块直接处理；其余 / 开头的命令转发给 harness 的 slash 命令
        处理器（复用 TUI 同一套实现，保持一致）。
        """

        # 命令可能带 bot 用户名后缀：/start@MyBot
        command = text.split(" ", 1)[0].split("@", 1)[0]
        if command == "/start":
            self._send_message(chat_id, self._help_text())
        elif command == "/status":
            self._send_message(chat_id, self._status_text())
        elif command == "/session":
            session_id = str(getattr(self._ensure_agent(), "current_session_id", "") or "")
            self._send_message(chat_id, f"当前会话 ID：`{session_id}`")
        elif command == "/reset":
            try:
                self._ensure_agent().reset_conversation()
                self._send_message(chat_id, "已开启新会话（对话历史已清空）。")
            except Exception as exc:  # noqa: BLE001
                LOGGER.exception("重置会话失败")
                self._send_message(chat_id, f"❌ 重置会话失败：{redact_sensitive_text(str(exc))}")
        elif command == "/cancel":
            self._request_cancel(chat_id)
        elif command in ("/approve", "/reject"):
            self._handle_approval(chat_id, user_id, approve=(command == "/approve"))
        elif command == "/thinking":
            self._handle_thinking_command(chat_id, text)
        elif command == "/workspace":
            self._handle_workspace_command(chat_id, text)
        elif text.startswith("/"):
            # 其余斜杠命令交给 harness 的 slash 处理；返回 None 表示未知命令。
            try:
                reply = self._handle_harness_command(text)
            except Exception as exc:  # noqa: BLE001 - 远程命令必须健壮
                LOGGER.exception("harness 命令执行失败：%s", text)
                reply = f"❌ 命令执行失败：{redact_sensitive_text(str(exc))}"
            if reply is None:
                reply = "未知命令。发送 /start 查看可用命令。"
            self._send_message(chat_id, reply)
        else:
            self._start_task(chat_id, user_id, text)

    # ------------------------------------------------------------------
    # 文件消息处理（下载 → 分类存放 → 通知 Agent）
    # ------------------------------------------------------------------

    def _handle_file_message(
        self, chat_id: int, user_id: int, message: dict[str, Any]
    ) -> None:
        """处理文件消息：提取文件信息后交给后台线程下载、分类存放并通知 Agent。

        下载与保存可能耗时（Telegram CDN 最长 120s），必须在后台线程执行，
        否则会阻塞轮询线程，导致期间 /cancel 等命令与新消息无法响应。
        失败时由后台线程向用户回传脱敏错误，不启动任务。
        """

        extracted = self._extract_telegram_file(message)
        if extracted is None:
            return  # 无文件（纯贴纸等）静默忽略，与原有非文本行为一致
        file_id, remote_name = extracted
        caption = str(message.get("caption") or "").strip()
        threading.Thread(
            target=self._process_file_message,
            args=(chat_id, user_id, file_id, remote_name, caption),
            daemon=True,
            name="omnicrawl-telegram-file",
        ).start()

    def _process_file_message(
        self,
        chat_id: int,
        user_id: int,
        file_id: str,
        remote_name: str,
        caption: str,
    ) -> None:
        """后台线程：下载文件 → 分类存放 → 通知用户 → 启动 Agent 任务。

        与 _handle_file_message 拆开，避免下载耗时阻塞轮询线程；
        各步失败均回传脱敏错误且不启动任务。
        """

        try:
            # 1) getFile 换取远程路径，再下载字节。
            payload = self._api("getFile", data={"file_id": file_id})
            remote_path = str((payload.get("result") or {}).get("file_path") or "").strip()
            if not remote_path:
                raise TelegramAPIError("getFile 未返回 file_path")
            content = self._download_file_bytes(remote_path)
        except Exception as exc:  # noqa: BLE001 - 下载失败向用户回传错误
            LOGGER.exception("Telegram 文件下载失败")
            self._send_message(
                chat_id,
                f"❌ 文件下载失败：{redact_sensitive_text(str(exc))}",
            )
            return

        # 2) 分类存放：无文件名时用远程路径的扩展名 + 时间戳命名。
        subdir = self._classify_file_name(remote_name or remote_path)
        if remote_name:
            base_name = remote_name
        else:
            base_name = f"telegram_{int(time.time())}{Path(remote_path).suffix}"
        dest = self._resolve_temp_destination(subdir, base_name)
        try:
            dest.write_bytes(content)
        except OSError as exc:
            LOGGER.exception("Telegram 文件保存失败")
            self._send_message(
                chat_id,
                f"❌ 文件保存失败：{redact_sensitive_text(str(exc))}",
            )
            return

        # 3) 通知用户 + 通知 Agent（任务文本即文件位置）。
        rel = self._relative_to_workspace(dest)
        self._send_message(chat_id, f"✅ 已收到文件：{dest.name} → {rel}")
        if self._stopped.is_set():
            # 服务已停止（close 已调用）：不再启动新任务，
            # 避免任务线程使用已关闭的 Agent。
            LOGGER.info("服务已停止，跳过文件任务启动：%s", rel)
            return
        task_text = f"已收到文件：位于 {rel}"
        if caption:
            task_text += f"\n\n用户补充说明：{caption}"
        self._start_task(chat_id, user_id, task_text)

    @staticmethod
    def _extract_telegram_file(message: dict[str, Any]) -> tuple[str, str] | None:
        """从消息中提取 (file_id, 文件名或空串)；无文件返回 None。

        文件字段按优先级检查：document（带 file_name）→ video/audio/
        animation（可带 file_name）→ voice/sticker（mime 推扩展名）→
        photo（取尺寸最大的那张，无文件名）。
        """

        candidates = []
        doc = message.get("document")
        if isinstance(doc, dict):
            candidates.append((doc.get("file_id"), doc.get("file_name") or ""))
        for key in ("video", "audio", "animation"):
            item = message.get(key)
            if isinstance(item, dict):
                candidates.append((item.get("file_id"), item.get("file_name") or ""))
        voice = message.get("voice")
        if isinstance(voice, dict):
            candidates.append((voice.get("file_id"), ""))
        sticker = message.get("sticker")
        if isinstance(sticker, dict):
            candidates.append((sticker.get("file_id"), ""))
        photo = message.get("photo")
        if isinstance(photo, list) and photo:
            largest = max(photo, key=lambda p: int(p.get("file_size") or 0))
            candidates.append((largest.get("file_id"), ""))
        for file_id, file_name in candidates:
            if file_id:
                return str(file_id), str(file_name)
        return None

    @staticmethod
    def _classify_file_name(filename: str) -> str:
        """按扩展名把文件分到 .agent_tmp 子目录，未识别归入 files。"""

        ext = Path(filename).suffix.casefold()
        for subdir, extensions in _FILE_CATEGORY_RULES:
            if ext in extensions:
                return subdir
        return "files"

    def _download_file_bytes(self, remote_path: str) -> bytes:
        """从 Telegram CDN 下载文件内容；失败抛 TelegramAPIError。"""

        url = FILE_DOWNLOAD_BASE.format(token=self._token, file_path=remote_path)
        try:
            response = requests.get(url, timeout=120)
            response.raise_for_status()
        except requests.RequestException as exc:
            raise TelegramAPIError(f"下载文件失败：{exc}") from exc
        return response.content

    def _resolve_temp_destination(self, subdir: str, file_name: str) -> Path:
        """计算 .agent_tmp/<subdir>/<安全文件名>，重名自动追加序号。

        文件名经 Path(...).name 净化，杜绝路径穿越；扩展名缺失时根据
        内容类型补充，避免后续处理无扩展名文件。
        """

        safe_name = Path(file_name).name.strip() or f"telegram_{int(time.time())}"
        agent = self._ensure_agent()
        workspace = Path(agent.workspace_root).resolve()
        temp_workspace = getattr(agent, "_temp_workspace", None)
        root = getattr(temp_workspace, "root", None)
        # AgentTempWorkspace.root 已是 .agent_tmp 绝对路径；未初始化时回退到
        # 工作区默认位置。
        if root:
            dest_dir = Path(root)
        else:
            dest_dir = workspace / ".omnicrawl" / ".agent_tmp"
        if dest_dir.name != subdir:
            dest_dir = dest_dir / subdir
        dest_dir.mkdir(parents=True, exist_ok=True)
        candidate = dest_dir / safe_name
        counter = 1
        while candidate.exists():
            stem, suffix = candidate.stem, candidate.suffix
            candidate = dest_dir / f"{stem}_{counter}{suffix}"
            counter += 1
        return candidate

    def _relative_to_workspace(self, path: Path) -> str:
        """返回相对工作区的展示路径（统一正斜杠，便于 Agent 直接使用）。"""

        try:
            relative = path.resolve().relative_to(
                Path(self._ensure_agent().workspace_root).resolve()
            )
        except ValueError:
            return str(path)
        return str(relative).replace(os.sep, "/")

    def _handle_harness_command(self, text: str) -> str | None:
        """把 harness 管理命令交给命令注册表执行。

        返回回复文本；返回 None 表示不是本模块支持的 harness 命令。
        解析/匹配/执行统一在 omnicrawl.commands.slash 的注册表中，跟随 TUI
        的命令集合变化而不需要在这里重复维护。

        远程安全边界（审批仅 manual/review、禁止 auto）由命令处理器读取
        ``channel`` 后判断，与本入口的实现保持一致。
        """

        from omnicrawl.commands.slash import REGISTRY

        agent = self._ensure_agent()
        normalized = text.strip().casefold()

        # /resume latest：恢复最近活动的会话（跨端接力一步到位，
        # 免去先 /sessions 查 ID 再 /resume 的两步操作）。
        parts = normalized.split()
        if len(parts) == 2 and parts[0] == "/resume" and parts[1] == "latest":
            return self._resume_latest_session(agent)

        result = REGISTRY.dispatch(text, agent=agent, channel="telegram")
        if not result.handled:
            return None
        # 连接器本身运行在工作线程，慢命令（/mcp、/review 等）直接同步执行。
        resolved = result.resolve()
        return resolved.error or resolved.message

    def _resume_latest_session(self, agent: Any) -> str:
        """恢复最近活动的会话（list_sessions 按 updated_at 倒序，取第一条）。

        跨端接力一步到位：电脑 TUI 中最近对话的会话，手机 tg bot 直接
        /resume latest 即可继续，免去先 /sessions 查 ID 的两步操作。
        无会话时给出可操作提示；失败信息脱敏回传。
        """

        from omnicrawl.agent import AgentError

        try:
            sessions = agent.list_sessions(limit=1)
        except AgentError as exc:
            return f"会话列表读取失败：{exc}"
        if not sessions:
            return "还没有可恢复的会话。先在 TUI 或本 Bot 中发起对话。"
        latest = sessions[0]
        session_id = str(getattr(latest, "session_id", "") or "")
        title = str(getattr(latest, "title", "") or "") or "未命名会话"
        try:
            state = agent.resume_session(session_id)
        except AgentError as exc:
            return f"会话恢复失败：{exc}"
        message_count = len(getattr(state, "messages", []) or [])
        return (
            f"✅ 已恢复最近会话：{session_id}\n"
            f"标题：{title}\n"
            f"已恢复 {message_count} 条上下文消息，直接发送任务即可继续。"
        )

    def _help_text(self) -> str:
        return (
            "🤖 OmniCrawl 远程控制\n\n"
            "直接发送文本即可让 Agent 执行任务，例如：\n"
            "  「列出当前目录的文件」\n"
            "  「修复 README.md 中的错别字」\n\n"
            "任务控制：\n"
            "  /start  显示本帮助\n"
            "  /cancel  取消当前任务\n"
            "  /approve / /reject  批准/拒绝工具调用确认\n"
            "  /thinking on|off  思考内容显示开关（默认关）\n"
            "  /status  查看 harness 状态\n"
            "  /session 查看当前会话 ID\n"
            "  /reset   开启新会话\n"
            "  /workspace [路径]  查看/切换工作区（切换会同步到 TUI）\n"
            "  /plan  启用计划模式，后续任务先制定 Markdown 计划\n\n"
            "文件：\n"
            "  直接发送图片/文档/视频/语音/音频，自动存入 .agent_tmp\n"
            "  的 images/videos/audio/files/code/scripts 分类目录，\n"
            "  并交给 Agent 处理（可附 caption 说明任务）\n\n"
            "会话管理：\n"
            "  /sessions  最近会话\n"
            "  /archives  归档会话\n"
            "  /archive   归档当前会话\n"
            "  /resume <id> / /resume latest  恢复会话（latest 恢复最近活动）\n"
            "  /rename <标题>  重命名当前会话\n"
            "  /undo      回退最近一轮\n"
            "  /compact  压缩上下文\n"
            "  /history [关键词]  提示历史\n\n"
            "子系统状态：\n"
            "  /tasks / /task <id> [cancel]  后台子任务\n"
            "  /mcp   /plugins   /skills\n"
            "  /memory:clean  清理过期记忆\n"
            "  /reasoning [级别]  推理强度\n"
            "  /approval  查看审批模式\n"
            "  /approval:manual|review（远程不支持 auto）  切换审批模式\n\n"
            "敏感操作（bash/powershell 等）在手动/审查模式下会请求确认，超时自动拒绝；\n"
            "完全自动（auto）仅限本地 TUI，远程默认自动审查（review）。"
        )

    def _status_text(self) -> str:
        agent = self._ensure_agent()
        with self._lock:
            task = self._active_task
        busy = task is not None and task.thread is not None and task.thread.is_alive()
        lines = [
            "📊 OmniCrawl 状态",
            f"工作区：{getattr(agent, 'workspace_root', '?')}",
            f"会话 ID：{str(getattr(agent, 'current_session_id', '') or '')}",
            f"状态：{'🔄 正在执行任务' if busy else '✅ 空闲'}",
        ]
        if busy and task is not None:
            elapsed = int(time.time() - task.started_at)
            lines.append(f"任务已运行 {elapsed} 秒，/cancel 可取消。")
        return "\n".join(lines)

    # ------------------------------------------------------------------
    # 任务执行（单活动）
    # ------------------------------------------------------------------

    def _start_task(self, chat_id: int, user_id: int, text: str) -> None:
        """单活动校验后，在后台线程执行 Agent 任务。"""

        with self._lock:
            current = self._active_task
            if current is not None and current.thread is not None and current.thread.is_alive():
                self._send_message(
                    chat_id,
                    "🔄 当前已有任务正在执行，请等待完成或发送 /cancel 取消。",
                )
                return
            task = _ActiveTask(chat_id=chat_id, user_id=user_id, text=text)
            self._active_task = task
        thread = threading.Thread(
            target=self._execute_task,
            args=(task,),
            daemon=True,
            name="omnicrawl-telegram-task",
        )
        task.thread = thread
        thread.start()
        self._send_message(chat_id, "✅ 已收到任务，开始执行（/cancel 可取消）。")

    def _execute_task(self, task: _ActiveTask) -> None:
        """后台线程：调用 run_stream 执行任务并把结果回发。

        输出消息相互独立、互不合并：
        - 思考内容（/thinking on 时）：独立消息流式更新（🧠 前缀）。
        - 最终回答：独立消息流式更新（editMessageText 打字机效果）。
        - 工具调用、状态提示、确认请求：各自独立发送。
        """

        deltas: list[str] = []
        reasoning: list[str] = []
        last_status = ""
        stream_message_id: int | None = None
        thinking_message_id: int | None = None
        # 节流时间戳：Telegram 对同一条消息的编辑频率有限制，约 1 次/秒。
        stream_edit_at = 0.0
        thinking_edit_at = 0.0

        def check_cancelled() -> None:
            if task.cancel_event.is_set():
                raise TaskCancelled("任务已被用户取消。")

        def on_delta(delta: str) -> None:
            """回答增量：累积后按节流编辑同一条流式消息（打字机效果）。"""

            nonlocal stream_message_id, stream_edit_at
            deltas.append(delta)
            now = time.monotonic()
            if now - stream_edit_at < STREAM_EDIT_INTERVAL:
                return
            stream_edit_at = now
            display = self._truncate_for_stream("".join(deltas))
            if stream_message_id is None:
                stream_message_id = self._create_stream_message(task.chat_id, display)
            else:
                self._edit_stream_message(task.chat_id, stream_message_id, display)

        def on_reasoning(delta: str) -> None:
            """思考增量：仅 /thinking on 时显示，独立消息流式更新。"""

            nonlocal thinking_message_id, thinking_edit_at
            if not self._show_thinking:
                return
            reasoning.append(delta)
            now = time.monotonic()
            if now - thinking_edit_at < STREAM_EDIT_INTERVAL:
                return
            thinking_edit_at = now
            display = "🧠 " + self._truncate_for_stream("".join(reasoning))
            if thinking_message_id is None:
                thinking_message_id = self._create_stream_message(task.chat_id, display)
            else:
                self._edit_stream_message(task.chat_id, thinking_message_id, display)

        def on_status(message: str) -> None:
            nonlocal last_status
            if message == last_status:
                return
            last_status = message
            self._send_message(task.chat_id, f"⏳ {message}")

        def on_tool_start(step: int, tool_call: Any) -> None:
            name = getattr(tool_call, "name", "?")
            args = getattr(tool_call, "arguments", {}) or {}
            safe = redact_sensitive_values(dict(args)) if isinstance(args, dict) else args
            summary = json.dumps(safe, ensure_ascii=False)[:300]
            self._send_message(task.chat_id, f"🛠 正在执行：{name}({summary})")

        def on_tool_result(tool_call: Any, tool_result: Any) -> None:
            name = getattr(tool_call, "name", "?")
            ok = bool(getattr(tool_result, "ok", True))
            if ok:
                return
            output = redact_sensitive_text(str(getattr(tool_result, "output", "") or ""))[:300]
            self._send_message(task.chat_id, f"⚠️ {name} 执行失败：{output}")

        try:
            agent = self._ensure_agent()
            # 任务开始前把 TUI 侧持久化的推理强度/工作区同步到本进程。
            self._sync_runtime_config()
            result = agent.run_stream(
                task.text,
                on_delta,
                on_status=on_status,
                on_tool_start=on_tool_start,
                on_tool_result=on_tool_result,
                cancel_check=check_cancelled,
                on_reasoning_delta=on_reasoning,
            )
            check_cancelled()
            reply = result or "".join(deltas) or "（任务完成，无文本输出）"
            self._finalize_stream(task.chat_id, stream_message_id, f"✅ {reply}")
        except TaskCancelled:
            # 保留已流式输出的部分，取消提示单独一条消息。
            self._abort_stream(task.chat_id, stream_message_id, deltas, "⏹ 任务已取消。")
        except Exception as exc:  # noqa: BLE001 - 任何异常都回发给用户
            LOGGER.exception("Telegram 任务执行失败")
            # 保留已流式输出的部分（不覆盖丢失），错误单独一条消息。
            error_text = f"❌ 任务执行失败：{redact_sensitive_text(str(exc))}"
            self._abort_stream(task.chat_id, stream_message_id, deltas, error_text)
        finally:
            with self._lock:
                if self._active_task is task:
                    self._active_task = None

    # ------------------------------------------------------------------
    # 流式消息辅助（打字机效果）
    # ------------------------------------------------------------------

    def _create_stream_message(self, chat_id: int, text: str) -> int | None:
        """创建一条流式消息，返回 message_id；失败返回 None 不阻断任务。"""

        try:
            payload = self._api(
                "sendMessage",
                data={"chat_id": chat_id, "text": text},
            )
            result = payload.get("result") or {}
            return int(result.get("message_id") or 0) or None
        except TelegramAPIError as exc:
            LOGGER.warning("创建流式消息失败（chat=%s）：%s", chat_id, exc)
            return None

    def _edit_stream_message(self, chat_id: int, message_id: int | None, text: str) -> None:
        """编辑流式消息；失败仅记录日志，不阻断后续输出。"""

        if not message_id:
            return
        try:
            self._api(
                "editMessageText",
                data={"chat_id": chat_id, "message_id": message_id, "text": text},
            )
        except TelegramAPIError as exc:
            LOGGER.warning("编辑流式消息失败（chat=%s, msg=%s）：%s", chat_id, message_id, exc)

    @staticmethod
    def _truncate_for_stream(text: str, limit: int = MAX_MESSAGE_LEN - 20) -> str:
        """流式中途的显示截断：预留前缀/定型空间，避免编辑超限。"""

        if len(text) <= limit:
            return text
        return text[: limit - 1] + "…"

    def _abort_stream(
        self,
        chat_id: int,
        stream_message_id: int | None,
        deltas: list[str],
        error_text: str,
    ) -> None:
        """任务异常/取消时保留已流式输出的内容，错误信息单独发送。

        若直接把流式消息覆盖成错误文本，用户已经看到的输出会丢失；
        这里把已输出内容定型（截断 + 中断标记），错误单独一条消息。
        没有任何部分输出时退化为普通消息发送。
        """

        partial = "".join(deltas)
        if stream_message_id is not None and partial:
            suffix = "…（输出中断）"
            head = partial[: MAX_MESSAGE_LEN - len(suffix) - 1] + suffix
            self._edit_stream_message(chat_id, stream_message_id, head)
        self._send_message(chat_id, error_text)

    def _finalize_stream(
        self,
        chat_id: int,
        stream_message_id: int | None,
        text: str,
    ) -> None:
        """定型流式消息：短文本编辑同一条定型；超长或无流式消息则正常发送。"""

        if stream_message_id is not None and len(text) <= MAX_MESSAGE_LEN - 8:
            self._edit_stream_message(chat_id, stream_message_id, text)
            return
        if stream_message_id is not None:
            suffix = "…（完整内容见下一条）"
            head = text[: MAX_MESSAGE_LEN - len(suffix) - 1] + suffix
            self._edit_stream_message(chat_id, stream_message_id, head)
        self._send_message(chat_id, text)

    # ------------------------------------------------------------------
    # 工具确认（由 Agent 工作线程回调）
    # ------------------------------------------------------------------

    def _confirm_tool_call(self, tool_name: str, arguments: dict[str, Any]) -> bool:
        """Agent 请求确认时的回调：转发到 Telegram 等待 /approve 或 /reject。"""

        with self._lock:
            task = self._active_task
            if task is None:
                return False
            pending = _PendingConfirm(
                tool_name=tool_name,
                arguments=arguments,
                chat_id=task.chat_id,
                user_id=task.user_id,
            )
            self._pending_confirm = pending
        safe_args = redact_sensitive_values(dict(arguments or {}))
        prompt = (
            f"⚠️ 需要确认执行敏感操作：\n"
            f"工具：{tool_name}\n"
            f"参数：\n{json.dumps(safe_args, ensure_ascii=False, indent=2)[:1500]}\n"
            f"回复 /approve 允许，/reject 拒绝。"
            f"{int(self._confirm_timeout)} 秒内未回复将自动拒绝。"
        )
        self._send_message(task.chat_id, prompt)
        decided = pending.event.wait(self._confirm_timeout)
        with self._lock:
            if self._pending_confirm is pending:
                self._pending_confirm = None
        if not decided:
            self._send_message(task.chat_id, "⏰ 确认超时，已自动拒绝该操作。")
            return False
        return pending.decision

    def _handle_approval(self, chat_id: int, user_id: int, approve: bool) -> None:
        """处理 /approve 与 /reject：仅发起任务的白名单用户本人可批准/拒绝。

        群聊场景下多个白名单用户共享同一 chat，必须校验发起者，防止
        他人批准自己发起的敏感工具确认。
        """

        with self._lock:
            pending = self._pending_confirm
            if pending is None:
                return
            if pending.chat_id != chat_id or pending.user_id != user_id:
                return
            pending.decision = approve
            pending.event.set()
        action = "已批准" if approve else "已拒绝"
        self._send_message(chat_id, f"✅ {action}：{pending.tool_name}")

    def _request_cancel(self, chat_id: int) -> None:
        """请求取消当前任务；无活动任务时仅提示。

        取消时同步释放挂起的工具确认请求（按拒绝处理），否则确认回调
        会一直阻塞到超时（最长 300 秒），任务无法及时退出。
        """

        with self._lock:
            task = self._active_task
            pending = self._pending_confirm
        if task is None or task.thread is None or not task.thread.is_alive():
            self._send_message(chat_id, "当前没有正在执行的任务。")
            return
        task.cancel_event.set()
        if pending is not None and pending.chat_id == chat_id:
            pending.decision = False
            pending.event.set()
        self._send_message(chat_id, "⏹ 已请求取消当前任务，请稍候……")

    def _handle_thinking_command(self, chat_id: int, text: str) -> None:
        """处理 /thinking 命令：查看或切换思考内容显示。"""

        parts = text.split()
        if len(parts) == 1:
            state = "开启" if self._show_thinking else "关闭"
            self._send_message(
                chat_id,
                f"思考内容显示：{state}（默认关闭）。\n/thinking on 开启，/thinking off 关闭。",
            )
        elif parts[1].casefold() in ("on", "1", "true", "yes", "开", "开启"):
            self._show_thinking = True
            self._send_message(chat_id, "已开启思考内容显示（🧠 独立消息）。")
        elif parts[1].casefold() in ("off", "0", "false", "no", "关", "关闭"):
            self._show_thinking = False
            self._send_message(chat_id, "已关闭思考内容显示。")
        else:
            self._send_message(chat_id, "用法：/thinking on 或 /thinking off。")

    def _handle_workspace_command(self, chat_id: int, text: str) -> None:
        """处理 /workspace：查看或切换当前工作区目录（切换会持久化）。"""

        parts = text.split(None, 1)
        agent = self._ensure_agent()
        if len(parts) == 1 or not parts[1].strip():
            self._send_message(
                chat_id,
                f"当前工作区：{agent.workspace_root}\n用法：/workspace <路径>",
            )
            return
        path = parts[1].strip()
        try:
            agent.switch_workspace(path)
        except Exception as exc:  # noqa: BLE001 - 错误回发给用户
            self._send_message(
                chat_id,
                f"❌ 切换工作区失败：{redact_sensitive_text(str(exc))}",
            )
            return
        note = ""
        try:
            from omnicrawl.config.core.workspace import save_workspace_root

            saved = save_workspace_root(path)
            note = f"（已持久化到 {saved}）"
        except Exception as exc:  # noqa: BLE001 - 持久化失败不阻断切换
            note = f"（持久化失败：{exc}）"
        self._send_message(chat_id, f"✅ 已切换工作区：{agent.workspace_root}{note}")

    def _sync_runtime_config(self) -> None:
        """任务开始前把 TUI 侧持久化的运行配置同步到本进程 Agent。

        推理强度（config.toml [llm] reasoning_effort）、审批模式
        （config.toml [approval] mode，默认 review）与工作区
        （config.toml [workspace] root）都由 TUI 切换时写回；这里重读并
        应用到当前 Agent，实现 TUI ↔ tg bot 跨进程同步。Telegram 远程
        仅支持 manual/review，禁止 auto。

        注意：工作区不同时调用 switch_workspace 会**重建 Agent 子系统并
        清空当前对话上下文**（与 TUI /workspace 行为一致），因此每次任务
        前同步到新工作区 = 在 tg bot 端开始一段新上下文。

        优化：config.toml 未变化（mtime 相同）时跳过重读与应用，避免每个
        任务都做磁盘 I/O；文件缺失或无法 stat 时退化为每次都同步。
        任一同步失败只记录日志，不阻断任务。
        """

        agent = self._ensure_agent()

        # 配置 mtime 短路：仅当配置文件被修改过才重新读取与应用。
        try:
            from omnicrawl.config.core.runtime import resolve_config_path

            cfg_path = resolve_config_path()
            try:
                mtime = cfg_path.stat().st_mtime_ns if cfg_path.exists() else 0
            except OSError:
                mtime = 0
            if mtime and mtime == self._config_mtime:
                return  # 配置未变，跳过本次同步
            self._config_mtime = mtime
        except Exception:  # noqa: BLE001 - 无法定位配置文件时每次都同步
            pass

        # 推理强度同步：TUI /reasoning 与设置面板都调用 save_reasoning_effort。
        try:
            from omnicrawl.config.models.llm import load_llm_config

            disk_effort = (load_llm_config().reasoning_effort or "").strip()
            current_effort = str(getattr(agent, "reasoning_effort", "") or "").strip()
            if disk_effort and disk_effort != current_effort:
                agent.set_reasoning_effort(disk_effort)
                LOGGER.info(
                    "已同步推理强度：%s -> %s",
                    current_effort or "(默认)",
                    disk_effort,
                )
        except Exception as exc:  # noqa: BLE001 - 同步失败不阻断任务
            LOGGER.warning("同步推理强度失败：%s", exc)

        # 工作区同步：TUI /workspace 切换后写回 [workspace] root。
        try:
            from omnicrawl.config.core.workspace import load_workspace_root

            disk_root = load_workspace_root()
            if disk_root:
                current_root = str(getattr(agent, "workspace_root", "") or "")
                if os.path.normcase(os.path.normpath(disk_root)) != os.path.normcase(
                    os.path.normpath(current_root)
                ):
                    agent.switch_workspace(disk_root)
                    LOGGER.info("已同步工作区：%s", disk_root)
        except Exception as exc:  # noqa: BLE001
            LOGGER.warning("同步工作区失败：%s", exc)

        # 审批模式同步：本地 TUI（支持 manual/review/auto）或 Telegram
        #（仅 manual/review，禁止 auto）任何一端切换后都写回 config.toml；
        # Telegram 每次任务前重读并应用，保证跨进程一致，且重启后保持上次设置；
        # 默认自动审查（review）。若磁盘上为 auto（来自 TUI），Telegram 侧按 review
        # 生效，不自动放行高危操作。
        try:
            from omnicrawl.config.features.approval import (
                APPROVAL_MODE_AUTO,
                APPROVAL_MODE_REVIEW,
                load_approval_mode,
            )

            disk_mode = load_approval_mode()
            # Telegram 安全边界：远程始终禁止完全自动；磁盘上为 auto（来自本地 TUI）
            # 时，在 Telegram 进程内降级为 review 应用，保证高危操作不自动放行。
            effective_mode = APPROVAL_MODE_REVIEW if disk_mode == APPROVAL_MODE_AUTO else disk_mode
            current_mode = str(getattr(agent, "approval_mode", "") or "")
            if effective_mode and effective_mode != current_mode:
                agent.set_approval_mode(effective_mode)
                if effective_mode != disk_mode:
                    LOGGER.info(
                        "已同步审批模式（远程降级）：%s -> %s（磁盘为 %s，已按 review 生效）",
                        current_mode or "(空)",
                        effective_mode,
                        disk_mode,
                    )
                else:
                    LOGGER.info("已同步审批模式：%s -> %s", current_mode or "(空)", effective_mode)
        except Exception as exc:  # noqa: BLE001
            LOGGER.warning("同步审批模式失败：%s", exc)


def load_telegram_config() -> dict[str, Any]:
    """读取 Telegram 接入配置：环境变量优先，其次 config.toml [telegram] 段。

    环境变量名与配置 key 不是一一对应（TELEGRAM_BOT_TOKEN -> bot_token、
    TELEGRAM_ALLOWED_USER_IDS -> allowed_user_ids、
    TELEGRAM_CONFIRM_TIMEOUT -> confirmation_timeout_seconds），需要显式映射；
    allowed_user_ids 在 TOML 里是数组，也要与逗号分隔字符串兼容。
    未配置任何来源时返回默认空值，由 main() 给出引导提示。
    """

    section: dict[str, Any] = {}
    try:
        from omnicrawl.config.core.runtime import get_section, load_config_data

        section = get_section(load_config_data(), "telegram")
    except Exception:  # noqa: BLE001 - 配置文件缺失时退化为纯环境变量
        pass

    def env_or(env_name: str, config_key: str, default: Any = "") -> Any:
        """环境变量优先，否则读配置段中对应 key 的值。"""

        value = os.getenv(env_name, "")
        if value and value.strip():
            return value.strip()
        # 配置文件里显式写了该 key 才用它的值，缺省用 default。
        if config_key in section:
            return section[config_key]
        return default

    token = str(env_or("TELEGRAM_BOT_TOKEN", "bot_token", "")).strip()
    raw_allowed = env_or("TELEGRAM_ALLOWED_USER_IDS", "allowed_user_ids", "")
    allowed: list[int] = []
    if isinstance(raw_allowed, (list, tuple, set)):
        # TOML 数组形式：[123456789, 987654321]
        for item in raw_allowed:
            try:
                allowed.append(int(item))
            except (TypeError, ValueError) as exc:
                raise ValueError(f"Telegram 允许用户 ID 必须是整数：{item}") from exc
    else:
        # 环境变量逗号分隔字符串形式："123456789,987654321"
        for item in str(raw_allowed).replace("，", ",").split(","):
            item = item.strip()
            if not item:
                continue
            try:
                allowed.append(int(item))
            except ValueError as exc:
                raise ValueError(f"Telegram 允许用户 ID 必须是整数：{item}") from exc

    raw_timeout = env_or("TELEGRAM_CONFIRM_TIMEOUT", "confirmation_timeout_seconds", "300")
    try:
        confirm_timeout = float(raw_timeout)
    except (TypeError, ValueError) as exc:
        raise ValueError(
            "TELEGRAM_CONFIRM_TIMEOUT / telegram.confirmation_timeout_seconds 必须是数字。"
        ) from exc
    return {
        "bot_token": token,
        "allowed_user_ids": allowed,
        "confirm_timeout_seconds": confirm_timeout,
    }


def main() -> int:
    """独立入口：python -m omnicrawl.connectors.telegram。"""

    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    try:
        from omnicrawl.ui.windows_launcher import configure_console_encoding

        configure_console_encoding()
    except Exception:  # noqa: BLE001 - 非 Windows 或依赖缺失时跳过
        pass

    try:
        config = load_telegram_config()
        if not config["bot_token"]:
            print(
                "缺少 Telegram Bot Token：请设置环境变量 TELEGRAM_BOT_TOKEN，"
                "或在 config.toml 的 [telegram] 段填写 bot_token（由 @BotFather 创建）。"
            )
            return 1
        if not config["allowed_user_ids"]:
            print(
                "缺少授权用户 ID：请设置环境变量 TELEGRAM_ALLOWED_USER_IDS 或"
                " config.toml 的 telegram.allowed_user_ids（安全白名单，必填）。"
            )
            return 1
    except ValueError as exc:
        print(f"Telegram 配置错误：{exc}")
        return 1

    bot = TelegramAgentBot(
        config["bot_token"],
        config["allowed_user_ids"],
        confirm_timeout_seconds=config["confirm_timeout_seconds"],
    )
    try:
        bot.run_forever()
    except TelegramAPIError as exc:
        # run_forever 内部会重试可恢复错误，只有不可恢复错误才会抛出；
        # 无论哪种情况都以清晰消息退出，不输出原始 traceback。
        print(f"Telegram Bot 停止：{exc}")
        return 1
    except KeyboardInterrupt:
        print("\n已停止 Telegram Bot。")
    finally:
        bot.close()
    return 0


__all__ = [
    "TelegramAgentBot",
    "TelegramAPIError",
    "TaskCancelled",
    "load_telegram_config",
    "main",
]


if __name__ == "__main__":
    raise SystemExit(main())
