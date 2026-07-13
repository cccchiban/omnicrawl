"""扩展能力模块：Skill 与 Hook / NPM 插件。"""

from .skill import SkillManager, SkillMatchResult, SkillMeta
from .plugin_models import (
    DispatchOutcome,
    HookEvent,
    HookPolicy,
    HookResult,
    PluginError,
    PluginManifest,
    PluginsConfig,
    parse_plugin_manifest,
    parse_plugins_config,
)
from .plugin_manager import HookDispatcher, PluginManager, PluginRuntime

__all__ = [
    "DispatchOutcome",
    "HookDispatcher",
    "HookEvent",
    "HookPolicy",
    "HookResult",
    "PluginError",
    "PluginManager",
    "PluginManifest",
    "PluginRuntime",
    "PluginsConfig",
    "SkillManager",
    "SkillMatchResult",
    "SkillMeta",
    "parse_plugin_manifest",
    "parse_plugins_config",
]
