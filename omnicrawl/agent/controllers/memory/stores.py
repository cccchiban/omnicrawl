"""三类作用域记忆存储的创建、绑定与清理。"""
from __future__ import annotations

import logging
import re
import shutil
from pathlib import Path
from ....memory import (
    MemoryStore,
    MemoryStoreError,
    migrate_legacy_memory,
)

from ..shared import (
    AgentError,
)

LOGGER = logging.getLogger(__name__)


class MemoryStoresMixin:
    """三类作用域记忆存储的创建、绑定与清理。"""

    def clean_memory(self) -> list[str]:
        """清理三类作用域中的过期记忆，供管理入口调用。"""

        deleted: list[str] = []
        stores = (
            ("project", getattr(self, "_project_memory_store", None)),
            ("session", getattr(self, "_session_memory_store", None)),
            ("user", getattr(self, "_user_memory_store", None)),
        )
        if not any(store is not None for _scope, store in stores):
            raise AgentError("记忆系统未启用。")
        try:
            for scope, store in stores:
                if store is None:
                    continue
                deleted.extend(
                    f"{scope}:{path}" for path in store.clean_expired_memories()
                )
        except MemoryStoreError as exc:
            raise AgentError(str(exc)) from exc
        return deleted

    def _create_memory_stores(self) -> tuple[MemoryStore, MemoryStore | None, MemoryStore]:
        """创建项目级、当前会话级和用户级记忆存储。"""

        raw_directory = str(
            getattr(self.config, "memory_directory", ".omnicrawl/.oclmemory")
        ).strip()
        candidate = Path(raw_directory)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        project_root = candidate.resolve()
        if not self._is_relative_to(project_root, self.workspace_root.resolve()):
            raise AgentError(f"项目级记忆目录必须位于工作区内：{raw_directory}")

        legacy_root = (self.workspace_root / "memory").resolve()
        try:
            migration = migrate_legacy_memory(legacy_root, project_root)
        except MemoryStoreError as exc:
            raise AgentError(str(exc)) from exc
        if migration.migrated and migration.backup_path is not None:
            LOGGER.info(
                "旧记忆已迁移到项目级目录；源目录备份为 %s（导入 %d 条）",
                migration.backup_path,
                migration.imported_count,
            )

        project_store = MemoryStore(project_root)
        user_root = self._memory_user_data_root()
        user_store = MemoryStore(user_root / "User_memory")
        session_store = self._create_current_session_memory_store()
        return project_store, session_store, user_store

    def _create_memory_store(self) -> MemoryStore:
        """兼容旧调用方，返回项目级记忆存储。"""

        return self._create_memory_stores()[0]

    def _memory_user_data_root(self) -> Path:
        """返回用户级记忆与会话级记忆共享的用户数据根目录。"""

        return (Path.home() / ".omnicrawl").resolve()

    def _create_current_session_memory_store(self) -> MemoryStore | None:
        state = getattr(self, "_session_state", None)
        if state is None:
            return None
        session_id = str(state.session_id).strip()
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", session_id):
            raise AgentError(f"会话 ID 不能用于记忆目录：{session_id}")
        return MemoryStore(self._memory_user_data_root() / "Session_memory" / session_id)

    def _bind_current_session_memory_store(self) -> None:
        """按当前 Session 重新绑定会话级记忆，防止跨会话读取。"""

        if not bool(getattr(self.config, "memory_enabled", False)):
            self._session_memory_store = None
            return
        self._session_memory_store = self._create_current_session_memory_store()

    def _delete_session_memory(self, session_id: str) -> None:
        """删除已删除 Session 的专属记忆目录，避免留下不可见孤儿数据。"""

        if not bool(getattr(self.config, "memory_enabled", False)):
            return
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", str(session_id).strip()):
            return
        path = self._memory_user_data_root() / "Session_memory" / str(session_id).strip()
        try:
            if path.is_dir():
                shutil.rmtree(path)
        except OSError as exc:
            raise AgentError(f"删除会话级记忆失败：{path}，{exc}") from exc
