"""PluginRuntime / PluginManager：Worker 生命周期、执行计划、dispatch 与熔断。"""

from __future__ import annotations

import copy
import json
import logging
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence

from .plugin_models import (
    CORE_HOOKS,
    DEFAULT_MAX_MESSAGE_BYTES,
    DispatchOutcome,
    HOOK_API_VERSION,
    HOOK_POLICIES,
    HOOK_PATCH_ALLOWLIST,
    HANDLER_MODE_GUARD,
    HANDLER_MODE_OBSERVE,
    HANDLER_MODE_NOTIFY,
    HANDLER_MODE_TRANSFORM,
    HookEvent,
    HookPolicy,
    HookResult,
    OMNICRAWL_VERSION,
    PluginDispatchError,
    PluginError,
    PluginManifest,
    PluginManifestError,
    PluginProtocolError,
    PluginRecord,
    PluginsConfig,
    ResolvedHandler,
    apply_json_patch,
    new_event_id,
    parse_plugin_manifest,
    parse_plugins_config,
    stable_hash,
    utc_now_iso,
    validate_json_patch,
    validate_payload_against_schema,
)
from .plugin_install import (
    verify_content_tree_hash,
    verify_lockfile_hash,
    verify_store_integrity,
)
from .plugin_protocol import PluginWorkerClient
from .plugin_registry import (
    PluginRegistryError,
    build_execution_plan,
    load_registry_document,
    merge_registry_documents,
    project_registry_path,
    user_registry_path,
    user_store_root,
)


logger = logging.getLogger(__name__)


def _is_relative_to(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


@dataclass
class PluginWorkerState:
    """单个启用插件的运行态。"""

    name: str
    manifest: PluginManifest
    scope: str
    record: PluginRecord
    root: Path
    client: PluginWorkerClient | None = None
    failures: int = 0
    circuit_open: bool = False
    last_error: str = ""
    active: bool = False


@dataclass
class AuditRecord:
    event_id: str
    hook: str
    handler_key: str
    plugin_name: str
    plugin_version: str
    mode: str
    status: str
    elapsed_ms: float
    patch_paths: list[str] = field(default_factory=list)
    before_hash: str = ""
    after_hash: str = ""
    error: str = ""


class HookDispatcher:
    """Core Hook 分发器。

    无外部插件时也可用：保持 Host 语义节点统一，行为与当前版本一致。
    """

    def __init__(
        self,
        *,
        config: PluginsConfig | None = None,
        audit_sink: Callable[[AuditRecord], None] | None = None,
    ) -> None:
        self.config = config or PluginsConfig()
        self._audit_sink = audit_sink
        self._sequence = 0
        self._sequence_lock = threading.Lock()
        self._plan: tuple[ResolvedHandler, ...] = ()
        self._turn_plan: tuple[ResolvedHandler, ...] | None = None
        self._workers: dict[str, PluginWorkerState] = {}
        self._plan_lock = threading.RLock()

    def set_execution_plan(
        self,
        handlers: Sequence[ResolvedHandler],
        workers: Mapping[str, PluginWorkerState] | None = None,
    ) -> None:
        with self._plan_lock:
            self._plan = tuple(handlers)
            if workers is not None:
                self._workers = dict(workers)
            # 若当前 turn 已冻结计划，则保留快照，使启停从下一轮生效。
            # 无活跃 turn 时 _turn_plan 为 None，下一次 begin_turn 会拍新快照。

    def begin_turn(self) -> None:
        """turn 开始时冻结当前执行计划。"""

        with self._plan_lock:
            self._turn_plan = self._plan

    def end_turn(self) -> None:
        with self._plan_lock:
            self._turn_plan = None

    def current_plan(self) -> tuple[ResolvedHandler, ...]:
        with self._plan_lock:
            if self._turn_plan is not None:
                return self._turn_plan
            return self._plan

    def next_sequence(self) -> int:
        with self._sequence_lock:
            self._sequence += 1
            return self._sequence

    def dispatch(
        self,
        hook_name: str,
        payload: Mapping[str, Any] | None = None,
        *,
        policy: HookPolicy | None = None,
        workspace: Mapping[str, Any] | None = None,
        session_id: str | None = None,
        turn_id: str | None = None,
        parent_event_id: str | None = None,
        depth: int = 0,
        handlers_override: Sequence[ResolvedHandler] | None = None,
    ) -> DispatchOutcome:
        """统一分发入口。

        handlers_override 用于自定义事件在 visibility 过滤后的精确投递。
        """

        if hook_name not in CORE_HOOKS and not hook_name.startswith("plugin."):
            raise PluginError(f"未知 Hook：{hook_name}")

        hook_policy = policy or HOOK_POLICIES.get(hook_name, HookPolicy())
        working_payload = copy.deepcopy(dict(payload or {}))
        outcome = DispatchOutcome(hook=hook_name, payload=working_payload)
        if handlers_override is not None:
            handlers = [item for item in handlers_override if item.hook == hook_name]
        else:
            handlers = [item for item in self.current_plan() if item.hook == hook_name]
        if not handlers or not self.config.enabled:
            return outcome

        event = self._build_event(
            hook_name,
            working_payload,
            workspace=workspace,
            session_id=session_id,
            turn_id=turn_id,
            parent_event_id=parent_event_id,
            depth=depth,
        )

        # 分 mode 阶段执行：guard → transform → observe/notify
        for mode in (
            HANDLER_MODE_GUARD,
            HANDLER_MODE_TRANSFORM,
            HANDLER_MODE_OBSERVE,
            HANDLER_MODE_NOTIFY,
        ):
            mode_handlers = [item for item in handlers if item.mode == mode]
            if not mode_handlers:
                continue
            if mode in {HANDLER_MODE_OBSERVE, HANDLER_MODE_NOTIFY}:
                self._run_parallel_readonly(
                    mode_handlers,
                    event=event,
                    payload=working_payload,
                    policy=hook_policy,
                    outcome=outcome,
                )
            else:
                for handler in mode_handlers:
                    result = self._invoke_handler(handler, event=event, payload=working_payload)
                    applied = self._apply_handler_result(
                        handler,
                        result,
                        payload=working_payload,
                        policy=hook_policy,
                        outcome=outcome,
                        event=event,
                    )
                    if applied is not None:
                        working_payload = applied
                        event = self._rebuild_event_payload(event, working_payload)
                    if outcome.denied:
                        outcome.payload = working_payload
                        return outcome

        outcome.payload = working_payload
        return outcome

    def _build_event(
        self,
        hook_name: str,
        payload: Mapping[str, Any],
        *,
        workspace: Mapping[str, Any] | None,
        session_id: str | None,
        turn_id: str | None,
        parent_event_id: str | None,
        depth: int,
    ) -> HookEvent:
        deadline = self.config.default_timeout_ms
        return HookEvent(
            api_version=HOOK_API_VERSION,
            event_id=new_event_id(),
            hook=hook_name,
            timestamp=utc_now_iso(),
            sequence=self.next_sequence(),
            workspace=dict(workspace or {"id": "default"}),
            payload=copy.deepcopy(dict(payload)),
            deadline_ms=deadline,
            session_id=session_id,
            turn_id=turn_id,
            trace={"parentEventId": parent_event_id, "depth": depth},
        )

    def _rebuild_event_payload(self, event: HookEvent, payload: Mapping[str, Any]) -> HookEvent:
        return HookEvent(
            api_version=event.api_version,
            event_id=event.event_id,
            hook=event.hook,
            timestamp=event.timestamp,
            sequence=event.sequence,
            workspace=event.workspace,
            payload=copy.deepcopy(dict(payload)),
            deadline_ms=event.deadline_ms,
            session_id=event.session_id,
            turn_id=event.turn_id,
            trace=event.trace,
        )

    def _run_parallel_readonly(
        self,
        handlers: Sequence[ResolvedHandler],
        *,
        event: HookEvent,
        payload: Mapping[str, Any],
        policy: HookPolicy,
        outcome: DispatchOutcome,
    ) -> None:
        if not handlers:
            return
        # 同插件串行、不同插件可并行：按插件分组。
        by_plugin: dict[str, list[ResolvedHandler]] = {}
        for handler in handlers:
            by_plugin.setdefault(handler.plugin_name, []).append(handler)

        def run_plugin_group(group: Sequence[ResolvedHandler]) -> list[tuple[ResolvedHandler, HookResult]]:
            results: list[tuple[ResolvedHandler, HookResult]] = []
            for handler in group:
                results.append((handler, self._invoke_handler(handler, event=event, payload=payload)))
            return results

        max_workers = max(1, min(8, len(by_plugin)))
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            futures = [executor.submit(run_plugin_group, group) for group in by_plugin.values()]
            for future in as_completed(futures):
                try:
                    pairs = future.result()
                except Exception as exc:  # noqa: BLE001
                    logger.warning("observe/notify 并行执行异常：%s", exc)
                    continue
                for handler, result in pairs:
                    self._apply_handler_result(
                        handler,
                        result,
                        payload=dict(payload),
                        policy=policy,
                        outcome=outcome,
                        event=event,
                    )

    def _invoke_handler(
        self,
        handler: ResolvedHandler,
        *,
        event: HookEvent,
        payload: Mapping[str, Any],
    ) -> HookResult:
        worker = self._workers.get(handler.plugin_name)
        if worker is None or worker.circuit_open or not worker.active or worker.client is None:
            return HookResult.continue_result(
                handler_key=handler.key,
                status="circuit-open" if worker and worker.circuit_open else "skip",
            )
        started = time.perf_counter()
        try:
            # 事件 payload 使用当前 authoritative 副本。工作区真实路径按插件
            # 权限逐个裁剪，不能因任一插件获批而泄露给同批其他插件。
            live_event = self._rebuild_event_payload(event, payload)
            if "workspace:metadata" not in worker.manifest.permissions and "root" in live_event.workspace:
                live_event = HookEvent(
                    api_version=live_event.api_version,
                    event_id=live_event.event_id,
                    hook=live_event.hook,
                    timestamp=live_event.timestamp,
                    sequence=live_event.sequence,
                    workspace={"id": live_event.workspace.get("id", "default")},
                    payload=live_event.payload,
                    deadline_ms=live_event.deadline_ms,
                    session_id=live_event.session_id,
                    turn_id=live_event.turn_id,
                    trace=live_event.trace,
                )
            raw = worker.client.invoke_handler(
                handler_id=handler.handler_id,
                event=live_event.to_dict(),
                timeout_ms=handler.timeout_ms,
            )
            elapsed = (time.perf_counter() - started) * 1000.0
            from .plugin_models import parse_hook_result

            result = parse_hook_result(raw, handler_key=handler.key, elapsed_ms=elapsed)
            worker.failures = 0
            return result
        except PluginProtocolError as exc:
            elapsed = (time.perf_counter() - started) * 1000.0
            self._register_failure(worker, str(exc))
            return HookResult(
                action="continue",
                handler_key=handler.key,
                elapsed_ms=elapsed,
                status="timeout" if "超时" in str(exc) else "protocol-error",
                reason=str(exc),
            )
        except Exception as exc:  # noqa: BLE001
            elapsed = (time.perf_counter() - started) * 1000.0
            self._register_failure(worker, str(exc))
            return HookResult(
                action="continue",
                handler_key=handler.key,
                elapsed_ms=elapsed,
                status="handler-error",
                reason=str(exc),
            )

    def _register_failure(self, worker: PluginWorkerState, error: str) -> None:
        worker.failures += 1
        worker.last_error = error
        if worker.failures >= self.config.failure_threshold:
            worker.circuit_open = True
            logger.warning(
                "插件熔断：%s 连续失败 %s 次，最近错误：%s",
                worker.name,
                worker.failures,
                error,
            )

    def _apply_handler_result(
        self,
        handler: ResolvedHandler,
        result: HookResult,
        *,
        payload: dict[str, Any],
        policy: HookPolicy,
        outcome: DispatchOutcome,
        event: HookEvent,
    ) -> dict[str, Any] | None:
        outcome.results.append(result)
        if result.annotations:
            outcome.annotations[handler.key] = result.annotations

        status = result.status
        if status in {"timeout", "protocol-error", "handler-error", "circuit-open", "skip"}:
            decision = {
                "timeout": policy.on_timeout,
                "protocol-error": policy.on_protocol_error,
                "handler-error": policy.on_handler_error,
                "circuit-open": "skip-handler",
                "skip": "skip-handler",
            }.get(status, "skip-handler")
            self._audit(
                event,
                handler,
                status=status,
                elapsed_ms=result.elapsed_ms,
                error=result.reason,
            )
            if decision == "reject-operation":
                outcome.denied = True
                outcome.deny_reason = result.reason or f"Hook {handler.hook} Handler 失败：{status}"
                outcome.deny_code = status
            if policy.disable_plugin_on_error:
                worker = self._workers.get(handler.plugin_name)
                if worker is not None:
                    worker.active = False
                    worker.circuit_open = True
            return None

        if result.action == "deny":
            # 只有 guard 可 deny；approve 在 parse 阶段已拒绝，这里再双保险。
            if handler.mode != HANDLER_MODE_GUARD:
                self._audit(
                    event,
                    handler,
                    status="invalid-deny",
                    elapsed_ms=result.elapsed_ms,
                    error="非 guard Handler 不能 deny",
                )
                return None
            self._audit(
                event,
                handler,
                status="deny",
                elapsed_ms=result.elapsed_ms,
                error=result.reason,
            )
            if policy.on_deny == "reject-operation":
                outcome.denied = True
                outcome.deny_reason = result.reason or f"插件拒绝：{handler.key}"
                outcome.deny_code = result.code or "deny"
            return None

        if result.action == "patch":
            if handler.mode != HANDLER_MODE_TRANSFORM:
                self._audit(
                    event,
                    handler,
                    status="invalid-patch",
                    elapsed_ms=result.elapsed_ms,
                    error="仅 transform Handler 可返回 patch",
                )
                return None
            before_hash = stable_hash(payload)
            try:
                normalized = validate_json_patch(result.patch, hook=handler.hook)
                # 白名单路径以 /payload/... 为根，文档包装为 {"payload": ...}
                patch_ops: list[dict[str, Any]] = []
                for item in normalized:
                    op: dict[str, Any] = {"op": item["op"], "path": item["path"]}
                    if "value" in item:
                        op["value"] = item["value"]
                    patch_ops.append(op)
                updated = apply_json_patch({"payload": payload}, patch_ops)
                new_payload = updated.get("payload", payload)
                if not isinstance(new_payload, dict):
                    raise PluginError("transform 后 payload 必须是对象")
                after_hash = stable_hash(new_payload)
                self._audit(
                    event,
                    handler,
                    status="patch",
                    elapsed_ms=result.elapsed_ms,
                    patch_paths=[item["path"] for item in normalized],
                    before_hash=before_hash,
                    after_hash=after_hash,
                )
                payload.clear()
                payload.update(new_payload)
                return new_payload
            except Exception as exc:  # noqa: BLE001 - 非法 Patch fail-open
                self._audit(
                    event,
                    handler,
                    status="invalid-patch",
                    elapsed_ms=result.elapsed_ms,
                    error=str(exc),
                    before_hash=before_hash,
                )
                if policy.on_handler_error == "reject-operation":
                    outcome.denied = True
                    outcome.deny_reason = f"非法 Patch：{exc}"
                    outcome.deny_code = "invalid-patch"
                return None

        self._audit(event, handler, status="continue", elapsed_ms=result.elapsed_ms)
        return None

    def _audit(
        self,
        event: HookEvent,
        handler: ResolvedHandler,
        *,
        status: str,
        elapsed_ms: float,
        patch_paths: list[str] | None = None,
        before_hash: str = "",
        after_hash: str = "",
        error: str = "",
    ) -> None:
        if not self.config.audit_log_enabled:
            return
        record = AuditRecord(
            event_id=event.event_id,
            hook=event.hook,
            handler_key=handler.key,
            plugin_name=handler.plugin_name,
            plugin_version=handler.plugin_version,
            mode=handler.mode,
            status=status,
            elapsed_ms=elapsed_ms,
            patch_paths=list(patch_paths or []),
            before_hash=before_hash,
            after_hash=after_hash,
            error=error,
        )
        if self._audit_sink is not None:
            try:
                self._audit_sink(record)
            except Exception:
                pass
        else:
            logger.debug(
                "hook audit %s %s %s %.1fms %s",
                record.hook,
                record.handler_key,
                record.status,
                record.elapsed_ms,
                record.error,
            )


class PluginManager:
    """工作区级插件管理器：加载 registry、启动 Worker、提供 dispatch。"""

    def __init__(
        self,
        *,
        workspace_root: Path,
        config: PluginsConfig | None = None,
        user_registry: Path | None = None,
        project_registry: Path | None = None,
        store_root: Path | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve()
        self.config = config or PluginsConfig()
        self.user_registry = Path(user_registry or user_registry_path())
        self.project_registry = Path(project_registry or project_registry_path(self.workspace_root))
        self.store_root = Path(store_root or user_store_root()).expanduser().resolve()
        self.dispatcher = HookDispatcher(config=self.config)
        self._workers: dict[str, PluginWorkerState] = {}
        self._closed = False
        self._lock = threading.RLock()

    @property
    def enabled(self) -> bool:
        return self.config.enabled

    def bootstrap(self) -> list[str]:
        """读取注册表、启动启用插件、构建执行计划。返回诊断信息。"""

        diagnostics: list[str] = []
        if not self.config.enabled:
            self.dispatcher.set_execution_plan([], {})
            diagnostics.append("plugins.enabled=false，以无插件模式运行。")
            return diagnostics

        try:
            user_doc = load_registry_document(self.user_registry)
            project_doc = load_registry_document(self.project_registry)
        except PluginRegistryError as exc:
            diagnostics.append(f"注册表读取失败，降级无插件：{exc}")
            self.dispatcher.set_execution_plan([], {})
            return diagnostics

        merged = merge_registry_documents(user_doc=user_doc, project_doc=project_doc)
        manifests: dict[str, tuple[PluginManifest, str, PluginRecord]] = {}
        workers: dict[str, PluginWorkerState] = {}

        for name, record in merged.plugins.items():
            if not record.enabled:
                continue
            scope = "project" if name in project_doc.plugins and project_doc.plugins[name].enabled else "user"
            # project 覆盖后 scope 以 project_doc 是否含该插件为准。
            if name in project_doc.plugins:
                scope = "project"
            else:
                scope = "user"
            try:
                root = self._resolve_plugin_root(record)
                manifest = self._load_manifest(root)
                if manifest.name != name:
                    diagnostics.append(f"插件目录包名 {manifest.name} 与注册表 {name} 不一致，已跳过。")
                    continue
                if not record.dev_mode:
                    if record.active is None:
                        raise PluginError(f"插件 {name} 缺少 active 版本指针。")
                    store_root = self.store_root
                    if root == store_root or not _is_relative_to(root, store_root):
                        raise PluginError(f"插件 {name} store 路径不在受信任目录：{root}")
                    if not record.active.integrity or not record.active.content_hash:
                        raise PluginError(f"插件 {name} 缺少完整性记录，拒绝启动。")
                    if manifest.version != record.active.version:
                        raise PluginError(
                            f"插件 {name} 版本不匹配：registry={record.active.version}，"
                            f"manifest={manifest.version}。"
                        )
                    verify_store_integrity(root, record.active.integrity)
                    verify_lockfile_hash(root, record.active.lockfile_hash)
                    verify_content_tree_hash(root, record.active.content_hash)
                unapproved = set(manifest.permissions) - set(record.approved_permissions)
                if unapproved:
                    raise PluginError(
                        f"插件 {name} 声明了未批准权限：{', '.join(sorted(unapproved))}"
                    )
                entry = (root / manifest.entry).resolve()
                if not entry.is_file() or not _is_relative_to(entry, root):
                    raise PluginError(f"插件 {name} entry 越界或不存在：{manifest.entry}")
                state = PluginWorkerState(
                    name=name,
                    manifest=manifest,
                    scope=scope,
                    record=record,
                    root=root,
                )
                if self._start_worker(state, diagnostics):
                    workers[name] = state
                    manifests[name] = (manifest, scope, record)
                else:
                    diagnostics.append(f"插件握手失败，已禁用：{name}")
            except Exception as exc:  # noqa: BLE001
                diagnostics.append(f"加载插件失败 {name}：{exc}")

        plan = build_execution_plan(
            manifests=manifests,
            disabled_handlers=merged.disabled_handlers,
            max_timeout_ms=self.config.max_timeout_ms,
        )
        self._workers = workers
        self.dispatcher.set_execution_plan(plan, workers)
        diagnostics.append(f"已加载 {len(workers)} 个插件，{len(plan)} 个 Handler。")
        return diagnostics

    def _resolve_plugin_root(self, record: PluginRecord) -> Path:
        if record.dev_mode and record.local_path:
            root = Path(record.local_path).expanduser().resolve()
            if not root.is_dir():
                raise PluginError(f"开发模式插件路径不存在：{root}")
            return root
        if record.active and record.active.store_path:
            root = Path(record.active.store_path).expanduser().resolve()
            if not root.is_dir():
                raise PluginError(f"插件 store 路径不存在：{root}")
            return root
        if record.local_path:
            root = Path(record.local_path).expanduser().resolve()
            if root.is_dir():
                return root
        raise PluginError(f"插件 {record.name} 缺少可用安装路径（localPath/storePath）。")

    def _load_manifest(self, root: Path) -> PluginManifest:
        package_path = root / "package.json"
        text = package_path.read_text(encoding="utf-8-sig")
        data = json.loads(text)
        return parse_plugin_manifest(data, source_path=str(package_path))

    def _start_worker(self, state: PluginWorkerState, diagnostics: list[str]) -> bool:
        client = PluginWorkerClient(
            plugin_root=state.root,
            plugin_name=state.name,
            timeout_ms=self.config.default_timeout_ms,
            max_message_bytes=self.config.max_message_bytes,
            on_stderr=lambda line: logger.debug("plugin[%s] %s", state.name, line),
            on_host_request=lambda method, params: self._handle_worker_request(
                state.name,
                method,
                params,
            ),
        )
        try:
            client.start()
            result = client.initialize(
                {
                    "apiVersion": HOOK_API_VERSION,
                    "omnicrawlVersion": OMNICRAWL_VERSION,
                    "permissions": sorted(
                        set(state.manifest.permissions)
                        & set(state.record.approved_permissions)
                    ),
                    "manifest": {
                        "name": state.manifest.name,
                        "version": state.manifest.version,
                        "hooks": [
                            {"id": h.id, "hook": h.hook, "mode": h.mode}
                            for h in state.manifest.hooks
                        ],
                    },
                },
                timeout_ms=min(self.config.max_timeout_ms, 5000),
            )
            actual_handlers = result.get("handlers", [])
            if not isinstance(actual_handlers, list):
                raise PluginProtocolError("initialized.handlers 必须是数组")
            declared = {
                item.id: (item.hook, item.mode)
                for item in state.manifest.hooks
            }
            actual_ids: set[str] = set()
            for item in actual_handlers:
                if not isinstance(item, Mapping):
                    continue
                handler_id = str(item.get("id", ""))
                actual = (str(item.get("hook", "")), str(item.get("mode", "")))
                if handler_id not in declared:
                    raise PluginProtocolError(f"运行期注册了 manifest 外 Handler：{handler_id}")
                actual_ids.add(handler_id)
                if actual != declared[handler_id]:
                    raise PluginProtocolError(
                        f"运行期 Handler 与 manifest 不一致：{handler_id} {actual} != {declared[handler_id]}"
                    )
            missing = set(declared) - actual_ids
            if missing:
                raise PluginProtocolError(
                    f"运行期缺少 manifest Handler：{', '.join(sorted(missing))}"
                )
            state.client = client
            state.active = True
            return True
        except Exception as exc:  # noqa: BLE001
            diagnostics.append(f"{state.name} initialize 失败：{exc}")
            try:
                client.close(force=True)
            except Exception:
                pass
            state.active = False
            state.last_error = str(exc)
            return False

    def _handle_worker_request(
        self,
        source_plugin: str,
        method: str,
        params: Mapping[str, Any],
    ) -> dict[str, Any]:
        """处理 Worker → Host 请求。V1 支持 custom.emit。"""

        if method != "custom.emit":
            raise PluginProtocolError(f"不支持的 Host method：{method}")
        event_name = str(params.get("event", "")).strip()
        version = int(params.get("version", 1) or 1)
        payload = params.get("payload", {})
        if not isinstance(payload, Mapping):
            raise PluginProtocolError("custom.emit payload 必须是对象。")
        return self.emit_custom_event(
            source_plugin=source_plugin,
            event_name=event_name,
            version=version,
            payload=dict(payload),
            parent_event_id=str(params.get("parentEventId") or "") or None,
            depth=int(params.get("depth", 0) or 0),
            local_delivered=int(params.get("localDelivered", 0) or 0),
        )

    def emit_custom_event(
        self,
        *,
        source_plugin: str,
        event_name: str,
        version: int,
        payload: Mapping[str, Any],
        parent_event_id: str | None = None,
        depth: int = 0,
        session_id: str | None = None,
        turn_id: str | None = None,
        local_delivered: int = 0,
    ) -> dict[str, Any]:
        """校验并分发自定义事件到已订阅 Handler。

        - 事件必须由 source 插件在 customEvents 中声明；
        - private 仅允许本插件订阅（由 Worker 本地投递）；
        - public 允许其他插件订阅（Host 跨 Worker 投递）；
        - 同插件订阅必须在 Worker 本地完成，避免 hook.invoke 重入死锁；
        - 超过 custom_event_max_depth 直接拒绝，防止递归放大。
        """

        if not self.config.enabled:
            raise PluginError("插件系统未启用，无法发布自定义事件。")
        if depth >= self.config.custom_event_max_depth:
            raise PluginError(
                f"自定义事件递归深度超限（{depth} >= {self.config.custom_event_max_depth}）。"
            )
        source = self._workers.get(source_plugin)
        if source is None or not source.active:
            raise PluginError(f"来源插件未激活：{source_plugin}")
        if "hook:custom-emit" not in source.manifest.permissions:
            raise PluginError(f"插件 {source_plugin} 缺少 hook:custom-emit 权限。")

        declaration = None
        for item in source.manifest.custom_events:
            if item.name == event_name and item.version == version:
                declaration = item
                break
        if declaration is None:
            for item in source.manifest.custom_events:
                if item.name == event_name:
                    declaration = item
                    break
        if declaration is None:
            raise PluginError(
                f"事件 {event_name}@v{version} 未在 {source_plugin} 的 customEvents 中声明。"
            )
        try:
            validate_payload_against_schema(payload, declaration.schema)
        except PluginManifestError as exc:
            raise PluginError(str(exc)) from exc

        # 跨插件投递：排除 source 自身，避免对同一 Worker 重入。
        subscribers = [
            item
            for item in self.dispatcher.current_plan()
            if item.hook == event_name and item.plugin_name != source_plugin
        ]
        if declaration.visibility == "private":
            subscribers = []
        else:
            filtered: list[ResolvedHandler] = []
            for item in subscribers:
                worker = self._workers.get(item.plugin_name)
                if worker is None:
                    continue
                perms = worker.manifest.permissions
                if "hook:custom-subscribe" in perms or f"hook:{event_name}" in perms:
                    filtered.append(item)
            subscribers = filtered

        cross_plugin = 0
        denied = False
        if subscribers:
            outcome = self.dispatcher.dispatch(
                event_name,
                dict(payload),
                policy=HookPolicy(
                    on_deny="ignore",
                    on_timeout="skip-handler",
                    on_protocol_error="skip-handler",
                    on_handler_error="skip-handler",
                    disable_plugin_on_error=False,
                ),
                workspace={
                    "id": stable_hash(str(self.workspace_root)),
                },
                session_id=session_id,
                turn_id=turn_id,
                parent_event_id=parent_event_id,
                depth=depth + 1,
                handlers_override=subscribers,
            )
            cross_plugin = len(outcome.results)
            denied = outcome.denied
        else:
            logger.debug(
                "custom event 无跨插件订阅者：%s from %s localDelivered=%s",
                event_name,
                source_plugin,
                local_delivered,
            )

        return {
            "ok": True,
            "delivered": cross_plugin,
            "crossPlugin": cross_plugin,
            "localDelivered": local_delivered,
            "event": event_name,
            "version": declaration.version,
            "denied": denied,
        }

    def dispatch(self, hook_name: str, payload: Mapping[str, Any] | None = None, **kwargs: Any) -> DispatchOutcome:
        workspace = kwargs.pop("workspace", None) or {
            "id": stable_hash(str(self.workspace_root)),
        }
        # 默认不暴露真实路径，除非插件获准 workspace:metadata。
        if any(
            "workspace:metadata" in (worker.manifest.permissions if worker.manifest else ())
            for worker in self._workers.values()
        ):
            workspace = {
                "id": stable_hash(str(self.workspace_root)),
                "root": str(self.workspace_root),
            }
        return self.dispatcher.dispatch(
            hook_name,
            payload,
            workspace=workspace,
            **kwargs,
        )

    def begin_turn(self) -> None:
        self.dispatcher.begin_turn()

    def end_turn(self) -> None:
        self.dispatcher.end_turn()

    def list_status(self) -> list[dict[str, Any]]:
        rows: list[dict[str, Any]] = []
        for name, worker in sorted(self._workers.items()):
            rows.append(
                {
                    "name": name,
                    "version": worker.manifest.version,
                    "scope": worker.scope,
                    "active": worker.active,
                    "circuitOpen": worker.circuit_open,
                    "failures": worker.failures,
                    "lastError": worker.last_error,
                    "handlers": [h.id for h in worker.manifest.hooks],
                    "devMode": worker.record.dev_mode,
                    "root": str(worker.root),
                }
            )
        return rows

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        for worker in list(self._workers.values()):
            client = worker.client
            worker.active = False
            if client is None:
                continue
            try:
                client.shutdown(timeout_ms=2000)
            except Exception:
                try:
                    client.close(force=True)
                except Exception:
                    pass
        self._workers.clear()
        self.dispatcher.set_execution_plan([], {})


class PluginRuntime:
    """进程级运行时：持有配置与当前工作区 PluginManager。"""

    def __init__(
        self,
        *,
        config: PluginsConfig | None = None,
        workspace_root: Path | None = None,
    ) -> None:
        self.config = config or PluginsConfig()
        self.workspace_root = Path(workspace_root or Path.cwd()).resolve()
        self.manager: PluginManager | None = None
        self.diagnostics: list[str] = []
        self._started = False

    @classmethod
    def from_config_data(
        cls,
        data: Mapping[str, Any] | None,
        *,
        workspace_root: Path | None = None,
    ) -> PluginRuntime:
        section = {}
        if isinstance(data, Mapping):
            raw = data.get("plugins", {})
            if isinstance(raw, Mapping):
                section = raw
        return cls(config=parse_plugins_config(section), workspace_root=workspace_root)

    def start(self) -> list[str]:
        if self._started and self.manager is not None:
            return list(self.diagnostics)
        self.manager = PluginManager(workspace_root=self.workspace_root, config=self.config)
        self.diagnostics = self.manager.bootstrap()
        self._started = True
        if self.config.enabled:
            try:
                outcome = self.manager.dispatch("app.start.before", {"phase": "bootstrap"})
                if outcome.denied:
                    # 显式 deny 阻断启动；插件故障已在 policy 中 skip。
                    raise PluginDispatchError(outcome.deny_reason or "app.start.before 被插件拒绝")
            except PluginDispatchError:
                raise
            except Exception as exc:  # noqa: BLE001
                self.diagnostics.append(f"app.start.before 分发异常：{exc}")
        return list(self.diagnostics)

    def notify_app_started(self) -> None:
        if self.manager is None or not self.config.enabled:
            return
        try:
            self.manager.dispatch("app.start.after", {"phase": "ready"})
        except Exception as exc:  # noqa: BLE001
            self.diagnostics.append(f"app.start.after 忽略故障：{exc}")

    def switch_workspace(self, new_root: Path, *, emit_hooks: bool = False) -> list[str]:
        """切换工作区并重建 PluginManager。

        默认不重复发 workspace.switch.*：Agent.switch_workspace 已负责 before/after。
        若从 CLI/测试直接调用，可传 emit_hooks=True。
        """

        new_root = Path(new_root).resolve()
        if emit_hooks and self.manager is not None and self.config.enabled:
            outcome = self.manager.dispatch(
                "workspace.switch.before",
                {"from": str(self.workspace_root), "to": str(new_root)},
            )
            if outcome.denied:
                raise PluginDispatchError(outcome.deny_reason or "workspace.switch.before 拒绝切换")
        self.close_manager_only()
        self.workspace_root = new_root
        self.manager = PluginManager(workspace_root=self.workspace_root, config=self.config)
        diagnostics = self.manager.bootstrap()
        self.diagnostics = diagnostics
        if emit_hooks and self.config.enabled and self.manager is not None:
            try:
                self.manager.dispatch(
                    "workspace.switch.after",
                    {"workspace": str(self.workspace_root)},
                )
            except Exception as exc:  # noqa: BLE001
                diagnostics.append(f"workspace.switch.after 忽略故障：{exc}")
        return diagnostics

    def close_manager_only(self) -> None:
        if self.manager is not None:
            try:
                self.manager.close()
            except Exception:
                pass
            self.manager = None

    def close(self) -> None:
        if self.manager is not None and self.config.enabled:
            try:
                self.manager.dispatch("app.stop.before", {"phase": "stopping"})
            except Exception:
                pass
        # Agent/会话资源由调用方先关闭，再调用这里。
        if self.manager is not None and self.config.enabled:
            try:
                self.manager.dispatch("app.stop.after", {"phase": "stopped"})
            except Exception:
                pass
        self.close_manager_only()
        self._started = False

    def dispatch(self, hook_name: str, payload: Mapping[str, Any] | None = None, **kwargs: Any) -> DispatchOutcome:
        if self.manager is None:
            return DispatchOutcome(hook=hook_name, payload=dict(payload or {}))
        return self.manager.dispatch(hook_name, payload, **kwargs)
