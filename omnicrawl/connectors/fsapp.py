"""飞书机器人连接器：通过 lark-oapi WebSocket 长连接驱动 OmniCrawl Agent。

本模块参考 GenericAgent 的 ``frontends/fsapp.py``，但把任务执行部分适配为
OmniCrawl 的 ``LocalToolAgent.run_stream`` 协议：

* 飞书侧使用 ``lark.ws.Client`` 长连接接收 ``im.message.receive_v1`` 事件；
* 应用凭证优先从环境变量读取，其次读取 ``config.toml`` 的 ``[feishu]`` 段；
* 普通文本、图片和文件消息会转成 OmniCrawl Agent 任务；
* Agent 的状态、工具调用和最终回答通过飞书消息/交互式卡片回传；
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
from pathlib import Path
from typing import Any, Iterable, Mapping

from omnicrawl.state.session_artifacts import redact_sensitive_text, redact_sensitive_values

try:  # lark-oapi 是可选依赖；不影响主包的普通导入和测试。
    import lark_oapi as lark
    from lark_oapi.api.im.v1 import (
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
except ImportError:  # pragma: no cover - 是否安装由部署环境决定
    lark = None  # type: ignore[assignment]

    CreateFileRequest = None  # type: ignore[assignment,misc]
    CreateFileRequestBody = None  # type: ignore[assignment,misc]
    CreateImageRequest = None  # type: ignore[assignment,misc]
    CreateImageRequestBody = None  # type: ignore[assignment,misc]
    CreateMessageRequest = None  # type: ignore[assignment,misc]
    CreateMessageRequestBody = None  # type: ignore[assignment,misc]
    GetMessageResourceRequest = None  # type: ignore[assignment,misc]
    PatchMessageRequest = None  # type: ignore[assignment,misc]
    PatchMessageRequestBody = None  # type: ignore[assignment,misc]


LOGGER = logging.getLogger(__name__)

# 飞书普通文本和卡片内容都有平台侧大小限制。保守切分，避免把边界字节数
# 交给 SDK 后才失败；卡片详情还会再使用更小的上限。
MAX_TEXT_CHARS = 4000
MAX_CARD_DETAIL_CHARS = 7000
MAX_CARD_FINAL_CHARS = 7000
MAX_TOOL_DETAIL_CHARS = 1600
CONFIRM_ARGUMENTS_CHARS = 1800

# 长连接断线后的指数退避范围。短暂网络抖动不会导致进程退出，认证/配置错误
# 也会持续输出日志，便于管理员在飞书后台修正后自动恢复。
RECONNECT_INITIAL_SECONDS = 5.0
RECONNECT_MAX_SECONDS = 120.0

# 下载飞书消息资源的文件名只用于展示和临时目录落盘，必须移除路径部分。
_SAFE_FILENAME_FALLBACK = "feishu_file"

# 与 GenericAgent 的文本协议保持兼容：若某个扩展仍返回这些展示标签，发给
# 飞书用户前去掉标签本身，但不改变真正的回答正文。
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

# WebSocket 重连后某些事件可能再次投递。进程内短期去重足够覆盖常见重连
# 场景，同时不会把长期会话状态写入全局配置。
_DEDUP_TTL_SECONDS = 10 * 60
_DEDUP_MAX_ENTRIES = 2000


class FeishuDependencyError(RuntimeError):
    """启动飞书连接器前缺少 lark-oapi 依赖。"""


class FeishuTaskCancelled(RuntimeError):
    """当前飞书任务被用户或连接器生命周期主动取消。"""


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


class _TaskCard:
    """一个持续 patch 的飞书任务卡片。

    Agent 的 token 增量不逐字 patch，避免飞书 API 频率限制；只有状态和工具
    步骤变化时更新卡片，最终回答完成后再一次性定型。卡片更新失败时由外层
    发送普通文本兜底，因此卡片不是任务完成的单点依赖。
    """

    def __init__(self, bot: "FeishuBot", receive_id: str, receive_id_type: str) -> None:
        self._bot = bot
        self.receive_id = receive_id
        self.receive_id_type = receive_id_type
        self.status = "🤔 思考中..."
        self.steps: list[tuple[str, str]] = []
        self.final: str | None = None
        self.message_id: str | None = None
        self.started = False

    def _step_panel(self, index: int, summary: str, detail: str) -> dict[str, Any]:
        detail = detail.strip() or "_(无输出)_"
        if len(detail) > MAX_CARD_DETAIL_CHARS:
            detail = detail[:MAX_CARD_DETAIL_CHARS] + f"\n\n…（已截断，原始长度 {len(detail)} 字符）"
        return {
            "tag": "collapsible_panel",
            "expanded": False,
            "header": {
                "title": {
                    "tag": "plain_text",
                    "content": f"步骤 {index} · {summary}"[:120],
                }
            },
            "elements": [{"tag": "markdown", "content": detail}],
        }

    def _payload(self) -> str:
        elements: list[dict[str, Any]] = [
            {"tag": "markdown", "content": f"**{self.status}**"},
        ]
        for index, (summary, detail) in enumerate(self.steps, start=1):
            elements.append(self._step_panel(index, summary, detail))
        if self.final:
            elements.extend(
                [
                    {"tag": "hr"},
                    {"tag": "markdown", "content": self.final},
                ]
            )
        return _card_json(elements)

    def _push(self) -> bool:
        payload = self._payload()
        if self.message_id:
            return self._bot._patch_card(self.message_id, payload)
        message_id = self._bot._send_raw(
            self.receive_id,
            payload,
            msg_type="interactive",
            receive_id_type=self.receive_id_type,
        )
        if not message_id:
            return False
        self.message_id = message_id
        self.started = True
        return True

    def start(self) -> bool:
        return self._push()

    def set_status(self, status: str) -> bool:
        status = str(status or "").strip()
        if not status or status == self.status:
            return True
        self.status = status[:200]
        return self._push()

    def add_step(self, summary: str, detail: str = "") -> bool:
        self.steps.append((str(summary or "步骤"), str(detail or "")))
        self.status = f"⏳ 工作中 · 步骤 {len(self.steps)}"
        return self._push()

    def done(self, text: str) -> bool:
        self.status = "✅ 已完成"
        display = _display_text(text)
        self.final = display[:MAX_CARD_FINAL_CHARS]
        if len(display) > MAX_CARD_FINAL_CHARS:
            self.final += "\n\n…（完整回答将以普通消息继续发送）"
        return self._push()

    def fail(self, message: str) -> bool:
        self.status = f"❌ {str(message or '任务失败')[:180]}"
        self.final = None
        return self._push()


def _require_lark() -> Any:
    if lark is None:
        raise FeishuDependencyError(
            "缺少可选依赖 lark-oapi，请先执行：pip install lark-oapi"
        )
    return lark


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


def _split_text(text: str, limit: int = MAX_TEXT_CHARS) -> list[str]:
    """按换行优先切分飞书文本，单行过长时再硬切。"""

    if not text:
        return []
    if len(text) <= limit:
        return [text]

    parts: list[str] = []
    current = ""
    for line in text.splitlines(keepends=True):
        if len(current) + len(line) <= limit:
            current += line
            continue
        if current:
            parts.append(current.rstrip("\n") or current)
            current = ""
        while len(line) > limit:
            parts.append(line[:limit])
            line = line[limit:]
        current = line
    if current:
        parts.append(current.rstrip("\n") or current)
    return parts or [text[:limit]]


def _display_text(text: Any) -> str:
    """清理内部展示标签、脱敏敏感值，并给空响应提供可读兜底。"""

    raw = str(text or "")
    cleaned = _DISPLAY_TAG_PATTERN.sub("", raw).strip()
    cleaned = redact_sensitive_text(cleaned)
    return cleaned or "（任务完成，无文本输出）"


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

    def __init__(self, config: FeishuConfig, *, agent: Any | None = None) -> None:
        self.config = config
        self._client: Any | None = None
        self._ws_client: Any | None = None
        self._agent = agent
        self._agent_confirm_bound = False
        self._stopped = threading.Event()
        self._lock = threading.RLock()
        self._active_task: _ActiveTask | None = None
        self._pending_confirmation: _PendingConfirmation | None = None
        self._seen_messages: dict[str, float] = {}
        self._show_thinking = False

    # ------------------------------------------------------------------
    # Agent 与生命周期
    # ------------------------------------------------------------------

    def _ensure_agent(self) -> Any:
        """惰性创建 Agent，并把工具确认回调绑定到当前 Bot。"""

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
            return self._agent

    def close(self) -> None:
        """停止任务、释放挂起审批、停止长连接并关闭 Agent 资源。"""

        self._stopped.set()
        with self._lock:
            task = self._active_task
            pending = self._pending_confirmation
            ws_client = self._ws_client
        if task is not None:
            task.cancel_event.set()
        if pending is not None:
            pending.decision = False
            pending.event.set()

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

    # ------------------------------------------------------------------
    # 飞书消息 API
    # ------------------------------------------------------------------

    def _create_client(self) -> Any:
        sdk = _require_lark()
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

    def _send_text(self, receive_id: str, text: str, *, receive_id_type: str) -> None:
        """按飞书文本上限分段发送普通文本。"""

        for part in _split_text(redact_sensitive_text(str(text or ""))):
            self._send_raw(
                receive_id,
                json.dumps({"text": part}, ensure_ascii=False),
                msg_type="text",
                receive_id_type=receive_id_type,
            )

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
        sender = getattr(event, "sender", None)
        sender_id = getattr(sender, "sender_id", None)
        return str(getattr(sender_id, "open_id", "") or "").strip()

    def _is_allowed(self, open_id: str) -> bool:
        return self.config.public_access or bool(open_id and open_id in self.config.allowed_user_ids)

    def handle_message(self, data: Any) -> None:
        """飞书 ``im.message.receive_v1`` 事件回调。"""

        try:
            event = getattr(data, "event", None)
            message = getattr(event, "message", None)
            if message is None:
                return
            message_id = str(getattr(message, "message_id", "") or "")
            if not self._claim_message_once(message_id):
                LOGGER.info("忽略重复飞书消息：%s", message_id)
                return

            open_id = self._sender_open_id(event)
            if not self._is_allowed(open_id):
                LOGGER.warning("忽略未授权飞书用户：%s", open_id or "(unknown)")
                return

            chat_id = str(getattr(message, "chat_id", "") or "").strip()
            receive_id = chat_id or open_id
            receive_id_type = "chat_id" if chat_id else "open_id"
            user_text, _local_files = self._build_user_message(message)
            if not user_text:
                self._send_text(
                    receive_id,
                    f"⚠️ 暂不支持处理此类飞书消息：{getattr(message, 'message_type', 'unknown')}",
                    receive_id_type=receive_id_type,
                )
                return

            LOGGER.info(
                "收到飞书消息（user=%s, type=%s）：%s",
                open_id or "(unknown)",
                getattr(message, "message_type", "unknown"),
                user_text[:200],
            )
            if str(getattr(message, "message_type", "") or "") == "text" and user_text.startswith("/"):
                self._spawn(
                    self._dispatch,
                    receive_id,
                    receive_id_type,
                    open_id,
                    user_text,
                )
                return
            self._start_task(
                receive_id,
                receive_id_type,
                open_id,
                user_text,
            )
        except Exception:  # noqa: BLE001 - 单条事件失败不能杀死 SDK 长连接
            LOGGER.exception("处理飞书消息事件失败")

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
        """复用 ``commands.slash``，保持飞书、Telegram 与 TUI 命令一致。"""

        from omnicrawl.commands.slash import (
            format_mcp_status,
            format_memory_clean_result,
            format_plugins_status,
            format_skills_list,
            handle_mode_command,
            handle_reasoning_command,
            handle_review_command,
            handle_session_command,
            handle_subagent_task_command,
        )
        from omnicrawl.config.features.approval import (
            APPROVAL_MODE_AUTO,
            APPROVAL_MODE_MANUAL,
            APPROVAL_MODE_REVIEW,
            approval_mode_label,
            load_approval_mode,
            save_approval_mode,
        )

        agent = self._ensure_agent()
        normalized = text.strip().casefold()

        mode_reply = handle_mode_command(agent, text)
        if mode_reply is not None:
            return mode_reply

        parts = normalized.split()
        if len(parts) == 2 and parts[0] == "/resume" and parts[1] == "latest":
            return self._resume_latest_session(agent)

        if normalized == "/mcp":
            return format_mcp_status(agent)
        if normalized == "/plugins":
            return format_plugins_status(agent)
        if normalized == "/skills":
            return format_skills_list(agent)
        if normalized == "/memory:clean":
            return format_memory_clean_result(agent)

        for handler in (
            handle_session_command,
            handle_subagent_task_command,
            handle_reasoning_command,
            handle_review_command,
        ):
            reply = handler(agent, text)
            if reply is not None:
                return reply

        if normalized == "/approval":
            return (
                f"当前工具审批模式：{approval_mode_label(agent.approval_mode)}。\n"
                "可用切换：/approval:manual（手动确认）\n"
                "          /approval:review（自动审查，默认）\n"
                "远程不支持 /approval:auto（完全自动仅限本地 TUI）"
            )
        if normalized in {"/approval:auto", "/auto-approve:on"}:
            return (
                "❌ 远程不支持完全自动批准。完全自动仅限本地 TUI 配置；"
                f"当前仍为 {approval_mode_label(agent.approval_mode)}。"
            )

        approval_remote_map = {
            "/approval:manual": APPROVAL_MODE_MANUAL,
            "/auto-approve:off": APPROVAL_MODE_MANUAL,
            "/approval:review": APPROVAL_MODE_REVIEW,
            "/auto-review:on": APPROVAL_MODE_REVIEW,
        }
        if normalized in approval_remote_map:
            mode = approval_remote_map[normalized]
            agent.set_approval_mode(mode)
            try:
                path = save_approval_mode(mode)
                saved = f"并已同步到 {path}"
            except Exception as exc:  # noqa: BLE001
                saved = f"但写入 config.toml 失败：{redact_sensitive_text(str(exc))}"
            return f"审批模式已切换为 {approval_mode_label(mode)}，{saved}。"

        # 若配置文件由本地 TUI 切成 auto，远程连接器不会因此绕过审批；该分支
        # 仅在兼容调用者读取磁盘模式时提供明确的安全解释，不实际应用 auto。
        if normalized == "/approval:disk":
            disk_mode = load_approval_mode()
            effective = APPROVAL_MODE_REVIEW if disk_mode == APPROVAL_MODE_AUTO else disk_mode
            return f"磁盘审批模式为 {approval_mode_label(disk_mode)}，飞书远程按 {approval_mode_label(effective)} 生效。"
        return None

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

    def _start_task(
        self,
        receive_id: str,
        receive_id_type: str,
        sender_open_id: str,
        text: str,
    ) -> None:
        with self._lock:
            current = self._active_task
            if current is not None and current.thread is not None and current.thread.is_alive():
                busy_id = current.receive_id
                self._send_text(
                    receive_id,
                    "🔄 当前已有任务正在执行，请等待完成或发送 /cancel 取消。",
                    receive_id_type=receive_id_type,
                )
                LOGGER.info("拒绝并发飞书任务（当前任务目标=%s，新目标=%s）", busy_id, receive_id)
                return
            task = _ActiveTask(
                receive_id=receive_id,
                receive_id_type=receive_id_type,
                sender_open_id=sender_open_id,
                text=text,
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
        self._send_text(
            receive_id,
            "✅ 已收到任务，开始执行（/cancel 可取消）。",
            receive_id_type=receive_id_type,
        )

    def _execute_task(self, task: _ActiveTask) -> None:
        deltas: list[str] = []
        reasoning: list[str] = []
        card = _TaskCard(self, task.receive_id, task.receive_id_type)
        card_available = card.start()

        def check_cancelled() -> None:
            if self._stopped.is_set() or task.cancel_event.is_set():
                raise FeishuTaskCancelled("任务已被取消。")

        def on_delta(delta: str) -> None:
            if delta:
                deltas.append(delta)

        def on_reasoning(delta: str) -> None:
            if self._show_thinking and delta:
                reasoning.append(delta)

        def on_status(message: str) -> None:
            if not card_available:
                return
            card.set_status(f"⏳ {str(message or '工作中')[:180]}")

        def on_tool_start(step: int, tool_call: Any) -> None:
            name = str(getattr(tool_call, "name", "?"))
            arguments = getattr(tool_call, "arguments", {}) or {}
            safe_arguments = redact_sensitive_values(arguments) if isinstance(arguments, dict) else arguments
            detail = json.dumps(safe_arguments, ensure_ascii=False, indent=2)
            card.add_step(
                f"工具：{name}",
                detail[:MAX_TOOL_DETAIL_CHARS],
            )

        def on_tool_result(tool_call: Any, tool_result: Any) -> None:
            if bool(getattr(tool_result, "ok", True)):
                return
            name = str(getattr(tool_call, "name", "?"))
            output = redact_sensitive_text(
                str(getattr(tool_result, "output", "") or "")
            )[:MAX_TOOL_DETAIL_CHARS]
            card.add_step(f"工具失败：{name}", output)

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
            )
            check_cancelled()
            raw_reply = str(result or "".join(deltas) or "")
            reply = _display_text(raw_reply)
            if self._show_thinking and reasoning:
                # 思考默认不展示；显式开启时作为独立卡片步骤，不混入最终回答。
                card.add_step("思考内容", "".join(reasoning))
            card_ok = card.done(reply)
            if not card_ok:
                self._send_text(task.receive_id, reply, receive_id_type=task.receive_id_type)
            elif len(reply) > MAX_CARD_FINAL_CHARS:
                self._send_text(task.receive_id, reply, receive_id_type=task.receive_id_type)
            self._send_generated_files(
                task.receive_id,
                raw_reply,
                receive_id_type=task.receive_id_type,
            )
        except FeishuTaskCancelled:
            partial = _display_text("".join(deltas)) if deltas else ""
            if card_available:
                card.fail("任务已取消")
            if partial:
                self._send_text(
                    task.receive_id,
                    f"{partial}\n\n⏹ 输出已中断，任务已取消。",
                    receive_id_type=task.receive_id_type,
                )
            else:
                self._send_text(task.receive_id, "⏹ 任务已取消。", receive_id_type=task.receive_id_type)
        except Exception as exc:  # noqa: BLE001 - 远程任务必须回传脱敏错误
            LOGGER.exception("飞书 Agent 任务失败")
            error = f"❌ 任务执行失败：{redact_sensitive_text(str(exc))}"
            if card_available:
                card.fail("任务执行失败")
            self._send_text(task.receive_id, error, receive_id_type=task.receive_id_type)
        finally:
            with self._lock:
                if self._active_task is task:
                    self._active_task = None
                pending = self._pending_confirmation
                if pending is not None and pending.receive_id == task.receive_id:
                    self._pending_confirmation = None

    def _request_cancel(self, receive_id: str, receive_id_type: str) -> None:
        with self._lock:
            task = self._active_task
            pending = self._pending_confirmation
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
        self._send_text(receive_id, "⏹ 已请求取消当前任务，请稍候……", receive_id_type=receive_id_type)

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
        detail = json.dumps(safe_arguments, ensure_ascii=False, indent=2)[:CONFIRM_ARGUMENTS_CHARS]
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
        self._ensure_agent()
        if self.config.public_access:
            LOGGER.warning("飞书 allowed_user_ids 为空或包含 *，当前为公开访问模式。")

        handler = (
            sdk.EventDispatcherHandler.builder("", "")
            .register_p2_im_message_receive_v1(self.handle_message)
            .build()
        )
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
    if lark is None:
        print("缺少飞书 SDK：请先执行 pip install lark-oapi。")
        return 1

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
