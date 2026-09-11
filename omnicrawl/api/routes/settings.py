"""运行设置路由：GET /settings 全量只读 + PUT /settings/<domain> 增量写。

对齐 TUI「运行设置」面板 16 个一级项。写端点按域接收白名单字段，
未传字段保持当前值；运行中亦可修改（设置属运行时配置，即时生效）。
敏感字段（api_key / token / 渠道凭据）不回传明文，仅回 ``has_*`` 标记；
客户端不提供时写操作保留原值。
"""

from __future__ import annotations

from dataclasses import replace
from typing import Any, Optional

from fastapi import APIRouter, Request
from pydantic import BaseModel, Field

from ...config.core.settings import (
    SettingsConfigError,
    load_show_thinking,
    save_context_compaction_trigger_percent,
    save_context_window_tokens,
    save_feature_enabled,
    save_show_thinking,
)
from ...config.features.agent_workspace import (
    AgentWorkspaceConfig,
    load_agent_workspace_config,
    save_agent_workspace_config,
)
from ...config.features.image_gen import (
    ImageGenConfiguration,
    load_image_gen_configuration,
    save_image_gen_configuration,
)
from ...config.features.run_guard import (
    RunGuardConfig,
    load_run_guard_config,
    save_run_guard_config,
)
from ...config.features.tts import (
    TTSConfiguration,
    load_tts_configuration,
    save_tts_configuration,
)
from ...config.models.channels import load_channel_configuration
from ...config.models.vision import VisionConfiguration, load_vision_configuration
from ...mcp.config import load_mcp_config
from ..deps import data, service
from ..models import APIServiceError

router = APIRouter(tags=["settings"])

# 内置工具开关清单（读写在 /settings 与 PUT /settings/tools 提供）
from ...config.features.tools import (  # noqa: E402
    TOOL_SWITCH_KEYS,
    save_tool_switches,
    validate_tool_switch_name,
)
from ...config.features.subagents import SUBAGENT_ADVANCED_SETTING_KEYS  # noqa: E402


# ---------- PUT 请求模型（白名单字段，宽松：未传保持原值） ----------


class BoolSetting(BaseModel):
    enabled: bool


class ContextWindowSetting(BaseModel):
    window_tokens: int = Field(gt=0, le=1_000_000_000)


class ContextCompactionSetting(BaseModel):
    trigger_percent: int = Field(ge=1, le=100)


class FeaturesSetting(BaseModel):
    """功能开关批量更新；strict 模式拒绝 'yes'/'1' 等隐式布尔。"""

    model_config = {"strict": True}

    memory: Optional[bool] = None
    plugins: Optional[bool] = None
    subagents: Optional[bool] = None
    show_thinking: Optional[bool] = None


class AgentWorkspaceSetting(BaseModel):
    enabled: Optional[bool] = None
    mode: Optional[str] = None
    base_branch: Optional[str] = None
    detached: Optional[bool] = None
    apply_on_exit: Optional[bool] = None
    cleanup_on_exit: Optional[str] = None
    sync_uncommitted: Optional[bool] = None


class ImageGenSetting(BaseModel):
    enabled: Optional[bool] = None
    base_url: Optional[str] = None
    api_key_env: Optional[str] = None
    model: Optional[str] = None
    size: Optional[str] = None
    quality: Optional[str] = None
    output_format: Optional[str] = None
    n: Optional[int] = Field(default=None, ge=1, le=10)
    timeout_seconds: Optional[int] = Field(default=None, ge=1, le=600)
    # api_key 仅允许空字符串（=清除）；传非空明文直接拒绝（凭据应走环境变量/本地）。
    api_key: Optional[str] = Field(default=None, max_length=0)


class TtsSetting(BaseModel):
    enabled: Optional[bool] = None
    model_dir: Optional[str] = None
    voice: Optional[str] = None
    auto_play: Optional[bool] = None
    thread_count: Optional[int] = Field(default=None, ge=1, le=32)
    device: Optional[str] = None
    streaming: Optional[bool] = None
    output_dir: Optional[str] = None


# ---------- 辅助：把 dataclass 安全序列化（过滤敏感） ----------


def _safe_config_payload(*, image_gen: bool = False, mcp: bool = False) -> None:
    """占位（避免误用）：实际过滤在各 load 后手工构建。"""


def _serialize_run_guard(config: RunGuardConfig) -> dict[str, Any]:
    return {
        "enabled": config.enabled,
        "guard": {
            "enabled": config.guard.enabled,
            "window_chars": config.guard.window_chars,
            "substr_len": config.guard.substr_len,
            "repeat_ratio": config.guard.repeat_ratio,
            "check_every": config.guard.check_every,
            "max_blocks": config.guard.max_blocks,
            "max_chars": config.guard.max_chars,
            "max_guard_retries": config.guard.max_guard_retries,
            "auto_retry_errors": list(config.guard.auto_retry_errors),
        },
        "continuation": {
            "enabled": config.continuation.enabled,
            "max_auto_followups": config.continuation.max_auto_followups,
        },
    }


def _serialize_agent_workspace(config: AgentWorkspaceConfig) -> dict[str, Any]:
    return {
        "enabled": config.enabled,
        "mode": config.mode,
        "base_branch": config.base_branch,
        "base_ref": config.base_ref,
        "detached": config.detached,
        "apply_on_exit": config.apply_on_exit,
        "cleanup_on_exit": config.cleanup_on_exit,
        "sync_uncommitted": config.sync_uncommitted,
        "copy_dirs": list(config.copy_dirs),
        "env_scripts": list(config.env_scripts),
    }


def _serialize_image_gen(config: ImageGenConfiguration) -> dict[str, Any]:
    return {
        "enabled": config.enabled,
        "base_url": config.base_url,
        "api_key_env": config.api_key_env,
        "has_api_key": bool(config.api_key),
        "model": config.model,
        "size": config.size,
        "quality": config.quality,
        "output_format": config.output_format,
        "n": config.n,
        "timeout_seconds": config.timeout_seconds,
    }


def _serialize_tts(config: TTSConfiguration) -> dict[str, Any]:
    return {
        "enabled": config.enabled,
        "model_dir": config.model_dir,
        "voice": config.voice,
        "auto_play": config.auto_play,
        "thread_count": config.thread_count,
        "device": config.device,
        "streaming": config.streaming,
        "output_dir": config.output_dir,
    }


def _serialize_vision(config: VisionConfiguration) -> dict[str, Any]:
    return {
        "enabled": config.enabled,
        "models": [ref.to_dict() for ref in config.models],
    }


def _config_path(request: Request) -> Optional[str]:
    """优先使用 agent 已解析的 config 路径；否则默认（None → 运行时默认）。"""
    return None


def _as_invalid_setting(exc: Exception) -> APIServiceError:
    """把配置构造/校验异常转为 400（避免落到 500 内部错误）。"""
    return APIServiceError("INVALID_SETTING", f"配置值无效：{exc}", status_code=400)


# ---------- GET /settings ----------


@router.get("/settings")
def get_settings(request: Request) -> dict[str, Any]:
    """全量只读设置快照（不含敏感明文）。"""
    agent = service(request).agent
    config = getattr(agent, "config", None)
    llm = getattr(config, "llm", None)
    if llm is None:
        from ...config.models.llm import load_llm_config

        llm = load_llm_config()
    compaction = getattr(config, "context_compaction", None)
    context_window = int(getattr(llm, "context_window_tokens", 128_000))

    def feature_enabled(name: str) -> bool:
        if name == "memory":
            return bool(getattr(agent, "_memory_store", None))
        if name == "plugins":
            return bool(getattr(getattr(agent, "_plugin_manager", None), "enabled", False))
        if name == "subagents":
            return bool(getattr(getattr(agent.config, "subagents", None), "enabled", False))
        if name == "mcp":
            return bool(getattr(getattr(agent, "_mcp_manager", None), "enabled", False))
        return False

    run_guard = getattr(agent.config, "run_guard", None)
    if not isinstance(run_guard, RunGuardConfig):
        run_guard = load_run_guard_config()
    workspace = getattr(agent.config, "agent_workspace", None)
    if not isinstance(workspace, AgentWorkspaceConfig):
        workspace = load_agent_workspace_config()

    return data(
        {
            "model": {
                "current": agent.current_model,
                "source": str(getattr(llm, "model_source", "legacy") or "legacy"),
                "catalog_key": str(getattr(llm, "catalog_key", "") or ""),
                "context_window_tokens": context_window,
                "reasoning_effort": str(getattr(llm, "reasoning_effort", "none") or "none"),
            },
            "approval": agent.approval_mode,
            "context": {"window_tokens": context_window},
            "context_compaction": {
                "trigger_percent": getattr(compaction, "trigger_context_percent", 80),
                "trigger_tokens": getattr(compaction, "trigger_context_tokens", 0),
            },
            "show_thinking": getattr(agent.config, "show_thinking", True),
            "features": {
                "memory": feature_enabled("memory"),
                "plugins": feature_enabled("plugins"),
                "subagents": feature_enabled("subagents"),
                "mcp": feature_enabled("mcp"),
            },
            "run_guard": _serialize_run_guard(run_guard),
            "agent_workspace": _serialize_agent_workspace(workspace),
            "vision": _serialize_vision(
                getattr(agent.config, "vision", None)
                or load_vision_configuration()
            ),
            "image_gen": _serialize_image_gen(
                getattr(agent.config, "image_gen", None)
                or load_image_gen_configuration()
            ),
            "tts": _serialize_tts(
                getattr(agent.config, "tts", None) or load_tts_configuration()
            ),
            "advisor": _load_advisor_payload(agent),
            "channels": _load_channels_payload(),
            "tools": {
                "switches": {
                    name: not (name in frozenset(getattr(agent.config, "disabled_tools", ())))
                    for name in TOOL_SWITCH_KEYS
                },
                "keys": list(TOOL_SWITCH_KEYS),
            },
            "mcp": _load_mcp_payload(),
            "subagents_advanced": _load_subagents_advanced_payload(agent),
        }
    )


def _load_advisor_payload(agent: Any) -> dict[str, Any]:
    config = getattr(agent.config, "advisor", None)
    if config is None:
        from ...config.features.advisor import AdvisorConfig, load_advisor_config

        try:
            config = load_advisor_config()
        except Exception:
            config = AdvisorConfig()
    return {
        "enabled": bool(getattr(config, "enabled", False)),
        "model_key": str(getattr(config, "model_key", "") or ""),
        "effort": str(getattr(config, "effort", "none") or "none"),
        "disabled_for_models": list(getattr(config, "disabled_for_models", ()) or ()),
    }


def _load_channels_payload() -> dict[str, Any]:
    try:
        configuration = load_channel_configuration()
        return {
            "default_key": configuration.default_key,
            "channels": [
                {
                    "name": channel.name,
                    "provider": channel.provider,
                    "enabled": channel.enabled,
                    "has_api_key": bool(getattr(channel, "api_key", "")),
                    "api_key_env": str(getattr(channel, "api_key_env", "") or ""),
                    "base_url": str(getattr(channel, "base_url", "") or ""),
                }
                for channel in configuration.channels
            ],
        }
    except Exception:
        return {"default_key": "", "channels": []}


def _load_mcp_payload() -> dict[str, Any]:
    try:
        config = load_mcp_config()
        return {
            "enabled": config.enabled,
            "default_timeout_seconds": config.default_timeout_seconds,
            "servers": [
                {
                    "name": server.name,
                    "enabled": server.enabled,
                    "transport": server.transport,
                    "command": server.command,
                    "url": server.url,
                    "timeout_seconds": server.timeout_seconds,
                    "risk_level": server.risk_level,
                }
                for server in config.servers.values()
            ],
            "policy": {
                "require_confirmation_for_write": config.policy.require_confirmation_for_write,
                "require_confirmation_for_command": config.policy.require_confirmation_for_command,
                "allow_external_network_tools": config.policy.allow_external_network_tools,
                "audit_log_enabled": config.policy.audit_log_enabled,
            },
        }
    except Exception:
        return {"enabled": False, "default_timeout_seconds": 30, "servers": [], "policy": {}}


def _load_subagents_advanced_payload(agent: Any) -> dict[str, Any]:
    config = getattr(agent.config, "subagents", None)
    if config is None:
        from ...config.features.subagents import load_subagent_config

        try:
            config = load_subagent_config()
        except Exception:
            return {"values": {}, "keys": list(SUBAGENT_ADVANCED_SETTING_KEYS)}
    return {
        "values": {
            key: getattr(config, key, None)
            for key in SUBAGENT_ADVANCED_SETTING_KEYS
            if hasattr(config, key)
        },
        "keys": list(SUBAGENT_ADVANCED_SETTING_KEYS),
    }


# ---------- 批次 1 写端点 ----------


@router.put("/settings/context")
def put_context(payload: ContextWindowSetting, request: Request) -> dict[str, Any]:
    current = service(request)
    agent = current.agent
    tokens = payload.window_tokens
    percent = int(
        getattr(
            getattr(agent.config, "context_compaction", None),
            "trigger_context_percent",
            80,
        )
        or 80
    )
    previous = int(getattr(agent, "context_window_tokens", 128_000))
    model_source = str(getattr(agent.config.llm, "model_source", "legacy") or "legacy")
    catalog_key = str(getattr(agent.config.llm, "catalog_key", "") or "")
    agent.set_context_window_tokens(tokens)
    try:
        path = save_context_window_tokens(
            tokens,
            model_source=model_source,
            catalog_key=catalog_key,
        )
        # 联动压缩阈值比例
        agent.set_context_compaction_trigger_percent(percent)
        save_context_compaction_trigger_percent(
            percent, context_window_tokens=tokens
        )
    except Exception:
        agent.set_context_window_tokens(previous)
        raise
    threshold = tokens * percent // 100
    return data(
        {
            "window_tokens": tokens,
            "trigger_percent": percent,
            "trigger_tokens": threshold,
            "saved_path": str(path),
        }
    )


@router.put("/settings/context_compaction")
def put_context_compaction(
    payload: ContextCompactionSetting, request: Request
) -> dict[str, Any]:
    current = service(request)
    agent = current.agent
    percent = payload.trigger_percent
    context_window = int(getattr(agent, "context_window_tokens", 128_000))
    previous = int(
        getattr(
            getattr(agent.config, "context_compaction", None),
            "trigger_context_percent",
            80,
        )
        or 80
    )
    agent.set_context_compaction_trigger_percent(percent)
    try:
        path = save_context_compaction_trigger_percent(
            percent, context_window_tokens=context_window
        )
    except Exception:
        agent.set_context_compaction_trigger_percent(previous)
        raise
    threshold = context_window * percent // 100
    return data(
        {
            "trigger_percent": percent,
            "trigger_tokens": threshold,
            "window_tokens": context_window,
            "saved_path": str(path),
        }
    )


@router.put("/settings/show_thinking")
def put_show_thinking(payload: BoolSetting, request: Request) -> dict[str, Any]:
    current = service(request)
    agent = current.agent
    previous = bool(getattr(agent.config, "show_thinking", True))
    agent.set_show_thinking(payload.enabled)
    try:
        path = save_show_thinking(payload.enabled)
    except Exception:
        agent.set_show_thinking(previous)
        raise
    return data({"show_thinking": payload.enabled, "saved_path": str(path)})


@router.put("/settings/features")
def put_features(payload: FeaturesSetting, request: Request) -> dict[str, Any]:
    """批量更新功能开关（memory/plugins/subagents/show_thinking）。"""
    current = service(request)
    agent = current.agent
    changes: dict[str, bool] = {}
    for key in ("memory", "plugins", "subagents", "show_thinking"):
        value = getattr(payload, key)
        if value is None:
            continue
        changes[key] = value

    # 逐个应用 + 落盘；任一失败回滚已应用项
    applied: dict[str, bool] = {}
    try:
        for key, value in changes.items():
            if key == "show_thinking":
                previous = bool(getattr(agent.config, "show_thinking", True))
                agent.set_show_thinking(value)
                try:
                    save_show_thinking(value)
                except Exception:
                    agent.set_show_thinking(previous)
                    raise
            else:
                previous = _feature_enabled(agent, key)
                if key == "memory":
                    agent.set_memory_enabled(value)
                elif key == "plugins":
                    agent.set_plugin_enabled(value)
                elif key == "subagents":
                    agent.set_subagents_enabled(value)
                try:
                    save_feature_enabled(key, value)
                except Exception:
                    # 回滚运行态
                    if key == "memory":
                        agent.set_memory_enabled(previous)
                    elif key == "plugins":
                        agent.set_plugin_enabled(previous)
                    elif key == "subagents":
                        agent.set_subagents_enabled(previous)
                    raise
            applied[key] = value
    except Exception as exc:
        raise APIServiceError(
            "SETTINGS_PARTIAL_FAILED",
            f"功能开关保存失败：{exc}",
            status_code=502,
        ) from exc
    return data(
        {
            "features": {
                "memory": _feature_enabled(agent, "memory"),
                "plugins": _feature_enabled(agent, "plugins"),
                "subagents": _feature_enabled(agent, "subagents"),
                "show_thinking": bool(getattr(agent.config, "show_thinking", True)),
            }
        }
    )


def _feature_enabled(agent: Any, name: str) -> bool:
    if name == "memory":
        return bool(getattr(agent, "_memory_store", None))
    if name == "plugins":
        return bool(getattr(getattr(agent, "_plugin_manager", None), "enabled", False))
    if name == "subagents":
        return bool(getattr(getattr(agent.config, "subagents", None), "enabled", False))
    if name == "show_thinking":
        return bool(getattr(agent.config, "show_thinking", True))
    return False


@router.put("/settings/run_guard")
def put_run_guard(payload: dict[str, Any], request: Request) -> dict[str, Any]:
    """更新持续运转配置：接受 {enabled} 或完整 {guard,continuation} 子集。"""
    current = service(request)
    agent = current.agent
    previous = getattr(agent.config, "run_guard", None)
    if not isinstance(previous, RunGuardConfig):
        previous = load_run_guard_config()

    next_config = previous
    try:
        if "enabled" in payload:
            enabled = payload["enabled"]
            if not isinstance(enabled, bool):
                raise APIServiceError("INVALID_SETTING", "run_guard.enabled 必须是布尔值。")
            next_config = replace(next_config, enabled=enabled)

        guard_updates: dict[str, Any] = {}
        if isinstance(payload.get("guard"), dict):
            for field_name in (
                "enabled",
                "window_chars",
                "substr_len",
                "repeat_ratio",
                "check_every",
                "max_blocks",
                "max_chars",
                "max_guard_retries",
                "auto_retry_errors",
            ):
                if field_name in payload["guard"]:
                    guard_updates[field_name] = payload["guard"][field_name]
        if guard_updates:
            next_config = replace(
                next_config,
                guard=replace(next_config.guard, **guard_updates),
            )

        continuation_updates: dict[str, Any] = {}
        if isinstance(payload.get("continuation"), dict):
            for field_name in ("enabled", "max_auto_followups"):
                if field_name in payload["continuation"]:
                    continuation_updates[field_name] = payload["continuation"][field_name]
        if continuation_updates:
            next_config = replace(
                next_config,
                continuation=replace(next_config.continuation, **continuation_updates),
            )
        # 校验（dataclass __post_init__ 抛 RunGuardConfigError）
        RunGuardConfig(
            enabled=next_config.enabled,
            guard=next_config.guard,
            continuation=next_config.continuation,
        )
    except APIServiceError:
        raise
    except Exception as exc:  # noqa: BLE001 - 配置校验异常统一转 400
        raise _as_invalid_setting(exc) from exc
    agent.set_run_guard_configuration(next_config)
    try:
        path = save_run_guard_config(next_config)
    except Exception:
        agent.set_run_guard_configuration(previous)
        raise
    return data({**_serialize_run_guard(next_config), "saved_path": str(path)})


@router.put("/settings/agent_workspace")
def put_agent_workspace(
    payload: AgentWorkspaceSetting, request: Request
) -> dict[str, Any]:
    """更新隔离工作区配置：只更新显式提供的字段，其余保持当前值。"""
    current = service(request)
    agent = current.agent
    previous = getattr(agent.config, "agent_workspace", None)
    if not isinstance(previous, AgentWorkspaceConfig):
        previous = load_agent_workspace_config()

    updates: dict[str, Any] = {}
    for field_name in (
        "enabled",
        "mode",
        "base_branch",
        "detached",
        "apply_on_exit",
        "cleanup_on_exit",
        "sync_uncommitted",
    ):
        value = getattr(payload, field_name)
        if value is not None:
            updates[field_name] = value
    if not updates:
        return data(_serialize_agent_workspace(previous))

    try:
        next_config = replace(previous, **updates)
        # 触发 __post_init__ 校验（replace 内部已重建实例）
        AgentWorkspaceConfig(**next_config.__dict__)
    except Exception as exc:  # noqa: BLE001 - 配置校验异常统一转 400
        raise _as_invalid_setting(exc) from exc
    agent.set_agent_workspace_configuration(next_config)
    try:
        path = save_agent_workspace_config(next_config)
    except Exception:
        agent.set_agent_workspace_configuration(previous)
        raise
    return data({**_serialize_agent_workspace(next_config), "saved_path": str(path)})


@router.put("/settings/vision")
def put_vision(payload: BoolSetting, request: Request) -> dict[str, Any]:
    """批次 1：仅视觉总开关（模型引用管理在后续批次）。"""
    current = service(request)
    agent = current.agent
    previous = getattr(agent.config, "vision", None)
    if not isinstance(previous, VisionConfiguration):
        previous = load_vision_configuration()
    next_config = replace(previous, enabled=payload.enabled)
    agent.set_vision_configuration(next_config)
    try:
        path = _save_vision(next_config)
    except Exception:
        agent.set_vision_configuration(previous)
        raise
    return data({**_serialize_vision(next_config), "saved_path": str(path)})


def _save_vision(config: VisionConfiguration):
    from ...config.models.vision import save_vision_configuration

    return save_vision_configuration(config)


@router.put("/settings/image_gen")
def put_image_gen(payload: ImageGenSetting, request: Request) -> dict[str, Any]:
    """更新图像生成配置；不接收 api_key 明文（保留原值或通过清空 api_key_env 禁用）。"""
    current = service(request)
    agent = current.agent
    previous = getattr(agent.config, "image_gen", None)
    if not isinstance(previous, ImageGenConfiguration):
        previous = load_image_gen_configuration()

    updates: dict[str, Any] = {}
    for field_name in (
        "enabled",
        "base_url",
        "api_key_env",
        "model",
        "size",
        "quality",
        "output_format",
        "n",
        "timeout_seconds",
    ):
        value = getattr(payload, field_name)
        if value is not None:
            updates[field_name] = value
    # api_key 不接收明文：仅显式传空字符串（api_key=""）时清除；未传/None 保留原值。
    if payload.api_key is not None:
        if payload.api_key != "":
            raise APIServiceError(
                "INVALID_SETTING",
                "image_gen.api_key 不接受明文写入；请使用 api_key_env 环境变量。",
            )
        updates["api_key"] = ""

    if not updates:
        return data(_serialize_image_gen(previous))
    try:
        next_config = replace(previous, **updates)
        ImageGenConfiguration(
            enabled=next_config.enabled,
            base_url=next_config.base_url,
            api_key=next_config.api_key,
            api_key_env=next_config.api_key_env,
            model=next_config.model,
            size=next_config.size,
            quality=next_config.quality,
            output_format=next_config.output_format,
            n=next_config.n,
            timeout_seconds=next_config.timeout_seconds,
        )
    except Exception as exc:  # noqa: BLE001 - 配置校验异常统一转 400
        raise _as_invalid_setting(exc) from exc
    agent.set_image_gen_configuration(next_config)
    try:
        path = save_image_gen_configuration(next_config)
    except Exception:
        agent.set_image_gen_configuration(previous)
        raise
    return data({**_serialize_image_gen(next_config), "saved_path": str(path)})


@router.put("/settings/tts")
def put_tts(payload: TtsSetting, request: Request) -> dict[str, Any]:
    """更新 TTS 配置：只更新显式提供的字段，其余保持当前值。"""
    current = service(request)
    agent = current.agent
    previous = getattr(agent.config, "tts", None)
    if not isinstance(previous, TTSConfiguration):
        previous = load_tts_configuration()

    updates: dict[str, Any] = {}
    for field_name in (
        "enabled",
        "model_dir",
        "voice",
        "auto_play",
        "thread_count",
        "device",
        "streaming",
        "output_dir",
    ):
        value = getattr(payload, field_name)
        if value is not None:
            updates[field_name] = value
    if not updates:
        return data(_serialize_tts(previous))
    try:
        next_config = replace(previous, **updates)
        TTSConfiguration(
            enabled=next_config.enabled,
            model_dir=next_config.model_dir,
            voice=next_config.voice,
            auto_play=next_config.auto_play,
            thread_count=next_config.thread_count,
            device=next_config.device,
            streaming=next_config.streaming,
            output_dir=next_config.output_dir,
        )
    except Exception as exc:  # noqa: BLE001 - 配置校验异常统一转 400
        raise _as_invalid_setting(exc) from exc
    agent.set_tts_configuration(next_config)
    try:
        path = save_tts_configuration(next_config)
    except Exception:
        agent.set_tts_configuration(previous)
        raise
    return data({**_serialize_tts(next_config), "saved_path": str(path)})


def _rollback_tool_switches(
    agent: Any,
    applied: dict[str, bool],
    previous_states: dict[str, bool],
) -> None:
    """尽力把已应用的开关恢复为原状态（回滚失败不掩盖原始异常）。"""

    for name in applied:
        try:
            agent.set_tool_enabled(name, previous_states[name])
        except Exception:  # noqa: BLE001 - 回滚尽力而为
            pass


@router.put("/settings/tools")
def put_tools(payload: dict[str, Any], request: Request) -> dict[str, Any]:
    """更新内置工具开关：接受 {name, enabled} 或 {switches: {name: bool}}。

    全部名称与取值先校验、运行时逐项应用，最后一次性原子落盘到
    config.toml [tools]；任一步失败都会回滚已应用的开关，不留部分修改。
    """
    current = service(request)
    agent = current.agent

    entries: list[tuple[str, bool]] = []
    raw_switches = payload.get("switches")
    if isinstance(raw_switches, dict):
        for name, enabled in raw_switches.items():
            if not isinstance(enabled, bool):
                raise APIServiceError(
                    "INVALID_SETTING", f"tools.{name} 必须是布尔值。", status_code=400
                )
            entries.append((str(name), enabled))
    else:
        name = payload.get("name")
        enabled = payload.get("enabled")
        if not isinstance(name, str) or not name.strip() or not isinstance(enabled, bool):
            raise APIServiceError(
                "INVALID_SETTING",
                "tools 更新需要 {name, enabled} 或 {switches: {name: bool}}。",
                status_code=400,
            )
        entries.append((name.strip(), enabled))

    # 先校验全部名称，避免批量更新中途失败留下部分改动。
    normalized_entries: list[tuple[str, bool]] = []
    for raw_name, enabled in entries:
        try:
            normalized_entries.append((validate_tool_switch_name(raw_name), enabled))
        except Exception as exc:  # noqa: BLE001 - 校验异常统一转 400
            raise _as_invalid_setting(exc) from exc

    previous_states = {
        name: name not in frozenset(getattr(agent.config, "disabled_tools", ()))
        for name, _ in normalized_entries
    }
    applied: dict[str, bool] = {}
    for name, enabled in normalized_entries:
        try:
            agent.set_tool_enabled(name, enabled)
        except Exception as exc:  # noqa: BLE001 - 运行时切换失败回滚并报 400
            _rollback_tool_switches(agent, applied, previous_states)
            raise _as_invalid_setting(exc) from exc
        applied[name] = enabled

    if normalized_entries:
        try:
            save_tool_switches({name: enabled for name, enabled in normalized_entries})
        except Exception:
            # 持久化失败：回滚全部运行时改动（恢复各自原开关）
            _rollback_tool_switches(agent, applied, previous_states)
            raise

    switches = {
        name: not (name in frozenset(getattr(agent.config, "disabled_tools", ())))
        for name in TOOL_SWITCH_KEYS
    }
    return data({"switches": switches, "applied": applied})


__all__ = ["router"]
