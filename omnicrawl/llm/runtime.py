"""ModelRuntimeManager：不可变运行时快照与回合边界热切换。"""

from __future__ import annotations

import threading
from dataclasses import dataclass, replace
from typing import Any, Callable

from .capabilities import ModelCapabilities
from .errors import ModelError, ModelErrorCode
from .protocol import ModelRuntime
from .registry import ModelDescriptor, ProviderProfile, build_runtime


@dataclass(frozen=True)
class RuntimeSnapshot:
    generation: int
    descriptor: ModelDescriptor
    runtime: ModelRuntime
    capabilities: ModelCapabilities
    context_window_tokens: int
    profile: ProviderProfile


class ModelRuntimeManager:
    """管理当前活跃 Runtime，并在回合边界完成安全切换。"""

    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._active: RuntimeSnapshot | None = None
        self._refs: dict[int, int] = {}
        self._retired: list[RuntimeSnapshot] = []
        self._generation = 0
        self._switch_lock = threading.Lock()
        self._active_turn_count = 0

    @property
    def generation(self) -> int:
        with self._lock:
            return self._generation

    @property
    def active_snapshot(self) -> RuntimeSnapshot | None:
        with self._lock:
            return self._active

    @property
    def has_active_turn(self) -> bool:
        with self._lock:
            return self._active_turn_count > 0

    def bootstrap(
        self,
        profile: ProviderProfile,
        descriptor: ModelDescriptor,
        *,
        runtime: ModelRuntime | None = None,
    ) -> RuntimeSnapshot:
        """初始化或强制替换当前快照（启动时使用）。"""

        with self._switch_lock:
            candidate_runtime = runtime or build_runtime(profile, descriptor)
            with self._lock:
                old = self._active
                self._generation += 1
                snapshot = RuntimeSnapshot(
                    generation=self._generation,
                    descriptor=descriptor,
                    runtime=candidate_runtime,
                    capabilities=descriptor.capabilities,
                    context_window_tokens=descriptor.context_window_tokens
                    or descriptor.capabilities.context_window_tokens,
                    profile=profile,
                )
                self._active = snapshot
                self._refs[snapshot.generation] = 0
                if old is not None:
                    self._retired.append(old)
            self._close_retired_if_idle()
            return snapshot

    def acquire_turn(self) -> RuntimeSnapshot:
        # 与 switch 共用锁，确保“检查无活动回合 → 交换 active”与新回合获取
        # 快照之间不存在竞态窗口。
        with self._switch_lock:
            with self._lock:
                if self._active is None:
                    raise ModelError(
                        code=ModelErrorCode.CONFIGURATION_ERROR,
                        message="模型运行时尚未初始化。",
                    )
                snapshot = self._active
                self._refs[snapshot.generation] = self._refs.get(snapshot.generation, 0) + 1
                self._active_turn_count += 1
                return snapshot

    def release_turn(self, snapshot: RuntimeSnapshot) -> None:
        with self._lock:
            current = self._refs.get(snapshot.generation, 0)
            self._refs[snapshot.generation] = max(0, current - 1)
            self._active_turn_count = max(0, self._active_turn_count - 1)
        self._close_retired_if_idle()

    def switch(
        self,
        profile: ProviderProfile,
        descriptor: ModelDescriptor,
        *,
        persist: Callable[[], None] | None = None,
        allow_during_turn: bool = False,
        runtime_factory: Callable[[ProviderProfile, ModelDescriptor], ModelRuntime] | None = None,
    ) -> RuntimeSnapshot:
        """构建候选 runtime → 持久化 → 原子替换 active snapshot。

        任一步失败都保留旧模型；旧 runtime 在引用归零后 close。

        ``allow_during_turn=True`` 时允许在回合进行中切换：当前回合仍持有
        旧快照引用，不会被误 close；下一次 ``acquire_turn`` 自动拿到新快照，
        即“从修改后的下一次请求开始生效”。
        """

        factory = runtime_factory or build_runtime
        candidate_runtime: ModelRuntime | None = None
        with self._switch_lock:
            if not allow_during_turn:
                with self._lock:
                    if self._active_turn_count > 0:
                        raise ModelError(
                            code=ModelErrorCode.INVALID_REQUEST,
                            message="当前仍有进行中的模型回合，请等待本轮结束后再切换模型。",
                        )
            try:
                candidate_runtime = factory(profile, descriptor)
            except Exception as exc:
                if isinstance(exc, ModelError):
                    raise
                raise ModelError(
                    code=ModelErrorCode.CONFIGURATION_ERROR,
                    message=f"创建模型运行时失败：{exc}",
                ) from exc

            try:
                if persist is not None:
                    persist()
            except Exception:
                try:
                    candidate_runtime.close()
                except Exception:  # noqa: BLE001 - 持久化失败后回收候选运行时，失败不掩盖原始异常
                    pass
                raise

            with self._lock:
                old = self._active
                self._generation += 1
                snapshot = RuntimeSnapshot(
                    generation=self._generation,
                    descriptor=descriptor,
                    runtime=candidate_runtime,
                    capabilities=descriptor.capabilities,
                    context_window_tokens=descriptor.context_window_tokens
                    or descriptor.capabilities.context_window_tokens,
                    profile=profile,
                )
                self._active = snapshot
                self._refs[snapshot.generation] = 0
                if old is not None:
                    self._retired.append(old)
            self._close_retired_if_idle()
            return snapshot

    def current_model_id(self) -> str:
        snapshot = self.active_snapshot
        if snapshot is None:
            return ""
        return snapshot.descriptor.model_id

    def current_context_window(self) -> int:
        snapshot = self.active_snapshot
        if snapshot is None:
            return 0
        return snapshot.context_window_tokens

    def set_context_window_tokens(self, tokens: int) -> int:
        """更新当前不可变快照的上下文窗口，不重建或关闭模型 Runtime。"""

        if isinstance(tokens, bool) or not isinstance(tokens, int) or tokens <= 0:
            raise ValueError("上下文长度必须是正整数 Token。")
        with self._lock:
            if self._active is not None:
                self._active = replace(self._active, context_window_tokens=tokens)
        return tokens

    def close(self) -> None:
        with self._lock:
            snapshots = list(self._retired)
            if self._active is not None:
                snapshots.append(self._active)
            self._active = None
            self._retired.clear()
            self._refs.clear()
            self._active_turn_count = 0
        for snapshot in snapshots:
            try:
                snapshot.runtime.close()
            except Exception:  # noqa: BLE001 - 关闭已退役运行时失败不阻断整体关闭
                pass

    def _close_retired_if_idle(self) -> None:
        to_close: list[RuntimeSnapshot] = []
        with self._lock:
            remaining: list[RuntimeSnapshot] = []
            for snapshot in self._retired:
                if self._refs.get(snapshot.generation, 0) <= 0:
                    to_close.append(snapshot)
                    self._refs.pop(snapshot.generation, None)
                else:
                    remaining.append(snapshot)
            self._retired = remaining
        for snapshot in to_close:
            try:
                snapshot.runtime.close()
            except Exception:  # noqa: BLE001 - 关闭空闲运行时失败不阻断后续回收
                pass


def run_with_snapshot(
    manager: ModelRuntimeManager,
    turn: Callable[[RuntimeSnapshot], Any],
) -> Any:
    """便捷：获取快照执行一回合并释放引用。"""

    snapshot = manager.acquire_turn()
    try:
        return turn(snapshot)
    finally:
        manager.release_turn(snapshot)
