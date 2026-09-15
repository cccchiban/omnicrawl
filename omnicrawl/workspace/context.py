"""工作区路径检测：启动目录即工作区，不做标记查找与回退。"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path


LAUNCH_CWD_ENV = "AI_VOICE_CHAT_LAUNCH_CWD"


@dataclass(frozen=True)
class ProjectContext:
    """Agent 当前要操作的工作区路径。"""

    workspace_root: Path

    @property
    def detection_summary(self) -> str:
        """返回适合注入系统提示词的中文检测说明。"""

        return f"使用启动目录作为工作区：{self.workspace_root}"


def detect_project_context(
    *,
    start_path: Path | None = None,
) -> ProjectContext:
    """检测本轮 Agent 应该操作的工作区目录。

    工作区始终是启动目录本身：不向上查找项目标记，也不做任何回退。
    """

    launch_start = _launch_start_path(start_path)
    return ProjectContext(workspace_root=launch_start)


def project_context_status_label(context: ProjectContext) -> str:
    """返回适合启动面板展示的短标签。"""

    return str(context.workspace_root)


def _launch_start_path(start_path: Path | None) -> Path:
    raw_launch_cwd = os.getenv(LAUNCH_CWD_ENV, "").strip()
    if raw_launch_cwd:
        candidate = Path(raw_launch_cwd).expanduser()
    elif start_path is not None:
        candidate = start_path.expanduser()
    else:
        candidate = Path.cwd()

    try:
        resolved = candidate.resolve()
    except OSError:
        return Path.cwd().resolve()

    if resolved.exists():
        return _directory_for_detection(resolved)
    return Path.cwd().resolve()


def _directory_for_detection(path: Path) -> Path:
    return path if path.is_dir() else path.parent
