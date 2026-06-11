from __future__ import annotations

import shutil
import threading
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any, Callable

from .runtime_config import RuntimeConfigError, get_section, load_config_data


DEFAULT_AGENT_TEMP_DIRECTORY = ".agent_tmp"
DEFAULT_AGENT_TEMP_CLEANUP_HOUR = 4
DEFAULT_AGENT_TEMP_SUBDIRECTORIES = ("files", "images", "code", "videos", "scripts")
PRESERVED_ROOT_NAMES = {".gitignore", "README.md"}


class AgentTempWorkspaceError(RuntimeError):
    """Agent 临时工作区配置、初始化或清理失败时抛出。"""


@dataclass(frozen=True)
class AgentTempWorkspaceConfig:
    """Agent 临时工作区配置。

    enabled 控制是否创建并暴露临时目录；cleanup_enabled 控制定时清理线程。
    directory 必须是工作区内的相对路径，避免清理任务越界影响用户文件。
    """

    enabled: bool = True
    directory: str = DEFAULT_AGENT_TEMP_DIRECTORY
    cleanup_enabled: bool = True
    cleanup_hour: int = DEFAULT_AGENT_TEMP_CLEANUP_HOUR
    subdirectories: tuple[str, ...] = DEFAULT_AGENT_TEMP_SUBDIRECTORIES


@dataclass(frozen=True)
class AgentTempCleanupResult:
    """一次临时目录清理的结果，便于日志、测试和命令行输出复用。"""

    root: Path
    deleted_entries: tuple[str, ...]
    failed_entries: tuple[str, ...]
    cleaned_at: datetime


class AgentTempWorkspace:
    """管理 Agent 专用临时目录，并在每日固定时间清理其中内容。"""

    def __init__(
        self,
        workspace_root: Path,
        config: AgentTempWorkspaceConfig | None = None,
        now_factory: Callable[[], datetime] | None = None,
    ) -> None:
        self.workspace_root = workspace_root.resolve()
        self.config = config or AgentTempWorkspaceConfig()
        self.root = resolve_agent_temp_dir(self.workspace_root, self.config.directory)
        self._now_factory = now_factory or datetime.now
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None

    @property
    def display_path(self) -> str:
        """返回适合提示词和终端展示的相对路径。"""

        try:
            return str(self.root.relative_to(self.workspace_root))
        except ValueError:
            return str(self.root)

    def ensure(self) -> None:
        """创建临时目录、分类子目录和本地说明文件。

        说明文件放在目录根部并在清理时保留；真正的一次性产物放进分类子目录，
        这样凌晨清理可以删除工作内容，同时保留目录用途说明。
        """

        if not self.config.enabled:
            return

        try:
            self.root.mkdir(parents=True, exist_ok=True)
            for name in self.config.subdirectories:
                self._resolve_child(name).mkdir(parents=True, exist_ok=True)
            self._write_marker_files()
        except OSError as exc:
            raise AgentTempWorkspaceError(f"初始化 Agent 临时目录失败：{self.root}，{exc}") from exc

    def clean(self, now: datetime | None = None) -> AgentTempCleanupResult:
        """清空临时目录中的临时产物，并重建分类子目录。

        清理边界固定为 self.root 内部；每个待删除条目都会先解析并校验位置，
        避免路径穿越、符号链接或配置错误把删除范围带到工作区之外。
        """

        cleaned_at = now or self._now_factory()
        if not self.config.enabled:
            return AgentTempCleanupResult(self.root, (), (), cleaned_at)

        self.ensure()
        deleted_entries: list[str] = []
        failed_entries: list[str] = []

        for entry in sorted(self.root.iterdir(), key=lambda item: item.name.lower()):
            if entry.name in PRESERVED_ROOT_NAMES:
                continue

            try:
                self._delete_entry(entry)
                deleted_entries.append(entry.name)
            except (AgentTempWorkspaceError, OSError):
                failed_entries.append(entry.name)

        try:
            for name in self.config.subdirectories:
                self._resolve_child(name).mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            raise AgentTempWorkspaceError(f"重建 Agent 临时目录分类子目录失败：{exc}") from exc

        return AgentTempCleanupResult(
            root=self.root,
            deleted_entries=tuple(deleted_entries),
            failed_entries=tuple(failed_entries),
            cleaned_at=cleaned_at,
        )

    def start_scheduler(self) -> None:
        """启动后台清理线程；线程只在 Agent 进程存活期间工作。"""

        if not self.config.enabled or not self.config.cleanup_enabled:
            return
        if self._thread is not None and self._thread.is_alive():
            return

        self.ensure()
        self._stop_event.clear()
        self._thread = threading.Thread(
            target=self._run_scheduler,
            name="agent-temp-cleanup",
            daemon=True,
        )
        self._thread.start()

    def close(self) -> None:
        """停止后台清理线程，避免程序退出时留下悬挂工作。"""

        self._stop_event.set()
        if self._thread is not None:
            self._thread.join(timeout=1)

    def seconds_until_next_cleanup(self, now: datetime | None = None) -> float:
        """计算距离下一次凌晨清理的秒数，单独暴露便于测试边界时间。"""

        current = now or self._now_factory()
        target = current.replace(
            hour=self.config.cleanup_hour,
            minute=0,
            second=0,
            microsecond=0,
        )
        if current >= target:
            target += timedelta(days=1)
        return max(1.0, (target - current).total_seconds())

    def _run_scheduler(self) -> None:
        while not self._stop_event.is_set():
            wait_seconds = self.seconds_until_next_cleanup()
            if self._stop_event.wait(wait_seconds):
                return
            self.clean(now=self._now_factory())

    def _resolve_child(self, relative_path: str) -> Path:
        child = Path(relative_path)
        if child.is_absolute() or not child.parts or any(part in {"", ".", ".."} for part in child.parts):
            raise AgentTempWorkspaceError(f"Agent 临时目录子路径不安全：{relative_path}")

        resolved = (self.root / child).resolve()
        if not _is_relative_to(resolved, self.root):
            raise AgentTempWorkspaceError(f"Agent 临时目录子路径越界：{relative_path}")
        return resolved

    def _delete_entry(self, entry: Path) -> None:
        if entry.is_symlink():
            # 符号链接的目标可能指向项目外，但删除链接本身仍然应该只受
            # “链接文件位于临时目录内”约束；不能跟随目标去扩大清理边界。
            parent = entry.parent.resolve()
            if parent != self.root and not _is_relative_to(parent, self.root):
                raise AgentTempWorkspaceError(f"拒绝清理 Agent 临时目录外路径：{entry}")
            entry.unlink()
            return

        resolved = entry.resolve()
        if resolved == self.root or not _is_relative_to(resolved, self.root):
            raise AgentTempWorkspaceError(f"拒绝清理 Agent 临时目录外路径：{entry}")

        if entry.is_file():
            entry.unlink()
            return
        if entry.is_dir():
            shutil.rmtree(entry)
            return
        entry.unlink()

    def _write_marker_files(self) -> None:
        readme_path = self.root / "README.md"
        if not readme_path.exists():
            readme_path.write_text(_temp_workspace_readme(), encoding="utf-8")

        gitignore_path = self.root / ".gitignore"
        if not gitignore_path.exists():
            gitignore_path.write_text("*\n!.gitignore\n!README.md\n", encoding="utf-8")

def load_agent_temp_workspace_config(
    config_path: str | Path | None = None,
) -> AgentTempWorkspaceConfig:
    """从 config.json 的 agent_temp 段读取临时工作区配置。"""

    try:
        data = load_config_data(config_path)
        section = get_section(data, "agent_temp")
    except RuntimeConfigError as exc:
        raise AgentTempWorkspaceError(str(exc)) from exc

    return AgentTempWorkspaceConfig(
        enabled=_read_bool_config(section, "enabled", True),
        directory=_read_text_config(section, "directory", DEFAULT_AGENT_TEMP_DIRECTORY),
        cleanup_enabled=_read_bool_config(section, "cleanup_enabled", True),
        cleanup_hour=_read_hour_config(section, "cleanup_hour", DEFAULT_AGENT_TEMP_CLEANUP_HOUR),
    )


def resolve_agent_temp_dir(workspace_root: Path, directory: str) -> Path:
    """把临时目录配置解析为工作区内的绝对路径。"""

    raw_directory = directory.strip() if isinstance(directory, str) else ""
    if not raw_directory:
        raise AgentTempWorkspaceError("配置项 agent_temp.directory 必须是非空字符串。")

    relative_path = Path(raw_directory)
    if relative_path.is_absolute() or any(part in {"", ".", ".."} for part in relative_path.parts):
        raise AgentTempWorkspaceError(
            "配置项 agent_temp.directory 必须是工作区内的普通相对路径。"
        )

    workspace = workspace_root.resolve()
    resolved = (workspace / relative_path).resolve()
    if resolved == workspace or not _is_relative_to(resolved, workspace):
        raise AgentTempWorkspaceError("配置项 agent_temp.directory 不能指向工作区根目录或工作区外。")
    return resolved


def agent_temp_status_label(config: AgentTempWorkspaceConfig) -> str:
    """返回启动面板里使用的简短状态文本。"""

    if not config.enabled:
        return "关闭"
    if not config.cleanup_enabled:
        return f"{config.directory}，自动清理关闭"
    return f"{config.directory}，每日 {config.cleanup_hour:02d}:00 自动清理"


def _read_bool_config(section: dict[str, Any], key: str, default: bool) -> bool:
    value = section.get(key, default)
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    raise AgentTempWorkspaceError(f"配置项 agent_temp.{key} 必须是布尔值 true 或 false。")


def _read_text_config(section: dict[str, Any], key: str, default: str) -> str:
    value = section.get(key, default)
    if value is None:
        return default
    if not isinstance(value, str):
        raise AgentTempWorkspaceError(f"配置项 agent_temp.{key} 必须是字符串。")
    return value.strip() or default


def _read_hour_config(section: dict[str, Any], key: str, default: int) -> int:
    value = section.get(key, default)
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, int) or value < 0 or value > 23:
        raise AgentTempWorkspaceError(f"配置项 agent_temp.{key} 必须是 0 到 23 的整数。")
    return value


def _temp_workspace_readme() -> str:
    return (
        "# Agent 临时目录\n\n"
        "这个目录用于存放 Agent 工作时产生的一次性文件、图片、代码、视频和脚本。\n\n"
        "- `files/`：普通临时文件和中间结果。\n"
        "- `images/`：截图、生成图片和图像处理中间文件。\n"
        "- `code/`：一次性验证代码、草稿代码和临时样例。\n"
        "- `videos/`：临时视频、录屏和转码中间文件。\n"
        "- `scripts/`：只为当前任务服务的临时脚本。\n\n"
        "长期需要保留的交付物不要放在这里。Agent 运行期间会在每日 04:00 清理临时内容，"
        "并在清理后重建上述分类子目录。\n"
    )


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False
