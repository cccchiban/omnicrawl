"""OmniCrawl Hook / NPM 插件数据模型与 schema 校验。

本模块只放纯数据结构、常量与校验函数，不启动 Worker、不访问磁盘注册表。
这样测试与 CLI/Host 可共享同一份契约，避免在 Agent 里散落魔法字符串。
"""

from __future__ import annotations

import hashlib
import re
import uuid
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence


HOOK_API_VERSION = "1"
OMNICRAWL_VERSION = "0.1.3"
PLUGIN_SCHEMA_VERSION = 1

HANDLER_MODE_OBSERVE = "observe"
HANDLER_MODE_TRANSFORM = "transform"
HANDLER_MODE_GUARD = "guard"
HANDLER_MODE_NOTIFY = "notify"
HANDLER_MODES = frozenset(
    {
        HANDLER_MODE_OBSERVE,
        HANDLER_MODE_TRANSFORM,
        HANDLER_MODE_GUARD,
        HANDLER_MODE_NOTIFY,
    }
)

JSON_PATCH_OPS = frozenset({"add", "replace", "remove"})
DEFAULT_OBSERVE_TIMEOUT_MS = 500
DEFAULT_TRANSFORM_TIMEOUT_MS = 2000
DEFAULT_GUARD_TIMEOUT_MS = 2000
DEFAULT_NOTIFY_TIMEOUT_MS = 500
DEFAULT_MAX_TIMEOUT_MS = 5000
DEFAULT_FAILURE_THRESHOLD = 3
DEFAULT_MAX_MESSAGE_BYTES = 1_048_576
DEFAULT_CUSTOM_EVENT_MAX_DEPTH = 4

# Core Hook 名称。插件只能订阅，不能删除这些阶段。
CORE_HOOKS = frozenset(
    {
        "app.start.before",
        "app.start.after",
        "app.stop.before",
        "app.stop.after",
        "workspace.switch.before",
        "workspace.switch.after",
        "workspace.switch.error",
        "session.start.after",
        "session.resume.before",
        "session.resume.after",
        "session.resume.error",
        "session.close.before",
        "session.close.after",
        "turn.start",
        "turn.end",
        "turn.error",
        "turn.cancelled",
        "context.build.before",
        "context.build.after",
        "model.request.before",
        "model.response.after",
        "model.request.error",
        "tool.call.before",
        "tool.approval.before",
        "tool.approval.after",
        "tool.execute.before",
        "tool.execute.after",
        "tool.execute.error",
    }
)

# 每个 Hook 允许的 Handler 模式。
HOOK_ALLOWED_MODES: dict[str, frozenset[str]] = {
    "app.start.before": frozenset({HANDLER_MODE_OBSERVE, HANDLER_MODE_GUARD}),
    "app.start.after": frozenset({HANDLER_MODE_NOTIFY}),
    "app.stop.before": frozenset({HANDLER_MODE_OBSERVE}),
    "app.stop.after": frozenset({HANDLER_MODE_NOTIFY}),
    "workspace.switch.before": frozenset({HANDLER_MODE_GUARD}),
    "workspace.switch.after": frozenset({HANDLER_MODE_NOTIFY}),
    "workspace.switch.error": frozenset({HANDLER_MODE_NOTIFY}),
    "session.start.after": frozenset({HANDLER_MODE_NOTIFY}),
    "session.resume.before": frozenset({HANDLER_MODE_GUARD}),
    "session.resume.after": frozenset({HANDLER_MODE_NOTIFY}),
    "session.resume.error": frozenset({HANDLER_MODE_NOTIFY}),
    "session.close.before": frozenset({HANDLER_MODE_OBSERVE}),
    "session.close.after": frozenset({HANDLER_MODE_NOTIFY}),
    "turn.start": frozenset({HANDLER_MODE_TRANSFORM, HANDLER_MODE_GUARD}),
    "turn.end": frozenset({HANDLER_MODE_NOTIFY}),
    "turn.error": frozenset({HANDLER_MODE_NOTIFY}),
    "turn.cancelled": frozenset({HANDLER_MODE_NOTIFY}),
    "context.build.before": frozenset({HANDLER_MODE_TRANSFORM}),
    "context.build.after": frozenset({HANDLER_MODE_OBSERVE}),
    "model.request.before": frozenset({HANDLER_MODE_TRANSFORM, HANDLER_MODE_GUARD}),
    "model.response.after": frozenset({HANDLER_MODE_OBSERVE}),
    "model.request.error": frozenset({HANDLER_MODE_NOTIFY}),
    "tool.call.before": frozenset({HANDLER_MODE_TRANSFORM, HANDLER_MODE_GUARD}),
    "tool.approval.before": frozenset({HANDLER_MODE_OBSERVE, HANDLER_MODE_GUARD}),
    "tool.approval.after": frozenset({HANDLER_MODE_NOTIFY}),
    "tool.execute.before": frozenset({HANDLER_MODE_GUARD}),
    "tool.execute.after": frozenset({HANDLER_MODE_OBSERVE, HANDLER_MODE_TRANSFORM}),
    "tool.execute.error": frozenset({HANDLER_MODE_NOTIFY}),
}

# transform 白名单路径（RFC 6902 风格，* 表示单段通配）。
HOOK_PATCH_ALLOWLIST: dict[str, frozenset[str]] = {
    "turn.start": frozenset({"/payload/userText", "/payload/tags", "/payload/tags/-"}),
    "context.build.before": frozenset({"/payload/additionalContext"}),
    "model.request.before": frozenset(
        {
            "/payload/messages/*/content",
            "/payload/temperature",
            "/payload/top_p",
            "/payload/max_tokens",
        }
    ),
    "tool.call.before": frozenset({"/payload/arguments", "/payload/arguments/*"}),
    "tool.execute.after": frozenset({"/payload/displayText", "/payload/annotations"}),
}

# 不能被 replace / tombstone 的 sealed Handler 前缀。
SEALED_HANDLER_PREFIX = "core/"

_SEMVER_RE = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
    r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
)
_NPM_NAME_RE = re.compile(
    r"^(?:@[a-z0-9-~][a-z0-9-._~]*/)?[a-z0-9-~][a-z0-9-._~]*$"
)
_HANDLER_ID_RE = re.compile(r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$")
_CUSTOM_EVENT_RE = re.compile(
    r"^plugin\.[a-z0-9][a-z0-9-]{0,62}(?:\.[a-z0-9][a-z0-9-]{0,62})+$"
)


class PluginError(RuntimeError):
    """插件子系统通用错误。"""


class PluginManifestError(PluginError):
    """manifest / package.json 校验失败。"""


class PluginProtocolError(PluginError):
    """Host ↔ Worker 协议错误。"""


class PluginDispatchError(PluginError):
    """Hook 分发失败且策略要求拒绝当前操作。"""


class PluginInstallError(PluginError):
    """安装 / 更新 / 卸载失败。"""


@dataclass(frozen=True)
class HookPolicy:
    """单个 Core Hook 的失败与 deny 策略。"""

    on_deny: str = "reject-operation"
    on_timeout: str = "skip-handler"
    on_protocol_error: str = "skip-handler"
    on_handler_error: str = "skip-handler"
    disable_plugin_on_error: bool = False

    def __post_init__(self) -> None:
        for field_name in (
            "on_deny",
            "on_timeout",
            "on_protocol_error",
            "on_handler_error",
        ):
            value = getattr(self, field_name)
            if value not in {"reject-operation", "ignore", "skip-handler"}:
                raise PluginError(f"HookPolicy.{field_name} 非法：{value}")


def _policy(
    *,
    on_deny: str = "reject-operation",
    on_timeout: str = "skip-handler",
    on_protocol_error: str = "skip-handler",
    on_handler_error: str = "skip-handler",
    disable_plugin_on_error: bool = False,
) -> HookPolicy:
    return HookPolicy(
        on_deny=on_deny,
        on_timeout=on_timeout,
        on_protocol_error=on_protocol_error,
        on_handler_error=on_handler_error,
        disable_plugin_on_error=disable_plugin_on_error,
    )


# 设计文档 §5.2：显式 deny 与故障分开处理。
HOOK_POLICIES: dict[str, HookPolicy] = {
    "app.start.before": _policy(
        on_deny="reject-operation",
        on_timeout="skip-handler",
        on_protocol_error="skip-handler",
        on_handler_error="skip-handler",
        disable_plugin_on_error=True,
    ),
    "app.start.after": _policy(on_deny="ignore"),
    "app.stop.before": _policy(on_deny="ignore"),
    "app.stop.after": _policy(on_deny="ignore"),
    "workspace.switch.before": _policy(
        on_deny="reject-operation",
        on_timeout="reject-operation",
        on_protocol_error="reject-operation",
        on_handler_error="reject-operation",
    ),
    "workspace.switch.after": _policy(on_deny="ignore"),
    "workspace.switch.error": _policy(on_deny="ignore"),
    "session.start.after": _policy(on_deny="ignore"),
    "session.resume.before": _policy(
        on_deny="reject-operation",
        on_timeout="reject-operation",
        on_protocol_error="reject-operation",
        on_handler_error="reject-operation",
    ),
    "session.resume.after": _policy(on_deny="ignore"),
    "session.resume.error": _policy(on_deny="ignore"),
    "session.close.before": _policy(on_deny="ignore"),
    "session.close.after": _policy(on_deny="ignore"),
    "turn.start": _policy(
        on_deny="reject-operation",
        on_timeout="reject-operation",
        on_protocol_error="reject-operation",
        on_handler_error="reject-operation",
    ),
    "turn.end": _policy(on_deny="ignore"),
    "turn.error": _policy(on_deny="ignore"),
    "turn.cancelled": _policy(on_deny="ignore"),
    "context.build.before": _policy(on_deny="ignore"),
    "context.build.after": _policy(on_deny="ignore"),
    "model.request.before": _policy(
        on_deny="reject-operation",
        on_timeout="reject-operation",
        on_protocol_error="reject-operation",
        on_handler_error="reject-operation",
    ),
    "model.response.after": _policy(on_deny="ignore"),
    "model.request.error": _policy(on_deny="ignore"),
    "tool.call.before": _policy(
        on_deny="reject-operation",
        on_timeout="reject-operation",
        on_protocol_error="reject-operation",
        on_handler_error="reject-operation",
    ),
    "tool.approval.before": _policy(
        on_deny="reject-operation",
        on_timeout="reject-operation",
        on_protocol_error="reject-operation",
        on_handler_error="reject-operation",
    ),
    "tool.approval.after": _policy(on_deny="ignore"),
    "tool.execute.before": _policy(
        on_deny="reject-operation",
        on_timeout="reject-operation",
        on_protocol_error="reject-operation",
        on_handler_error="reject-operation",
    ),
    "tool.execute.after": _policy(on_deny="ignore"),
    "tool.execute.error": _policy(on_deny="ignore"),
}


@dataclass(frozen=True)
class PluginsConfig:
    """config.yaml 中的 plugins 段。"""

    enabled: bool = False
    default_timeout_ms: int = 1000
    max_timeout_ms: int = DEFAULT_MAX_TIMEOUT_MS
    failure_threshold: int = DEFAULT_FAILURE_THRESHOLD
    max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES
    custom_event_max_depth: int = DEFAULT_CUSTOM_EVENT_MAX_DEPTH
    # 默认关闭网络安装，降低供应链攻击面；需要时在 config 中显式开启。
    allow_network_install: bool = False
    audit_log_enabled: bool = True

    def __post_init__(self) -> None:
        if self.default_timeout_ms < 50 or self.default_timeout_ms > self.max_timeout_ms:
            raise PluginError("plugins.default_timeout_ms 必须在 50 与 max_timeout_ms 之间。")
        if self.max_timeout_ms < 100 or self.max_timeout_ms > 60_000:
            raise PluginError("plugins.max_timeout_ms 必须在 100 到 60000 之间。")
        if self.failure_threshold < 1 or self.failure_threshold > 20:
            raise PluginError("plugins.failure_threshold 必须在 1 到 20 之间。")
        if self.max_message_bytes < 1024 or self.max_message_bytes > 8_388_608:
            raise PluginError("plugins.max_message_bytes 必须在 1KiB 到 8MiB 之间。")
        if self.custom_event_max_depth < 1 or self.custom_event_max_depth > 16:
            raise PluginError("plugins.custom_event_max_depth 必须在 1 到 16 之间。")


@dataclass(frozen=True)
class HandlerRegistration:
    """插件声明的一个 Handler。"""

    id: str
    hook: str
    mode: str
    priority: int = 0
    replaces: tuple[str, ...] = ()
    event_version: str | None = None
    timeout_ms: int | None = None

    @property
    def is_custom_hook(self) -> bool:
        return self.hook.startswith("plugin.")


@dataclass(frozen=True)
class CustomEventDeclaration:
    """插件静态声明的自定义事件。"""

    name: str
    version: int
    visibility: str
    schema: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class PluginManifest:
    """package.json + omnicrawl 字段解析后的完整 manifest。"""

    name: str
    version: str
    api_version: str
    entry: str
    permissions: tuple[str, ...]
    hooks: tuple[HandlerRegistration, ...]
    engines_omnicrawl: str
    engines_node: str
    timeout_ms: int | None = None
    custom_events: tuple[CustomEventDeclaration, ...] = ()
    agents: tuple[str, ...] = ()
    package_type: str = "module"
    source_path: str = ""

    @property
    def handler_keys(self) -> tuple[str, ...]:
        return tuple(f"{self.name}/{item.id}" for item in self.hooks)


@dataclass(frozen=True)
class PluginVersionRef:
    """注册表中的精确版本指针。"""

    version: str
    integrity: str
    lockfile_hash: str
    source: str
    store_path: str = ""
    content_hash: str = ""

    def to_dict(self) -> dict[str, Any]:
        data = {
            "version": self.version,
            "integrity": self.integrity,
            "lockfileHash": self.lockfile_hash,
            "source": self.source,
        }
        if self.store_path:
            data["storePath"] = self.store_path
        if self.content_hash:
            data["contentHash"] = self.content_hash
        return data

    @classmethod
    def from_dict(cls, data: Mapping[str, Any] | None) -> PluginVersionRef | None:
        if not data:
            return None
        return cls(
            version=str(data.get("version", "")),
            integrity=str(data.get("integrity", "")),
            lockfile_hash=str(data.get("lockfileHash", data.get("lockfile_hash", ""))),
            source=str(data.get("source", "")),
            store_path=str(data.get("storePath", data.get("store_path", ""))),
            content_hash=str(data.get("contentHash", data.get("content_hash", ""))),
        )


@dataclass
class PluginRecord:
    """单个插件在 registry 中的记录。"""

    name: str
    enabled: bool = False
    active: PluginVersionRef | None = None
    candidate: PluginVersionRef | None = None
    previous: PluginVersionRef | None = None
    approved_permissions: list[str] = field(default_factory=list)
    local_path: str = ""
    dev_mode: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled": self.enabled,
            "active": self.active.to_dict() if self.active else None,
            "candidate": self.candidate.to_dict() if self.candidate else None,
            "previous": self.previous.to_dict() if self.previous else None,
            "approvedPermissions": list(self.approved_permissions),
            "localPath": self.local_path,
            "devMode": self.dev_mode,
        }

    @classmethod
    def from_dict(cls, name: str, data: Mapping[str, Any]) -> PluginRecord:
        return cls(
            name=name,
            enabled=bool(data.get("enabled", False)),
            active=PluginVersionRef.from_dict(data.get("active")),
            candidate=PluginVersionRef.from_dict(data.get("candidate")),
            previous=PluginVersionRef.from_dict(data.get("previous")),
            approved_permissions=[
                str(item) for item in data.get("approvedPermissions", data.get("approved_permissions", []))
            ],
            local_path=str(data.get("localPath", data.get("local_path", ""))),
            dev_mode=bool(data.get("devMode", data.get("dev_mode", False))),
        )


@dataclass
class PluginRegistryDocument:
    """user/project 注册表文档。"""

    schema_version: int = PLUGIN_SCHEMA_VERSION
    plugins: dict[str, PluginRecord] = field(default_factory=dict)
    disabled_handlers: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "schemaVersion": self.schema_version,
            "plugins": {name: record.to_dict() for name, record in sorted(self.plugins.items())},
            "disabledHandlers": list(self.disabled_handlers),
        }

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> PluginRegistryDocument:
        raw_plugins = data.get("plugins", {})
        if not isinstance(raw_plugins, Mapping):
            raise PluginError("registry.plugins 必须是对象。")
        plugins = {
            str(name): PluginRecord.from_dict(str(name), value)
            for name, value in raw_plugins.items()
            if isinstance(value, Mapping)
        }
        disabled = [
            str(item)
            for item in data.get("disabledHandlers", data.get("disabled_handlers", []))
        ]
        schema_version = int(data.get("schemaVersion", data.get("schema_version", PLUGIN_SCHEMA_VERSION)))
        return cls(schema_version=schema_version, plugins=plugins, disabled_handlers=disabled)


@dataclass(frozen=True)
class ResolvedHandler:
    """合并 project/user 后、按稳定顺序排序的 Handler。"""

    key: str
    plugin_name: str
    plugin_version: str
    handler_id: str
    hook: str
    mode: str
    priority: int
    scope: str
    timeout_ms: int
    replaces: tuple[str, ...] = ()
    integrity_prefix: str = ""
    local_path: str = ""
    permissions: tuple[str, ...] = ()


@dataclass(frozen=True)
class HookEvent:
    """统一事件信封。"""

    api_version: str
    event_id: str
    hook: str
    timestamp: str
    sequence: int
    workspace: dict[str, Any]
    payload: dict[str, Any]
    deadline_ms: int
    session_id: str | None = None
    turn_id: str | None = None
    trace: dict[str, Any] = field(default_factory=lambda: {"depth": 0})

    def to_dict(self) -> dict[str, Any]:
        data = asdict(self)
        # dataclass 字段名是 snake_case；协议使用 camelCase。
        return {
            "apiVersion": data["api_version"],
            "eventId": data["event_id"],
            "hook": data["hook"],
            "timestamp": data["timestamp"],
            "sequence": data["sequence"],
            "workspace": data["workspace"],
            "sessionId": data["session_id"],
            "turnId": data["turn_id"],
            "payload": data["payload"],
            "deadlineMs": data["deadline_ms"],
            "trace": data["trace"],
        }


@dataclass(frozen=True)
class HookResult:
    """单个 Handler 的规范化结果。"""

    action: str
    reason: str = ""
    code: str = ""
    patch: tuple[dict[str, Any], ...] = ()
    annotations: dict[str, Any] = field(default_factory=dict)
    handler_key: str = ""
    elapsed_ms: float = 0.0
    status: str = "continue"

    @classmethod
    def continue_result(
        cls,
        *,
        handler_key: str = "",
        annotations: Mapping[str, Any] | None = None,
        elapsed_ms: float = 0.0,
        status: str = "continue",
    ) -> HookResult:
        return cls(
            action="continue",
            annotations=dict(annotations or {}),
            handler_key=handler_key,
            elapsed_ms=elapsed_ms,
            status=status,
        )

    @classmethod
    def deny_result(
        cls,
        reason: str,
        *,
        code: str = "",
        handler_key: str = "",
        elapsed_ms: float = 0.0,
    ) -> HookResult:
        return cls(
            action="deny",
            reason=reason,
            code=code,
            handler_key=handler_key,
            elapsed_ms=elapsed_ms,
            status="deny",
        )

    @classmethod
    def patch_result(
        cls,
        patch: Sequence[Mapping[str, Any]],
        *,
        handler_key: str = "",
        annotations: Mapping[str, Any] | None = None,
        elapsed_ms: float = 0.0,
    ) -> HookResult:
        return cls(
            action="patch",
            patch=tuple(dict(item) for item in patch),
            annotations=dict(annotations or {}),
            handler_key=handler_key,
            elapsed_ms=elapsed_ms,
            status="patch",
        )


@dataclass
class DispatchOutcome:
    """一次 Hook 分发的聚合结果。"""

    hook: str
    payload: dict[str, Any]
    denied: bool = False
    deny_reason: str = ""
    deny_code: str = ""
    results: list[HookResult] = field(default_factory=list)
    annotations: dict[str, Any] = field(default_factory=dict)

    def raise_if_denied(self) -> None:
        if self.denied:
            raise PluginDispatchError(self.deny_reason or f"Hook {self.hook} 被拒绝。")


def utc_now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def new_event_id() -> str:
    return uuid.uuid4().hex


def stable_hash(value: Any) -> str:
    """对任意 JSON 可序列化值计算稳定短哈希，用于审计而非完整性锁。"""

    encoded = json_dumps_stable(value).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()[:16]


def json_dumps_stable(value: Any) -> str:
    return json_module().dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def json_module():
    import json

    return json


def is_valid_semver(version: str) -> bool:
    return bool(_SEMVER_RE.match(version.strip()))


def is_valid_npm_name(name: str) -> bool:
    return bool(_NPM_NAME_RE.match(name.strip()))


def normalize_plugin_namespace(package_name: str) -> str:
    """把 NPM 包名规范化为自定义 Hook 命名空间片段。"""

    text = package_name.strip().lower()
    if text.startswith("@"):
        text = text[1:]
    text = text.replace("/", "-")
    text = re.sub(r"[^a-z0-9-]+", "-", text)
    text = re.sub(r"-{2,}", "-", text).strip("-")
    return text or "plugin"


def default_timeout_for_mode(mode: str) -> int:
    if mode == HANDLER_MODE_TRANSFORM:
        return DEFAULT_TRANSFORM_TIMEOUT_MS
    if mode == HANDLER_MODE_GUARD:
        return DEFAULT_GUARD_TIMEOUT_MS
    if mode == HANDLER_MODE_NOTIFY:
        return DEFAULT_NOTIFY_TIMEOUT_MS
    return DEFAULT_OBSERVE_TIMEOUT_MS


def parse_plugins_config(data: Mapping[str, Any] | None) -> PluginsConfig:
    """从 config.yaml 的 plugins 段构造配置；缺省时默认关闭插件。"""

    if not data:
        return PluginsConfig()
    if not isinstance(data, Mapping):
        raise PluginError("plugins 配置必须是 JSON 对象。")
    return PluginsConfig(
        enabled=bool(data.get("enabled", False)),
        default_timeout_ms=int(data.get("default_timeout_ms", data.get("defaultTimeoutMs", 1000))),
        max_timeout_ms=int(data.get("max_timeout_ms", data.get("maxTimeoutMs", DEFAULT_MAX_TIMEOUT_MS))),
        failure_threshold=int(data.get("failure_threshold", data.get("failureThreshold", DEFAULT_FAILURE_THRESHOLD))),
        max_message_bytes=int(data.get("max_message_bytes", data.get("maxMessageBytes", DEFAULT_MAX_MESSAGE_BYTES))),
        custom_event_max_depth=int(
            data.get("custom_event_max_depth", data.get("customEventMaxDepth", DEFAULT_CUSTOM_EVENT_MAX_DEPTH))
        ),
        allow_network_install=bool(
            data.get("allow_network_install", data.get("allowNetworkInstall", False))
        ),
        audit_log_enabled=bool(data.get("audit_log_enabled", data.get("auditLogEnabled", True))),
    )


def parse_handler_registration(raw: Mapping[str, Any], *, package_name: str) -> HandlerRegistration:
    if not isinstance(raw, Mapping):
        raise PluginManifestError("hooks 项必须是对象。")
    handler_id = str(raw.get("id", "")).strip()
    hook = str(raw.get("hook", "")).strip()
    mode = str(raw.get("mode", "")).strip()
    if not _HANDLER_ID_RE.match(handler_id):
        raise PluginManifestError(f"非法 Handler ID：{handler_id}")
    if mode not in HANDLER_MODES:
        raise PluginManifestError(f"Handler {handler_id} mode 非法：{mode}")
    if hook in CORE_HOOKS:
        allowed = HOOK_ALLOWED_MODES.get(hook, frozenset())
        if mode not in allowed:
            raise PluginManifestError(f"Hook {hook} 不支持 mode={mode}")
    elif hook.startswith("plugin."):
        if not _CUSTOM_EVENT_RE.match(hook):
            raise PluginManifestError(f"自定义 Hook 名称非法：{hook}")
    else:
        raise PluginManifestError(f"未知 Hook：{hook}")

    priority = int(raw.get("priority", 0))
    replaces_raw = raw.get("replaces", [])
    if replaces_raw in (None, ""):
        replaces: tuple[str, ...] = ()
    elif isinstance(replaces_raw, list):
        replaces = tuple(str(item).strip() for item in replaces_raw if str(item).strip())
    else:
        raise PluginManifestError(f"Handler {handler_id} replaces 必须是数组。")
    for target in replaces:
        if target.startswith(SEALED_HANDLER_PREFIX):
            raise PluginManifestError(f"不能替换 sealed Handler：{target}")
        if "/" not in target:
            raise PluginManifestError(f"replaces 必须是 package/handler 形式：{target}")

    timeout_ms = raw.get("timeoutMs", raw.get("timeout_ms"))
    timeout_value = int(timeout_ms) if timeout_ms is not None else None
    event_version = raw.get("eventVersion", raw.get("event_version"))
    return HandlerRegistration(
        id=handler_id,
        hook=hook,
        mode=mode,
        priority=priority,
        replaces=replaces,
        event_version=str(event_version) if event_version is not None else None,
        timeout_ms=timeout_value,
    )


def parse_custom_event(raw: Mapping[str, Any], *, package_name: str) -> CustomEventDeclaration:
    if not isinstance(raw, Mapping):
        raise PluginManifestError("customEvents 项必须是对象。")
    name = str(raw.get("name", "")).strip()
    namespace = normalize_plugin_namespace(package_name)
    expected_prefix = f"plugin.{namespace}."
    if not name.startswith(expected_prefix):
        raise PluginManifestError(
            f"自定义事件必须位于本插件命名空间 {expected_prefix}*，当前：{name}"
        )
    if not _CUSTOM_EVENT_RE.match(name):
        raise PluginManifestError(f"自定义事件名称非法：{name}")
    version = int(raw.get("version", 1))
    if version < 1:
        raise PluginManifestError(f"自定义事件版本必须 >= 1：{name}")
    visibility = str(raw.get("visibility", "private")).strip()
    if visibility not in {"private", "public"}:
        raise PluginManifestError(f"自定义事件 visibility 非法：{visibility}")
    schema = raw.get("schema", {"type": "object"})
    if not isinstance(schema, Mapping):
        raise PluginManifestError(f"自定义事件 schema 必须是对象：{name}")
    return CustomEventDeclaration(
        name=name,
        version=version,
        visibility=visibility,
        schema=dict(schema),
    )


def parse_plugin_manifest(package_data: Mapping[str, Any], *, source_path: str = "") -> PluginManifest:
    """解析并校验 package.json 中的 omnicrawl 扩展元数据。"""

    if not isinstance(package_data, Mapping):
        raise PluginManifestError("package.json 顶层必须是对象。")
    name = str(package_data.get("name", "")).strip()
    version = str(package_data.get("version", "")).strip()
    if not is_valid_npm_name(name):
        raise PluginManifestError(f"非法 NPM 包名：{name}")
    if not is_valid_semver(version):
        raise PluginManifestError(f"非法 SemVer：{version}")

    omnicrawl = package_data.get("omnicrawl")
    if not isinstance(omnicrawl, Mapping):
        raise PluginManifestError("缺少 omnicrawl 扩展字段。")

    api_version = str(omnicrawl.get("apiVersion", omnicrawl.get("api_version", ""))).strip()
    if api_version != HOOK_API_VERSION:
        raise PluginManifestError(f"不支持的 apiVersion：{api_version}，当前仅接受 {HOOK_API_VERSION}。")

    engines = omnicrawl.get("engines", {})
    if not isinstance(engines, Mapping):
        raise PluginManifestError("omnicrawl.engines 必须是对象。")
    engines_omnicrawl = str(engines.get("omnicrawl", "")).strip()
    engines_node = str(engines.get("node", "")).strip()
    if not engines_omnicrawl:
        raise PluginManifestError("缺少 omnicrawl.engines.omnicrawl。")
    if not engines_node:
        raise PluginManifestError("缺少 omnicrawl.engines.node。")

    entry = str(omnicrawl.get("entry", package_data.get("main", ""))).strip()
    if not entry:
        raise PluginManifestError("缺少 omnicrawl.entry。")
    if entry.startswith("/") or entry.startswith("\\") or ".." in entry.replace("\\", "/").split("/"):
        # 绝对路径与任何 .. 片段都视为逃逸风险；规范化后的包内相对路径再由安装器二次校验。
        raise PluginManifestError(f"entry 路径非法或不允许逃逸：{entry}")

    permissions_raw = omnicrawl.get("permissions", [])
    if not isinstance(permissions_raw, list) or not permissions_raw:
        raise PluginManifestError("omnicrawl.permissions 必须是非空数组。")
    permissions = tuple(str(item).strip() for item in permissions_raw if str(item).strip())

    agents_raw = omnicrawl.get("agents", [])
    if agents_raw in (None, ""):
        agents_raw = []
    if not isinstance(agents_raw, list) or not all(isinstance(item, str) for item in agents_raw):
        raise PluginManifestError("omnicrawl.agents 必须是包内 Markdown 相对路径数组。")
    agents: list[str] = []
    for raw_path in agents_raw:
        normalized_path = raw_path.strip().replace("\\", "/")
        path_parts = normalized_path.split("/")
        if (
            not normalized_path
            or normalized_path.startswith("/")
            or Path(normalized_path).is_absolute()
            or ":" in path_parts[0]
            or ".." in path_parts
            or Path(normalized_path).suffix.casefold() != ".md"
        ):
            raise PluginManifestError(f"omnicrawl.agents 路径非法或不允许逃逸：{raw_path}")
        if normalized_path in agents:
            raise PluginManifestError(f"omnicrawl.agents 不允许重复路径：{normalized_path}")
        agents.append(normalized_path)
    if agents and "agent:definitions" not in permissions:
        raise PluginManifestError("声明 omnicrawl.agents 需要 agent:definitions 权限。")

    hooks_raw = omnicrawl.get("hooks", [])
    if not isinstance(hooks_raw, list):
        raise PluginManifestError("omnicrawl.hooks 必须是数组。")
    if not hooks_raw and not agents:
        raise PluginManifestError("omnicrawl.hooks 必须是非空数组，或声明 omnicrawl.agents。")
    hooks = [parse_handler_registration(item, package_name=name) for item in hooks_raw]
    handler_ids = [item.id for item in hooks]
    if len(handler_ids) != len(set(handler_ids)):
        raise PluginManifestError("Handler ID 在包内必须唯一。")

    for handler in hooks:
        required_perm = f"hook:{handler.hook}"
        if handler.hook.startswith("plugin."):
            # 订阅自定义 Hook 使用 hook:custom-subscribe；发布权限另计。
            if "hook:custom-subscribe" not in permissions and required_perm not in permissions:
                raise PluginManifestError(
                    f"Handler {handler.id} 订阅自定义 Hook 需要 hook:custom-subscribe 权限。"
                )
        elif required_perm not in permissions:
            raise PluginManifestError(
                f"Handler {handler.id} 注册 {handler.hook} 需要权限 {required_perm}。"
            )

    custom_events_raw = omnicrawl.get("customEvents", omnicrawl.get("custom_events", []))
    custom_events: list[CustomEventDeclaration] = []
    if custom_events_raw not in (None, ""):
        if not isinstance(custom_events_raw, list):
            raise PluginManifestError("omnicrawl.customEvents 必须是数组。")
        custom_events = [parse_custom_event(item, package_name=name) for item in custom_events_raw]
        if "hook:custom-emit" not in permissions and custom_events:
            raise PluginManifestError("声明 customEvents 需要 hook:custom-emit 权限。")

    timeout_ms = omnicrawl.get("timeoutMs", omnicrawl.get("timeout_ms"))
    timeout_value = int(timeout_ms) if timeout_ms is not None else None
    package_type = str(package_data.get("type", "module")).strip() or "module"

    return PluginManifest(
        name=name,
        version=version,
        api_version=api_version,
        entry=entry.replace("\\", "/"),
        permissions=permissions,
        hooks=tuple(hooks),
        engines_omnicrawl=engines_omnicrawl,
        engines_node=engines_node,
        timeout_ms=timeout_value,
        custom_events=tuple(custom_events),
        agents=tuple(agents),
        package_type=package_type,
        source_path=source_path,
    )


def validate_payload_against_schema(payload: Any, schema: Mapping[str, Any] | None) -> None:
    """对自定义事件 payload 做 V1 轻量 schema 校验。

    支持 type/properties/required/additionalProperties 的常见子集；
    不引入 jsonschema 依赖，避免安装面扩大。
    """

    if not schema:
        if payload is not None and not isinstance(payload, Mapping):
            raise PluginManifestError("自定义事件 payload 默认必须是对象。")
        return

    expected_type = schema.get("type")
    if expected_type == "object":
        if not isinstance(payload, Mapping):
            raise PluginManifestError("自定义事件 payload 必须是对象。")
        properties = schema.get("properties", {})
        if properties is not None and not isinstance(properties, Mapping):
            raise PluginManifestError("schema.properties 必须是对象。")
        required = schema.get("required", [])
        if required is None:
            required = []
        if not isinstance(required, list):
            raise PluginManifestError("schema.required 必须是数组。")
        for key in required:
            if str(key) not in payload:
                raise PluginManifestError(f"自定义事件 payload 缺少必填字段：{key}")
        additional = schema.get("additionalProperties", True)
        if additional is False and isinstance(properties, Mapping):
            allowed = set(str(item) for item in properties.keys())
            extras = [key for key in payload.keys() if str(key) not in allowed]
            if extras:
                raise PluginManifestError(
                    f"自定义事件 payload 含未声明字段：{', '.join(sorted(map(str, extras)))}"
                )
        if isinstance(properties, Mapping):
            for key, child_schema in properties.items():
                if key not in payload:
                    continue
                if isinstance(child_schema, Mapping):
                    child_type = child_schema.get("type")
                    value = payload[key]
                    if child_type == "string" and not isinstance(value, str):
                        raise PluginManifestError(f"字段 {key} 必须是 string")
                    if child_type == "number" and not isinstance(value, (int, float)):
                        raise PluginManifestError(f"字段 {key} 必须是 number")
                    if child_type == "integer" and not isinstance(value, int):
                        raise PluginManifestError(f"字段 {key} 必须是 integer")
                    if child_type == "boolean" and not isinstance(value, bool):
                        raise PluginManifestError(f"字段 {key} 必须是 boolean")
                    if child_type == "object" and not isinstance(value, Mapping):
                        raise PluginManifestError(f"字段 {key} 必须是 object")
                    if child_type == "array" and not isinstance(value, list):
                        raise PluginManifestError(f"字段 {key} 必须是 array")
        return
    if expected_type == "array":
        if not isinstance(payload, list):
            raise PluginManifestError("自定义事件 payload 必须是数组。")
        return
    if expected_type == "string":
        if not isinstance(payload, str):
            raise PluginManifestError("自定义事件 payload 必须是 string。")
        return
    # 未识别 type 时仅要求可 JSON 序列化对象/标量。
    if payload is not None and not isinstance(payload, (Mapping, list, str, int, float, bool)):
        raise PluginManifestError("自定义事件 payload 类型不受支持。")


def path_matches_allowlist(path: str, allowlist: Iterable[str]) -> bool:
    """判断 JSON Pointer 是否命中 allowlist；支持单段 * 通配。"""

    normalized = path if path.startswith("/") else f"/{path}"
    for pattern in allowlist:
        if _pointer_match(normalized, pattern):
            return True
    return False


def _pointer_match(path: str, pattern: str) -> bool:
    path_parts = path.split("/")[1:]
    pattern_parts = pattern.split("/")[1:]
    if len(path_parts) != len(pattern_parts):
        # 允许数组尾插：/payload/tags/- 与 /payload/tags 同级规则由 allowlist 显式列出。
        return False
    for actual, expected in zip(path_parts, pattern_parts):
        if expected == "*":
            continue
        if actual != expected:
            return False
    return True


def validate_json_patch(
    patch: Sequence[Mapping[str, Any]],
    *,
    hook: str,
) -> list[dict[str, Any]]:
    """校验 transform Patch，并返回规范化后的操作列表。"""

    allowlist = HOOK_PATCH_ALLOWLIST.get(hook, frozenset())
    if not allowlist:
        raise PluginManifestError(f"Hook {hook} 不允许 transform Patch。")
    if not isinstance(patch, Sequence) or isinstance(patch, (str, bytes)):
        raise PluginManifestError("patch 必须是数组。")
    normalized: list[dict[str, Any]] = []
    for index, item in enumerate(patch):
        if not isinstance(item, Mapping):
            raise PluginManifestError(f"patch[{index}] 必须是对象。")
        op = str(item.get("op", "")).strip()
        path = str(item.get("path", "")).strip()
        if op not in JSON_PATCH_OPS:
            raise PluginManifestError(f"patch[{index}] 不支持 op={op}，V1 仅允许 add/replace/remove。")
        if not path.startswith("/"):
            raise PluginManifestError(f"patch[{index}].path 必须是 JSON Pointer：{path}")
        if not path_matches_allowlist(path, allowlist):
            raise PluginManifestError(f"patch[{index}].path 不在 Hook {hook} 白名单：{path}")
        entry: dict[str, Any] = {"op": op, "path": path}
        if op in {"add", "replace"}:
            if "value" not in item:
                raise PluginManifestError(f"patch[{index}] 缺少 value。")
            entry["value"] = item["value"]
        normalized.append(entry)
    return normalized


def apply_json_patch(document: dict[str, Any], patch: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """在深拷贝上应用受限 JSON Patch。

    只实现 V1 需要的 add/replace/remove，以及数组尾插 path/-。
    """

    import copy

    result = copy.deepcopy(document)
    for operation in patch:
        op = operation["op"]
        path = str(operation["path"])
        parts = path.split("/")[1:]
        if not parts:
            raise PluginManifestError("不允许 Patch 根文档。")
        parent = result
        for part in parts[:-1]:
            key = _decode_pointer_token(part)
            if isinstance(parent, list):
                parent = parent[int(key)]
            else:
                if key not in parent or not isinstance(parent[key], (dict, list)):
                    parent[key] = {}
                parent = parent[key]
        last = _decode_pointer_token(parts[-1])
        if op == "add":
            value = operation["value"]
            if isinstance(parent, list):
                if last == "-":
                    parent.append(value)
                else:
                    parent.insert(int(last), value)
            else:
                parent[last] = value
        elif op == "replace":
            value = operation["value"]
            if isinstance(parent, list):
                parent[int(last)] = value
            else:
                if last not in parent:
                    raise PluginManifestError(f"replace 目标不存在：{path}")
                parent[last] = value
        elif op == "remove":
            if isinstance(parent, list):
                del parent[int(last)]
            else:
                if last not in parent:
                    raise PluginManifestError(f"remove 目标不存在：{path}")
                del parent[last]
        else:
            raise PluginManifestError(f"不支持的 Patch op：{op}")
    return result


def _decode_pointer_token(token: str) -> str:
    return token.replace("~1", "/").replace("~0", "~")


def mode_rank(mode: str) -> int:
    """执行顺序：guard → transform → observe/notify。"""

    order = {
        HANDLER_MODE_GUARD: 0,
        HANDLER_MODE_TRANSFORM: 1,
        HANDLER_MODE_OBSERVE: 2,
        HANDLER_MODE_NOTIFY: 2,
    }
    return order.get(mode, 9)


def scope_rank(scope: str) -> int:
    # project 高于 user：排序时更小的 rank 先执行。
    return 0 if scope == "project" else 1


def sort_handlers(handlers: Sequence[ResolvedHandler]) -> list[ResolvedHandler]:
    return sorted(
        handlers,
        key=lambda item: (
            mode_rank(item.mode),
            -item.priority,
            scope_rank(item.scope),
            item.plugin_name,
            item.handler_id,
        ),
    )


def parse_hook_result(raw: Any, *, handler_key: str = "", elapsed_ms: float = 0.0) -> HookResult:
    """把 Worker 返回值规范化为 HookResult。"""

    if raw is None:
        return HookResult.continue_result(handler_key=handler_key, elapsed_ms=elapsed_ms)
    if not isinstance(raw, Mapping):
        raise PluginProtocolError(f"Handler {handler_key} 返回值必须是对象。")
    action = str(raw.get("action", "continue")).strip()
    annotations = raw.get("annotations") or {}
    if annotations and not isinstance(annotations, Mapping):
        raise PluginProtocolError(f"Handler {handler_key} annotations 必须是对象。")
    if action == "continue":
        return HookResult.continue_result(
            handler_key=handler_key,
            annotations=annotations,
            elapsed_ms=elapsed_ms,
        )
    if action == "approve":
        # 插件永远不能代替 Host 审批放行；该 action 一律视为协议错误。
        raise PluginProtocolError(
            f"Handler {handler_key} 返回了非法 action=approve；插件只能 deny，不能批准。"
        )
    if action == "deny":
        reason = str(raw.get("reason", "")).strip() or "插件拒绝操作"
        return HookResult.deny_result(
            reason,
            code=str(raw.get("code", "")).strip(),
            handler_key=handler_key,
            elapsed_ms=elapsed_ms,
        )
    if action == "patch":
        patch = raw.get("patch", [])
        if not isinstance(patch, list):
            raise PluginProtocolError(f"Handler {handler_key} patch 必须是数组。")
        return HookResult.patch_result(
            patch,
            handler_key=handler_key,
            annotations=annotations,
            elapsed_ms=elapsed_ms,
        )
    raise PluginProtocolError(f"Handler {handler_key} action 非法：{action}")
