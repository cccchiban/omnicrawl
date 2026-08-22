"""运行时配置 setter：模型、审批、压缩、工具开关、记忆/MCP/插件。

全部为 ``LocalToolAgent.config`` 的运行时修改入口，持久化由调用方负责；
重建工具表或压缩服务实例的失败都走事务式回滚。"""
from __future__ import annotations

import threading
from dataclasses import dataclass, field, replace
from typing import Any, Callable, Mapping, Sequence
from ....approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_REVIEW,
    load_approval_mode,
    load_approval_review_model,
    normalize_approval_mode,
)
from ....config.features.image_gen import (
    ImageGenConfiguration,
    load_image_gen_configuration,
)
from ....config.features.tts import (
    TTSConfiguration,
    load_tts_configuration,
)
from ....config.models.llm_multi import apply_model_selection, llm_config_to_profile_and_descriptor
from ....config.features.subagents import (
    SubAgentConfig,
    SubAgentConfigError,
    load_subagent_config,
    validate_subagent_advanced_setting,
)
from ....config.features.tools import (
    ToolSwitchConfigError,
    load_disabled_tools,
    validate_tool_switch_name,
)
from ....config.models.vision import VisionConfiguration, load_vision_configuration
from ....llm import (
    LLMConfig,
    LLMError,
    ModelError,
    ModelErrorCode,
    ModelRuntimeManager,
    OpenAIResponseLLM,
    load_llm_config,
    normalize_reasoning_effort,
)
from ....mcp import MCPClientManager, MCPConfig, MCPConfigError, MCPToolMeta, load_mcp_config

from ..shared import (
    AgentError,
    SUBAGENT_LIFECYCLE_WAIT_SECONDS,
    _validate_context_compaction_window,
)


class SessionSettingsMixin:
    """运行时配置 setter：模型、审批、压缩、工具开关、记忆/MCP/插件。"""

    @property
    def approval_mode(self) -> str:
        """当前工具审批模式，供 TUI 展示和斜杠命令切换。"""

        return self.config.approval_mode

    def set_approval_mode(self, mode: str) -> None:
        """运行时切换审批模式；持久化由调用方负责写入 config.toml。"""

        self.config.approval_mode = normalize_approval_mode(mode)

    @property
    def current_model(self) -> str:
        """当前会话用于展示/切换的模型标识。

        自定义模型优先返回 models.toml key；否则返回真实 model_id。
        实际请求使用 config.llm.model。
        """

        catalog_key = getattr(self.config.llm, "catalog_key", "") or ""
        if catalog_key:
            return catalog_key
        return self.config.llm.model

    def set_model(
        self,
        model: str,
        *,
        persist: Callable[[], None] | None = None,
    ) -> None:
        """原子切换运行时模型；可选持久化必须在 Runtime 交换前成功。

        支持：
        - 裸 model_id（兼容旧行为）
        - models.toml key / alias
        - profile/model_id
        """

        selection = model.strip()
        if not selection:
            raise AgentError("模型 ID 不能为空。")

        previous_llm = self.config.llm
        try:
            # 解析失败直接报错，禁止静默把无效 key/配置损坏降级成裸 model_id。
            next_llm = apply_model_selection(previous_llm, selection)
        except LLMError as exc:
            raise AgentError(str(exc)) from exc

        runtime_token = next_llm.catalog_key or next_llm.model
        _validate_context_compaction_window(
            getattr(self.config, "context_compaction", None),
            next_llm,
        )

        # 先构建候选 Runtime 并持久化，全部成功后才更新 Agent 内存配置。
        manager = getattr(self, "_runtime_manager", None)
        if manager is None:
            manager = ModelRuntimeManager()
            self._runtime_manager = manager
        try:
            profile, descriptor = llm_config_to_profile_and_descriptor(next_llm)
            manager.switch(profile, descriptor, persist=persist)
        except Exception as exc:
            raise AgentError(f"模型运行时切换失败：{exc}") from exc

        self.config.llm = next_llm
        self._runtime_model_id = runtime_token
        self.__dict__.pop("_client", None)
        self.__dict__.pop("_context_compaction_service_instance", None)

    @property
    def context_window_tokens(self) -> int:
        """当前模型配置的上下文窗口，用于界面计算 Token 占用率。"""

        return self.config.llm.context_window_tokens

    @property
    def reasoning_effort(self) -> str:
        """当前推理强度，供 TUI 与 API 客户端展示和切换。"""

        return self.config.llm.reasoning_effort

    def set_reasoning_effort(self, effort: str) -> str:
        """运行时切换推理强度；持久化由斜杠命令或 UI 调用方负责。"""

        try:
            normalized = normalize_reasoning_effort(effort)
        except LLMError as exc:
            raise AgentError(str(exc)) from exc
        self.config.llm.reasoning_effort = normalized
        self.config.llm.thinking_type = (
            "disabled" if normalized in {"none", "disabled"} else "enabled"
        )
        return normalized

    def set_context_window_tokens(self, tokens: int) -> int:
        """运行时切换上下文窗口，并实时联动百分比压缩阈值。"""

        if isinstance(tokens, bool) or not isinstance(tokens, int) or tokens <= 0:
            raise AgentError("上下文长度必须是正整数 Token。")
        compaction = getattr(self.config, "context_compaction", None)
        compaction_percent = getattr(compaction, "trigger_context_percent", None)
        if compaction is not None and compaction_percent is not None:
            compaction = replace(
                compaction,
                trigger_context_tokens=max(
                    1,
                    tokens * compaction_percent // 100,
                ),
            )
        _validate_context_compaction_window(compaction, self.config.llm, context_window_tokens=tokens)
        manager = getattr(self, "_runtime_manager", None)
        if manager is not None:
            manager.set_context_window_tokens(tokens)
        self.config.llm.context_window_tokens = tokens
        if compaction is not None:
            self.config.context_compaction = compaction
        self.__dict__.pop("_context_compaction_service_instance", None)
        return tokens

    def set_vision_configuration(self, configuration: VisionConfiguration) -> None:
        """运行时更新视觉代理配置；持久化由视觉设置面板负责。"""

        if not isinstance(configuration, VisionConfiguration):
            raise AgentError("视觉代理配置必须是 VisionConfiguration。")
        self.config.vision = configuration

    def set_image_gen_configuration(self, configuration: ImageGenConfiguration) -> None:
        """运行时更新图像生成配置；持久化由图像生成设置面板负责。"""

        if not isinstance(configuration, ImageGenConfiguration):
            raise AgentError("图像生成配置必须是 ImageGenConfiguration。")
        self.config.image_gen = configuration

    def set_tts_configuration(self, configuration: TTSConfiguration) -> None:
        """运行时更新 TTS 配置；持久化由 TTS 设置面板负责。"""

        if not isinstance(configuration, TTSConfiguration):
            raise AgentError("TTS 配置必须是 TTSConfiguration。")
        previous_tools = self._tools
        previous_enabled = getattr(self.config, "tts", None)
        self.config.tts = configuration
        # 启用/停用变化影响 tts_synthesize 工具的注册，重建工具表。
        try:
            self._tools = self._build_tools()
        except Exception:
            self.config.tts = previous_enabled
            self._tools = previous_tools
            raise
        # 配置变化后缓存引擎失效，下次调用按新参数重建。
        self.__dict__.pop("_tts_engine", None)

    def set_tts_enabled(self, enabled: bool) -> None:
        """事务式切换 TTS 功能开关（等价于更新 tts.enabled）。"""

        if not isinstance(enabled, bool):
            raise AgentError("TTS 开关必须是布尔值。")
        current = getattr(self.config, "tts", None)
        if current is None:
            current = load_tts_configuration()
        from dataclasses import replace

        self.set_tts_configuration(replace(current, enabled=enabled))

    def set_context_compaction_enabled(self, enabled: bool) -> None:
        """切换模型辅助压缩，并同步受摘要授权的证据恢复工具。"""

        if not isinstance(enabled, bool):
            raise AgentError("上下文压缩开关必须是布尔值。")
        current = self.config.context_compaction
        if current.enabled == enabled:
            return
        next_config = replace(current, enabled=enabled)
        _validate_context_compaction_window(next_config, self.config.llm)

        previous_tools = self._tools
        previous_service = self.__dict__.get("_context_compaction_service_instance")
        self.config.context_compaction = next_config
        self.__dict__.pop("_context_compaction_service_instance", None)
        try:
            self._tools = self._build_tools()
        except Exception:
            self.config.context_compaction = current
            self._tools = previous_tools
            if previous_service is not None:
                self._context_compaction_service_instance = previous_service
            raise

    def set_context_compaction_trigger_percent(self, percent: int) -> None:
        """按当前模型上下文窗口的百分比设置自动压缩触发阈值。

        换算公式：``trigger_context_tokens = context_window_tokens * percent // 100``。
        只更新运行态并重建压缩服务实例；持久化由设置面板负责。
        """

        if isinstance(percent, bool) or not isinstance(percent, int) or percent <= 0:
            raise AgentError("上下文压缩阈值百分比必须是正整数。")
        current = self.config.context_compaction
        context_window = int(
            getattr(self.config.llm, "context_window_tokens", 128_000)
        )
        tokens = max(1, context_window * percent // 100)
        if (
            tokens == current.trigger_context_tokens
            and current.trigger_context_percent == percent
        ):
            return
        next_config = replace(
            current,
            trigger_context_tokens=tokens,
            trigger_context_percent=percent,
        )
        _validate_context_compaction_window(next_config, self.config.llm)
        self.config.context_compaction = next_config
        # 阈值变化会改变压缩时机，旧 service 实例若已缓存参数应失效重建。
        self.__dict__.pop("_context_compaction_service_instance", None)

    def set_context_compaction_trigger_tokens(self, tokens: int) -> None:
        """运行时直接设置自动压缩触发阈值（Token）；持久化由设置面板负责。

        用于上下文窗口联动重算或失败回滚时精确恢复阈值；
        只更新运行态并重建压缩服务实例。
        """

        if isinstance(tokens, bool) or not isinstance(tokens, int) or tokens <= 0:
            raise AgentError("上下文压缩阈值必须是正整数 Token。")
        current = self.config.context_compaction
        if (
            tokens == current.trigger_context_tokens
            and current.trigger_context_percent is None
        ):
            return
        next_config = replace(
            current,
            trigger_context_tokens=tokens,
            trigger_context_percent=None,
        )
        _validate_context_compaction_window(next_config, self.config.llm)
        self.config.context_compaction = next_config
        # 阈值变化会改变压缩时机，旧 service 实例若已缓存参数应失效重建。
        self.__dict__.pop("_context_compaction_service_instance", None)

    def set_tool_enabled(self, name: str, enabled: bool) -> None:
        """运行时切换内置工具开关并重建工具表；持久化由设置面板负责。

        开关只影响 Agent 工具表的注册（模型不可见即不可调用），
        不影响正在执行的调用和审批等其他配置。
        """

        if not isinstance(enabled, bool):
            raise AgentError("工具开关必须是布尔值。")
        try:
            normalized = validate_tool_switch_name(name)
        except ToolSwitchConfigError as exc:
            raise AgentError(str(exc)) from exc
        next_disabled = set(self.config.disabled_tools)
        if enabled:
            next_disabled.discard(normalized)
        else:
            next_disabled.add(normalized)
        next_disabled = frozenset(next_disabled)
        if next_disabled == self.config.disabled_tools:
            return
        previous_tools = self._tools
        previous_disabled = self.config.disabled_tools
        self.config.disabled_tools = next_disabled
        try:
            self._tools = self._build_tools()
        except Exception:
            self.config.disabled_tools = previous_disabled
            self._tools = previous_tools
            raise

    def set_show_thinking(self, enabled: bool) -> bool:
        """运行时切换思考块显示；持久化由设置面板负责。

        关闭只隐藏对话区的思考块渲染（Markdown 渲染），思考内容仍照常产生
        并进入推理链路，与模型侧 thinking 开关互不影响。
        """

        if not isinstance(enabled, bool):
            raise AgentError("思考显示开关必须是布尔值。")
        self.config.show_thinking = enabled
        return enabled

    def set_memory_enabled(self, enabled: bool) -> None:
        """切换 Memory 工具，并在新存储准备成功后替换旧运行态。"""

        if not isinstance(enabled, bool):
            raise AgentError("Memory 开关必须是布尔值。")
        if enabled:
            if not hasattr(self.config, "memory_directory") and hasattr(self, "_create_memory_store"):
                # 兼容旧嵌入调用方覆盖的单 Store 工厂。
                project_store = self._create_memory_store()
                session_store = None
                user_store = None
            else:
                project_store, session_store, user_store = self._create_memory_stores()
        else:
            project_store = session_store = user_store = None
        previous_enabled = self.config.memory_enabled
        previous_project_store = getattr(self, "_project_memory_store", getattr(self, "_memory_store", None))
        previous_session_store = getattr(self, "_session_memory_store", None)
        previous_user_store = getattr(self, "_user_memory_store", None)
        previous_legacy_store = getattr(self, "_memory_store", None)
        self.config.memory_enabled = enabled
        self._project_memory_store = project_store
        self._session_memory_store = session_store
        self._user_memory_store = user_store
        self._memory_store = project_store
        try:
            next_tools = self._build_tools()
        except Exception:
            self.config.memory_enabled = previous_enabled
            self._project_memory_store = previous_project_store
            self._session_memory_store = previous_session_store
            self._user_memory_store = previous_user_store
            self._memory_store = previous_legacy_store
            raise
        self._tools = next_tools
        # MemoryStore 当前没有外部进程资源；引用替换后旧实例自然失效。

    def set_mcp_enabled(self, enabled: bool) -> None:
        """事务式切换 MCP；候选 Manager 成功后才关闭旧 Manager。"""

        if not isinstance(enabled, bool):
            raise AgentError("MCP 开关必须是布尔值。")
        current_config = self.config.mcp_config or load_mcp_config()
        self.apply_mcp_config(replace(current_config, enabled=enabled))

    def apply_mcp_config(self, next_config: MCPConfig) -> None:
        """事务式替换 MCP 配置并重建运行中的连接管理器。"""

        if not isinstance(next_config, MCPConfig):
            raise AgentError("MCP 配置类型无效。")
        previous_manager = getattr(self, "_mcp_manager", None)
        previous_config = self.config.mcp_config
        try:
            self.config.mcp_config = next_config
            next_manager = self._create_mcp_manager()
        except Exception as exc:
            self.config.mcp_config = previous_config
            if isinstance(exc, AgentError):
                raise
            raise AgentError(f"MCP 设置应用失败：{exc}") from exc
        self._mcp_manager = next_manager
        try:
            self._tools = self._build_tools()
        except Exception:
            self._mcp_manager = previous_manager
            self.config.mcp_config = previous_config
            try:
                next_manager.close()
            except Exception:
                pass
            raise
        if previous_manager is not None:
            try:
                previous_manager.close()
            except Exception:
                pass

    def set_plugin_enabled(self, enabled: bool) -> None:
        """通过进程级 PluginRuntime 事务切换插件 Worker。"""

        if not isinstance(enabled, bool):
            raise AgentError("Plugin 开关必须是布尔值。")
        callback = getattr(self, "_on_plugin_settings_changed", None)
        if not callable(callback):
            raise AgentError("Plugin Runtime 未连接，无法即时切换插件。")
        try:
            manager = callback(enabled)
        except Exception as exc:
            raise AgentError(f"Plugin 设置应用失败：{exc}") from exc
        self._plugin_manager = manager
        if getattr(self.config.subagents, "enabled", False):
            self._refresh_subagent_definitions()
            # Agent 定义来源变化后同步刷新 subagent_type 枚举，避免模型继续
            # 使用旧插件状态下的角色 Schema。
            self._tools = self._build_tools()

    def set_subagent_advanced_setting(self, name: str, value: int | float) -> None:
        """即时更新面板开放的 SubAgent 资源参数，不改变权限边界。"""

        try:
            normalized = validate_subagent_advanced_setting(name, value)
        except SubAgentConfigError as exc:
            raise AgentError(str(exc)) from exc

        current = self.config.subagents
        next_config = replace(current, **{name: normalized})
        coordinator = getattr(self, "_subagent_coordinator", None)
        if coordinator is not None:
            coordinator.config = next_config
            task_manager = coordinator._task_manager
            if name == "task_retention_minutes":
                task_manager.retention_seconds = max(60.0, normalized * 60)
            if name == "max_concurrency":
                task_manager.set_max_workers(int(normalized))
        self.config.subagents = next_config
        if name == "model_request_concurrency":
            self._subagent_model_request_semaphore = (
                threading.BoundedSemaphore(int(normalized))
                if next_config.enabled
                else None
            )

    def set_subagents_enabled(self, enabled: bool) -> None:
        """安全切换 SubAgent；关闭前等待现有任务退出。"""

        if not isinstance(enabled, bool):
            raise AgentError("SubAgent 开关必须是布尔值。")
        current = self.config.subagents
        if current.enabled == enabled:
            return
        coordinator = getattr(self, "_subagent_coordinator", None)
        if not enabled and coordinator is not None:
            drained = coordinator.cancel_and_wait(
                reason="SubAgent 功能即将关闭，当前子任务已取消。",
                timeout_seconds=SUBAGENT_LIFECYCLE_WAIT_SECONDS,
                permanent=True,
            )
            if not drained:
                raise AgentError("SubAgent 关闭失败：仍有子任务未退出。")
        previous_semaphore = getattr(self, "_subagent_model_request_semaphore", None)
        self.config.subagents = replace(current, enabled=enabled)
        if enabled:
            try:
                self._subagent_model_request_semaphore = threading.BoundedSemaphore(
                    self.config.subagents.model_request_concurrency
                )
                self._refresh_subagent_definitions()
                self._tools = self._build_tools()
            except Exception:
                self.config.subagents = current
                self._subagent_model_request_semaphore = previous_semaphore
                self._subagent_coordinator = None
                raise
        else:
            self._subagent_coordinator = None
            self._subagent_model_request_semaphore = None
            self._tools = self._build_tools()

    def set_confirm_handler(self, confirm: Callable[[str, dict[str, Any]], bool]) -> None:
        """替换确认交互，便于全屏 TUI 和行内 UI 使用不同展示方式。"""

        self._confirm = confirm

    def set_user_confirmation_handler(
        self,
        handler: Callable[[list[str] | list[list[str]] | None], None] | None,
    ) -> None:
        """注册模型向用户提问/请求决策时的 UI 状态观察器。

        回调参数是模型提供的可选答案列表；空列表表示本轮不需要用户选择。
        """

        self._user_confirmation_callback = handler

    def set_subagent_event_handler(
        self,
        handler: Callable[[str, dict[str, Any]], None] | None,
    ) -> None:
        """注册跨父回合存活的 SubAgent 公开事件观察者。

        ``run_stream`` 的回调只覆盖一次父回合。后台任务可能在该回合结束后
        才继续运行，因此 API 服务使用本入口接收相同的脱敏事件流；它不替代当前
        TUI/API Run 的临时回调，也不接触模型 prompt 或工具原始输出。
        """

        self._subagent_event_handler = handler
