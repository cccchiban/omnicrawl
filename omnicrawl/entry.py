"""OmniCrawl 应用入口：TUI 启动与 plugin 子命令路由。

console script（omnicrawl）与项目根 main.py 共用本模块，避免两套启动逻辑漂移。
Windows 下「弹新 PowerShell 窗口」只由项目根 main.py 的 ``__main__`` 负责；
``omnicrawl`` / ``python -m omnicrawl`` 始终在当前控制台运行，保证脚本能拿到真实 stdout/退出码。
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Any, Sequence

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent
from omnicrawl.approval import approval_mode_label, load_approval_mode
from omnicrawl.config.bootstrap import (
    format_startup_report,
    initialize_user_configuration,
)
from omnicrawl.llm import LLMError, load_llm_config
from omnicrawl.project_context import (
    ProjectContextError,
    detect_project_context,
    project_context_status_label,
)
from omnicrawl.runtime_config import RuntimeConfigError, load_config_data
from omnicrawl.config.settings import load_feature_enabled, load_show_thinking
from omnicrawl.config.router import load_router_mode
from omnicrawl.config.subagents import load_subagent_config
from omnicrawl.temp_workspace import (
    AgentTempWorkspaceError,
    agent_temp_status_label,
    load_agent_temp_workspace_config,
)
from omnicrawl.ui import UIStartupError
from omnicrawl.ui.splash import run_startup_splash
from omnicrawl.ui.windows_launcher import configure_console_encoding


# 启动画面不再人为固定展示时长；实际启动路径传入 0，完全由所有准备项
#（包括 MCP 能力发现）是否完成决定何时进入可发送的 TUI。
# 保留这个常量名兼容外部启动包装器；run_startup_splash 仍支持显式最短时长。
SPLASH_DURATION_SECONDS = 0.0


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


def _run_channel_setup_wizard(config_path: Path, models_path: Path) -> bool:
    """按需加载首次启动渠道向导，避免插件 CLI 提前加载 Textual。"""

    try:
        from omnicrawl.ui.fullscreen.channel_manager import run_channel_setup
    except ModuleNotFoundError as exc:
        dependency = exc.name or "textual"
        raise UIStartupError(
            f"缺少首次配置界面依赖：{dependency}。请重新安装 omnicrawl-agent。"
        ) from exc
    return run_channel_setup(config_path, models_path)


def run_plugin_cli(argv: Sequence[str]) -> int | None:
    """若 argv 以 plugin 开头则执行插件 CLI 并返回退出码；否则返回 None。"""

    if not argv or argv[0] != "plugin":
        return None
    from omnicrawl.cli import build_parser, run_plugin_command

    parser = build_parser()
    args = parser.parse_args(list(argv))
    return run_plugin_command(args)


def _prepare_startup(
    *,
    app_root: Path,
    resume_session_id: str,
) -> dict[str, Any]:
    """启动画面期间在后台执行的完整准备：配置、git 检测、插件、Agent（含索引）。

    Returns:
        字典，包含 ``agent``、``config``、``approval_mode``、
        ``temp_workspace_config``、``subagent_config``、``project_context``、
        ``fullscreen_startup``、``run_fullscreen_tui``、``plugin_runtime``、
        ``plugin_lines``。
    """

    # 配置加载与项目检测的异常（LLMError 等）由 run_application 的 except
    # 分支处理；插件启动失败在这里降级为无插件模式继续，保持历史语义。
    config = load_llm_config()
    approval_mode = load_approval_mode()
    temp_workspace_config = load_agent_temp_workspace_config()
    subagent_config = load_subagent_config()
    project_context = detect_project_context(app_root=app_root)
    fullscreen_startup, run_fullscreen_tui = _load_fullscreen_ui()

    plugin_runtime = None
    plugin_lines: list[str] = []
    try:
        from omnicrawl.extensions.plugin_manager import PluginRuntime

        plugin_runtime = PluginRuntime.from_config_data(
            load_config_data(),
            workspace_root=project_context.workspace_root,
        )
        for line in plugin_runtime.start():
            plugin_lines.append(line)
    except Exception as exc:  # noqa: BLE001
        plugin_lines.append(f"[plugins] 初始化失败，继续无插件模式：{exc}")
        plugin_runtime = None

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

    def _on_plugin_settings_changed(enabled: bool):
        if plugin_runtime is None:
            raise AgentError("Plugin Runtime 未连接。")
        return plugin_runtime.set_enabled(enabled)

    agent = LocalToolAgent(
        AgentConfig(
            llm=config,
            workspace_root=project_context.workspace_root,
            workspace_detection_summary=project_context.detection_summary,
            approval_mode=approval_mode,
            memory_enabled=load_feature_enabled("memory", default=True),
            show_thinking=load_show_thinking(),
            router_enabled=load_feature_enabled("router", default=False),
            router_mode=load_router_mode(),
            temp_workspace=temp_workspace_config,
            subagents=subagent_config,
            resume_session_id=resume_session_id,
        ),
        plugin_manager=None if plugin_runtime is None else plugin_runtime.manager,
        on_workspace_switched=_on_workspace_switched,
        on_plugin_settings_changed=_on_plugin_settings_changed,
    )
    startup_messages: list[str] = []
    try:
        # MCP 原先在 TUI 首屏之后后台发现，导致用户先看到主界面但暂时不能
        # 输入。把这一步纳入 splash 的 prepare，使 splash 结束即代表可以发送。
        agent.preload_mcp_tools()
    except AgentError as exc:
        # MCP 是增量能力：发现失败不应阻止内置工具可用；把失败延迟到主界面
        # 展示，同时仍视为该加载项已结束，避免启动页永久等待。
        startup_messages.append(f"MCP 能力加载失败：{exc}")
    except Exception as exc:  # noqa: BLE001
        startup_messages.append(f"MCP 能力加载异常：{exc}")
    if plugin_runtime is not None:
        agent.add_close_callback(plugin_runtime.close)
        plugin_runtime.notify_app_started()
    return {
        "agent": agent,
        "config": config,
        "approval_mode": approval_mode,
        "temp_workspace_config": temp_workspace_config,
        "subagent_config": subagent_config,
        "project_context": project_context,
        "fullscreen_startup": fullscreen_startup,
        "run_fullscreen_tui": run_fullscreen_tui,
        "plugin_runtime": plugin_runtime,
        "plugin_lines": plugin_lines,
        "startup_messages": tuple(startup_messages),
    }


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
    setup = initialize_user_configuration(
        channel_setup=_run_channel_setup_wizard,
    )
    for line in format_startup_report(setup):
        print(line)
    if setup.errors:
        return 1
    if not setup.api_key_configured:
        return 2

    # 主目录/盘根等过宽启动目录回退时使用 Agent 程序目录（而非 cwd），
    # 否则用户在主目录直接启动会把整个主目录当工作区（历史上 git add
    # 全量快照会遍历 AppData/.cargo/.codex 等巨量文件而卡死，见旧版
    # turn_snapshot 卡死 bug；现改为 git diff 机制后仍应避免以盘根为
    # 工作区）。包安装后本文件位于 site-packages/omnicrawl/，parent.parent
    # 即程序根；工作区检测仍从启动目录向上找项目标记，不影响从项目内
    # 启动的路径。
    app_root = Path(__file__).resolve().parent.parent

    # 显示启动画面（fastfetch 式：左侧黄色 Logo + 右侧系统信息 + 底部 XP 滚动条），
    # 后台并行完成全部准备；启动页没有固定时长，直到准备完成且 TUI 可以直接发送。
    # 非交互终端（测试、管道）下 splash 直接同步执行准备，行为不变。
    try:
        prepared = run_startup_splash(
            lambda: _prepare_startup(
                app_root=app_root,
                resume_session_id=args.resume,
            ),
            duration=SPLASH_DURATION_SECONDS,
        )
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

    agent: LocalToolAgent | None = prepared["agent"]
    plugin_runtime = prepared["plugin_runtime"]
    for line in prepared["plugin_lines"]:
        print(line, file=sys.stderr)

    exit_code = 0
    try:
        tui_exit_code = prepared["run_fullscreen_tui"](
            agent,
            prepared["fullscreen_startup"](
                thinking_enabled=prepared["config"].thinking_enabled,
                reasoning_effort=prepared["config"].reasoning_effort,
                approval_label=approval_mode_label(prepared["approval_mode"]),
                workspace_label=project_context_status_label(prepared["project_context"]),
                temp_label=agent_temp_status_label(prepared["temp_workspace_config"]),
                version_check_enabled=True,
                startup_ready=True,
                startup_messages=prepared.get("startup_messages", ()),
            ),
        )
        # 真实全屏入口返回 Textual 退出码；测试替身和旧扩展可能仍返回 None
        # 或其他哨兵值，此时保持历史上的正常退出语义。
        exit_code = tui_exit_code if isinstance(tui_exit_code, int) else 0
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
