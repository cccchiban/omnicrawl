"""OmniCrawl 应用入口：TUI 启动与 plugin 子命令路由。

console script（omnicrawl）与项目根 main.py 共用本模块，避免两套启动逻辑漂移。
Windows 下「弹新 PowerShell 窗口」只由项目根 main.py 的 ``__main__`` 负责；
``omnicrawl`` / ``python -m omnicrawl`` 始终在当前控制台运行，保证脚本能拿到真实 stdout/退出码。
"""

from __future__ import annotations

import argparse
import logging
import sys
import threading
from pathlib import Path
from typing import Any, Sequence

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent
from omnicrawl.approval import approval_mode_label, load_approval_mode
from omnicrawl.config.core.bootstrap import (
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
from omnicrawl.config.core.settings import load_feature_enabled, load_show_thinking
from omnicrawl.config.features.subagents import load_subagent_config
from omnicrawl.temp_workspace import (
    AgentTempWorkspaceError,
    agent_temp_status_label,
    load_agent_temp_workspace_config,
)
from omnicrawl.ui import UIStartupError
from omnicrawl.ui.splash import run_startup_splash
from omnicrawl.ui.windows_launcher import configure_console_encoding


LOGGER = logging.getLogger(__name__)

# 启动画面最短展示时长：0 表示不人为延长"准备耗时"，配置、隔离区、插件、
# Agent、连接器等准备项完成后即进入 TUI（MCP 能力发现已改为后台预热，不
# 计入等待）。保留这个常量名兼容外部启动包装器；run_startup_splash 仍支持
# 显式最短时长。
SPLASH_DURATION_SECONDS = 0.0

# prepare 完成后进入 TUI 前的停留秒数：仅保留日志框的短暂可读窗口，启动
# 速度优先（历史上为 2 秒固定等待，见 run_startup_splash 的 hold_after_done）。
SPLASH_HOLD_AFTER_DONE_SECONDS = 0.5


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
        from omnicrawl.ui.fullscreen.screens.channel_manager import run_channel_setup
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


def _log_startup(
    sink: Any,
    message: str,
    level: str = "info",
) -> None:
    """向启动画面日志框写一行；无 sink（旧调用方/测试）时静默忽略。"""

    if sink is not None:
        sink.write_line(message, level=level)


def _preload_mcp_tools_background(agent: Any, log_sink: Any = None) -> None:
    """后台发现 MCP 能力；异常只记录日志与诊断，不阻断启动。

    发现失败不再写入首屏 ``startup_messages``（线程与首屏时序已解耦）；
    失败原因仍由 MCP registry 诊断保留，可通过 ``/mcp`` 查看。
    """

    try:
        agent.preload_mcp_tools()
        _log_startup(log_sink, "MCP 初始化完成")
    except AgentError as exc:
        LOGGER.warning("MCP 能力加载失败：%s", exc)
        _log_startup(log_sink, f"MCP 能力加载失败：{exc}", level="warning")
    except Exception as exc:  # noqa: BLE001 - MCP 是增量能力，失败不阻断启动
        LOGGER.warning("MCP 能力加载异常：%s", exc, exc_info=True)
        _log_startup(log_sink, f"MCP 能力加载异常：{exc}", level="error")


def _prepare_startup(
    *,
    app_root: Path,
    resume_session_id: str,
    log_sink: Any = None,
) -> dict[str, Any]:
    """启动画面期间在后台执行的完整准备：配置、git 检测、插件、Agent（含索引）。

    Args:
        log_sink: 可选的 ``StartupLogSink``；提供时把各阶段的启动日志
            （MCP / 插件 / Agent / 连接器等）写入启动画面日志框。

    Returns:
        字典，包含 ``agent``、``config``、``approval_mode``、
        ``temp_workspace_config``、``subagent_config``、``project_context``、
        ``fullscreen_startup``、``run_fullscreen_tui``、``plugin_runtime``、
        ``plugin_lines``、``connector_manager``。
    """

    # 配置加载与项目检测的异常（LLMError 等）由 run_application 的 except
    # 分支处理；插件启动失败在这里降级为无插件模式继续，保持历史语义。
    config = load_llm_config()
    approval_mode = load_approval_mode()
    temp_workspace_config = load_agent_temp_workspace_config()
    subagent_config = load_subagent_config()
    project_context = detect_project_context(app_root=app_root)
    fullscreen_startup, run_fullscreen_tui = _load_fullscreen_ui()
    _log_startup(log_sink, "配置与项目上下文加载完成")

    # 主 Agent 隔离工作区：多个进程并行时各自在独立目录/ worktree 中读写，
    # 互不写穿；创建失败仅告警并回退主工作区，不阻断启动。
    isolation_session = None
    try:
        from omnicrawl.config.features.agent_workspace import load_agent_workspace_config
        from omnicrawl.workspace.agent_isolation import prepare_isolated_workspace

        agent_workspace_root, isolation_session = prepare_isolated_workspace(
            main_workspace=project_context.workspace_root,
            config=load_agent_workspace_config(),
        )
    except Exception as exc:  # noqa: BLE001 - 隔离失败不阻断 TUI
        LOGGER.warning("隔离工作区初始化失败，回退到主工作区：%s", exc)
        _log_startup(
            log_sink,
            f"隔离工作区初始化失败，回退到主工作区：{exc}",
            level="warning",
        )
        agent_workspace_root = project_context.workspace_root
        isolation_session = None

    # 启动清扫（后台挂载）：回收上次崩溃 / 被强杀（如连接器随 TUI 退出）遗留
    # 的过期隔离区。放到隔离区创建之后再后台执行：新会话已注册，清扫的 in_use
    # 保护与保留期判定都会跳过它；历史会话多时清扫要对每个过期会话运行多次
    # git 子进程，不应占用 splash / 首屏时间。
    try:
        from omnicrawl.workspace.agent_isolation import start_background_isolation_sweep

        start_background_isolation_sweep()
    except Exception as exc:  # noqa: BLE001 - 清扫启动失败不阻断 TUI
        LOGGER.warning("隔离区后台清扫启动失败：%s", exc)

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
        _log_startup(log_sink, "插件初始化完成")
    except Exception as exc:  # noqa: BLE001
        plugin_lines.append(f"[plugins] 初始化失败，继续无插件模式：{exc}")
        _log_startup(
            log_sink,
            f"插件初始化失败，继续无插件模式：{exc}",
            level="warning",
        )
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
            workspace_root=agent_workspace_root,
            workspace_detection_summary=project_context.detection_summary,
            approval_mode=approval_mode,
            memory_enabled=load_feature_enabled("memory", default=True),
            show_thinking=load_show_thinking(),
            temp_workspace=temp_workspace_config,
            subagents=subagent_config,
            resume_session_id=resume_session_id,
        ),
        plugin_manager=None if plugin_runtime is None else plugin_runtime.manager,
        on_workspace_switched=_on_workspace_switched,
        on_plugin_settings_changed=_on_plugin_settings_changed,
    )

    # 主 Agent 隔离工作区收尾挂载：agent.close() 时按配置把变更应用回主工作区
    # 并清理（新进程 / 新 Agent 生效），TUI / API / 连接器共用同一收尾路径。
    if isolation_session is not None:
        agent.attach_isolation_session(
            isolation_session,
            on_finalized=lambda summary: print(f"[isolation] {summary}", file=sys.stderr),
        )
    _log_startup(log_sink, "Agent 初始化完成")
    # MCP 能力发现放后台线程：首次连接 + 能力枚举（远程 HTTP Server 还要
    # TLS 握手）通常需要 1–4 秒，不应让 splash / 首屏等待。首个回合在模型
    # 请求前会经 _ensure_mcp_tools_ready 等待发现完成，工具表不会缺失；失败
    # 诊断由 MCP registry 记录（/mcp 可查），并写入启动日志与 tui.log。
    startup_messages: list[str] = []
    try:
        threading.Thread(
            target=_preload_mcp_tools_background,
            args=(agent, log_sink),
            name="omnicrawl-mcp-preload",
            daemon=True,
        ).start()
    except Exception as exc:  # noqa: BLE001 - 线程创建失败降级为同步预热
        LOGGER.warning("MCP 后台预热线程创建失败，改为同步预热：%s", exc)
        _preload_mcp_tools_background(agent, log_sink)
    if plugin_runtime is not None:
        agent.add_close_callback(plugin_runtime.close)
        plugin_runtime.notify_app_started()

    # Telegram/飞书连接器与开屏动画并行拉起：动画结束即代表连接器子进程已
    # 启动完成，进入 TUI 前不再有额外等待；启动失败只记录警告，不阻塞本地界面。
    connector_manager = None
    try:
        from omnicrawl.connectors.autostart import start_configured_connectors

        connector_manager = start_configured_connectors(project_context.workspace_root)
        for connector_name in connector_manager.started_connectors:
            _log_startup(log_sink, f"{connector_name}连接器启动成功")
    except Exception as exc:  # noqa: BLE001 - 远程接入失败不阻塞 TUI
        LOGGER.warning("Telegram/飞书自动启动失败，TUI 将继续运行：%s", exc)
        _log_startup(
            log_sink,
            f"Telegram/飞书连接器自动启动失败，TUI 将继续运行：{exc}",
            level="warning",
        )
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
        "connector_manager": connector_manager,
        "isolation_session": isolation_session,
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

    # 启动自动更新：联网比对 PyPI 最新版，版本落后且为 pip 安装环境时先打印
    # 说明并自动 pip 升级，成功后以子进程重新拉起 TUI 并返回其退出码；跳过、
    # 无新版本或升级失败均返回 None 继续正常启动（失败策略：用当前版本启动）。
    try:
        from omnicrawl.updater import run_startup_update_if_due

        update_exit_code = run_startup_update_if_due(raw_argv)
        if update_exit_code is not None:
            return update_exit_code
    except Exception:  # noqa: BLE001 - 更新链路异常不能阻止 TUI 启动
        LOGGER.warning("启动自动更新失败，继续正常启动。", exc_info=True)

    # 显示启动画面（左侧黄色 Logo + 右侧圆角日志框 + 底部 XP 滚动条），
    # 后台并行完成全部准备；准备阶段把插件/Agent/连接器进度写入日志框
    #（MCP 能力发现已改为后台预热，不占用此处等待）。
    # 启动页不设人为最短总时长（SPLASH_DURATION_SECONDS=0），prepare 完成后
    # 仅保留 SPLASH_HOLD_AFTER_DONE_SECONDS 的日志框可读窗口即进入 TUI。
    # 非交互终端（测试、管道）下 splash 直接同步执行准备，行为不变。
    # splash 期间 run_startup_splash 会把 root logging 的 WARNING/ERROR
    # 桥接进日志框：默认 root 无 handler 时这些警告会经 lastResort 直接落到
    # stderr（画面外），而准备阶段的隔离区/插件/连接器警告正是启动日志。
    try:
        prepared = run_startup_splash(
            lambda log_sink: _prepare_startup(
                app_root=app_root,
                resume_session_id=args.resume,
                log_sink=log_sink,
            ),
            duration=SPLASH_DURATION_SECONDS,
            hold_after_done=SPLASH_HOLD_AFTER_DONE_SECONDS,
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
    isolation_session = prepared.get("isolation_session")
    for line in prepared["plugin_lines"]:
        print(line, file=sys.stderr)

    # 连接器已在 Splash 阶段随动画并行拉起（见 _prepare_startup），失败只
    # 记录警告不阻塞；监督器在 finally 中先于主 Agent 回收其远程子进程。
    connector_manager = prepared.get("connector_manager")

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
        if connector_manager is not None:
            # 连接器任务可能仍在使用各自的 Agent；必须先终止并等待子进程，
            # 再关闭本地 Agent 与其 PluginRuntime。
            connector_manager.close()
        if agent is not None:
            # 主 Agent 隔离工作区收尾（apply + 清理）已在 agent.close() 内完成。
            agent.close()
        elif plugin_runtime is not None:
            # Agent 尚未创建成功时没有关闭回调，只能由入口直接回收 Runtime。
            try:
                plugin_runtime.close()
            except Exception:  # noqa: BLE001 - 入口退出阶段回收 Runtime 失败不阻断收尾
                pass
        # Agent 创建失败（agent is None）时隔离会话无人收尾，兜底应用并清理。
        if isolation_session is not None and agent is None:
            try:
                from omnicrawl.workspace.agent_isolation import finalize_isolation_session

                summary = finalize_isolation_session(
                    isolation_session,
                    apply_on_exit=True,
                    cleanup_on_exit="auto",
                )
                if summary:
                    print(f"[isolation] {summary}", file=sys.stderr)
            except Exception as exc:  # noqa: BLE001 - 收尾失败不阻断退出
                print(f"[isolation] 收尾失败：{exc}", file=sys.stderr)
    return exit_code
