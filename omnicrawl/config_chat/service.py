"""配置对话：本地推理、类型校验、TOML 写回和运行态同步。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List

from ..config.core.runtime import (
    get_section,
    load_config_data,
    resolve_subagents_path,
    resolve_subagents_write_path,
    save_config_data,
)
from ..config.core.settings import save_feature_enabled, save_show_thinking, save_subagent_setting
from .router import ConfigRouter, ConfigRouterUnavailable


@dataclass(frozen=True)
class ConfigChatCommand:
    action: str
    config: str
    value: str = ""
    score: float = 0.0


@dataclass(frozen=True)
class ConfigChange:
    path: str
    value: Any


class ConfigChatError(ValueError):
    """配置对话输入或写回失败。"""


class ConfigChatService:
    def __init__(self, agent: Any | None = None) -> None:
        self.agent = agent
        self._router: ConfigRouter | None = None
        self._router_error: str | None = None
        self._load_schema()

    def _load_schema(self) -> None:
        try:
            from importlib.resources import files
            import json
            labels = json.loads(files("omnicrawl.config_chat.assets").joinpath("labels.json").read_text(encoding="utf-8"))["keys"]
        except Exception as exc:
            self._router_error = f"配置对话资源不可用：{exc}"
            self._labels = {}
            return
        self._labels = {item["path"]: item for item in labels}

    @property
    def available(self) -> bool:
        return self._router is not None or self._router_error is None

    @property
    def unavailable_reason(self) -> str:
        return self._router_error or "配置对话模型尚未加载。"

    def _get_router(self) -> ConfigRouter:
        if self._router is None:
            try:
                self._router = ConfigRouter()
            except ConfigRouterUnavailable as exc:
                self._router_error = str(exc)
                raise ConfigChatError(str(exc)) from exc
            except Exception as exc:
                self._router_error = f"配置对话模型加载失败：{exc}"
                raise ConfigChatError(self._router_error) from exc
        return self._router

    def predict(self, text: str) -> List[ConfigChatCommand]:
        if not text.strip():
            return []
        try:
            rows = self._get_router().predict(text)
        except ConfigChatError:
            raise
        except Exception as exc:
            raise ConfigChatError(f"配置对话推理失败：{exc}") from exc
        return [ConfigChatCommand(**row) for row in rows]

    def apply_text(self, text: str) -> List[ConfigChange]:
        commands = self.predict(text)
        if not commands:
            raise ConfigChatError("没有识别到可修改的配置。")
        prepared = [
            (command, self._prepare_command(command))
            for command in commands
        ]
        changes: List[ConfigChange] = []
        for command, value in prepared:
            self._write_value(command.config, value)
            self._sync_runtime(command.config, value)
            changes.append(ConfigChange(command.config, value))
        return changes

    def _prepare_command(self, command: ConfigChatCommand) -> Any:
        if command.config not in self._labels:
            raise ConfigChatError(f"不允许修改未知配置：{command.config}")
        if command.action in {"OPEN", "TOGGLE", "RESET"}:
            if command.action == "OPEN":
                raise ConfigChatError(f"“{command.config}”是查看请求，不是修改请求。")
            raise ConfigChatError(f"暂不支持“{command.action}”操作：{command.config}")
        return self._coerce_value(command.config, command.value, command.action)

    def _coerce_value(self, path: str, raw: str, action: str) -> Any:
        kind = self._labels[path].get("type", "str")
        if action == "ENABLE":
            return True
        if action == "DISABLE":
            return False
        value = raw.strip()
        if kind == "bool":
            lowered = value.casefold()
            if lowered in {"true", "1", "on", "开", "开启", "启用", "打开"}:
                return True
            if lowered in {"false", "0", "off", "关", "关闭", "禁用"}:
                return False
            raise ConfigChatError(f"{path} 需要布尔值，收到：{raw}")
        try:
            if kind == "int":
                return int(value)
            if kind == "float":
                return float(value)
        except ValueError as exc:
            raise ConfigChatError(f"{path} 需要 {kind}，收到：{raw}") from exc
        if kind in {"list", "dict"}:
            raise ConfigChatError(f"暂不支持直接修改复合配置：{path}")
        return value

    def _write_value(self, path: str, value: Any) -> None:
        target = resolve_subagents_write_path() if path.startswith("subagents.") else None
        parts = path.split(".")
        data = load_config_data(target or None)
        cursor: Dict[str, Any] = data
        for part in parts[:-1]:
            section = cursor.get(part, {})
            if not isinstance(section, dict):
                raise ConfigChatError(f"配置路径不是对象：{part}")
            section = dict(section)
            cursor[part] = section
            cursor = section
        cursor[parts[-1]] = value
        save_config_data(data, target or None)

    def _sync_runtime(self, path: str, value: Any) -> None:
        if self.agent is None:
            return
        section, _, field = path.rpartition(".")
        setter_map: Dict[str, Callable[[Any], None]] = {
            "tts.enabled": getattr(self.agent, "set_tts_enabled", lambda _value: None),
            "memory.enabled": getattr(self.agent, "set_memory_enabled", lambda _value: None),
            "plugins.enabled": getattr(self.agent, "set_plugin_enabled", lambda _value: None),
            "subagents.enabled": getattr(self.agent, "set_subagents_enabled", lambda _value: None),
            "ui.show_thinking": getattr(self.agent, "set_show_thinking", lambda _value: None),
            "mcp.enabled": getattr(self.agent, "set_mcp_enabled", lambda _value: None),
            "context_compaction.trigger_context_percent": getattr(self.agent, "set_context_compaction_trigger_percent", lambda _value: None),
        }
        setter = setter_map.get(path)
        if setter is not None:
            setter(value)
            return
        if section in {"tts", "image_gen", "vision", "desensitization", "run_guard", "agent_workspace", "advisor"}:
            # 复杂段的运行态由下次启动或设置页完整编辑器加载；持久化已成功。
            return
