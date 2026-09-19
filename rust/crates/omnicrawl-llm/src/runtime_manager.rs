//! 模型运行时管理器（对应 `omnicrawl/llm/runtime.py`）。
//!
//! 不可变快照 + 回合边界热切换：切换时先造候选运行时、再持久化、最后原子替换
//! 活跃快照；旧快照进退役列表，引用归零后释放。与 Python 的唯一差别是内核的
//! [`ModelRuntime`] 没有 `close()` 关闭态（取消由 `TurnSink` 表达），因此
//! 「引用归零后关闭」在这里等价于把快照移出退役列表并释放引用。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::adapter::{build_runtime, ModelDescriptor, ProviderProfile};
use crate::capabilities::ModelCapabilities;
use crate::errors::{ModelError, ModelErrorCode};
use crate::runtime::ModelRuntime;

/// 一次构建的不可变运行时快照。
#[derive(Clone)]
pub struct RuntimeSnapshot {
    pub generation: u64,
    pub descriptor: ModelDescriptor,
    pub runtime: Arc<dyn ModelRuntime>,
    pub capabilities: ModelCapabilities,
    pub context_window_tokens: i64,
    pub profile: ProviderProfile,
}

/// 候选运行时的工厂：`Err(message)` 会被包装成「创建模型运行时失败」。
pub type RuntimeFactory<'a> =
    &'a dyn Fn(&ProviderProfile, &ModelDescriptor) -> Result<Arc<dyn ModelRuntime>, String>;

impl std::fmt::Debug for RuntimeSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeSnapshot")
            .field("generation", &self.generation)
            .field("descriptor", &self.descriptor)
            .field("context_window_tokens", &self.context_window_tokens)
            .finish_non_exhaustive()
    }
}

/// 切换前的持久化动作；失败时保留旧模型并把错误原样抛出。
pub type PersistFn<'a> = &'a dyn Fn() -> Result<(), ModelError>;

#[derive(Default)]
struct ManagerState {
    active: Option<RuntimeSnapshot>,
    refs: BTreeMap<u64, i64>,
    retired: Vec<RuntimeSnapshot>,
    generation: u64,
    active_turns: i64,
}

/// 管理当前活跃运行时的热切换。
pub struct ModelRuntimeManager {
    state: Mutex<ManagerState>,
    switch_lock: Mutex<()>,
}

impl Default for ModelRuntimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelRuntimeManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ManagerState::default()),
            switch_lock: Mutex::new(()),
        }
    }

    pub fn generation(&self) -> u64 {
        lock(&self.state).generation
    }

    pub fn active_snapshot(&self) -> Option<RuntimeSnapshot> {
        lock(&self.state).active.clone()
    }

    pub fn has_active_turn(&self) -> bool {
        lock(&self.state).active_turns > 0
    }

    /// 初始化或强制替换当前快照（启动时使用）。
    pub fn bootstrap(
        &self,
        profile: &ProviderProfile,
        descriptor: &ModelDescriptor,
    ) -> Result<RuntimeSnapshot, ModelError> {
        let runtime: Arc<dyn ModelRuntime> = Arc::from(build_runtime(profile, descriptor)?.runtime);
        Ok(self.install(profile, descriptor, runtime))
    }

    /// 与 [`Self::bootstrap`] 相同，但由调用方给出运行时（测试与外部装配使用）。
    pub fn bootstrap_with(
        &self,
        profile: &ProviderProfile,
        descriptor: &ModelDescriptor,
        runtime: Arc<dyn ModelRuntime>,
    ) -> RuntimeSnapshot {
        self.install(profile, descriptor, runtime)
    }

    fn install(
        &self,
        profile: &ProviderProfile,
        descriptor: &ModelDescriptor,
        runtime: Arc<dyn ModelRuntime>,
    ) -> RuntimeSnapshot {
        let _switch = lock(&self.switch_lock);
        let snapshot = {
            let mut state = lock(&self.state);
            state.generation += 1;
            let snapshot = make_snapshot(state.generation, profile, descriptor, runtime);
            let old = state.active.replace(snapshot.clone());
            state.refs.insert(snapshot.generation, 0);
            if let Some(old) = old {
                state.retired.push(old);
            }
            snapshot
        };
        self.drain_retired();
        snapshot
    }

    /// 取得当前快照并记一次回合占用。
    pub fn acquire_turn(&self) -> Result<RuntimeSnapshot, ModelError> {
        // 与 switch 共用锁，确保「检查无活动回合 → 交换 active」与新回合获取快照之间无竞态。
        let _switch = lock(&self.switch_lock);
        let mut state = lock(&self.state);
        let Some(snapshot) = state.active.clone() else {
            return Err(ModelError::configuration("模型运行时尚未初始化。"));
        };
        *state.refs.entry(snapshot.generation).or_insert(0) += 1;
        state.active_turns += 1;
        Ok(snapshot)
    }

    /// 释放一次回合占用。
    pub fn release_turn(&self, snapshot: &RuntimeSnapshot) {
        {
            let mut state = lock(&self.state);
            let current = state.refs.get(&snapshot.generation).copied().unwrap_or(0);
            state.refs.insert(snapshot.generation, (current - 1).max(0));
            state.active_turns = (state.active_turns - 1).max(0);
        }
        self.drain_retired();
    }

    /// 构建候选运行时 → 持久化 → 原子替换活跃快照。
    ///
    /// 任一步失败都保留旧模型；`allow_during_turn` 为假时拒绝在回合进行中切换。
    pub fn switch(
        &self,
        profile: &ProviderProfile,
        descriptor: &ModelDescriptor,
        persist: Option<PersistFn<'_>>,
        allow_during_turn: bool,
        runtime_factory: Option<RuntimeFactory<'_>>,
    ) -> Result<RuntimeSnapshot, ModelError> {
        let _switch = lock(&self.switch_lock);
        if !allow_during_turn && lock(&self.state).active_turns > 0 {
            return Err(invalid_request(
                "当前仍有进行中的模型回合，请等待本轮结束后再切换模型。",
            ));
        }

        let runtime = match runtime_factory {
            Some(factory) => match factory(profile, descriptor) {
                Ok(runtime) => runtime,
                Err(message) => {
                    return Err(ModelError::configuration(format!(
                        "创建模型运行时失败：{message}"
                    )))
                }
            },
            None => Arc::from(build_runtime(profile, descriptor)?.runtime),
        };

        if let Some(persist) = persist {
            persist()?;
        }

        let snapshot = {
            let mut state = lock(&self.state);
            state.generation += 1;
            let snapshot = make_snapshot(state.generation, profile, descriptor, runtime);
            let old = state.active.replace(snapshot.clone());
            state.refs.insert(snapshot.generation, 0);
            if let Some(old) = old {
                state.retired.push(old);
            }
            snapshot
        };
        self.drain_retired();
        Ok(snapshot)
    }

    pub fn current_model_id(&self) -> String {
        self.active_snapshot()
            .map(|snapshot| snapshot.descriptor.model_id.clone())
            .unwrap_or_default()
    }

    pub fn current_context_window(&self) -> i64 {
        self.active_snapshot()
            .map(|snapshot| snapshot.context_window_tokens)
            .unwrap_or(0)
    }

    /// 更新当前不可变快照的上下文窗口，不重建运行时。
    pub fn set_context_window_tokens(&self, tokens: i64) -> Result<i64, ModelError> {
        if tokens <= 0 {
            return Err(ModelError::configuration("上下文长度必须是正整数 Token。"));
        }
        let mut state = lock(&self.state);
        if let Some(active) = state.active.take() {
            state.active = Some(RuntimeSnapshot {
                context_window_tokens: tokens,
                ..active
            });
        }
        Ok(tokens)
    }

    /// 清空管理器：活跃与退役快照一并释放。
    pub fn close(&self) {
        let mut snapshots: Vec<RuntimeSnapshot> = Vec::new();
        {
            let mut state = lock(&self.state);
            snapshots.append(&mut state.retired);
            if let Some(active) = state.active.take() {
                snapshots.push(active);
            }
            state.refs.clear();
            state.active_turns = 0;
        }
        drop(snapshots);
    }

    /// 退役列表中引用归零的快照在此释放（对应 Python 的 `_close_retired_if_idle`）。
    fn drain_retired(&self) {
        let mut state = lock(&self.state);
        let retired = std::mem::take(&mut state.retired);
        let mut remaining: Vec<RuntimeSnapshot> = Vec::new();
        for snapshot in retired {
            if state.refs.get(&snapshot.generation).copied().unwrap_or(0) <= 0 {
                state.refs.remove(&snapshot.generation);
            } else {
                remaining.push(snapshot);
            }
        }
        state.retired = remaining;
    }
}

/// 便捷：取得快照执行一回合并释放引用。
pub fn run_with_snapshot<T>(
    manager: &ModelRuntimeManager,
    turn: impl FnOnce(&RuntimeSnapshot) -> T,
) -> Result<T, ModelError> {
    let snapshot = manager.acquire_turn()?;
    let result = turn(&snapshot);
    manager.release_turn(&snapshot);
    Ok(result)
}

fn make_snapshot(
    generation: u64,
    profile: &ProviderProfile,
    descriptor: &ModelDescriptor,
    runtime: Arc<dyn ModelRuntime>,
) -> RuntimeSnapshot {
    let capabilities = descriptor.capabilities.unwrap_or_default();
    let context_window_tokens = if descriptor.context_window_tokens != 0 {
        descriptor.context_window_tokens
    } else {
        capabilities.context_window_tokens
    };
    RuntimeSnapshot {
        generation,
        descriptor: descriptor.clone(),
        runtime,
        capabilities,
        context_window_tokens,
        profile: profile.clone(),
    }
}

fn invalid_request(message: impl Into<String>) -> ModelError {
    let mut error = ModelError::configuration(message);
    error.code = ModelErrorCode::InvalidRequest;
    error
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
