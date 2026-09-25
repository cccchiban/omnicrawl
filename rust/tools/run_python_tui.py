#!/usr/bin/env python3
"""开发对照用：跑仓库内保留的 Python Textual 工作台。

**这不是产品入口。** 产品是 npm 分发的 Rust 二进制（`npm install -g omnicrawl-cli`），
Python 侧的 `main.py` / `omnicrawl/compat.py` / `omnicrawl/entry.py` / console script
已随「彻底脱离 Python 宿主」删除，`pyproject.toml` 也不再声明入口点。

保留 `omnicrawl/ui/` 的唯一目的是**逐屏对照**：Rust TUI 的样式对齐、抓帧 A/B 都拿它当参照
（见 `rust/docs/frozen-reference.md` 第三节）。本脚本把原来 `entry.py` 的 TUI 装配路径
抽出来，好让这个参照仍然跑得起来——改动语义时它不需要跟产品保持一致。

与旧 `entry.py` 的差异（有意精简）：不做插件管理 CLI / API / PyPI 自动更新 / 连接器
自动启动分流；插件、隔离工作区与 MCP 预热保留，因为它们会影响首屏内容。

用法（仓库根目录，解释器要有 textual 等依赖，例如 Anaconda 的 python）：

    python rust/tools/run_python_tui.py
    python rust/tools/run_python_tui.py --resume <会话 ID>
"""

from __future__ import annotations

import argparse
import logging
import sys
import threading
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent  # noqa: E402
from omnicrawl.approval import approval_mode_label, load_approval_mode  # noqa: E402
from omnicrawl.config.core.settings import load_feature_enabled, load_show_thinking  # noqa: E402
from omnicrawl.config.features.subagents import load_subagent_config  # noqa: E402
from omnicrawl.llm import LLMError, load_llm_config  # noqa: E402
from omnicrawl.project_context import (  # noqa: E402
    detect_project_context,
    project_context_status_label,
)
from omnicrawl.runtime_config import RuntimeConfigError, load_config_data  # noqa: E402
from omnicrawl.temp_workspace import (  # noqa: E402
    AgentTempWorkspaceError,
    agent_temp_status_label,
    load_agent_temp_workspace_config,
)
from omnicrawl.ui import UIStartupError  # noqa: E402
from omnicrawl.ui.splash import run_startup_splash  # noqa: E402
from omnicrawl.ui.windows_launcher import configure_console_encoding  # noqa: E402

LOGGER = logging.getLogger(__name__)

# 与旧入口保持一致：不人为延长准备耗时，准备完成后只留日志框可读窗口。
SPLASH_DURATION_SECONDS = 0.0
SPLASH_HOLD_AFTER_DONE_SECONDS = 0.5


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="OmniCrawl Python Textual 对照参照")
    parser.add_argument("--resume", metavar="SESSION_ID", default="", help="启动时恢复指定会话 ID")
    return parser.parse_args(argv)


def _load_fullscreen_ui():
    try:
        from omnicrawl.ui.fullscreen import FullscreenStartup, run_fullscreen_tui
    except ModuleNotFoundError as exc:
        dependency = exc.name or "textual"
        raise UIStartupError(
            f"缺少可选终端界面依赖：{dependency}。请先安装 textual 等运行依赖。"
        ) from exc
    return FullscreenStartup, run_fullscreen_tui


def _prepare(resume_session_id: str, log_sink=None) -> dict:
    """装配置、隔离工作区、插件与 Agent；沿用旧 entry.py 的顺序与降级策略。"""

    def log(message: str, level: str = "info") -> None:
        if log_sink is not None:
            log_sink(message, level=level)

    config = load_llm_config()
    approval_mode = load_approval_mode()
    temp_workspace_config = load_agent_temp_workspace_config()
    subagent_config = load_subagent_config()
    project_context = detect_project_context()
    fullscreen_startup, run_fullscreen_tui = _load_fullscreen_ui()
    log("配置与项目上下文加载完成")

    isolation_session = None
    try:
        from omnicrawl.config.features.agent_workspace import load_agent_workspace_config
        from omnicrawl.workspace.agent_isolation import prepare_isolated_workspace

        agent_workspace_root, isolation_session = prepare_isolated_workspace(
            main_workspace=project_context.workspace_root,
            config=load_agent_workspace_config(),
        )
    except Exception as exc:  # noqa: BLE001 - 隔离失败不阻断对照
        LOGGER.warning("隔离工作区初始化失败，回退到主工作区：%s", exc)
        log(f"隔离工作区初始化失败，回退到主工作区：{exc}", level="warning")
        agent_workspace_root = project_context.workspace_root
        isolation_session = None

    try:
        from omnicrawl.workspace.agent_isolation import start_background_isolation_sweep

        start_background_isolation_sweep()
    except Exception as exc:  # noqa: BLE001 - 清扫失败不阻断对照
        LOGGER.warning("隔离区后台清扫启动失败：%s", exc)

    plugin_runtime = None
    plugin_lines: list[str] = []
    try:
        from omnicrawl.extensions.plugin_manager import PluginRuntime

        plugin_runtime = PluginRuntime.from_config_data(
            load_config_data(),
            workspace_root=project_context.workspace_root,
        )
        plugin_lines.extend(plugin_runtime.start())
        log("插件初始化完成")
    except Exception as exc:  # noqa: BLE001 - 插件失败降级为无插件
        plugin_lines.append(f"[plugins] 初始化失败，继续无插件模式：{exc}")
        log(f"插件初始化失败，继续无插件模式：{exc}", level="warning")
        plugin_runtime = None

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
    )
    if isolation_session is not None:
        agent.attach_isolation_session(
            isolation_session,
            on_finalized=lambda summary: print(f"[isolation] {summary}", file=sys.stderr),
        )
    log("Agent 初始化完成")

    try:
        threading.Thread(
            target=agent.preload_mcp_tools,
            name="omnicrawl-mcp-preload",
            daemon=True,
        ).start()
    except Exception as exc:  # noqa: BLE001 - 线程创建失败时 MCP 在首回合懒加载
        LOGGER.warning("MCP 后台预热线程创建失败，改为首回合懒加载：%s", exc)
    if plugin_runtime is not None:
        agent.add_close_callback(plugin_runtime.close)
        plugin_runtime.notify_app_started()

    return {
        "agent": agent,
        "config": config,
        "approval_mode": approval_mode,
        "temp_workspace_config": temp_workspace_config,
        "project_context": project_context,
        "fullscreen_startup": fullscreen_startup,
        "run_fullscreen_tui": run_fullscreen_tui,
        "plugin_runtime": plugin_runtime,
        "plugin_lines": plugin_lines,
        "isolation_session": isolation_session,
    }


def main(argv: list[str] | None = None) -> int:
    configure_console_encoding()
    args = _parse_args(argv)

    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        print("本脚本只用于交互式对照，当前不是交互式终端。", file=sys.stderr)
        return 2

    try:
        prepared = run_startup_splash(
            lambda log_sink: _prepare(args.resume, log_sink),
            duration=SPLASH_DURATION_SECONDS,
            hold_after_done=SPLASH_HOLD_AFTER_DONE_SECONDS,
        )
    except (LLMError, RuntimeConfigError, AgentTempWorkspaceError, UIStartupError) as exc:
        print(f"启动失败：{exc}", file=sys.stderr)
        return 1

    agent: LocalToolAgent | None = prepared["agent"]
    plugin_runtime = prepared["plugin_runtime"]
    isolation_session = prepared.get("isolation_session")
    for line in prepared["plugin_lines"]:
        print(line, file=sys.stderr)

    try:
        exit_code = prepared["run_fullscreen_tui"](
            agent,
            prepared["fullscreen_startup"](
                thinking_enabled=prepared["config"].thinking_enabled,
                reasoning_effort=prepared["config"].reasoning_effort,
                approval_label=approval_mode_label(prepared["approval_mode"]),
                workspace_label=project_context_status_label(prepared["project_context"]),
                temp_label=agent_temp_status_label(prepared["temp_workspace_config"]),
                version_check_enabled=True,
                startup_ready=True,
                startup_messages=(),
            ),
        )
        return exit_code if isinstance(exit_code, int) else 0
    except AgentError as exc:
        print(f"Agent 初始化失败：{exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\n对话结束。")
        return 0
    finally:
        if agent is not None:
            agent.close()
        elif plugin_runtime is not None:
            try:
                plugin_runtime.close()
            except Exception:  # noqa: BLE001 - 退出阶段回收失败不阻断
                pass
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


if __name__ == "__main__":
    raise SystemExit(main())
