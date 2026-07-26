"""插件注册表：user/project 合并、tombstone、replace 解析与原子写。"""

from __future__ import annotations

import json
import os
import tempfile
import threading
from pathlib import Path
from typing import Any, Iterable, Mapping

from .plugin_models import (
    PluginError,
    PluginManifest,
    PluginRecord,
    PluginRegistryDocument,
    PluginVersionRef,
    ResolvedHandler,
    SEALED_HANDLER_PREFIX,
    default_timeout_for_mode,
    sort_handlers,
)


class PluginRegistryError(PluginError):
    """注册表读写或合并失败。"""


_WRITE_LOCK = threading.RLock()


def user_plugins_root() -> Path:
    """返回统一用户配置目录下的插件根目录。"""

    return Path.home() / ".OmniCrawl" / "plugins"


def user_registry_path() -> Path:
    return user_plugins_root() / "registry.json"


def user_store_root() -> Path:
    return user_plugins_root() / "store"


def project_registry_path(workspace_root: Path) -> Path:
    return Path(workspace_root).resolve() / ".omnicrawl" / "plugins.json"


def project_lock_path(workspace_root: Path) -> Path:
    return Path(workspace_root).resolve() / ".omnicrawl" / "plugins.lock.json"


def load_registry_document(path: Path) -> PluginRegistryDocument:
    """读取注册表；文件不存在时返回空文档。损坏时抛错且不覆盖原文件。"""

    if not path.exists():
        return PluginRegistryDocument()
    try:
        text = path.read_text(encoding="utf-8-sig")
    except OSError as exc:
        raise PluginRegistryError(f"读取注册表失败：{path}，{exc}") from exc
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise PluginRegistryError(
            f"注册表 JSON 损坏：{path}，第 {exc.lineno} 行：{exc.msg}。"
            "已进入无插件降级保护，请使用 plugin doctor 恢复。"
        ) from exc
    if not isinstance(data, Mapping):
        raise PluginRegistryError(f"注册表顶层必须是对象：{path}")
    try:
        return PluginRegistryDocument.from_dict(data)
    except Exception as exc:  # noqa: BLE001 - 统一包装为注册表错误
        raise PluginRegistryError(f"注册表结构无效：{path}，{exc}") from exc


def atomic_write_json(path: Path, data: Mapping[str, Any]) -> None:
    """同目录临时文件 + flush/fsync + 原子 replace。"""

    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(dict(data), ensure_ascii=False, indent=2) + "\n"
    with _WRITE_LOCK:
        fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=str(path.parent))
        tmp_path = Path(tmp_name)
        try:
            with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(tmp_path, path)
        except Exception:
            try:
                if tmp_path.exists():
                    tmp_path.unlink()
            except OSError:
                pass
            raise


def save_registry_document(path: Path, document: PluginRegistryDocument) -> None:
    atomic_write_json(path, document.to_dict())


def merge_registry_documents(
    *,
    user_doc: PluginRegistryDocument,
    project_doc: PluginRegistryDocument,
) -> PluginRegistryDocument:
    """project scope 覆盖 user scope 的同名插件记录。"""

    merged = PluginRegistryDocument(
        schema_version=max(user_doc.schema_version, project_doc.schema_version),
        plugins=dict(user_doc.plugins),
        disabled_handlers=list(dict.fromkeys([*user_doc.disabled_handlers, *project_doc.disabled_handlers])),
    )
    for name, record in project_doc.plugins.items():
        merged.plugins[name] = record
    return merged


def build_execution_plan(
    *,
    manifests: Mapping[str, tuple[PluginManifest, str, PluginRecord]],
    disabled_handlers: Iterable[str],
    max_timeout_ms: int,
) -> list[ResolvedHandler]:
    """根据已加载 manifest 构建不可变执行计划。

    manifests 值：`(manifest, scope, record)`。
    """

    disabled = set(disabled_handlers)
    candidates: list[ResolvedHandler] = []
    for plugin_name, (manifest, scope, record) in manifests.items():
        if not record.enabled:
            continue
        version = (
            record.active.version
            if record.active is not None
            else (manifest.version if record.dev_mode else "")
        )
        integrity_prefix = ""
        if record.active is not None and record.active.integrity:
            integrity_prefix = record.active.integrity.split("-", 1)[-1][:12]
        for handler in manifest.hooks:
            key = f"{plugin_name}/{handler.id}"
            if key in disabled:
                continue
            timeout = handler.timeout_ms or manifest.timeout_ms or default_timeout_for_mode(handler.mode)
            timeout = min(int(timeout), max_timeout_ms)
            candidates.append(
                ResolvedHandler(
                    key=key,
                    plugin_name=plugin_name,
                    plugin_version=version or manifest.version,
                    handler_id=handler.id,
                    hook=handler.hook,
                    mode=handler.mode,
                    priority=handler.priority,
                    scope=scope,
                    timeout_ms=timeout,
                    replaces=handler.replaces,
                    integrity_prefix=integrity_prefix,
                    local_path=record.local_path,
                    permissions=manifest.permissions,
                )
            )

    return resolve_replacements(candidates)


def resolve_replacements(handlers: SequenceLike) -> list[ResolvedHandler]:
    """处理显式 replaces：冲突或循环时丢弃相关 Handler。"""

    by_key = {item.key: item for item in handlers}
    replace_edges: dict[str, set[str]] = {}
    targets: dict[str, list[str]] = {}
    for item in handlers:
        for target in item.replaces:
            if target.startswith(SEALED_HANDLER_PREFIX):
                # sealed 目标在 manifest 阶段已拒绝；这里双保险。
                by_key.pop(item.key, None)
                continue
            if item.hook != by_key.get(target, item).hook and target in by_key:
                # 只能替换同一 Hook 上的 Handler。
                if by_key[target].hook != item.hook:
                    by_key.pop(item.key, None)
                    continue
            replace_edges.setdefault(item.key, set()).add(target)
            targets.setdefault(target, []).append(item.key)

    # 多个插件同时替换同一目标 → 冲突，相关替换方不启用。
    conflicted: set[str] = set()
    for target, sources in targets.items():
        if len(sources) > 1:
            conflicted.update(sources)

    # 环检测。
    visiting: set[str] = set()
    visited: set[str] = set()
    cyclic: set[str] = set()

    def dfs(node: str) -> None:
        if node in visited:
            return
        if node in visiting:
            cyclic.add(node)
            return
        visiting.add(node)
        for nxt in replace_edges.get(node, ()):
            dfs(nxt)
            if nxt in cyclic:
                cyclic.add(node)
        visiting.remove(node)
        visited.add(node)

    for key in list(replace_edges):
        dfs(key)

    removed_targets: set[str] = set()
    disabled_sources: set[str] = set(conflicted) | set(cyclic)
    for source, edges in replace_edges.items():
        if source in disabled_sources:
            continue
        if source not in by_key:
            continue
        for target in edges:
            removed_targets.add(target)

    result = [
        item
        for key, item in by_key.items()
        if key not in removed_targets and key not in disabled_sources
    ]
    return sort_handlers(result)


# typing helper without importing Sequence from typing in older pythons for mypy simplicity
SequenceLike = Iterable[ResolvedHandler]


def set_plugin_enabled(
    document: PluginRegistryDocument,
    name: str,
    enabled: bool,
) -> PluginRegistryDocument:
    record = document.plugins.get(name)
    if record is None:
        raise PluginRegistryError(f"注册表中不存在插件：{name}")
    record.enabled = enabled
    document.plugins[name] = record
    return document


def upsert_plugin_record(document: PluginRegistryDocument, record: PluginRecord) -> PluginRegistryDocument:
    document.plugins[record.name] = record
    return document


def remove_plugin_record(document: PluginRegistryDocument, name: str) -> PluginRegistryDocument:
    document.plugins.pop(name, None)
    document.disabled_handlers = [
        item for item in document.disabled_handlers if not item.startswith(f"{name}/")
    ]
    return document


def add_tombstone(document: PluginRegistryDocument, handler_key: str) -> PluginRegistryDocument:
    if handler_key.startswith(SEALED_HANDLER_PREFIX):
        raise PluginRegistryError(f"不能 tombstone sealed Handler：{handler_key}")
    if handler_key not in document.disabled_handlers:
        document.disabled_handlers.append(handler_key)
    return document


def version_ref_dict(ref: PluginVersionRef | None) -> dict[str, Any] | None:
    return ref.to_dict() if ref is not None else None
