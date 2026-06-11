from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path


WORKSPACE_ROOT_ENV = "AI_WORKSPACE_ROOT"
LAUNCH_CWD_ENV = "AI_VOICE_CHAT_LAUNCH_CWD"

PROJECT_MARKERS = (
    ".git",
    ".hg",
    ".svn",
    "AGENTS.md",
    "pyproject.toml",
    "package.json",
    "requirements.txt",
    "go.mod",
    "Cargo.toml",
    "pom.xml",
    "build.gradle",
    "composer.json",
)


class ProjectContextError(RuntimeError):
    """项目路径检测失败时抛出，通常由显式环境变量配置错误触发。"""


@dataclass(frozen=True)
class ProjectContext:
    """Agent 当前要操作的项目路径，以及这个路径的检测来源。"""

    workspace_root: Path
    start_path: Path
    source: str
    marker: str | None = None

    @property
    def detection_summary(self) -> str:
        """返回适合注入系统提示词的中文检测说明。"""

        if self.source == "environment":
            return f"由环境变量 {WORKSPACE_ROOT_ENV} 指定：{self.workspace_root}"
        if self.source == "marker" and self.marker:
            return f"从启动目录 {self.start_path} 向上发现项目标记 {self.marker}，选定：{self.workspace_root}"
        if self.source == "fallback_start":
            return f"未发现项目标记，使用启动目录作为工作区：{self.workspace_root}"
        if self.source == "fallback_app":
            return f"启动目录 {self.start_path} 不适合作为项目根目录，使用 Agent 程序目录：{self.workspace_root}"
        return f"使用工作区：{self.workspace_root}"


def detect_project_context(
    *,
    app_root: Path,
    start_path: Path | None = None,
) -> ProjectContext:
    """检测本轮 Agent 应该操作的项目根目录。

    优先级：
    1. 用户显式设置的 `AI_WORKSPACE_ROOT`，用于桌面快捷方式或脚本固定项目。
    2. 启动器保留下来的原始启动目录；没有弹窗启动时使用当前进程目录。
    3. 从启动目录向上查找常见项目标记，避免用户在 `src/` 等子目录启动时丢失根路径。
    4. 找不到标记时使用启动目录；若启动目录过宽（如用户主目录或磁盘根），回退到 Agent 程序目录。
    """

    app_root = app_root.expanduser().resolve()

    raw_workspace = os.getenv(WORKSPACE_ROOT_ENV, "").strip()
    if raw_workspace:
        workspace = _resolve_existing_directory(raw_workspace, WORKSPACE_ROOT_ENV)
        return ProjectContext(
            workspace_root=workspace,
            start_path=workspace,
            source="environment",
        )

    launch_start = _launch_start_path(start_path)
    detected = find_project_root(launch_start)
    if detected is not None:
        workspace, marker = detected
        return ProjectContext(
            workspace_root=workspace,
            start_path=launch_start,
            source="marker",
            marker=marker,
        )

    if _is_too_broad_workspace(launch_start):
        return ProjectContext(
            workspace_root=app_root,
            start_path=launch_start,
            source="fallback_app",
        )

    return ProjectContext(
        workspace_root=launch_start,
        start_path=launch_start,
        source="fallback_start",
    )


def find_project_root(start_path: Path) -> tuple[Path, str] | None:
    """从给定路径向上寻找最近的项目根标记。"""

    current = _directory_for_detection(start_path.expanduser().resolve())
    for candidate in (current, *current.parents):
        marker = _first_existing_marker(candidate)
        if marker is not None:
            return candidate, marker
    return None


def project_context_status_label(context: ProjectContext) -> str:
    """返回适合启动面板展示的短标签。"""

    if context.source == "environment":
        return f"{context.workspace_root}（env:{WORKSPACE_ROOT_ENV}）"
    if context.marker:
        return f"{context.workspace_root}（{context.marker}）"
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


def _resolve_existing_directory(raw_path: str, source_name: str) -> Path:
    try:
        path = Path(raw_path).expanduser().resolve()
    except OSError as exc:
        raise ProjectContextError(f"{source_name} 不是有效路径：{raw_path}") from exc

    if not path.exists():
        raise ProjectContextError(f"{source_name} 指向的路径不存在：{path}")
    if not path.is_dir():
        raise ProjectContextError(f"{source_name} 必须指向目录：{path}")
    return path


def _directory_for_detection(path: Path) -> Path:
    return path if path.is_dir() else path.parent


def _first_existing_marker(directory: Path) -> str | None:
    for marker in PROJECT_MARKERS:
        if (directory / marker).exists():
            return marker
    return None


def _is_too_broad_workspace(path: Path) -> bool:
    resolved = path.resolve()
    if resolved.parent == resolved:
        return True

    try:
        if resolved == Path.home().resolve():
            return True
    except OSError:
        return False

    return False
