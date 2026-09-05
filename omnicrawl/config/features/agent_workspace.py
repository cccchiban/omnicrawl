"""主 Agent 隔离工作区配置：``config.toml`` 的 ``[agent_workspace]`` 段。

该段控制主 Agent（TUI / API / 飞书连接器）是否在独立工作区（git worktree
或本地目录）中运行，避免多个 Agent 进程同时写同一个工作区导致文件冲突。

模式：
- ``worktree``：在仓库外 ``~/.omnicrawl/agent-worktrees/`` 下创建 git
  worktree（Detached HEAD，不产生临时分支），共享物理 ``.git``，每个实例
  独立 HEAD/Index/工作目录；退出时以 patch 应用回主工作区（推荐）。
- ``local``：在 ``~/.omnicrawl/agent-worktrees/<实例ID>/`` 下把主工作区复制为
  普通目录（排除 .git，不依赖 git），退出时把隔离区内容镜像回主工作区。

结束行为：
- ``apply_on_exit``：进程退出 / 会话归档时把隔离区变更应用回主工作区
  （worktree 走 patch，local 走目录镜像）。
- ``cleanup_on_exit``：auto（四层门禁过滤后可安全自动清理，见下）/ keep / never。

自动清理（``cleanup_on_exit=auto``）统一按四层门禁过滤，四层全部通过的
隔离区才被自动删除（退出收尾与启动清扫共用）：
  第一层：只清理临时隔离区（目录名以 aw- 开头）；
  第二层：跳过当前使用中与未过期的（保留期）；
  第三层：fail-closed 变更检查——有未提交/未跟踪改动不删；
  第四层：有未推送远端（origin）的 commit 也不删（即使变更已应用回主工作区）。

进程崩溃 / 被强杀遗留的过期隔离区由下一次启动清扫
（``sweep_expired_isolation_sessions``）先 apply 回写再按四层门禁回收。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from ..core.runtime import (
    RuntimeConfigError,
    load_config_data,
    save_config_data,
)

_CONFIG_SECTION = "agent_workspace"


class AgentWorkspaceConfigError(RuntimeConfigError):
    """agent_workspace 配置错误。"""


@dataclass
class AgentWorkspaceConfig:
    """主 Agent 隔离工作区配置。"""

    enabled: bool = True
    mode: str = "worktree"          # "worktree" | "local"
    base_branch: str = ""           # 基线分支名（空 = 当前分支）
    base_ref: str = "HEAD"          # 基线提交（默认 HEAD）
    detached: bool = True           # worktree 用 Detached HEAD
    apply_on_exit: bool = True      # 结束时把变更应用回主工作区
    cleanup_on_exit: str = "auto"   # "auto" | "keep" | "never"
    sync_uncommitted: bool = True   # 创建时把主工作区未提交改动带进隔离区
    copy_dirs: tuple[str, ...] = field(default_factory=tuple)   # 复制目录（如 .env）
    env_scripts: tuple[str, ...] = field(default_factory=tuple)  # 创建后运行的环境脚本

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise AgentWorkspaceConfigError("agent_workspace.enabled 必须是布尔值。")
        if self.mode not in {"worktree", "local"}:
            raise AgentWorkspaceConfigError(
                f"agent_workspace.mode 必须是 worktree 或 local，当前值：{self.mode}"
            )
        if self.cleanup_on_exit not in {"auto", "keep", "never"}:
            raise AgentWorkspaceConfigError(
                f"agent_workspace.cleanup_on_exit 必须是 auto/keep/never，当前值：{self.cleanup_on_exit}"
            )
        if not isinstance(self.apply_on_exit, bool):
            raise AgentWorkspaceConfigError("agent_workspace.apply_on_exit 必须是布尔值。")
        if not isinstance(self.sync_uncommitted, bool):
            raise AgentWorkspaceConfigError("agent_workspace.sync_uncommitted 必须是布尔值。")
        if not isinstance(self.copy_dirs, tuple) or not all(isinstance(x, str) for x in self.copy_dirs):
            raise AgentWorkspaceConfigError("agent_workspace.copy_dirs 必须是字符串数组。")
        if not isinstance(self.env_scripts, tuple) or not all(isinstance(x, str) for x in self.env_scripts):
            raise AgentWorkspaceConfigError("agent_workspace.env_scripts 必须是字符串数组。")


def load_agent_workspace_config(
    config_path: str | Path | None = None,
) -> AgentWorkspaceConfig:
    """读取 ``[agent_workspace]`` 段，缺省使用默认值（默认开启）。"""

    try:
        data = load_config_data(config_path)
    except RuntimeConfigError as exc:
        raise AgentWorkspaceConfigError(str(exc)) from exc

    section = data.get(_CONFIG_SECTION)
    if section is None:
        return AgentWorkspaceConfig()
    if not isinstance(section, dict):
        raise AgentWorkspaceConfigError(f"配置项 {_CONFIG_SECTION} 必须是表（table）。")

    def _bool(name: str, default: bool) -> bool:
        value = section.get(name, default)
        if not isinstance(value, bool):
            raise AgentWorkspaceConfigError(
                f"配置项 {_CONFIG_SECTION}.{name} 必须是布尔值。"
            )
        return value

    def _str(name: str, default: str) -> str:
        value = section.get(name, default)
        if not isinstance(value, str):
            raise AgentWorkspaceConfigError(
                f"配置项 {_CONFIG_SECTION}.{name} 必须是字符串。"
            )
        return value

    def _strs(name: str) -> tuple[str, ...]:
        value = section.get(name, ())
        if not isinstance(value, (list, tuple)) or not all(isinstance(x, str) for x in value):
            raise AgentWorkspaceConfigError(
                f"配置项 {_CONFIG_SECTION}.{name} 必须是字符串数组。"
            )
        return tuple(value)

    return AgentWorkspaceConfig(
        enabled=_bool("enabled", True),
        mode=_str("mode", "worktree"),
        base_branch=_str("base_branch", ""),
        base_ref=_str("base_ref", "HEAD"),
        detached=_bool("detached", True),
        apply_on_exit=_bool("apply_on_exit", True),
        cleanup_on_exit=_str("cleanup_on_exit", "auto"),
        sync_uncommitted=_bool("sync_uncommitted", True),
        copy_dirs=_strs("copy_dirs"),
        env_scripts=_strs("env_scripts"),
    )


def save_agent_workspace_config(
    configuration: AgentWorkspaceConfig,
    config_path: str | Path | None = None,
) -> Path:
    """把隔离工作区配置写回 ``config.toml``（保留其他段）。"""

    try:
        data = load_config_data(config_path)
    except RuntimeConfigError as exc:
        raise AgentWorkspaceConfigError(str(exc)) from exc
    data[_CONFIG_SECTION] = {
        "enabled": configuration.enabled,
        "mode": configuration.mode,
        "base_branch": configuration.base_branch,
        "base_ref": configuration.base_ref,
        "detached": configuration.detached,
        "apply_on_exit": configuration.apply_on_exit,
        "cleanup_on_exit": configuration.cleanup_on_exit,
        "sync_uncommitted": configuration.sync_uncommitted,
        "copy_dirs": list(configuration.copy_dirs),
        "env_scripts": list(configuration.env_scripts),
    }
    try:
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise AgentWorkspaceConfigError(str(exc)) from exc
