"""OmniCrawl 应用入口：TUI 启动与 plugin 子命令路由。

console script（omnicrawl）与项目根 main.py 共用本模块，避免两套启动逻辑漂移。
Windows 下「弹新 PowerShell 窗口」只由项目根 main.py 的 ``__main__`` 负责；
``omnicrawl`` / ``python -m omnicrawl`` 始终在当前控制台运行，保证脚本能拿到真实 stdout/退出码。
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Sequence

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent
from omnicrawl.approval import approval_mode_label, load_approval_mode
from omnicrawl.llm import LLMError, load_llm_config
from omnicrawl.project_context import (
    ProjectContextError,
    detect_project_context,
    project_context_status_label,
)
from omnicrawl.runtime_config import RuntimeConfigError, load_config_data
from omnicrawl.config.subagents import load_subagent_config
from omnicrawl.temp_workspace import (
    AgentTempWorkspaceError,
    agent_temp_status_label,
    load_agent_temp_workspace_config,
)
from omnicrawl.ui import UIStartupError
from omnicrawl.ui.windows_launcher import configure_console_encoding


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """解析 TUI 启动参数。"""

    parser = argparse.ArgumentParser(description="OmniCrawl")
    parser.add_argument(
        "--resume",
        metavar="SESSION_ID",
        default="",
        help="启动时恢复指定会话 ID",
    )
    return parser.parse_args(argv)


def _load_fullscreen_ui():
    """按需加载 Textual 前端，让缺少依赖时仍可给出可执行的启动提示。"""

    try:
        from omnicrawl.ui.fullscreen import FullscreenStartup, run_fullscreen_tui
    except ModuleNotFoundError as exc:
        dependency = exc.name or "textual"
        raise UIStartupError(
            f"缺少可选终端界面依赖：{dependency}。请执行 pip install -r requirements.txt。"
        ) from exc
    return FullscreenStartup, run_fullscreen_tui


def run_plugin_cli(argv: Sequence[str]) -> int | None:
    """若 argv 以 plugin 开头则执行插件 CLI 并返回退出码；否则返回 None。"""

    if not argv or argv[0] != "plugin":
        return None
    from omnicrawl.cli import build_parser, run_plugin_command

    parser = build_parser()
    args = parser.parse_args(list(argv))
    return run_plugin_command(args)


def run_application(argv: Sequence[str] | None = None) -> int:
    """统一应用入口。

    Returns:
        进程退出码。plugin 子命令返回 CLI 码；TUI 正常结束返回 0。
    """

    configure_console_encoding()
    raw_argv = list(sys.argv[1:] if argv is None else argv)

    # 插件管理必须在 LLM 配置和 UI 加载之前完成。
    plugin_exit = run_plugin_cli(raw_argv)
    if plugin_exit is not None:
        return int(plugin_exit)

    args = _parse_args(raw_argv)
    # 包安装后 app_root 可能是 site-packages；工作区检测仍从 cwd 向上找项目标记。
    app_root = Path.cwd().resolve()
    plugin_runtime = None
    try:
        config = load_llm_config()
        approval_mode = load_approval_mode()
        temp_workspace_config = load_agent_temp_workspace_config()
        subagent_config = load_subagent_config()
        project_context = detect_project_context(app_root=app_root)
        fullscreen_startup, run_fullscreen_tui = _load_fullscreen_ui()

        from omnicrawl.extensions.plugin_manager import PluginRuntime

        plugin_runtime = PluginRuntime.from_config_data(
            load_config_data(),
            workspace_root=project_context.workspace_root,
        )
        for line in plugin_runtime.start():
            print(f"[plugins] {line}", file=sys.stderr)
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return 1
    except RuntimeConfigError as exc:
        print(f"配置加载失败：{exc}")
        return 1
    except AgentTempWorkspaceError as exc:
        print(f"配置加载失败：{exc}")
        return 1
    except ProjectContextError as exc:
        print(f"项目路径检测失败：{exc}")
        return 1
    except UIStartupError as exc:
        print(f"界面启动失败：{exc}")
        return 1
    except Exception as exc:  # noqa: BLE001
        print(f"[plugins] 初始化失败，继续无插件模式：{exc}", file=sys.stderr)
        plugin_runtime = None

    agent: LocalToolAgent | None = None
    exit_code = 0
    try:
        def _on_workspace_switched(new_root: Path):
            if plugin_runtime is None:
                return None
            try:
                plugin_runtime.switch_workspace(new_root)
            except BaseException:
                # Agent 的工作区主体已经提交；候选插件启动失败时不能继续把
                # 旧工作区 Manager 注入新工作区。关闭旧 Manager 后降级无插件。
                plugin_runtime.close_manager_only()
                plugin_runtime.workspace_root = new_root
                raise
            return plugin_runtime.manager

        agent = LocalToolAgent(
            AgentConfig(
                llm=config,
                workspace_root=project_context.workspace_root,
                workspace_detection_summary=project_context.detection_summary,
                approval_mode=approval_mode,
                temp_workspace=temp_workspace_config,
                subagents=subagent_config,
                resume_session_id=args.resume,
            ),
            plugin_manager=None if plugin_runtime is None else plugin_runtime.manager,
            on_workspace_switched=_on_workspace_switched,
        )
        if plugin_runtime is not None:
            agent.add_close_callback(plugin_runtime.close)
            plugin_runtime.notify_app_started()
        run_fullscreen_tui(
            agent,
            fullscreen_startup(
                thinking_enabled=config.thinking_enabled,
                reasoning_effort=config.reasoning_effort,
                approval_label=approval_mode_label(approval_mode),
                workspace_label=project_context_status_label(project_context),
                temp_label=agent_temp_status_label(temp_workspace_config),
            ),
        )
    except AgentError as exc:
        print(f"Agent 初始化失败：{exc}")
        exit_code = 1
    except KeyboardInterrupt:
        print("\n对话结束。")
        exit_code = 0
    except NameError:
        # 配置阶段已失败但未 return 的兜底（理论不应到达）。
        exit_code = 1
    finally:
        if agent is not None:
            agent.close()
        elif plugin_runtime is not None:
            # Agent 尚未创建成功时没有关闭回调，只能由入口直接回收 Runtime。
            try:
                plugin_runtime.close()
            except Exception:
                pass
    return exit_code
