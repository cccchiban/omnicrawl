from __future__ import annotations

import argparse
from pathlib import Path

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent
from omnicrawl.approval import approval_mode_label, load_approval_mode
from omnicrawl.llm import LLMError, load_llm_config
from omnicrawl.project_context import (
    ProjectContextError,
    detect_project_context,
    project_context_status_label,
)
from omnicrawl.runtime_config import RuntimeConfigError
from omnicrawl.temp_workspace import (
    AgentTempWorkspaceError,
    agent_temp_status_label,
    load_agent_temp_workspace_config,
)
from omnicrawl.ui import UIStartupError
from omnicrawl.ui.windows_launcher import configure_console_encoding, launch_in_powershell_window


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """解析启动参数；当前只暴露会话恢复入口。"""

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


def main(argv: list[str] | None = None) -> None:
    """启动 OmniCrawl 终端交互界面。"""

    configure_console_encoding()
    args = _parse_args(argv)
    app_root = Path(__file__).resolve().parent
    try:
        config = load_llm_config()
        approval_mode = load_approval_mode()
        temp_workspace_config = load_agent_temp_workspace_config()
        project_context = detect_project_context(app_root=app_root)
        fullscreen_startup, run_fullscreen_tui = _load_fullscreen_ui()
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return
    except RuntimeConfigError as exc:
        print(f"配置加载失败：{exc}")
        return
    except AgentTempWorkspaceError as exc:
        print(f"配置加载失败：{exc}")
        return
    except ProjectContextError as exc:
        print(f"项目路径检测失败：{exc}")
        return
    except UIStartupError as exc:
        print(f"界面启动失败：{exc}")
        return

    agent: LocalToolAgent | None = None
    try:
        agent = LocalToolAgent(
            AgentConfig(
                llm=config,
                workspace_root=project_context.workspace_root,
                workspace_detection_summary=project_context.detection_summary,
                approval_mode=approval_mode,
                temp_workspace=temp_workspace_config,
                resume_session_id=args.resume,
            )
        )
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
    except KeyboardInterrupt:
        print("\n对话结束。")
    finally:
        if agent is not None:
            agent.close()


if __name__ == "__main__":
    if not launch_in_powershell_window(Path(__file__)):
        main()
