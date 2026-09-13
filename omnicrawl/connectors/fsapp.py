"""飞书机器人连接器：通过 lark-oapi WebSocket 长连接驱动 OmniCrawl Agent。

本模块参考 GenericAgent 的 ``frontends/fsapp.py``，但把任务执行部分适配为
OmniCrawl 的 ``LocalToolAgent.run_stream`` 协议：

* 飞书侧使用 ``lark.ws.Client`` 长连接接收 ``im.message.receive_v1`` 事件；
* 应用凭证优先从环境变量读取，其次读取 ``config.toml`` 的 ``[feishu]`` 段；
* 普通文本、图片和文件消息会转成 OmniCrawl Agent 任务；
* 显示方式对齐 TUI 消息流：每个条目（正文段、工具调用、思考、执行计划、
  子任务进度）独立成一条消息、按发生顺序出现；
* 敏感工具确认通过飞书回复 ``/approve`` 或 ``/reject`` 完成；
* 任务取消、会话命令、计划模式和审批模式命令复用 OmniCrawl 的共享实现；
* 单个进程只允许一个活动 Agent 回合，避免并发驱动同一个 Agent 实例造成上下文
  或 Session 状态竞争。

安装可选依赖：

    pip install lark-oapi

配置方式（环境变量优先）：

    FEISHU_APP_ID=cli_xxx
    FEISHU_APP_SECRET=xxx
    FEISHU_ALLOWED_USER_IDS=ou_xxx,ou_yyy
    FEISHU_CONFIRM_TIMEOUT=300

也可以在用户配置文件 ``~/.OmniCrawl/config.toml`` 中填写：

    [feishu]
    app_id = "cli_xxx"
    app_secret = "xxx"
    allowed_user_ids = ["ou_xxx"]
    confirmation_timeout_seconds = 300

为了兼容 GenericAgent 的配置习惯，本模块也接受根级别的
``fs_app_id``、``fs_app_secret`` 和 ``fs_allowed_users``。空白白名单表示允许
所有能找到该机器人的用户；生产环境建议始终配置明确的 ``ou_...`` 白名单。

启动：

    python -m omnicrawl.connectors.fsapp

检查配置（不会建立长连接）：

    python -m omnicrawl.connectors.fsapp --check
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
import threading
import time

from dataclasses import dataclass, field
from difflib import SequenceMatcher
from pathlib import Path
from typing import Any, Iterable, Mapping

from omnicrawl.agent.toolkit.tools import ASK_USER_TOOL_NAME, TODO_TOOL_NAME
from omnicrawl.state.session_artifacts import redact_sensitive_text, redact_sensitive_values
from omnicrawl.ui.tool_labels import format_duration, format_tool_status

from omnicrawl.connectors.feishu_inbox import FeishuInbox

# lark-oapi 是可选依赖：通过模块级 __getattr__（PEP 562）惰性加载，未安装或
# 未实际使用（如 --check、TUI 自动启动的配置探测）时不付出导入成本；首次
# 访问时导入并缓存到模块命名空间，此后与普通导入无异。
_LARK_REQUEST_NAMES = (
    "CreateFileRequest",
    "CreateFileRequestBody",
    "CreateImageRequest",
    "CreateImageRequestBody",
    "CreateMessageRequest",
    "CreateMessageRequestBody",
    "GetMessageResourceRequest",
    "PatchMessageRequest",
    "PatchMessageRequestBody",
)


def __getattr__(name: str) -> Any:
    """惰性加载 lark-oapi 模块与其 IM 请求类（首次访问时导入并缓存）。"""

    if name == "lark":
        try:
            import lark_oapi as lark_module
        except ImportError as exc:
            raise AttributeError(name) from exc
        globals()["lark"] = lark_module
        return lark_module
    if name in _LARK_REQUEST_NAMES:
        try:
            from lark_oapi.api.im.v1 import (  # noqa: PLC0415 - 惰性导入
                CreateFileRequest,
                CreateFileRequestBody,
                CreateImageRequest,
                CreateImageRequestBody,
                CreateMessageRequest,
                CreateMessageRequestBody,
                GetMessageResourceRequest,
                PatchMessageRequest,
                PatchMessageRequestBody,
            )
        except ImportError as exc:
            raise AttributeError(name) from exc
        loaded = {
            "CreateFileRequest": CreateFileRequest,
            "CreateFileRequestBody": CreateFileRequestBody,
            "CreateImageRequest": CreateImageRequest,
            "CreateImageRequestBody": CreateImageRequestBody,
            "CreateMessageRequest": CreateMessageRequest,
            "CreateMessageRequestBody": CreateMessageRequestBody,
            "GetMessageResourceRequest": GetMessageResourceRequest,
            "PatchMessageRequest": PatchMessageRequest,
            "PatchMessageRequestBody": PatchMessageRequestBody,
        }
        globals().update(loaded)
        return loaded[name]
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


LOGGER = logging.getLogger(__name__)

# 长连接断线后的指数退避范围。短暂网络抖动不会导致进程退出，认证/配置错误
# 也会持续输出日志，便于管理员在飞书后台修正后自动恢复。
RECONNECT_INITIAL_SECONDS = 5.0
RECONNECT_MAX_SECONDS = 120.0

# 下载飞书消息资源的文件名只用于展示和临时目录落盘，必须移除路径部分。
_SAFE_FILENAME_FALLBACK = "feishu_file"

# 扩展可能返回内部展示片段；飞书只发送对用户可读的正文。
_DISPLAY_TAG_PATTERN = re.compile(
    r"<(?:thinking|summary|tool_use|file_content)>.*?</(?:thinking|summary|tool_use|file_content)>",
    flags=re.DOTALL,
)
_FILE_MARKER_PATTERN = re.compile(r"\[FILE:([^\]]+)\]")

_IMAGE_EXTENSIONS = frozenset(
    {".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".ico", ".tif", ".tiff", ".svg"}
)
_AUDIO_EXTENSIONS = frozenset(
    {".opus", ".mp3", ".wav", ".m4a", ".aac", ".ogg", ".oga", ".flac", ".wma", ".mid", ".midi"}
)
_VIDEO_EXTENSIONS = frozenset({".mp4", ".mov", ".avi", ".mkv", ".webm", ".flv", ".wmv"})
_FILE_TYPE_MAP = {
    ".opus": "opus",
    ".mp4": "mp4",
    ".pdf": "pdf",
    ".doc": "doc",
    ".docx": "doc",
    ".xls": "xls",
    ".xlsx": "xls",
    ".ppt": "ppt",
    ".pptx": "ppt",
}

_MESSAGE_RESOURCE_TYPES = frozenset({"image", "audio", "file", "media"})

# 飞书单条文本消息的上限约 4000 字符，保守取 3000 以便分段后仍有余量；
# 卡片正文同样受平台限制，超长内容先分段再发送。
MAX_TEXT_CHARS = 3000

# ----------------------------------------------------------------------
# 显示规则（对齐 TUI 的消息流语义）
# ----------------------------------------------------------------------

# 与 TUI 的消息流一致：每个条目（正文段、工具调用、思考、计划、子任务）独立
# 成一条消息，按发生顺序出现，各自原地更新；正文单条上限之外的部分在封口时
# 改用文本消息分片补发。
SEGMENT_MAX_CHARS = 6000
# 流式 patch 节拍（秒）：飞书对消息更新有频率限制，不做逐字 patch。
STREAM_PATCH_INTERVAL_SECONDS = 1.5
# 工具正文采样：与 TUI 一致，最多 5 行，超出时保留首尾各 2 行有效行。
TOOL_BODY_MAX_LINES = 5
TOOL_BODY_HEAD_LINES = 2
TOOL_BODY_TAIL_LINES = 2
TOOL_BODY_MAX_CHARS_PER_LINE = 160
# 文件变更预览行数与结果摘要行数。
FILE_CHANGE_PREVIEW_LINES = 5
FILE_CHANGE_RESULT_LINES = 1
# 折叠思考：运行中只显示最新 5 行，定型后保留最多 4000 字符。
REASONING_PREVIEW_LINES = 5
REASONING_MAX_CHARS = 4000
# 执行计划与子任务进度消息的最大行数。
MAX_TODO_LINES = 20
MAX_SUBAGENT_LINES = 20
# 工具摘要中路径/目标/命令/参数的压缩上限。
MAX_PATH_CHARS = 48
MAX_PATTERN_CHARS = 36
MAX_COMMAND_CHARS = 120
MAX_ARGS_CHARS = 120

# 记忆与知识库工具：正文对远程用户没有展示价值，与 TUI 一致只保留摘要行。
_MEMORY_TOOL_OPERATIONS = frozenset(
    f"memory_{action}" for action in ("search", "read", "expand_related", "write")
)
_KB_TOOL_OPERATIONS = frozenset({"kb_search", "kb_read", "kb_write", "kb_append", "kb_list"})
# 正文完全隐藏的工具（与 TUI 的 HIDDEN_BODY_TOOLS 同规则）。
_HIDDEN_BODY_OPERATIONS = frozenset({"read"}) | _MEMORY_TOOL_OPERATIONS | _KB_TOOL_OPERATIONS
# 文件变更工具：展示变更统计与变更预览，而不是采样后的原始输出。
_FILE_CHANGE_OPERATIONS = frozenset({"write_file", "Edit_file"})
# 提问与执行计划不产生工具消息：提问有独立卡片，计划由独立消息原地更新。
_TOOLS_WITHOUT_RECORD = frozenset({ASK_USER_TOOL_NAME, TODO_TOOL_NAME})

# 子任务进度：状态图标与中文标签沿用 TUI 进度树的取值。
_SUBAGENT_TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled"})
_SUBAGENT_STATUS_PRESENTATION = {
    "queued": ("○", "等待中"),
    "running": ("●", "运行中"),
    "waiting_approval": ("◆", "等待审批"),
    "completed": ("✓", "完成"),
    "failed": ("×", "失败"),
    "cancelled": ("–", "已取消"),
}
_SUBAGENT_EVENT_STATUS = {
    "subagent.task.queued": "queued",
    "subagent.task.started": "running",
    "subagent.task.running": "running",
    "subagent.task.waiting_approval": "waiting_approval",
    "subagent.task.completed": "completed",
    "subagent.task.failed": "failed",
    "subagent.task.cancelled": "cancelled",
    "subagent.task.approval_cancelled": "cancelled",
}
# 工具记录终态：不再被迟到的完成事件或取消收口覆盖。
_TOOL_TERMINAL_STATUSES = frozenset({"成功", "失败", "已取消"})

# WebSocket 重连后某些事件可能再次投递。进程内短期去重足够覆盖常见重连
# 场景，同时不会把长期会话状态写入全局配置。
_DEDUP_TTL_SECONDS = 10 * 60
_DEDUP_MAX_ENTRIES = 2000


def _operation_of(tool_name: str) -> str:
    """返回工具末级操作名，兼容 ``server.operation`` 形式的 MCP 工具。"""

    return str(tool_name or "").rsplit(".", 1)[-1]


def _safe_label(value: Any, *, max_chars: int) -> str:
    """折叠空白并限制长度，避免超长文本撑爆卡片字段。"""

    return " ".join(str(value or "").split())[:max_chars]


def _compact_line(value: Any, *, max_chars: int) -> str:
    """把参数压缩成单行摘要；超长时截断并附加省略号。"""

    text = " ".join(str(value or "").split())
    if not text:
        return ""
    if len(text) <= max_chars:
        return text
    return text[: max_chars - 1] + "…"


def _clip_line(line: str, *, max_chars: int = TOOL_BODY_MAX_CHARS_PER_LINE) -> str:
    """限制单行宽度，避免一行超长输出在卡片里横向溢出。"""

    text = str(line or "").rstrip()
    if len(text) <= max_chars:
        return text
    return text[: max_chars - 1] + "…"


def _fenced_body(body: str) -> str:
    """把工具正文放入围栏代码块；正文内部的三反引号先替换掉。"""

    return "```\n" + str(body or "").replace("```", "'''") + "\n```"


def _sample_output_lines(output: str) -> list[str]:
    """按 TUI 规则采样工具输出：最多 5 行，超出时保留首尾各 2 行有效行。"""

    lines = [_clip_line(line) for line in str(output or "").splitlines()]
    if len(lines) <= TOOL_BODY_MAX_LINES:
        return lines
    effective = [line for line in lines if line.strip()]
    if len(effective) <= TOOL_BODY_MAX_LINES:
        return effective
    return effective[:TOOL_BODY_HEAD_LINES] + effective[-TOOL_BODY_TAIL_LINES:]


def _read_result_line_range(result_text: str) -> tuple[int, int] | None:
    """从 read 的行号输出中解析实际返回的首尾源码行号（与 TUI 同规则）。"""

    if not result_text:
        return None
    line_numbers = [
        int(match) for match in re.findall(r"(?m)^\s*(\d+):\s", result_text)
    ]
    if line_numbers:
        return line_numbers[0], line_numbers[-1]
    return None


def _list_result_summary(result_text: str) -> str | None:
    """把目录列表结果压缩为「N 项」摘要（与 TUI 同规则）。"""

    if not result_text:
        return None
    lines = [line.strip() for line in result_text.splitlines() if line.strip()]
    if not lines or lines == ["目录为空。"]:
        return "0 项"
    truncated = any(line.startswith("...") for line in lines)
    visible_count = sum(not line.startswith("...") for line in lines)
    return f"{visible_count}{'+' if truncated else ''} 项"


def _format_line_stats(*, added: int, removed: int) -> str:
    parts: list[str] = []
    if added:
        parts.append(f"+{added}")
    if removed:
        parts.append(f"-{removed}")
    return " ".join(parts) if parts else "0"


def _diff_preview_lines(old_text: str, new_text: str) -> tuple[list[str], int, int]:
    """生成带 ``+``/``-`` 前缀的变更预览，并返回完整改动的 (added, removed)。"""

    old_lines = old_text.splitlines()
    new_lines = new_text.splitlines()
    matcher = SequenceMatcher(a=old_lines, b=new_lines, autojunk=False)
    preview: list[str] = []
    added = removed = 0
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            continue
        if tag in {"replace", "delete"}:
            removed += i2 - i1
            preview.extend(f"- {line}" for line in old_lines[i1:i2])
        if tag in {"replace", "insert"}:
            added += j2 - j1
            preview.extend(f"+ {line}" for line in new_lines[j1:j2])
    return preview, added, removed


def _file_change_summary(operation: str, arguments: Mapping[str, Any]) -> str:
    """文件变更统计：Edit_file 用 ``+N -M``，write_file 用 ``rewrite +N lines``。"""

    if operation == "Edit_file":
        _preview, added, removed = _diff_preview_lines(
            str(arguments.get("old_text") or ""),
            str(arguments.get("new_text") or ""),
        )
        return _format_line_stats(added=added, removed=removed)
    content = str(arguments.get("content") or "")
    line_count = 0 if content == "" else len(content.splitlines())
    label = "append" if str(arguments.get("mode") or "").casefold() == "append" else "rewrite"
    return f"{label} +{line_count} lines"


def _file_change_preview(operation: str, arguments: Mapping[str, Any]) -> str:
    """文件变更预览：Edit_file 输出 +/- diff，write_file 输出新增行。"""

    if operation == "Edit_file":
        preview, _added, _removed = _diff_preview_lines(
            str(arguments.get("old_text") or ""),
            str(arguments.get("new_text") or ""),
        )
    else:
        content = str(arguments.get("content") or "")
        preview = [f"+ {line}" for line in content.splitlines()] or ["+ (empty)"]
    visible = preview[:FILE_CHANGE_PREVIEW_LINES]
    body = "\n".join(_clip_line(line) for line in visible)
    omitted = len(preview) - len(visible)
    if omitted > 0:
        body += f"\n… 还有 {omitted} 行未展示"
    return body


def _tool_summary(tool_name: str, arguments: Any, result_text: str = "") -> str:
    """生成工具记录标题摘要（工具名 + 关键参数 + 结果摘要），与 TUI 同规则。"""

    name = str(tool_name or "?")
    operation = _operation_of(name)
    args = arguments if isinstance(arguments, Mapping) else {}
    if operation == "list":
        summary = f"{name} {_compact_line(args.get('path') or '.', max_chars=MAX_PATH_CHARS)}"
        count = _list_result_summary(result_text)
        return f"{summary} · {count}" if count else summary
    if operation == "read":
        summary = f"{name} {_compact_line(args.get('path') or '(未指定文件)', max_chars=MAX_PATH_CHARS)}"
        line_range = _read_result_line_range(result_text)
        if line_range is not None:
            summary += f" · 第 {line_range[0]}-{line_range[1]} 行"
        return summary
    if operation == "read_image":
        path = _compact_line(args.get("path") or "(未指定图片)", max_chars=MAX_PATH_CHARS)
        return f"{name} {path} · 图片"
    if operation in {"find", "grep"}:
        path = _compact_line(args.get("path") or ".", max_chars=MAX_PATH_CHARS)
        target = _compact_line(args.get("pattern"), max_chars=MAX_PATTERN_CHARS) or "(未指定)"
        return f"{name} {path} · 目标: {target}"
    if operation in {"bash", "powershell"}:
        command = _compact_line(args.get("command"), max_chars=MAX_COMMAND_CHARS)
        return f"{name} {command}".rstrip()
    if operation in _FILE_CHANGE_OPERATIONS:
        path = _compact_line(args.get("path") or "(unknown path)", max_chars=MAX_PATH_CHARS)
        return f"{name} {path} · {_file_change_summary(operation, args)}"
    if operation == "monitor":
        context = _compact_line(args.get("action") or "任务", max_chars=MAX_PATTERN_CHARS)
        detail = _compact_line(
            args.get("command") or args.get("monitor_id"),
            max_chars=MAX_COMMAND_CHARS,
        )
        return f"{name} {context} {detail}".rstrip()
    if operation == "subagent":
        context = _compact_line(args.get("action") or "任务", max_chars=MAX_PATTERN_CHARS)
        tasks = args.get("tasks")
        count = len(tasks) if isinstance(tasks, list) else 0
        return f"{name} {context}" + (f" · {count} 项" if count else "")
    if operation in _MEMORY_TOOL_OPERATIONS:
        query = _compact_line(args.get("query"), max_chars=MAX_PATH_CHARS)
        if query:
            return f"{name} {query}"
        ids = args.get("memory_ids")
        if isinstance(ids, list):
            return f"{name} {len(ids)} 条记忆"
        return name
    if operation in _KB_TOOL_OPERATIONS:
        detail = _compact_line(
            args.get("query") or args.get("path"),
            max_chars=MAX_PATH_CHARS,
        )
        return f"{name} {detail}".rstrip()
    # 其余工具（含 MCP）：附带紧凑参数摘要，便于远程判断这次调用了什么。
    safe = redact_sensitive_values(dict(args)) if args else {}
    payload = (
        _compact_line(json.dumps(safe, ensure_ascii=False), max_chars=MAX_ARGS_CHARS)
        if safe
        else ""
    )
    return f"{name} {payload}".rstrip()


def _tool_body(tool_name: str, arguments: Any, result_text: str) -> str:
    """生成工具记录正文：与 TUI 一致展示原始输出，或隐藏/展示文件变更预览。"""

    operation = _operation_of(tool_name)
    args = arguments if isinstance(arguments, Mapping) else {}
    if operation in _HIDDEN_BODY_OPERATIONS:
        return ""
    if operation in _FILE_CHANGE_OPERATIONS:
        preview = _file_change_preview(operation, args)
        note = _file_change_result_note(operation, result_text)
        return f"{preview}\n{note}" if note else preview
    return "\n".join(_sample_output_lines(result_text)).strip("\n")


def _file_change_result_note(operation: str, result_text: str) -> str:
    """文件变更工具的结果摘要：Edit_file 只保留「替换 N 处」（与 TUI 一致）。"""

    if operation == "Edit_file":
        match = re.search(r"替换\s*\d+\s*处", str(result_text or ""))
        if match:
            return match.group(0)
    return next(
        (line for line in _sample_output_lines(result_text) if line.strip()),
        "",
    )


def _split_segment_for_card(text: str) -> tuple[str, str]:
    """把长正文切成「单条消息正文」与「需要文本消息补发的剩余部分」。"""

    if len(text) <= SEGMENT_MAX_CHARS:
        return text, ""
    head = text[:SEGMENT_MAX_CHARS]
    for boundary in ("\n\n", "\n"):
        index = head.rfind(boundary)
        if index > SEGMENT_MAX_CHARS // 2:
            head = head[:index]
            break
    return head, text[len(head):].lstrip("\n")


def _format_elapsed(seconds: float) -> str:
    """子任务耗时格式，与 TUI 进度树一致（MM:SS / HH:MM:SS）。"""

    total_seconds = max(0, int(seconds))
    hours, remainder = divmod(total_seconds, 3600)
    minutes, seconds_part = divmod(remainder, 60)
    if hours:
        return f"{hours:02d}:{minutes:02d}:{seconds_part:02d}"
    return f"{minutes:02d}:{seconds_part:02d}"


class FeishuDependencyError(RuntimeError):
    """启动飞书连接器前缺少 lark-oapi 依赖。"""


class FeishuTaskCancelled(RuntimeError):
    """当前飞书任务被用户或连接器生命周期主动取消。"""


@dataclass
class _PendingUserQuestion:
    """等待飞书用户回答的一个 ask_user 请求。"""

    question_id: str
    kind: str
    question: str
    options: tuple[str, ...]
    receive_id: str
    receive_id_type: str
    sender_open_id: str
    event: threading.Event = field(default_factory=threading.Event)
    answer: str | None = None
    message_id: str | None = None


@dataclass(frozen=True)
class FeishuConfig:
    """飞书连接器运行配置；不保存任何日志中应隐藏的凭证副本。"""

    app_id: str
    app_secret: str
    allowed_user_ids: frozenset[str] = frozenset()
    confirmation_timeout_seconds: float = 300.0

    @property
    def public_access(self) -> bool:
        """空白白名单保持 GenericAgent 兼容行为，但启动时会发出警告。"""

        return not self.allowed_user_ids or "*" in self.allowed_user_ids


@dataclass
class _PendingConfirmation:
    """等待飞书用户批准的一个敏感工具调用。"""

    tool_name: str
    arguments: dict[str, Any]
    receive_id: str
    receive_id_type: str
    sender_open_id: str
    event: threading.Event = field(default_factory=threading.Event)
    decision: bool = False


@dataclass
class _ActiveTask:
    """当前唯一活动 Agent 回合的最小状态。"""

    receive_id: str
    receive_id_type: str
    sender_open_id: str
    text: str
    cancel_event: threading.Event = field(default_factory=threading.Event)
    thread: threading.Thread | None = None
    started_at: float = field(default_factory=time.time)
    # 持久入站队列去重键；由队列 worker 填充，任务终结时用于确认（出队）。
    dedupe_key: str | None = None


@dataclass
class _ToolRecord:
    """一条工具调用记录，展示语义与 TUI 工具卡一致。"""

    key: str
    name: str
    summary: str
    arguments: Any = None
    status: str = "调用中"
    started_at: float = field(default_factory=time.monotonic)
    finished_at: float | None = None
    body: str = ""

    @property
    def running(self) -> bool:
        return self.status == "调用中"

    @property
    def duration_seconds(self) -> float:
        ended = self.finished_at if self.finished_at is not None else time.monotonic()
        return max(0.0, ended - self.started_at)

    def render(self, *, with_body: bool = True) -> str:
        """渲染为 markdown：``● 摘要 · ✓ 成功 · 136ms``，正文放围栏代码块。

        运行中不带耗时：本地不做逐秒 patch，冻结的耗时会误导；收口时再给出
        真实时长。摘要与正文发往飞书前统一做敏感信息脱敏。
        """

        status = format_tool_status(self.status)
        line = f"● {redact_sensitive_text(self.summary)} · {status.icon} {status.label}"
        if self.finished_at is not None:
            line += f" · {format_duration(self.duration_seconds)}"
        if not with_body or not self.body:
            return line
        return f"{line}\n{_fenced_body(redact_sensitive_text(self.body))}"

    def finish(self, *, ok: bool, output: str) -> None:
        self.summary = _tool_summary(self.name, self.arguments, output)
        self.status = "成功" if ok else "失败"
        self.body = _tool_body(self.name, self.arguments, output)
        self.finished_at = time.monotonic()
        # 结果落地后释放参数：长参数不再随卡片常驻内存。
        self.arguments = None

    def abort(self) -> None:
        """把尚未完成的记录收口为「已取消」，避免长期停在「调用中」。"""

        if self.status in _TOOL_TERMINAL_STATUSES:
            return
        self.status = "已取消"
        self.finished_at = time.monotonic()


@dataclass
class _SubAgentNode:
    """子任务进度节点；只保留安全字段，不展示 prompt 或结果。"""

    key: str
    agent_type: str
    description: str
    status: str = "queued"
    first_seen_at: float = field(default_factory=time.monotonic)
    started_at: float | None = None
    finished_at: float | None = None


def _markdown_card(content: str) -> str:
    """把一段 Markdown 组装为单元素卡片消息。"""

    return _card_json([{"tag": "markdown", "content": content}])


def _reasoning_panel(text: str, *, streaming: bool) -> dict[str, Any]:
    """思考折叠面板：运行中只显示最新五行，收口后给完整内容。"""

    cleaned = _clean_text(text)
    if streaming:
        lines = [line for line in cleaned.splitlines() if line.strip()]
        preview = "\n".join(lines[-REASONING_PREVIEW_LINES:])
        hint = f"· 正在思考，仅显示最新 {REASONING_PREVIEW_LINES} 行"
        content = f"{preview}\n\n{hint}" if preview else hint
    else:
        content = cleaned
        if len(content) > REASONING_MAX_CHARS:
            content = "…（更早内容已省略）\n" + content[-REASONING_MAX_CHARS:]
    return {
        "tag": "collapsible_panel",
        "expanded": False,
        "header": {"title": {"tag": "plain_text", "content": "💭 思考内容"}},
        "elements": [{"tag": "markdown", "content": content or "（无内容）"}],
    }


def _normalize_todos(items: Any) -> list[tuple[str, bool]]:
    """规范化执行计划清单：过滤空步骤并限制条数。"""

    normalized: list[tuple[str, bool]] = []
    if isinstance(items, (list, tuple)):
        for item in items[:MAX_TODO_LINES]:
            if not isinstance(item, Mapping):
                continue
            text = str(
                item.get("step") or item.get("description") or item.get("title") or ""
            ).strip()
            if not text:
                continue
            completed = bool(item.get("completed")) or str(
                item.get("status") or ""
            ).casefold() in {"completed", "done", "complete"}
            normalized.append((" ".join(text.split())[:240], completed))
    return normalized


def _todos_text(todos: list[tuple[str, bool]]) -> str:
    completed = sum(1 for _step, done in todos if done)
    lines = [f"**执行计划 · {completed}/{len(todos)} 完成**"]
    lines.extend(f"{'▣' if done else '▢'} {step}" for step, done in todos)
    return "\n".join(lines)


def _subagents_text(order: list[str], nodes: Mapping[str, _SubAgentNode]) -> str:
    """子任务进度段：根标签 + ├─/└─ 节点，与 TUI 进度树同形。"""

    total = len(order)
    completed = sum(1 for key in order if nodes[key].status == "completed")
    root = "◇ 并行子任务" if total > 1 else "◇ 子任务进度"
    lines = [f"**{root} · {completed}/{total} 完成**"]
    now = time.monotonic()
    visible = order[:MAX_SUBAGENT_LINES]
    for index, key in enumerate(visible):
        node = nodes[key]
        icon, label = _SUBAGENT_STATUS_PRESENTATION.get(
            node.status,
            ("·", node.status),
        )
        connector = "└─" if index == total - 1 else "├─"
        line = f"{connector} {icon} {node.description} · {node.agent_type} · {label}"
        if node.started_at is not None:
            ended = node.finished_at if node.finished_at is not None else now
            line += f" · {_format_elapsed(ended - node.started_at)}"
        lines.append(line)
    if total > len(visible):
        lines.append(f"… 还有 {total - len(visible)} 个任务")
    return "\n".join(lines)


class _TimelineMessage:
    """时间线条目消息：首次发送卡片，之后原地 patch 同一条消息。

    与 TUI 的消息流一致：每个条目独立占一条消息、按发生顺序出现；条目自身
    的流式更新只影响自己的消息，条目之间互不覆盖。
    """

    def __init__(self, bot: "FeishuBot", receive_id: str, receive_id_type: str) -> None:
        self._bot = bot
        self.receive_id = receive_id
        self.receive_id_type = receive_id_type
        self.message_id: str | None = None
        self.available = True

    def _deliver(self, payload: str) -> bool:
        """把卡片内容交给飞书：首次发送，之后 patch 原消息。"""

        if not self.available:
            return False
        if self.message_id:
            return self._bot._patch_card(self.message_id, payload)
        message_id = self._bot._send_raw(
            self.receive_id,
            payload,
            msg_type="interactive",
            receive_id_type=self.receive_id_type,
        )
        if not message_id:
            self.available = False
            return False
        self.message_id = message_id
        return True


class _TextMessage(_TimelineMessage):
    """一个模型 pass 的正文消息：``◇`` 前缀流式更新，工具调用处封口。

    TUI 在每次工具调用处把回答记录封口、下一段另起一条；这里用独立消息承载
    同一语义：封口后本条消息不再变化，下一段正文由新的消息承接。
    """

    def stream(self, text: str) -> bool:
        """流式刷新正文预览（节拍由调用方控制）。"""

        head, _tail = _split_segment_for_card(_clean_text(text))
        if not head:
            # 首片内容可能只剩内部标签，避免先发一条空消息。
            return False
        return self._deliver(_markdown_card(f"◇ {head}"))

    def seal(self, text: str, *, suffix: str = "") -> str | None:
        """封口本条正文；返回仍需以文本消息补发的剩余内容。

        卡片不可用时返回全文，调用方回退为普通文本消息，保证正文不会因为
        卡片接口失败而丢失。
        """

        cleaned = _clean_text(text)
        if not cleaned:
            return None
        if not self.available:
            return f"{cleaned}{suffix}"
        head, tail = _split_segment_for_card(cleaned)
        if tail:
            head += "\n\n…（内容较长，其余部分以消息形式发送）"
        if not self._deliver(_markdown_card(f"◇ {head}{suffix}")):
            self.available = False
            return f"{cleaned}{suffix}"
        return tail or None


class _ToolMessage(_TimelineMessage):
    """一次工具调用的独立消息：开始即出现，完成时原地收口。

    与 TUI 工具卡一致：调用一开始就进入消息流（``… 调用中``），结果落地后
    在同一张卡上补齐状态、耗时与采样正文；卡片不可用时以文本消息兜底整条
    记录，避免结果丢失。
    """

    def __init__(
        self,
        bot: "FeishuBot",
        receive_id: str,
        receive_id_type: str,
        record: _ToolRecord,
    ) -> None:
        super().__init__(bot, receive_id, receive_id_type)
        self.record = record

    @property
    def running(self) -> bool:
        return self.record.running

    def start(self) -> bool:
        return self._deliver(_markdown_card(self.record.render(with_body=False)))

    def finish(self, *, ok: bool, output: str) -> bool:
        self.record.finish(ok=ok, output=output)
        content = self.record.render(with_body=True)
        if self._deliver(_markdown_card(content)):
            return True
        # 卡片创建或更新失败：整条记录改用文本消息兜底，用户仍能看到结果。
        self.available = False
        self._bot._send_text(self.receive_id, content, receive_id_type=self.receive_id_type)
        return False

    def abort(self) -> bool:
        if not self.running:
            return False
        self.record.abort()
        if self.message_id is None:
            return False
        return self._deliver(_markdown_card(self.record.render(with_body=False)))


class _ReasoningMessage(_TimelineMessage):
    """一个 pass 的思考折叠面板消息（仅 ``/thinking on`` 时创建）。"""

    def stream(self, text: str) -> bool:
        return self._deliver(_card_json([_reasoning_panel(text, streaming=True)]))

    def seal(self, text: str) -> bool:
        return self._deliver(_card_json([_reasoning_panel(text, streaming=False)]))


class _PlanMessage(_TimelineMessage):
    """执行计划消息：首次更新时出现，之后原地替换整份清单。"""

    def __init__(self, bot: "FeishuBot", receive_id: str, receive_id_type: str) -> None:
        super().__init__(bot, receive_id, receive_id_type)
        self.todos: list[tuple[str, bool]] = []

    def update(self, items: Any) -> bool:
        normalized = _normalize_todos(items)
        if not normalized or normalized == self.todos:
            return False
        self.todos = normalized
        return self._deliver(_markdown_card(_todos_text(self.todos)))


class _SubAgentMessage(_TimelineMessage):
    """同一批子任务的进度消息：首次事件出现，之后原地更新进度树。"""

    def __init__(self, bot: "FeishuBot", receive_id: str, receive_id_type: str) -> None:
        super().__init__(bot, receive_id, receive_id_type)
        self._nodes: dict[str, _SubAgentNode] = {}
        self._order: list[str] = []

    def update(self, event_name: str, payload: Mapping[str, Any]) -> bool:
        """按 task_id 原地更新子任务节点；终态节点拒绝迟到的活动事件。"""

        status = _SUBAGENT_EVENT_STATUS.get(str(event_name))
        if status is None:
            return False
        task_id = str(payload.get("task_id") or "task")
        now = time.monotonic()
        node = self._nodes.get(task_id)
        if node is None:
            node = _SubAgentNode(
                key=task_id,
                agent_type=_safe_label(
                    payload.get("agent_type") or "subagent",
                    max_chars=80,
                ),
                description=_safe_label(
                    payload.get("description") or task_id,
                    max_chars=120,
                ),
            )
            self._nodes[task_id] = node
            self._order.append(task_id)
        elif node.status in _SUBAGENT_TERMINAL_STATUSES:
            return False
        else:
            node.agent_type = _safe_label(
                payload.get("agent_type") or node.agent_type,
                max_chars=80,
            )
            node.description = _safe_label(
                payload.get("description") or node.description,
                max_chars=120,
            )
        node.status = status
        if status in {"running", "waiting_approval"} and node.started_at is None:
            node.started_at = now
        if status in _SUBAGENT_TERMINAL_STATUSES:
            if node.started_at is None:
                node.started_at = node.first_seen_at
            node.finished_at = now
        return self._deliver(_markdown_card(_subagents_text(self._order, self._nodes)))


def _require_lark() -> Any:
    try:
        lark_module = __getattr__("lark")
    except AttributeError as exc:
        raise FeishuDependencyError(
            "缺少可选依赖 lark-oapi，请先执行：pip install lark-oapi"
        ) from exc
    return lark_module


def _require_im_requests() -> None:
    """把 lark-oapi IM 请求类加载进模块 globals。

    模块级 ``__getattr__``（PEP 562）只拦截属性访问，函数体内的裸名引用
    走 globals 查找、不会触发它；因此必须先通过属性访问显式触发一次加载，
    之后的 ``CreateMessageRequest`` 等裸名才能解析。
    """

    for name in _LARK_REQUEST_NAMES:
        __getattr__(name)


def _card_json(elements: list[dict[str, Any]]) -> str:
    """生成飞书卡片 JSON；使用 ensure_ascii=False 保留中文可读性。"""

    return json.dumps(
        {
            "schema": "2.0",
            "config": {"streaming_mode": False, "width_mode": "fill"},
            "body": {"elements": elements},
        },
        ensure_ascii=False,
    )


def _question_card_json(question: _PendingUserQuestion) -> str:
    """生成 ask_user 选项卡片；按钮值只携带不可变问题 ID 和答案。"""

    elements: list[dict[str, Any]] = [
        {"tag": "markdown", "content": f"**{question.question}**"},
    ]
    for answer in question.options:
        elements.append(
            {
                "tag": "button",
                "text": {"tag": "plain_text", "content": answer},
                "type": "primary",
                "value": {
                    "type": "ask_user",
                    "question_id": question.question_id,
                    "answer": answer,
                },
            }
        )
    return _card_json(elements)


def _question_resolved_card_json(question: _PendingUserQuestion, status: str) -> str:
    """生成已终结的提问卡片：只保留状态行，移除所有可选按钮。

    超时/已作答/取消后的选项按钮若继续留在聊天里，用户点击只会得到
    “该问题已处理或已失效”的失败提示，观感上是“选项依然可点”。这里用
    ``_patch_card`` 原地把卡片改写为只读状态，按钮不再出现。
    """

    return _card_json(
        [
            {
                "tag": "markdown",
                "content": redact_sensitive_text(f"**{question.question}**\n{status}"),
            }
        ]
    )


def _split_text(text: str) -> list[str]:
    """把文本整理为待发送片段：空文本返回空列表，超长文本按安全上限分段。

    飞书单条文本消息有平台侧大小限制，长回答整段发送会被截断；这里在
    ``MAX_TEXT_CHARS`` 上限内优先按段落/列表项边界切分，保留可读性。
    """

    if not text:
        return []
    cleaned = text.rstrip("\n") or text
    if len(cleaned) <= MAX_TEXT_CHARS:
        return [cleaned]

    parts: list[str] = []
    current = ""
    for line in cleaned.split("\n"):
        # 段落边界（空行）和列表项是天然的切分点，避免把一段话从中间切断。
        boundary = line == "" or line.startswith(("-", "*", "#", ">"))
        candidate = f"{current}\n{line}" if current else line
        if len(candidate) > MAX_TEXT_CHARS or (boundary and current and len(current) >= MAX_TEXT_CHARS // 2):
            if current:
                parts.append(current)
                current = ""
        if current:
            current = f"{current}\n{line}" if current else line
        else:
            current = line
    if current:
        parts.append(current)
    return parts


def _clean_text(text: Any) -> str:
    """清理内部展示标签、脱敏敏感值，并折叠空行与行尾空格。

    文本仍保留原始换行结构（表格/代码块依赖它）；空文本返回空串，由
    调用方决定兜底文案（流式预览不需要兜底，定型回答需要）。
    """

    raw = str(text or "")
    cleaned = _DISPLAY_TAG_PATTERN.sub("", raw).strip()
    cleaned = redact_sensitive_text(cleaned)
    # 折叠 3 个及以上连续空行为最多 2 个，并清理行尾空格（不影响代码块/表格）。
    cleaned = re.sub(r"\n{3,}", "\n\n", cleaned)
    cleaned = re.sub(r"[ \t]+\n", "\n", cleaned)
    return cleaned


def _display_text(text: Any) -> str:
    """清理文本并给空响应提供可读兜底（定型回答与兜底文本共用）。"""

    return _clean_text(text) or "（任务完成，无文本输出）"


def _resolve_final_text(streamed: str, reply: str) -> str:
    """正文定型：流式片段不完整时用最终回答补齐，否则沿用已展示内容。

    最终回答是模型最后一次回复的文本；当它比流式片段更完整（忽略空白后以
    流式片段为前缀）时改用最终回答，其余情况保留用户已经看过的流式内容，
    避免定型时正文突然变化，也避免重复展示早先段落。
    """

    streamed_text = _clean_text(streamed)
    reply_text = _clean_text(reply)
    if not streamed_text:
        return reply_text
    if not reply_text:
        return streamed_text
    squeezed_streamed = "".join(streamed_text.split())
    squeezed_reply = "".join(reply_text.split())
    if squeezed_reply.startswith(squeezed_streamed):
        return reply_text
    return streamed_text


def _parse_json(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    if not isinstance(value, str) or not value.strip():
        return {}
    try:
        parsed = json.loads(value)
    except (TypeError, ValueError):
        return {}
    return parsed if isinstance(parsed, dict) else {}


def _field(value: Any, name: str, default: Any = None) -> Any:
    """读取 SDK 对象或字典事件的同名字段。"""

    if isinstance(value, Mapping):
        return value.get(name, default)
    return getattr(value, name, default)


def _text_value(value: Any) -> str:
    if isinstance(value, dict):
        return str(value.get("text") or value.get("content") or "").strip()
    return str(value or "").strip()


def _coerce_string_set(value: Any) -> frozenset[str]:
    """兼容 TOML 数组和环境变量逗号字符串，并统一 open_id 大小写外的空白。"""

    if value is None:
        return frozenset()
    if isinstance(value, str):
        values: Iterable[Any] = value.replace("，", ",").split(",")
    elif isinstance(value, Iterable) and not isinstance(value, (bytes, bytearray, Mapping)):
        values = value
    else:
        values = [value]
    return frozenset(
        str(item).strip()
        for item in values
        if str(item).strip()
    )


def _first_nonempty(*values: Any) -> Any:
    for value in values:
        if isinstance(value, str):
            if value.strip():
                return value.strip()
        elif value is not None:
            return value
    return ""


def _load_runtime_config() -> dict[str, Any]:
    """读取 OmniCrawl TOML；配置不存在时退化为空对象。"""

    try:
        from omnicrawl.config.core.runtime import load_config_data

        data = load_config_data()
    except Exception as exc:  # noqa: BLE001 - 环境变量仍可独立驱动连接器
        LOGGER.warning("读取 OmniCrawl config.toml 失败，将仅使用环境变量：%s", exc)
        return {}
    return data if isinstance(data, dict) else {}


def load_feishu_config() -> FeishuConfig:
    """按环境变量优先、``[feishu]`` 次之的规则加载飞书配置。"""

    data = _load_runtime_config()
    section = data.get("feishu", {})
    if not isinstance(section, dict):
        raise ValueError("config.toml 的 [feishu] 必须是对象。")

    app_id = str(
        _first_nonempty(
            os.getenv("FEISHU_APP_ID", ""),
            section.get("app_id"),
            section.get("fs_app_id"),
            data.get("fs_app_id"),
        )
        or ""
    ).strip()
    app_secret = str(
        _first_nonempty(
            os.getenv("FEISHU_APP_SECRET", ""),
            section.get("app_secret"),
            section.get("fs_app_secret"),
            data.get("fs_app_secret"),
        )
        or ""
    ).strip()

    raw_allowed = _first_nonempty(
        os.getenv("FEISHU_ALLOWED_USER_IDS", ""),
        section.get("allowed_user_ids"),
        section.get("allowed_users"),
        section.get("fs_allowed_users"),
        data.get("fs_allowed_users"),
    )
    allowed_user_ids = _coerce_string_set(raw_allowed)

    raw_timeout = _first_nonempty(
        os.getenv("FEISHU_CONFIRM_TIMEOUT", ""),
        section.get("confirmation_timeout_seconds"),
        section.get("confirm_timeout_seconds"),
        300,
    )
    try:
        timeout = max(1.0, float(raw_timeout))
    except (TypeError, ValueError) as exc:
        raise ValueError(
            "FEISHU_CONFIRM_TIMEOUT / feishu.confirmation_timeout_seconds 必须是数字。"
        ) from exc

    return FeishuConfig(
        app_id=app_id,
        app_secret=app_secret,
        allowed_user_ids=allowed_user_ids,
        confirmation_timeout_seconds=timeout,
    )


def _mask_secret(value: str) -> str:
    value = str(value or "")
    if len(value) <= 8:
        return "*" * len(value)
    return f"{value[:4]}{'*' * (len(value) - 8)}{value[-4:]}"


def check_config(*, init_agent: bool = False) -> dict[str, Any]:
    """返回不泄露 Secret 的配置诊断信息。"""

    try:
        config = load_feishu_config()
    except Exception as exc:  # noqa: BLE001 - --check 应返回可读诊断
        return {"ready": False, "error": str(exc)}

    result: dict[str, Any] = {
        "app_id": config.app_id,
        "app_secret": _mask_secret(config.app_secret),
        "app_secret_present": bool(config.app_secret),
        "allowed_users": sorted(config.allowed_user_ids),
        "public_access": config.public_access,
        "confirmation_timeout_seconds": config.confirmation_timeout_seconds,
        "ready": bool(config.app_id and config.app_secret),
    }
    if init_agent:
        try:
            agent = FeishuBot(config)._ensure_agent()
            result["agent_ready"] = True
            result["workspace"] = str(getattr(agent, "workspace_root", ""))
        except Exception as exc:  # noqa: BLE001 - 诊断命令保留错误摘要
            result["agent_ready"] = False
            result["agent_error"] = redact_sensitive_text(str(exc))
    return result


class FeishuBot:
    """通过飞书长连接远程驱动一个 OmniCrawl ``LocalToolAgent``。"""

    def __init__(
        self,
        config: FeishuConfig,
        *,
        agent: Any | None = None,
        inbox: FeishuInbox | None = None,
        inbox_root: Path | None = None,
    ) -> None:
        self.config = config
        self._client: Any | None = None
        self._ws_client: Any | None = None
        self._agent = agent
        self._agent_confirm_bound = False
        self._agent_ask_user_bound = False
        self._stopped = threading.Event()
        self._lock = threading.RLock()
        self._active_task: _ActiveTask | None = None
        self._pending_confirmation: _PendingConfirmation | None = None
        self._pending_user_question: _PendingUserQuestion | None = None
        self._seen_messages: dict[str, float] = {}
        self._show_thinking = False
        # 持久入站队列：外部可注入（测试/嵌入方），否则按 inbox_root（或默认
        # 用户配置目录）惰性创建。用于先落盘再分发、跨重启去重与排队串行。
        self._inbox = inbox
        self._inbox_root = inbox_root
        self._pump_lock = threading.Lock()

    def _get_inbox(self) -> FeishuInbox:
        """惰性创建并返回持久入站队列。"""

        with self._lock:
            if self._inbox is None:
                root = self._inbox_root
                if root is None:
                    try:
                        from omnicrawl.config.core.runtime import user_config_dir

                        root = user_config_dir() / "connector-inbox" / "feishu"
                    except Exception:  # noqa: BLE001 - 拿不到目录时退化为内存
                        root = None
                self._inbox = FeishuInbox(root=root)
            return self._inbox

    # ------------------------------------------------------------------
    # Agent 与生命周期
    # ------------------------------------------------------------------

    def _ensure_agent(self) -> Any:
        """惰性创建 Agent，并绑定工具审批和 ask_user 回调。"""

        with self._lock:
            if self._agent is None:
                from omnicrawl.api.app import create_default_agent

                self._agent = create_default_agent()
            if not self._agent_confirm_bound:
                setter = getattr(self._agent, "set_confirm_handler", None)
                if not callable(setter):
                    raise RuntimeError("当前 Agent 不支持工具确认回调。")
                setter(self._confirm_tool_call)
                self._agent_confirm_bound = True
            if not self._agent_ask_user_bound:
                setter = getattr(self._agent, "set_ask_user_handler", None)
                if not callable(setter):
                    raise RuntimeError("当前 Agent 不支持 ask_user 回调。")
                setter(self._ask_user)
                self._agent_ask_user_bound = True
            return self._agent

    def close(self) -> None:
        """停止任务、释放挂起审批、停止长连接并关闭 Agent 资源。"""

        self._stopped.set()
        with self._lock:
            task = self._active_task
            pending = self._pending_confirmation
            question = self._pending_user_question
            ws_client = self._ws_client
        if task is not None:
            task.cancel_event.set()
        if pending is not None:
            pending.decision = False
            pending.event.set()
        if question is not None:
            question.answer = None
            question.event.set()

        for method_name in ("stop", "close"):
            method = getattr(ws_client, method_name, None)
            if callable(method):
                try:
                    method()
                except Exception:  # noqa: BLE001 - 退出阶段只记录
                    LOGGER.debug("停止飞书 WebSocket 客户端失败", exc_info=True)
                break

        if task is not None and task.thread is not None and task.thread.is_alive():
            task.thread.join(timeout=10.0)
        if self._agent is not None:
            try:
                self._agent.close()
            except Exception:  # noqa: BLE001 - 退出阶段不覆盖原始错误
                LOGGER.exception("关闭 OmniCrawl Agent 失败")
        inbox = self._inbox
        if inbox is not None:
            try:
                inbox.close()
            except Exception:  # noqa: BLE001 - 退出阶段只记录
                LOGGER.debug("关闭飞书入站队列失败", exc_info=True)

    # ------------------------------------------------------------------
    # 飞书消息 API
    # ------------------------------------------------------------------

    def _prewarm_agent(self) -> None:
        """后台线程预构建 Agent，避免阻塞 WebSocket 建连。

        惰性路径（``_ensure_agent``）在每条消息处理前都会调用，因此预热失败
        不会让机器人失联：首个消息会再次尝试构建，失败时按既有异常处理回传
        错误卡片。预热线程只负责提前把常用的 Agent 准备就绪。
        """

        def _build() -> None:
            try:
                self._ensure_agent()
            except Exception:  # noqa: BLE001 - 预热失败只记录，首个消息会重试
                LOGGER.exception(
                    "飞书连接器预构建 Agent 失败，将在收到首个消息时重试"
                )

        thread = threading.Thread(
            target=_build,
            name="omnicrawl-feishu-agent-prewarm",
            daemon=True,
        )
        thread.start()

    def _create_client(self) -> Any:
        sdk = _require_lark()
        # 客户端就绪前把 IM 请求类加载进 globals：函数体内的裸名不触发
        # 模块级 __getattr__，漏加载会在首次发消息时 NameError。
        _require_im_requests()
        return (
            sdk.Client.builder()
            .app_id(self.config.app_id)
            .app_secret(self.config.app_secret)
            .log_level(sdk.LogLevel.INFO)
            .build()
        )

    def _send_raw(
        self,
        receive_id: str,
        payload: str,
        *,
        msg_type: str = "text",
        receive_id_type: str = "open_id",
    ) -> str | None:
        """创建飞书消息，返回 message_id；发送失败只记录并返回 None。"""

        if not receive_id or self._client is None:
            return None
        try:
            request = (
                CreateMessageRequest.builder()
                .receive_id_type(receive_id_type)
                .request_body(
                    CreateMessageRequestBody.builder()
                    .receive_id(str(receive_id))
                    .msg_type(msg_type)
                    .content(payload)
                    .build()
                )
                .build()
            )
            response = self._client.im.v1.message.create(request)
            if response.success():
                data = getattr(response, "data", None)
                return str(getattr(data, "message_id", "") or "") or None
            LOGGER.warning(
                "发送飞书消息失败（receive_id=%s, code=%s, msg=%s）",
                receive_id,
                getattr(response, "code", ""),
                getattr(response, "msg", ""),
            )
        except Exception:  # noqa: BLE001 - 发送失败不能阻断 Agent 回合
            LOGGER.exception("发送飞书消息异常（receive_id=%s）", receive_id)
        return None

    def _send_text(self, receive_id: str, text: str, *, receive_id_type: str) -> bool:
        """发送普通文本消息，并返回所有片段是否发送成功。"""

        parts = _split_text(redact_sensitive_text(str(text or "")))
        if not parts:
            return False
        sent = True
        for part in parts:
            sent = (
                self._send_raw(
                    receive_id,
                    json.dumps({"text": part}, ensure_ascii=False),
                    msg_type="text",
                    receive_id_type=receive_id_type,
                )
                is not None
            ) and sent
        return sent

    def _patch_card(self, message_id: str, card_payload: str) -> bool:
        if not message_id or self._client is None:
            return False
        try:
            request = (
                PatchMessageRequest.builder()
                .message_id(message_id)
                .request_body(
                    PatchMessageRequestBody.builder()
                    .content(card_payload)
                    .build()
                )
                .build()
            )
            response = self._client.im.v1.message.patch(request)
            if response.success():
                return True
            LOGGER.warning(
                "更新飞书任务卡片失败（message_id=%s, code=%s, msg=%s）",
                message_id,
                getattr(response, "code", ""),
                getattr(response, "msg", ""),
            )
        except Exception:  # noqa: BLE001 - 卡片失败由普通文本兜底
            LOGGER.exception("更新飞书任务卡片异常（message_id=%s）", message_id)
        return False

    def _upload_image(self, file_path: Path) -> str | None:
        if self._client is None:
            return None
        try:
            with file_path.open("rb") as file:
                request = (
                    CreateImageRequest.builder()
                    .request_body(
                        CreateImageRequestBody.builder()
                        .image_type("message")
                        .image(file)
                        .build()
                    )
                    .build()
                )
                response = self._client.im.v1.image.create(request)
            if response.success():
                return str(getattr(getattr(response, "data", None), "image_key", "") or "") or None
            LOGGER.warning("上传飞书图片失败：%s %s", response.code, response.msg)
        except Exception:  # noqa: BLE001
            LOGGER.exception("上传飞书图片异常：%s", file_path)
        return None

    def _upload_file(self, file_path: Path) -> str | None:
        if self._client is None:
            return None
        file_type = _FILE_TYPE_MAP.get(file_path.suffix.casefold(), "stream")
        try:
            with file_path.open("rb") as file:
                request = (
                    CreateFileRequest.builder()
                    .request_body(
                        CreateFileRequestBody.builder()
                        .file_type(file_type)
                        .file_name(file_path.name)
                        .file(file)
                        .build()
                    )
                    .build()
                )
                response = self._client.im.v1.file.create(request)
            if response.success():
                return str(getattr(getattr(response, "data", None), "file_key", "") or "") or None
            LOGGER.warning("上传飞书文件失败：%s %s", response.code, response.msg)
        except Exception:  # noqa: BLE001
            LOGGER.exception("上传飞书文件异常：%s", file_path)
        return None

    def _send_local_file(self, receive_id: str, file_path: str, *, receive_id_type: str) -> bool:
        """把 Agent 输出的 ``[FILE:path]`` 文件上传回飞书。"""

        path = Path(file_path).expanduser()
        try:
            path = path.resolve(strict=True)
        except OSError:
            self._send_text(receive_id, f"⚠️ 文件不存在：{file_path}", receive_id_type=receive_id_type)
            return False
        if not path.is_file():
            self._send_text(receive_id, f"⚠️ 输出路径不是文件：{file_path}", receive_id_type=receive_id_type)
            return False

        suffix = path.suffix.casefold()
        if suffix in _IMAGE_EXTENSIONS:
            image_key = self._upload_image(path)
            if image_key:
                self._send_raw(
                    receive_id,
                    json.dumps({"image_key": image_key}, ensure_ascii=False),
                    msg_type="image",
                    receive_id_type=receive_id_type,
                )
                return True
        else:
            file_key = self._upload_file(path)
            if file_key:
                msg_type = "media" if suffix in _AUDIO_EXTENSIONS | _VIDEO_EXTENSIONS else "file"
                self._send_raw(
                    receive_id,
                    json.dumps({"file_key": file_key}, ensure_ascii=False),
                    msg_type=msg_type,
                    receive_id_type=receive_id_type,
                )
                return True
        self._send_text(receive_id, f"⚠️ 文件发送失败：{path.name}", receive_id_type=receive_id_type)
        return False

    def _send_generated_files(
        self,
        receive_id: str,
        raw_text: str,
        *,
        receive_id_type: str,
    ) -> None:
        for match in _FILE_MARKER_PATTERN.finditer(raw_text or ""):
            self._send_local_file(
                receive_id,
                match.group(1).strip(),
                receive_id_type=receive_id_type,
            )

    # ------------------------------------------------------------------
    # 飞书消息资源与临时文件
    # ------------------------------------------------------------------

    @staticmethod
    def _classify_filename(filename: str) -> str:
        suffix = Path(filename).suffix.casefold()
        if suffix in _IMAGE_EXTENSIONS:
            return "images"
        if suffix in _AUDIO_EXTENSIONS:
            return "audio"
        if suffix in _VIDEO_EXTENSIONS:
            return "videos"
        if suffix in {".py", ".js", ".ts", ".sh", ".ps1", ".bat", ".cmd", ".rb", ".lua"}:
            return "scripts"
        if suffix in {
            ".c", ".cpp", ".h", ".hpp", ".java", ".go", ".rs", ".cs", ".json",
            ".toml", ".yaml", ".yml", ".xml", ".html", ".css", ".sql", ".md",
            ".ini", ".cfg", ".csv",
        }:
            return "code"
        return "files"

    def _resolve_temp_destination(self, filename: str) -> Path:
        """把飞书资源保存到 Agent 临时目录，并防止文件名路径穿越。"""

        agent = self._ensure_agent()
        temp_workspace = getattr(agent, "_temp_workspace", None)
        root = getattr(temp_workspace, "root", None)
        if root is None:
            root = Path(agent.workspace_root) / ".omnicrawl" / ".agent_tmp"
        root = Path(root).resolve()
        category = self._classify_filename(filename)
        category_dir = (root / category).resolve()
        if root not in category_dir.parents:
            raise RuntimeError("飞书文件分类目录越出 Agent 临时目录。")
        category_dir.mkdir(parents=True, exist_ok=True)

        safe_name = Path(str(filename or "")).name.strip() or _SAFE_FILENAME_FALLBACK
        candidate = category_dir / safe_name
        index = 1
        while candidate.exists():
            candidate = category_dir / f"{candidate.stem}_{index}{candidate.suffix}"
            index += 1
        return candidate

    def _download_message_resource(
        self,
        message_id: str,
        file_key: str,
        resource_type: str,
    ) -> tuple[bytes, str] | None:
        if self._client is None or not message_id or not file_key:
            return None
        if resource_type == "audio":
            # 飞书消息资源接口对语音同样使用 file 类型读取二进制内容。
            resource_type = "file"
        try:
            request = (
                GetMessageResourceRequest.builder()
                .message_id(message_id)
                .file_key(file_key)
                .type(resource_type)
                .build()
            )
            response = self._client.im.v1.message_resource.get(request)
            if not response.success():
                LOGGER.warning(
                    "下载飞书消息资源失败（message_id=%s, code=%s, msg=%s）",
                    message_id,
                    getattr(response, "code", ""),
                    getattr(response, "msg", ""),
                )
                return None
            raw_file = getattr(response, "file", None)
            data = raw_file.read() if hasattr(raw_file, "read") else raw_file
            if not isinstance(data, (bytes, bytearray)):
                return None
            name = str(getattr(response, "file_name", "") or file_key)
            if resource_type == "file" and not Path(name).suffix and name == file_key:
                name = f"{name}.bin"
            return bytes(data), name
        except Exception:  # noqa: BLE001
            LOGGER.exception("下载飞书消息资源异常（message_id=%s）", message_id)
            return None

    def _save_message_resource(
        self,
        message_id: str,
        content: dict[str, Any],
        message_type: str,
    ) -> Path | None:
        file_key = str(content.get("file_key") or content.get("image_key") or "").strip()
        if not file_key:
            return None
        result = self._download_message_resource(message_id, file_key, message_type)
        if result is None:
            return None
        data, filename = result
        if message_type == "image" and not Path(filename).suffix:
            filename = f"{filename}.jpg"
        if message_type == "audio" and not Path(filename).suffix:
            filename = f"{filename}.opus"
        destination = self._resolve_temp_destination(filename)
        destination.write_bytes(data)
        return destination

    @staticmethod
    def _post_text_and_images(content: dict[str, Any]) -> tuple[str, list[str]]:
        """提取飞书 post 多语言结构中的文字和图片 key。"""

        def parse_block(block: Any) -> tuple[str, list[str]]:
            if not isinstance(block, dict):
                return "", []
            rows = block.get("content")
            if not isinstance(rows, list):
                return "", []
            texts: list[str] = []
            images: list[str] = []
            title = block.get("title")
            if title:
                texts.append(str(title))
            for row in rows:
                if not isinstance(row, list):
                    continue
                for element in row:
                    if not isinstance(element, dict):
                        continue
                    tag = element.get("tag")
                    if tag in {"text", "a"} and element.get("text"):
                        texts.append(str(element["text"]))
                    elif tag == "at":
                        texts.append(f"@{element.get('user_name', 'user')}")
                    elif tag == "img" and element.get("image_key"):
                        images.append(str(element["image_key"]))
            return " ".join(texts).strip(), images

        root: Any = content.get("post", content)
        if not isinstance(root, dict):
            return "", []
        candidates = [root]
        for language in ("zh_cn", "en_us", "ja_jp"):
            if isinstance(root.get(language), dict):
                candidates.insert(0, root[language])
        for candidate in candidates:
            text, images = parse_block(candidate)
            if text or images:
                return text, images
        return "", []

    def _build_user_message(
        self,
        message: Any,
    ) -> tuple[str, list[Path]]:
        """把飞书消息转换为 Agent 可理解的文本和本地文件路径。"""

        message_type = str(getattr(message, "message_type", "") or "")
        message_id = str(getattr(message, "message_id", "") or "")
        content = _parse_json(getattr(message, "content", ""))
        parts: list[str] = []
        local_files: list[Path] = []

        if message_type == "text":
            text = _text_value(content.get("text"))
            if text:
                parts.append(text)
        elif message_type == "post":
            text, image_keys = self._post_text_and_images(content)
            if text:
                parts.append(text)
            for image_key in image_keys:
                path = self._save_message_resource(
                    message_id,
                    {"image_key": image_key},
                    "image",
                )
                if path is None:
                    parts.append("[飞书图片下载失败]")
                else:
                    local_files.append(path)
        elif message_type in _MESSAGE_RESOURCE_TYPES:
            path = self._save_message_resource(message_id, content, message_type)
            if path is None:
                parts.append(f"[飞书 {message_type} 下载失败]")
            else:
                local_files.append(path)
        elif message_type in {
            "share_chat",
            "share_user",
            "interactive",
            "share_calendar_event",
            "system",
            "merge_forward",
        }:
            parts.append(f"[飞书消息类型：{message_type}]")
        else:
            parts.append(f"[飞书消息类型：{message_type or 'unknown'}]")

        for path in local_files:
            try:
                relative = path.relative_to(Path(self._ensure_agent().workspace_root).resolve())
                display_path = str(relative).replace(os.sep, "/")
            except ValueError:
                display_path = str(path)
            parts.append(
                f"已收到飞书文件：位于 {display_path}。如需分析图片或文件，请使用可用工具读取该路径。"
            )
        return "\n".join(part for part in parts if part).strip(), local_files

    # ------------------------------------------------------------------
    # 事件接收与消息分派
    # ------------------------------------------------------------------

    def _claim_message_once(self, message_id: str) -> bool:
        if not message_id:
            return True
        now = time.time()
        with self._lock:
            expired = [
                key
                for key, timestamp in self._seen_messages.items()
                if now - timestamp > _DEDUP_TTL_SECONDS
            ]
            for key in expired:
                self._seen_messages.pop(key, None)
            if len(self._seen_messages) >= _DEDUP_MAX_ENTRIES:
                oldest = sorted(self._seen_messages.items(), key=lambda item: item[1])
                for key, _ in oldest[: max(1, len(oldest) - _DEDUP_MAX_ENTRIES + 1)]:
                    self._seen_messages.pop(key, None)
            if message_id in self._seen_messages:
                return False
            self._seen_messages[message_id] = now
        return True

    @staticmethod
    def _sender_open_id(event: Any) -> str:
        sender = _field(event, "sender", None)
        sender_id = _field(sender, "sender_id", None)
        return str(_field(sender_id, "open_id", "") or "").strip()

    def _is_allowed(self, open_id: str) -> bool:
        return self.config.public_access or bool(open_id and open_id in self.config.allowed_user_ids)

    def handle_message(self, data: Any) -> None:
        """飞书 ``im.message.receive_v1`` 事件回调。"""

        try:
            event = _field(data, "event", None)
            message = _field(event, "message", None)
            if message is None:
                return
            message_id = str(_field(message, "message_id", "") or "")
            if not self._claim_message_once(message_id):
                LOGGER.info("忽略重复飞书消息：%s", message_id)
                return

            open_id = self._sender_open_id(event)
            if not self._is_allowed(open_id):
                LOGGER.warning("忽略未授权飞书用户：%s", open_id or "(unknown)")
                return

            chat_id = str(_field(message, "chat_id", "") or "").strip()
            receive_id = chat_id or open_id
            receive_id_type = "chat_id" if chat_id else "open_id"
            user_text, _local_files = self._build_user_message(message)
            if self._answer_pending_user_question(
                receive_id,
                receive_id_type,
                open_id,
                user_text,
                message_type=str(getattr(message, "message_type", "") or ""),
            ):
                return
            if not user_text:
                self._send_text(
                    receive_id,
                    f"⚠️ 暂不支持处理此类飞书消息：{_field(message, 'message_type', 'unknown')}",
                    receive_id_type=receive_id_type,
                )
                return

            LOGGER.info(
                "收到飞书消息（user=%s, type=%s）：%s",
                open_id or "(unknown)",
                _field(message, "message_type", "unknown"),
                user_text[:200],
            )
            is_command = (
                str(_field(message, "message_type", "") or "") == "text"
                and user_text.startswith("/")
            )
            if is_command:
                self._spawn(
                    self._dispatch,
                    receive_id,
                    receive_id_type,
                    open_id,
                    user_text,
                )
                return

            # 普通任务消息：先持久化入队（崩溃/重启不丢、跨重启去重），再由
            # 串行 worker 依次执行；任务执行期间到达的消息会排队而不是被拒绝。
            dedupe_key = self._inbox_dedupe_key(event, message, user_text)
            if not self._get_inbox().enqueue(
                event_id=message_id,
                dedupe_key=dedupe_key,
                payload={
                    "receive_id": receive_id,
                    "receive_id_type": receive_id_type,
                    "sender_open_id": open_id,
                    "text": user_text,
                },
            ):
                LOGGER.info("忽略已处理的飞书消息：%s", dedupe_key)
                return
            self._send_text(
                receive_id,
                "📥 已收到任务，正在排队执行（/cancel 可取消，发送 /status 查看队列）。",
                receive_id_type=receive_id_type,
            )
            self._pump()
        except Exception:  # noqa: BLE001 - 单条事件失败不能杀死 SDK 长连接
            LOGGER.exception("处理飞书消息事件失败")

    def _inbox_dedupe_key(self, event: Any, message: Any, user_text: str) -> str:
        """计算跨重启去重键。

        飞书在重连/重试时可能用新的 ``message_id`` 重投同一条逻辑消息
        （openclaw #46778），只按 message_id 去重会漏。对文本消息使用
        sender + chat + create_time + 内容哈希的稳定指纹；缺字段时回退到
        message_id。图片/文件等媒体仍按 message_id 去重。
        """

        import hashlib

        message_type = str(getattr(message, "message_type", "") or "")
        message_id = str(_field(message, "message_id", "") or "")
        if message_type != "text":
            return message_id or f"type:{message_type}"
        create_time = str(_field(message, "create_time", "") or "").strip()
        chat_id = str(_field(message, "chat_id", "") or "").strip()
        sender = _field(event, "sender", None)
        sender_id = _field(sender, "sender_id", None)
        open_id = str(_field(sender_id, "open_id", "") or "").strip()
        if not (create_time and chat_id and open_id and user_text):
            return message_id or "text:unknown"
        digest = hashlib.sha256(user_text.encode("utf-8")).hexdigest()[:32]
        return f"text:{open_id}:{chat_id}:{create_time}:{digest}"

    @staticmethod
    def _spawn(target: Any, *args: Any) -> threading.Thread:
        thread = threading.Thread(target=target, args=args, daemon=True)
        thread.start()
        return thread

    def _dispatch(
        self,
        receive_id: str,
        receive_id_type: str,
        sender_open_id: str,
        text: str,
    ) -> None:
        """处理飞书命令；可能访问 Agent 的命令统一在后台线程执行。"""

        command = text.split(None, 1)[0].split("@", 1)[0].casefold()
        try:
            if command in {"/start", "/help"}:
                self._send_text(receive_id, self._help_text(), receive_id_type=receive_id_type)
            elif command == "/status":
                self._send_text(receive_id, self._status_text(), receive_id_type=receive_id_type)
            elif command == "/session":
                agent = self._ensure_agent()
                session_id = str(getattr(agent, "current_session_id", "") or "")
                self._send_text(receive_id, f"当前会话 ID：{session_id or '（尚未创建）'}", receive_id_type=receive_id_type)
            elif command in {"/reset", "/new"}:
                self._reset_conversation(receive_id, receive_id_type)
            elif command == "/cancel":
                self._request_cancel(receive_id, receive_id_type)
            elif command in {"/approve", "/reject"}:
                self._handle_approval(
                    receive_id,
                    receive_id_type,
                    sender_open_id,
                    approve=command == "/approve",
                )
            elif command == "/thinking":
                self._handle_thinking_command(receive_id, receive_id_type, text)
            elif command == "/workspace":
                self._handle_workspace_command(receive_id, receive_id_type, text)
            else:
                reply = self._handle_harness_command(text)
                if reply is None:
                    reply = "未知命令。发送 /start 查看可用命令。"
                self._send_text(receive_id, reply, receive_id_type=receive_id_type)
        except Exception as exc:  # noqa: BLE001 - 远程命令只回传脱敏错误
            LOGGER.exception("执行飞书命令失败：%s", text)
            self._send_text(
                receive_id,
                f"❌ 命令执行失败：{redact_sensitive_text(str(exc))}",
                receive_id_type=receive_id_type,
            )

    def _reset_conversation(self, receive_id: str, receive_id_type: str) -> None:
        with self._lock:
            active = self._active_task
        if active is not None and active.thread is not None and active.thread.is_alive():
            self._send_text(
                receive_id,
                "当前有任务正在执行，请先 /cancel 后再开启新会话。",
                receive_id_type=receive_id_type,
            )
            return
        agent = self._ensure_agent()
        agent.reset_conversation()
        self._send_text(receive_id, "已开启新会话（对话历史已清空）。", receive_id_type=receive_id_type)

    def _status_text(self) -> str:
        agent = self._ensure_agent()
        with self._lock:
            task = self._active_task
        busy = task is not None and task.thread is not None and task.thread.is_alive()
        lines = [
            "📊 OmniCrawl 飞书连接器状态",
            f"工作区：{getattr(agent, 'workspace_root', '?')}",
            f"会话 ID：{str(getattr(agent, 'current_session_id', '') or '（尚未创建）')}",
            f"状态：{'🔄 正在执行任务' if busy else '✅ 空闲'}",
        ]
        if busy and task is not None:
            lines.append(f"任务已运行 {int(max(0, time.time() - task.started_at))} 秒，可发送 /cancel。")
        try:
            pending = self._get_inbox().pending_count
            memory_only = self._get_inbox().memory_only
            if pending:
                lines.append(f"排队任务：{pending} 条（将依次执行）")
            if memory_only:
                lines.append("⚠️ 入站队列为纯内存模式（未启用跨重启持久化）")
        except Exception:  # noqa: BLE001 - 状态展示不应失败
            LOGGER.debug("读取飞书入站队列状态失败", exc_info=True)
        return "\n".join(lines)

    def _help_text(self) -> str:
        return (
            "🤖 OmniCrawl 飞书机器人\n\n"
            "直接发送文本即可让 Agent 执行任务，例如：\n"
            "  列出当前工作区的文件\n"
            "  分析我刚发送的图片\n\n"
            "基础命令：\n"
            "  /start 或 /help  查看帮助\n"
            "  /status          查看工作区、会话和任务状态\n"
            "  /session         查看当前会话 ID\n"
            "  /reset 或 /new   开启新会话\n"
            "  /cancel          取消当前任务\n"
            "  /approve         批准敏感工具调用\n"
            "  /reject          拒绝敏感工具调用\n"
            "  /thinking on|off  是否展示思考增量\n"
            "  /model [选择]     查看或切换模型（下一次请求生效）\n"
            "  /workspace [路径] 查看或切换工作区\n"
            "  /plan            启用主 Agent 计划模式\n\n"
            "会话与系统命令：\n"
            "  /sessions /resume <id> /resume latest\n"
            "  /history /undo /compact /rename <标题> /archive /archives\n"
            "  /tasks /task <id> /mcp /plugins /skills /memory:clean\n"
            "  /reasoning [级别] /approval /approval:manual|review\n\n"
            "可直接发送图片、文档、视频或音频；文件会保存到 Agent 临时目录后交给 Agent。\n"
            "敏感工具在手动/审查模式下必须经确认，远程不支持完全自动批准。"
        )

    # ------------------------------------------------------------------
    # OmniCrawl 共享命令适配
    # ------------------------------------------------------------------

    def _handle_harness_command(self, text: str) -> str | None:
        """把 harness 管理命令交给命令注册表执行。

        解析/匹配/执行统一在 ``commands.slash`` 的注册表中，飞书、Telegram
        与 TUI 共用同一套命令声明，不再在此重复维护命令顺序。
        远程安全边界（审批仅 manual/review、禁止 auto）由命令处理器读取
        ``channel`` 后判断。
        """

        from omnicrawl.commands.slash import REGISTRY
        from omnicrawl.config.features.approval import (
            APPROVAL_MODE_AUTO,
            APPROVAL_MODE_REVIEW,
            approval_mode_label,
            load_approval_mode,
        )

        agent = self._ensure_agent()
        normalized = text.strip().casefold()

        parts = normalized.split()
        if len(parts) == 2 and parts[0] == "/resume" and parts[1] == "latest":
            return self._resume_latest_session(agent)

        # 若配置文件由本地 TUI 切成 auto，远程连接器不会因此绕过审批；该分支
        # 仅在兼容调用者读取磁盘模式时提供明确的安全解释，不实际应用 auto。
        if normalized == "/approval:disk":
            disk_mode = load_approval_mode()
            effective = APPROVAL_MODE_REVIEW if disk_mode == APPROVAL_MODE_AUTO else disk_mode
            return f"磁盘审批模式为 {approval_mode_label(disk_mode)}，飞书远程按 {approval_mode_label(effective)} 生效。"

        result = REGISTRY.dispatch(text, agent=agent, channel="feishu")
        if not result.handled:
            return None
        # 连接器本身运行在工作线程，慢命令（/mcp、/review 等）直接同步执行。
        resolved = result.resolve()
        return resolved.error or resolved.message

    @staticmethod
    def _resume_latest_session(agent: Any) -> str:
        from omnicrawl.agent import AgentError

        try:
            sessions = agent.list_sessions(limit=1)
        except AgentError as exc:
            return f"会话列表读取失败：{exc}"
        if not sessions:
            return "还没有可恢复的会话。先发送一条任务。"
        latest = sessions[0]
        session_id = str(getattr(latest, "session_id", "") or "")
        title = str(getattr(latest, "title", "") or "") or "未命名会话"
        try:
            state = agent.resume_session(session_id)
        except AgentError as exc:
            return f"会话恢复失败：{exc}"
        count = len(getattr(state, "messages", []) or [])
        return f"✅ 已恢复最近会话：{session_id}\n标题：{title}\n已恢复 {count} 条上下文消息。"

    def _handle_thinking_command(self, receive_id: str, receive_id_type: str, text: str) -> None:
        parts = text.split()
        if len(parts) == 1:
            state = "开启" if self._show_thinking else "关闭"
            self._send_text(
                receive_id,
                f"思考内容显示：{state}。用法：/thinking on 或 /thinking off。",
                receive_id_type=receive_id_type,
            )
            return
        value = parts[1].casefold()
        if value in {"on", "1", "true", "yes", "开", "开启"}:
            self._show_thinking = True
            message = "已开启思考内容显示。"
        elif value in {"off", "0", "false", "no", "关", "关闭"}:
            self._show_thinking = False
            message = "已关闭思考内容显示。"
        else:
            message = "用法：/thinking on 或 /thinking off。"
        self._send_text(receive_id, message, receive_id_type=receive_id_type)

    def _handle_workspace_command(self, receive_id: str, receive_id_type: str, text: str) -> None:
        parts = text.split(None, 1)
        agent = self._ensure_agent()
        if len(parts) == 1 or not parts[1].strip():
            self._send_text(
                receive_id,
                f"当前工作区：{agent.workspace_root}\n用法：/workspace <路径>",
                receive_id_type=receive_id_type,
            )
            return
        path = parts[1].strip()
        try:
            agent.switch_workspace(path)
        except Exception as exc:  # noqa: BLE001
            self._send_text(
                receive_id,
                f"❌ 切换工作区失败：{redact_sensitive_text(str(exc))}",
                receive_id_type=receive_id_type,
            )
            return
        note = ""
        try:
            from omnicrawl.config.core.workspace import save_workspace_root

            note = f"（已持久化到 {save_workspace_root(path)}）"
        except Exception as exc:  # noqa: BLE001 - 切换成功不因持久化失败回滚
            note = f"（持久化失败：{redact_sensitive_text(str(exc))}）"
        self._send_text(
            receive_id,
            f"✅ 已切换工作区：{agent.workspace_root}{note}",
            receive_id_type=receive_id_type,
        )

    # ------------------------------------------------------------------
    # Agent 任务、取消和工具审批
    # ------------------------------------------------------------------

    def _pump(self) -> None:
        """从持久入站队列取一条消息并启动任务（若当前空闲）。

        由入站消息和任务终结回调触发；使用 ``_pump_lock`` 防止并发重复启动。
        消息只有在任务真正终结（``_execute_task`` finally 中 confirm）后才从
        队列移除，因此执行中崩溃会在重启后由 ``recover`` 重新分发。
        """

        if self._stopped.is_set():
            return
        if not self._pump_lock.acquire(blocking=False):
            return  # 已有 pump 在处理
        try:
            while not self._stopped.is_set():
                with self._lock:
                    active = self._active_task
                if active is not None and active.thread is not None and active.thread.is_alive():
                    return  # 已有任务在执行，队列会由该任务终结时继续推进
                records = self._get_inbox().recover()
                if not records:
                    return
                record = records[0]
                payload = record.payload or {}
                text = str(payload.get("text") or "").strip()
                if not text:
                    # 无法执行的记录直接确认丢弃，避免死循环。
                    LOGGER.warning("丢弃无法执行的飞书入站记录：%s", record.dedupe_key)
                    self._get_inbox().confirm(record.dedupe_key)
                    continue
                started = self._start_task(
                    str(payload.get("receive_id") or ""),
                    str(payload.get("receive_id_type") or "open_id"),
                    str(payload.get("sender_open_id") or ""),
                    text,
                    dedupe_key=record.dedupe_key,
                )
                if not started:
                    return
                return  # 单条任务已启动，后续由 finally 中的 _pump 推进
        finally:
            self._pump_lock.release()

    def _start_task(
        self,
        receive_id: str,
        receive_id_type: str,
        sender_open_id: str,
        text: str,
        *,
        dedupe_key: str | None = None,
    ) -> bool:
        """启动一个任务线程；若已有活动任务则返回 False（调用方自行处理）。"""

        with self._lock:
            current = self._active_task
            if current is not None and current.thread is not None and current.thread.is_alive():
                busy_id = current.receive_id
                LOGGER.info("拒绝并发飞书任务（当前任务目标=%s，新目标=%s）", busy_id, receive_id)
                return False
            task = _ActiveTask(
                receive_id=receive_id,
                receive_id_type=receive_id_type,
                sender_open_id=sender_open_id,
                text=text,
                dedupe_key=dedupe_key,
            )
            thread = threading.Thread(
                target=self._execute_task,
                args=(task,),
                daemon=True,
                name="omnicrawl-feishu-task",
            )
            task.thread = thread
            self._active_task = task
        thread.start()
        return True

    def _execute_task(self, task: _ActiveTask) -> None:
        # 与 TUI 消息流一致：每个条目独立成消息、按发生顺序出现。deltas 只装
        # 当前 pass 的正文增量，工具调用处封口并清空，下一段正文另起一条消息。
        deltas: list[str] = []
        all_deltas: list[str] = []
        reasoning: list[str] = []
        text_message: _TextMessage | None = None
        reasoning_message: _ReasoningMessage | None = None
        plan_message: _PlanMessage | None = None
        subagent_messages: dict[str, _SubAgentMessage] = {}
        tool_messages: dict[str, _ToolMessage] = {}
        streamed_any = False
        last_status = ""
        now = time.monotonic()
        last_stream_patch = now
        last_reasoning_patch = now

        def check_cancelled() -> None:
            if self._stopped.is_set() or task.cancel_event.is_set():
                raise FeishuTaskCancelled("任务已被取消。")

        def seal_reasoning() -> None:
            nonlocal reasoning_message
            if reasoning_message is None:
                return
            reasoning_message.seal("".join(reasoning))
            reasoning_message = None
            reasoning.clear()

        def seal_text(*, text: str | None = None, suffix: str = "") -> None:
            """封口当前正文段：默认用流式累积文本，超长尾部补发为文本消息。"""

            nonlocal text_message
            if text_message is None:
                return
            remaining = text_message.seal(
                "".join(deltas) if text is None else text,
                suffix=suffix,
            )
            text_message = None
            deltas.clear()
            if remaining:
                self._send_text(
                    task.receive_id,
                    remaining,
                    receive_id_type=task.receive_id_type,
                )

        def abort_running_tools() -> None:
            for message in tool_messages.values():
                message.abort()

        def on_delta(delta: str) -> None:
            nonlocal last_stream_patch, text_message, streamed_any
            if not delta:
                return
            deltas.append(delta)
            all_deltas.append(delta)
            if text_message is None:
                joined = _clean_text("".join(deltas))
                if not joined:
                    # 首片内容可能只剩内部标签，避免先发一条空消息。
                    return
                # 正文开始：思考阶段到此结束，面板收口为完整内容（与 TUI 一致）。
                seal_reasoning()
                text_message = _TextMessage(self, task.receive_id, task.receive_id_type)
                text_message.stream(joined)
                streamed_any = True
                last_stream_patch = time.monotonic()
                return
            now = time.monotonic()
            if now - last_stream_patch < STREAM_PATCH_INTERVAL_SECONDS:
                return
            last_stream_patch = now
            text_message.stream("".join(deltas))

        def on_reasoning(delta: str) -> None:
            nonlocal last_reasoning_patch, reasoning_message
            if not (self._show_thinking and delta):
                return
            reasoning.append(delta)
            if reasoning_message is None:
                reasoning_message = _ReasoningMessage(
                    self,
                    task.receive_id,
                    task.receive_id_type,
                )
                reasoning_message.stream("".join(reasoning))
                last_reasoning_patch = time.monotonic()
                return
            now = time.monotonic()
            if now - last_reasoning_patch < STREAM_PATCH_INTERVAL_SECONDS:
                return
            last_reasoning_patch = now
            reasoning_message.stream("".join(reasoning))

        def on_status(message: str) -> None:
            nonlocal last_status
            text = str(message or "").strip()
            if not text or text == last_status:
                return
            last_status = text
            self._send_text(
                task.receive_id,
                f"⏳ {text}",
                receive_id_type=task.receive_id_type,
            )

        def on_tool_start(step: int, tool_call: Any) -> None:
            # 工具调用是模型 pass 的边界（与 TUI 一致）：先封口思考与正文，
            # 再让这次调用以独立消息出现在时间线里。
            seal_reasoning()
            seal_text()
            name = str(getattr(tool_call, "name", "?") or "?")
            if _operation_of(name) in _TOOLS_WITHOUT_RECORD:
                return
            arguments = getattr(tool_call, "arguments", {}) or {}
            key = str(getattr(tool_call, "id", "") or "") or f"step-{step}"
            if key in tool_messages:
                return
            message = _ToolMessage(
                self,
                task.receive_id,
                task.receive_id_type,
                _ToolRecord(
                    key=key,
                    name=name,
                    summary=_tool_summary(name, arguments),
                    arguments=dict(arguments) if isinstance(arguments, Mapping) else arguments,
                ),
            )
            tool_messages[key] = message
            message.start()

        def on_tool_result(tool_call: Any, tool_result: Any) -> None:
            name = str(getattr(tool_call, "name", "") or "")
            if _operation_of(name) in _TOOLS_WITHOUT_RECORD:
                return
            key = str(getattr(tool_call, "id", "") or "")
            message = tool_messages.get(key) if key else None
            if message is None:
                # 无 id 时按开始顺序回退匹配仍未完成的记录。
                message = next(
                    (item for item in tool_messages.values() if item.running),
                    None,
                )
            if message is None:
                return
            message.finish(
                ok=bool(getattr(tool_result, "ok", True)),
                output=str(getattr(tool_result, "output", "") or ""),
            )

        def on_subagent_event(event_name: str, payload: Any) -> None:
            if not isinstance(payload, Mapping):
                return
            batch_id = str(
                payload.get("batch_id") or f"batch-{payload.get('task_id') or 'task'}"
            )
            message = subagent_messages.get(batch_id)
            if message is None:
                message = _SubAgentMessage(self, task.receive_id, task.receive_id_type)
                subagent_messages[batch_id] = message
            message.update(event_name, payload)

        def on_todo_update(payload: Any) -> None:
            nonlocal plan_message
            if not isinstance(payload, Mapping):
                return
            if plan_message is None:
                plan_message = _PlanMessage(self, task.receive_id, task.receive_id_type)
            plan_message.update(payload.get("todos"))

        def on_stream_rollback() -> None:
            # 模型流中断后自动重试：丢弃已展示但作废的半截输出（与 TUI 一致）。
            # 已封口的正文段不受影响；当前段的消息保留，重试内容到达后覆盖。
            nonlocal last_stream_patch, last_reasoning_patch
            deltas.clear()
            all_deltas.clear()
            reasoning.clear()
            last_stream_patch = 0.0
            last_reasoning_patch = 0.0

        try:
            agent = self._ensure_agent()
            result = agent.run_stream(
                task.text,
                on_delta,
                on_status=on_status,
                on_tool_start=on_tool_start,
                on_tool_result=on_tool_result,
                cancel_check=check_cancelled,
                on_reasoning_delta=on_reasoning,
                on_subagent_event=on_subagent_event,
                on_todo_update=on_todo_update,
                on_stream_rollback=on_stream_rollback,
            )
            check_cancelled()
            reply_text = str(result or "")
            # 文件标记扫描沿用全量文本兜底；正文定型只用最终回答与当前段，
            # 避免把早先段落重复补发一遍。
            raw_reply = reply_text or "".join(all_deltas)
            if self._show_thinking and reasoning_message is not None:
                # 思考默认不展示；显式开启时收口为完整内容（超长保留尾部）。
                seal_reasoning()
            if text_message is not None:
                # 正文定型：流式片段不完整时用最终回答补齐，其余情况保留
                # 用户已经看过的流式内容，不重复展示早先段落。
                seal_text(
                    text=_resolve_final_text("".join(deltas), _display_text(reply_text))
                )
            elif reply_text or not streamed_any:
                # 末段没有流式正文：模型未流式输出最终回答，或整轮都没有可见
                # 正文（如只执行了工具）时补发一条消息。
                message = _TextMessage(self, task.receive_id, task.receive_id_type)
                remaining = message.seal(_display_text(reply_text))
                if remaining:
                    self._send_text(
                        task.receive_id,
                        remaining,
                        receive_id_type=task.receive_id_type,
                    )
            self._send_generated_files(
                task.receive_id,
                raw_reply,
                receive_id_type=task.receive_id_type,
            )
        except FeishuTaskCancelled:
            abort_running_tools()
            partial = _clean_text("".join(deltas)) if deltas else ""
            if text_message is not None and partial:
                seal_text(suffix="\n\n⏹ 输出已中断，任务已取消。")
            else:
                self._send_text(
                    task.receive_id,
                    "⏹ 任务已取消。",
                    receive_id_type=task.receive_id_type,
                )
        except Exception as exc:  # noqa: BLE001 - 远程任务必须回传脱敏错误
            LOGGER.exception("飞书 Agent 任务失败")
            abort_running_tools()
            # 封口已展示的正文，避免半截段落永远停在流式状态。
            seal_text()
            error = f"❌ 任务执行失败：{redact_sensitive_text(str(exc))}"
            self._send_text(task.receive_id, error, receive_id_type=task.receive_id_type)
        finally:
            with self._lock:
                if self._active_task is task:
                    self._active_task = None
                pending = self._pending_confirmation
                if pending is not None and pending.receive_id == task.receive_id:
                    self._pending_confirmation = None
                question = self._pending_user_question
                if question is not None and question.receive_id == task.receive_id:
                    self._pending_user_question = None
            # 任务终结（成功/失败/取消）后，确认队列中的这条已处理，并继续
            # 执行后续排队消息。
            if task.dedupe_key:
                try:
                    self._get_inbox().confirm(task.dedupe_key)
                except Exception:  # noqa: BLE001 - 确认失败不掩盖任务结果
                    LOGGER.debug("确认飞书入站队列任务失败", exc_info=True)
            self._pump()


    def _request_cancel(self, receive_id: str, receive_id_type: str) -> None:
        with self._lock:
            task = self._active_task
            pending = self._pending_confirmation
            question = self._pending_user_question
        if task is None or task.thread is None or not task.thread.is_alive():
            self._send_text(receive_id, "当前没有正在执行的任务。", receive_id_type=receive_id_type)
            return
        if task.receive_id != receive_id:
            self._send_text(receive_id, "只有发起当前任务的会话可以取消它。", receive_id_type=receive_id_type)
            return
        task.cancel_event.set()
        if pending is not None and pending.receive_id == receive_id:
            pending.decision = False
            pending.event.set()
        if question is not None and question.receive_id == receive_id:
            question.answer = None
            question.event.set()
        self._send_text(receive_id, "⏹ 已请求取消当前任务，请稍候……", receive_id_type=receive_id_type)

    def _ask_user(self, request: Any) -> str | None:
        """发送飞书提问并阻塞当前任务，直到文本或卡片回答到达。"""

        with self._lock:
            task = self._active_task
            if task is None or task.cancel_event.is_set() or self._stopped.is_set():
                return None
            question = _PendingUserQuestion(
                question_id=str(getattr(request, "request_id", "") or "").strip()
                or f"question-{time.time_ns()}",
                kind=str(getattr(request, "kind", "question") or "question"),
                question=str(getattr(request, "question", "") or "").strip(),
                options=tuple(
                    str(item)
                    for item in (getattr(request, "options", ()) or ())
                ),
                receive_id=task.receive_id,
                receive_id_type=task.receive_id_type,
                sender_open_id=task.sender_open_id,
            )
            self._pending_user_question = question

        if question.options:
            message_id = self._send_raw(
                question.receive_id,
                _question_card_json(question),
                msg_type="interactive",
                receive_id_type=question.receive_id_type,
            )
            sent = message_id is not None
            if sent:
                with self._lock:
                    question.message_id = message_id
        else:
            prompt = f"❓ {question.question}\n请直接回复答案。"
            if question.kind == "confirm":
                prompt += "（例如：是/否，或 yes/no）"
            sent = self._send_text(
                question.receive_id,
                prompt,
                receive_id_type=question.receive_id_type,
            )
        if not sent:
            with self._lock:
                if self._pending_user_question is question:
                    self._pending_user_question = None
            return None

        decided = question.event.wait(self.config.confirmation_timeout_seconds)
        with self._lock:
            if self._pending_user_question is question:
                self._pending_user_question = None
        if not decided:
            self._send_text(
                question.receive_id,
                "⏰ 问题超时，已取消本次等待。",
                receive_id_type=question.receive_id_type,
            )
            self._settle_question_card(question, "⏰ 问题超时，等待已取消。")
            return None
        if question.answer is None:
            # 用户取消任务时 event 被唤醒但 answer 保持 None，卡片一并终结。
            self._settle_question_card(question, "⏹ 任务已取消，问题已失效。")
            return None
        self._settle_question_card(question, f"✅ 已收到回答：{question.answer}")
        return question.answer

    def _settle_question_card(self, question: _PendingUserQuestion, status: str) -> None:
        """把提问卡片原地改写为只读终态，移除全部选项按钮。

        回答到达、超时或任务取消后，卡片若仍保留可点按钮，会持续暗示
        “还可以作答”；点击也只能得到“已失效”提示。patch 成功与否不影响
        主流程（文本结果已经返回），失败只记日志。
        """

        if not question.options or not question.message_id:
            return
        self._patch_card(question.message_id, _question_resolved_card_json(question, status))

    def _answer_pending_user_question(
        self,
        receive_id: str,
        receive_id_type: str,
        sender_open_id: str,
        text: str,
        *,
        message_type: str,
    ) -> bool:
        """把普通文本消息分派为当前 ask_user 的回答，避免启动新任务。"""

        if message_type != "text":
            return False
        with self._lock:
            question = self._pending_user_question
            if question is None:
                return False
            if question.event.is_set():
                return True
            if (
                question.receive_id != receive_id
                or question.sender_open_id != sender_open_id
            ):
                return False
            answer = str(text or "").strip()
            if answer.casefold() == "/cancel":
                question.answer = None
                question.event.set()
                cancel_task = True
            else:
                cancel_task = False
            if cancel_task:
                # 取消仍复用统一任务取消路径；本消息不能再被当作新任务。
                self._active_task.cancel_event.set()
                self._send_text(
                    receive_id,
                    "⏹ 已请求取消当前任务，请稍候……",
                    receive_id_type=receive_id_type,
                )
                return True
            if not answer:
                self._send_text(
                    receive_id,
                    "请发送非空回答。",
                    receive_id_type=receive_id_type,
                )
                return True
            if question.kind == "select" and answer not in question.options:
                self._send_text(
                    receive_id,
                    "请点击问题卡片中的选项按钮作答。",
                    receive_id_type=receive_id_type,
                )
                return True
            question.answer = answer
            question.event.set()
        self._send_text(receive_id, "✅ 已收到回答。", receive_id_type=receive_id_type)
        return True

    def _answer_user_question_action(self, data: Any) -> dict[str, Any]:
        """处理飞书交互卡片回调；兼容 SDK 对象和 dict 测试替身。

        返回符合 lark-oapi ``P2CardActionTriggerResponse`` 形状的 ACK 字典
        （含 toast）。不能返回布尔值：WebSocket 模式下 SDK 会把 handler 返回值
        序列化为 ACK data，布尔值不满足飞书卡片回调严格 schema，会被拒绝并
        触发 200672 card_update_illegal_format，客户端弹出"请稍后重试"。
        """

        payload = _field(data, "event", data)
        action = _field(payload, "action", {}) or {}
        value = _field(action, "value", {}) or {}
        if isinstance(value, str):
            value = _parse_json(value)
        if not isinstance(value, Mapping) or value.get("type") != "ask_user":
            return {"toast": {"type": "info", "content": "已忽略该卡片操作。"}}
        question_id = str(value.get("question_id") or "").strip()
        answer = str(value.get("answer") or "").strip()
        operator = _field(payload, "operator", None) or _field(payload, "user", None)
        open_id = str(_field(operator, "open_id", "") or "").strip()
        with self._lock:
            question = self._pending_user_question
            if question is None or question.question_id != question_id:
                message = "该问题已处理或已失效。"
                reply_to = None
            elif question.event.is_set():
                message = "该问题已处理或已失效。"
                reply_to = None
            elif question.sender_open_id != open_id:
                message = "只有发起任务的用户可以回答该问题。"
                reply_to = question
            elif not answer or (
                question.kind == "select" and answer not in question.options
            ):
                message = "无效的提问选项。"
                reply_to = question
            else:
                question.answer = answer
                question.event.set()
                message = "✅ 已收到回答。"
                reply_to = question
        if reply_to is not None:
            self._send_text(
                reply_to.receive_id,
                message,
                receive_id_type=reply_to.receive_id_type,
            )
        answered = reply_to is not None and bool(question is not None and question.event.is_set())
        return {
            "toast": {
                "type": "success" if answered else "info",
                "content": message,
            }
        }

    def _confirm_tool_call(self, tool_name: str, arguments: dict[str, Any]) -> bool:
        """Agent 工具确认回调：阻塞当前任务线程，等待飞书命令唤醒。"""

        with self._lock:
            task = self._active_task
            if task is None or task.cancel_event.is_set() or self._stopped.is_set():
                return False
            pending = _PendingConfirmation(
                tool_name=str(tool_name or "?"),
                arguments=dict(arguments or {}),
                receive_id=task.receive_id,
                receive_id_type=task.receive_id_type,
                sender_open_id=task.sender_open_id,
            )
            self._pending_confirmation = pending

        safe_arguments = redact_sensitive_values(dict(arguments or {}))
        detail = json.dumps(safe_arguments, ensure_ascii=False, indent=2)
        prompt = (
            "⚠️ 需要确认执行敏感操作\n"
            f"工具：{pending.tool_name}\n"
            f"参数：\n{detail}\n"
            "回复 /approve 允许，/reject 拒绝。"
            f"{int(self.config.confirmation_timeout_seconds)} 秒内未回复将自动拒绝。"
        )
        self._send_text(
            pending.receive_id,
            prompt,
            receive_id_type=pending.receive_id_type,
        )
        decided = pending.event.wait(self.config.confirmation_timeout_seconds)
        with self._lock:
            if self._pending_confirmation is pending:
                self._pending_confirmation = None
        if not decided:
            self._send_text(
                pending.receive_id,
                "⏰ 确认超时，已自动拒绝该操作。",
                receive_id_type=pending.receive_id_type,
            )
            return False
        return pending.decision

    def _handle_approval(
        self,
        receive_id: str,
        receive_id_type: str,
        sender_open_id: str,
        *,
        approve: bool,
    ) -> None:
        with self._lock:
            pending = self._pending_confirmation
            if pending is None:
                message = "当前没有等待确认的工具调用。"
            elif pending.receive_id != receive_id or pending.sender_open_id != sender_open_id:
                message = "只有发起当前任务的用户可以批准或拒绝该操作。"
            else:
                pending.decision = approve
                pending.event.set()
                message = f"✅ {'已批准' if approve else '已拒绝'}：{pending.tool_name}"
        self._send_text(receive_id, message, receive_id_type=receive_id_type)

    # ------------------------------------------------------------------
    # WebSocket 长连接
    # ------------------------------------------------------------------

    def run_forever(self) -> None:
        """建立飞书 WebSocket 长连接，断线后指数退避重连。"""

        sdk = _require_lark()
        if self.config.public_access:
            LOGGER.warning("飞书 allowed_user_ids 为空或包含 *，当前为公开访问模式。")

        # Agent 构建（隔离工作区、子代理/Skill 发现、Session 等）与 WebSocket
        # 建连并行：先让机器人尽快上线，Agent 由后台线程预热，首个消息通常
        # 已就绪；预热失败只记日志，首个消息会再次构建并按既有路径回传错误。
        self._prewarm_agent()

        # 恢复上次进程退出时尚未完成的入站消息（先落盘后处理的设计保证
        # 崩溃/重启不丢任务）。
        try:
            pending = self._get_inbox().recover()
            if pending:
                LOGGER.info("发现 %d 条未完成的飞书入站消息，开始恢复执行", len(pending))
                self._pump()
        except Exception:  # noqa: BLE001 - 恢复失败不能阻止连接器上线
            LOGGER.exception("恢复飞书入站队列失败")

        handler_builder = (
            sdk.EventDispatcherHandler.builder("", "")
            .register_p2_im_message_receive_v1(self.handle_message)
        )
        # 消息已读回执对机器人无用途，但 SDK 找不到处理器时会以 ERROR 级别
        # 输出 "processor not found"；注册空处理器消除该噪音（老版本无入口则跳过）。
        register_message_read = getattr(
            handler_builder, "register_p2_im_message_message_read_v1", None
        )
        if callable(register_message_read):
            handler_builder = register_message_read(lambda data: None)
        card_handler_registered = False
        for method_name in (
            "register_p1_card_action_trigger",
            "register_p2_card_action_trigger",
        ):
            register_card_handler = getattr(handler_builder, method_name, None)
            if callable(register_card_handler):
                handler_builder = register_card_handler(self._answer_user_question_action)
                card_handler_registered = True
                break
        if not card_handler_registered:
            LOGGER.warning("当前 lark-oapi 未提供卡片回调注册入口，select 提问不可用。")
        handler = handler_builder.build()
        retry_delay = RECONNECT_INITIAL_SECONDS
        while not self._stopped.is_set():
            try:
                self._client = self._create_client()
                ws_client = sdk.ws.Client(
                    self.config.app_id,
                    self.config.app_secret,
                    event_handler=handler,
                    log_level=sdk.LogLevel.INFO,
                )
                if not _install_ws_card_support(ws_client, handler):
                    LOGGER.warning(
                        "当前 lark-oapi 版本不支持 CARD 帧补丁，select 提问卡片按钮不可用。"
                    )
                with self._lock:
                    self._ws_client = ws_client
                LOGGER.info(
                    "飞书 Agent 已启动（WebSocket 长连接），App ID：%s，等待消息...",
                    self.config.app_id,
                )
                ws_client.start()
                retry_delay = RECONNECT_INITIAL_SECONDS
            except KeyboardInterrupt:
                raise
            except Exception:  # noqa: BLE001 - 网络断线自动重连
                LOGGER.exception("飞书长连接断开或启动失败，将在 %.0f 秒后重连", retry_delay)
            finally:
                with self._lock:
                    self._ws_client = None
            if self._stopped.wait(retry_delay):
                break
            retry_delay = min(retry_delay * 2, RECONNECT_MAX_SECONDS)
        LOGGER.info("飞书 Agent 已停止。")


def _install_ws_card_support(ws_client: Any, handler: Any) -> bool:
    """在 lark-oapi WebSocket 客户端实例上安装 CARD 数据帧处理补丁。

    lark-oapi 1.7.3 的 ``ws/client.py::_handle_data_frame`` 对 ``MessageType.CARD``
    帧直接 ``return``：既不分发给已注册的卡片回调（``p2.card.action.trigger``），
    也不回 ACK。飞书 select 提问卡片按钮的点击因此永远不会到达 fsapp 的
    ``_answer_user_question_action``。这里把 SDK 对 EVENT 帧的派发 + ACK 路径
    复制给 CARD 帧，其余帧类型仍走 SDK 原始实现（不修改 site-packages，只
    覆盖当前实例的同名异步方法）。返回是否安装成功。
    """

    original = getattr(type(ws_client), "_handle_data_frame", None)
    do_dispatch = getattr(handler, "_do_without_validation", None)
    if not callable(original) or not callable(do_dispatch):
        return False
    try:
        import base64

        from lark_oapi.core.json import JSON as sdk_json
        from lark_oapi.ws.model import Response as ws_response
    except Exception:  # pragma: no cover - SDK 内部结构变化时放弃补丁
        return False

    def _type_header(frame: Any) -> str | None:
        for header in frame.headers:
            if getattr(header, "key", None) == "type":
                return str(getattr(header, "value", "") or "")
        return None

    async def _handle_data_frame(frame: Any) -> None:  # noqa: N807 - 实例级覆盖 SDK 方法
        if _type_header(frame) != "card":
            await original(ws_client, frame)
            return
        if not frame.payload:
            return
        resp = ws_response(code=200)
        try:
            result = do_dispatch(frame.payload)
            if result is not None:
                resp.data = base64.b64encode(sdk_json.marshal(result).encode("utf-8"))
        except Exception as exc:  # noqa: BLE001 - 单帧失败不能杀死长连接
            LOGGER.warning("处理飞书卡片回调失败：%s", exc)
            resp = ws_response(code=500)
        frame.payload = sdk_json.marshal(resp).encode("utf-8")
        try:
            await ws_client._write_message(frame.SerializeToString())
        except Exception:  # noqa: BLE001 - ACK 失败只记录
            LOGGER.warning("回传飞书卡片回调 ACK 失败", exc_info=True)

    setattr(ws_client, "_handle_data_frame", _handle_data_frame)
    return True


def main(argv: list[str] | None = None) -> int:
    """命令行入口：``python -m omnicrawl.connectors.fsapp``。"""

    parser = argparse.ArgumentParser(description="OmniCrawl Feishu/Lark connector")
    parser.add_argument("--check", action="store_true", help="只检查飞书配置，不启动长连接")
    parser.add_argument("--check-agent", action="store_true", help="检查配置并初始化 OmniCrawl Agent")
    args = parser.parse_args(argv)
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    if args.check or args.check_agent:
        print(
            json.dumps(
                check_config(init_agent=args.check_agent),
                ensure_ascii=False,
                indent=2,
            )
        )
        return 0

    try:
        config = load_feishu_config()
    except ValueError as exc:
        print(f"飞书配置错误：{exc}")
        return 1
    if not config.app_id or not config.app_secret:
        print(
            "缺少飞书 App ID 或 App Secret：请设置 FEISHU_APP_ID/FEISHU_APP_SECRET，"
            "或在 config.toml 的 [feishu] 段填写。"
        )
        return 1
    try:
        _require_lark()
    except FeishuDependencyError as exc:
        print(f"缺少飞书 SDK：{exc}")
        return 1
    try:
        # 函数体内的裸名（CreateMessageRequest 等）不触发模块级 __getattr__，
        # 必须先显式加载进 globals，否则收到消息后无法回复（NameError）。
        _require_im_requests()
    except AttributeError as exc:
        print(f"飞书 IM 请求类加载失败：{exc}")
        return 1

    # 同一平台只允许一个活动连接器实例：多个 TUI/API 进程并存时，后启动的
    # 连接器检测到已有实例（例如由某个 TUI 自动拉起）就优雅退出，避免飞书
    # WebSocket 被重复建立。手工运行与自动启动共用同一把单例锁。
    from omnicrawl.workspace.connector_singleton import ConnectorInstanceLock

    # 拿不到锁说明已有其他实例在运行：不构造 Bot、不建长连接，直接退出。
    instance_lock = ConnectorInstanceLock("飞书")
    if not instance_lock.try_acquire():
        print("已有飞书连接器实例在运行，本次启动被跳过。")
        return 0
    try:
        bot = FeishuBot(config)
        try:
            bot.run_forever()
        except FeishuDependencyError as exc:
            print(f"飞书连接器无法启动：{exc}")
            return 1
        except KeyboardInterrupt:
            print("\n已停止飞书 Bot。")
        finally:
            bot.close()
    finally:
        instance_lock.release()
    return 0


__all__ = [
    "FeishuBot",
    "FeishuConfig",
    "FeishuDependencyError",
    "FeishuTaskCancelled",
    "check_config",
    "load_feishu_config",
    "main",
]


if __name__ == "__main__":
    raise SystemExit(main())
