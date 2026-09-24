//! 内核侧的压缩接线：会话状态（会话目录、会话 id、运行期历史）与回合结束后的压缩触发。
//!
//! 摘要提示词模板在编译期嵌入：内核二进制自带这份文本，不依赖运行时的目录布局。

use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_compaction::{
    AfterTurnReport, CompactionConfig, CompactionDriver, SummaryAdapterSettings,
    SummaryModelAdapter, TurnBoundary,
};
use omnicrawl_controllers::context_compaction::{
    ArtifactReadFailure, ContextCompactionService, ModelSummaryCompactor,
    SessionEvidenceRecallService, SourceEvent, TokenUsageSample, DEFAULT_MAX_INPUT_TOKENS,
};
use omnicrawl_controllers::control::{session_closed_action, SessionCloseAction};
use omnicrawl_controllers::json::python_dumps_compact;
use omnicrawl_controllers::memory::session_memory_root;
use omnicrawl_ipc::bridge::{KernelCompactionConfig, KernelModelConfig, KernelSessionConfig};
use omnicrawl_session::{
    project_session_history, recover_run_guard_state, utc_now, MemoryStore, SessionArtifactStore,
    SessionStore,
};
use serde_json::{json, Value};

const SUMMARY_PROMPT: &str = include_str!("../../../../rust/assets/templates/summary_prompt.md");

/// 内核侧的会话状态：会话目录、会话 id、运行期历史与压缩策略。
pub struct KernelSession {
    pub store: Arc<SessionStore>,
    pub session_id: String,
    /// 运行期历史（模型上下文里除系统提示词与工具声明之外的部分）。
    pub history: Vec<Value>,
    pub config: CompactionConfig,
    /// 会话级记忆的用户数据根；为空则不做记忆回写与自动召回。
    pub memory_root: Option<PathBuf>,
    /// 宿主工作区根：回合内用它拍工作区快照（`/undo` 的副作用回滚）。
    pub workspace: Option<String>,
}

impl KernelSession {
    /// 打开会话：给了 session_id 就续跑，否则新建一条。
    pub fn open(settings: KernelSessionConfig) -> Result<Self, String> {
        let store = Arc::new(SessionStore::open(settings.root.clone()));
        store
            .ensure()
            .map_err(|error| error.message().to_string())?;
        let requested = settings.session_id.trim().to_string();
        let session_id = if requested.is_empty() {
            let workspace = std::env::current_dir()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            store
                .start_session(&workspace, "", utc_now())
                .map_err(|error| error.message().to_string())?
                .session_id
        } else {
            requested
        };
        let mut state = Self {
            store,
            session_id,
            history: Vec::new(),
            config: compaction_config(settings.compaction.as_ref()),
            memory_root: settings
                .memory_root
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            workspace: settings
                .workspace_root
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        };
        state.reload_history()?;
        Ok(state)
    }

    /// 按转录重建运行期历史：重启后与压缩后走同一条路径。
    pub fn reload_history(&mut self) -> Result<(), String> {
        let events = self
            .store
            .read_active_events(&self.session_id)
            .map_err(|error| error.message().to_string())?;
        self.history = project_session_history(&events)
            .into_iter()
            .map(|(_, message)| message)
            .collect();
        Ok(())
    }

    /// 切换到已有会话：重建运行期历史。调用方负责先校验会话存在（含归档处理）。
    pub fn reopen(&mut self, session_id: &str) -> Result<(), String> {
        self.session_id = session_id.to_string();
        self.reload_history()
    }

    /// 在当前存储上新建一条会话并切换过去，返回新会话 id。
    ///
    /// 与 `open` 的新建分支同一口径（工作区缺省取进程当前目录）。
    pub fn start_new(&mut self) -> Result<String, String> {
        let workspace = self.workspace.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default()
        });
        let created = self
            .store
            .start_session(&workspace, "", utc_now())
            .map_err(|error| error.message().to_string())?;
        self.session_id = created.session_id;
        self.reload_history()?;
        Ok(self.session_id.clone())
    }

    /// 运行护栏恢复出来的待续任务文本与执行清单投影。
    ///
    /// 用户发短「继续/重试」时用它还原上一轮任务：与 `read_active_events` 同一视图
    /// （被回退的轮次不参与），读不出来就当成「没有待续任务」，不阻断本次回合。
    pub fn run_guard_state(&self) -> (String, Vec<Value>) {
        match self.store.read_active_events(&self.session_id) {
            Ok(events) => recover_run_guard_state(&events),
            Err(error) => {
                eprintln!("[kernel] 读取待续任务失败：{}", error.message());
                (String::new(), Vec::new())
            }
        }
    }

    /// 落一条会话事件：回合消息与压缩事件都走这里。
    pub fn append(&self, event_type: &str, payload: Value) -> Result<(), String> {
        let payload = payload.as_object().cloned().unwrap_or_default();
        self.store
            .append_event(&self.session_id, event_type, payload, None, utc_now())
            .map(|_| ())
            .map_err(|error| error.message().to_string())
    }

    /// 正常退出时收尾当前会话：按最后一次事件类型决定补写 `session_closed` 还是
    /// 直接丢弃空占位。
    ///
    /// 对映 Python `LocalToolAgent._append_session_closed_event`：
    /// * 已是 `session_closed`——只丢空占位；
    /// * 已是 `session_interrupted`——什么都不做（中断的会话要保留）；
    /// * 其余（含正常回合收尾）——先补写 `session_closed`，再丢空占位。
    ///
    /// 判定复用 `omnicrawl-controllers` 的决策层（与 Python 同一份对照数据集），
    /// 因此「最后一次事件类型 → 动作」的映射不会在两侧分叉。本方法是幂等的：
    /// 会话已被丢弃后再调用会因读不到事件而落到「无会话」分支，不做第二次写入。
    pub fn close(&self) {
        let last_event_type = self
            .store
            .read_active_events(&self.session_id)
            .ok()
            .and_then(|events| events.last().map(|event| event.event_type.clone()));
        match session_closed_action(last_event_type.as_deref()) {
            SessionCloseAction::None => {}
            SessionCloseAction::Discard => {
                let _ = self.store.discard_empty_session(&self.session_id);
            }
            SessionCloseAction::AppendAndDiscard => {
                if let Err(detail) = self.append("session_closed", json!({})) {
                    eprintln!("[kernel] 补写 session_closed 失败：{detail}");
                }
                let _ = self.store.discard_empty_session(&self.session_id);
            }
        }
    }

    /// 运行中切换工作区：把会话的工作区指向新根，并转录 `workspace_switched`。
    ///
    /// 对映 Python `WorkspaceSwitchingMixin.switch_workspace` 的收尾一步
    /// （`_append_session_event("workspace_switched", {"from": str(old), "to": str(new)})`）。
    /// 会话已全局化、且切换保持同一会话，所以这里不新建/重建会话，只改工作区与事件。
    ///
    /// 先追加事件再改内存字段：追加失败时两者都不变，不会出现「工作区已换、转录里没有
    /// 这条切换」的半截状态。目标与当前相同时直接返回，不重复写事件（与 Python 的早退一致）。
    ///
    /// 返回 `(from, to)`；`from` 取会话记录的工作区，未记录时用空串。
    pub fn switch_workspace(&mut self, new_root: &str) -> Result<(String, String), String> {
        let from = self.workspace.clone().unwrap_or_default();
        let to = new_root.to_string();
        if from == to {
            return Ok((from, to));
        }
        self.append("workspace_switched", json!({ "from": from, "to": to }))?;
        self.workspace = Some(to.clone());
        Ok((from, to))
    }
}

/// 协议里的压缩配置 → 驱动配置；缺字段沿用内核默认值。
pub fn compaction_config(settings: Option<&KernelCompactionConfig>) -> CompactionConfig {
    let mut config = CompactionConfig::default();
    let Some(settings) = settings else {
        return config;
    };
    overlay_compaction_config(&mut config, settings);
    config
}

/// 把协议里的压缩配置覆盖到既有配置上（只动给出的字段），返回被改动的字段路径。
///
/// `session.settings` 用它做运行期更新；`compaction_config` 用它做首次装载——
/// 两条路径共用同一份字段映射，避免「初始化认的字段」与「热更新认的字段」漂移。
pub fn overlay_compaction_config(
    config: &mut CompactionConfig,
    settings: &KernelCompactionConfig,
) -> Vec<&'static str> {
    let mut applied = Vec::new();
    if let Some(value) = settings.recent_turns {
        config.recent_turns = value;
        applied.push("compaction.recent_turns");
    }
    if let Some(value) = settings.target_summary_tokens {
        config.target_summary_tokens = value;
        applied.push("compaction.target_summary_tokens");
    }
    if let Some(value) = settings.next_user_reserve_tokens {
        config.next_user_reserve_tokens = value;
        applied.push("compaction.next_user_reserve_tokens");
    }
    if let Some(value) = settings.trigger_context_tokens {
        config.trigger_context_tokens = value;
        applied.push("compaction.trigger_context_tokens");
    }
    if let Some(value) = settings.context_window_tokens {
        config.context_window_tokens = value;
        applied.push("compaction.context_window_tokens");
    }
    if let Some(value) = settings.emergency_context_ratio {
        config.emergency_context_ratio = value;
        applied.push("compaction.emergency_context_ratio");
    }
    if let Some(value) = settings.reasoning_effort.as_ref() {
        config.reasoning_effort = value.clone();
        applied.push("compaction.reasoning_effort");
    }
    if let Some(value) = settings.preserve_exact_evidence {
        config.preserve_exact_evidence = value;
        applied.push("compaction.preserve_exact_evidence");
    }
    if let Some(value) = settings.archive_compacted_events {
        config.archive_compacted_events = value;
        applied.push("compaction.archive_compacted_events");
    }
    if let Some(value) = settings.auto_memory_recall {
        config.auto_memory_recall = value;
        applied.push("compaction.auto_memory_recall");
    }
    applied
}

/// 构造压缩驱动：摘要请求走与主请求同一个内核模型配置。
fn build_driver(
    session: &KernelSession,
    model: &KernelModelConfig,
    api_key: &str,
    last_request_messages: &[Value],
) -> Result<CompactionDriver, String> {
    let runtime = match crate::session::build_model_runtime_with_key(model, api_key.to_string()) {
        Ok(runtime) => runtime,
        Err(omnicrawl_core::LoopError::ReplySource(message)) => return Err(message),
        Err(other) => return Err(format!("{other:?}")),
    };
    let adapter = SummaryModelAdapter::new(SummaryAdapterSettings {
        runtime,
        model: model.model.clone(),
        provider: String::new(),
        system_prompt: model.system_prompt.clone(),
        prefix: last_request_messages.to_vec(),
        tools: model.tools.clone(),
        options: crate::session::parse_options(model)
            .map_err(|error| error.message().to_string())?,
        prompt_cache_identity: model.prompt_cache_identity.clone(),
        context_window_tokens: session.config.context_window_tokens,
    });
    let compactor = ModelSummaryCompactor::new(
        Box::new(adapter),
        SUMMARY_PROMPT.trim(),
        DEFAULT_MAX_INPUT_TOKENS,
    )
    .map_err(|error| error.message().to_string())?;
    let service = ContextCompactionService::new().with_compactor(Box::new(compactor));
    let mut driver =
        CompactionDriver::new(Arc::clone(&session.store), service, session.config.clone());
    if let Some(root) = session.memory_root.as_ref() {
        if let Ok(path) = session_memory_root(root, &session.session_id) {
            driver = driver.with_memory(MemoryStore::open(path));
        }
    }
    Ok(driver)
}

/// 回合结束后跑一次压缩：摘要请求复用主请求前缀与工具面。
#[allow(clippy::too_many_arguments)]
pub fn compact_after_turn(
    session: &KernelSession,
    model: &KernelModelConfig,
    api_key: &str,
    usage: TokenUsageSample,
    last_request_input_tokens: i64,
    last_request_messages: &[Value],
    history_messages: &[Value],
) -> Result<AfterTurnReport, String> {
    let driver = build_driver(session, model, api_key, last_request_messages)?;
    let boundary = TurnBoundary {
        session_id: &session.session_id,
        system_prompt: model.system_prompt.as_str(),
        context_messages: &[],
        history_messages,
        tool_schemas: &model.tools,
        usage,
        last_request_input_tokens,
    };
    driver.after_turn(&boundary)
}

/// 显式压缩当前会话（`session.compact` 命令）：不做阈值判定，直接请求一次摘要。
///
/// 命令本身不带参数；摘要请求复用主请求的前缀与工具面，压缩后的历史回写运行期历史。
pub fn compact_now(
    session: &KernelSession,
    model: &KernelModelConfig,
    api_key: &str,
    last_request_messages: &[Value],
) -> Result<AfterTurnReport, String> {
    let driver = build_driver(session, model, api_key, last_request_messages)?;
    driver.manual_compact(&session.session_id)
}

/// 上下文超限后的恢复：压缩当前未完成回合，返回可继续的历史投影。
pub fn recover_after_overflow(
    session: &KernelSession,
    model: &KernelModelConfig,
    api_key: &str,
    last_request_messages: &[Value],
    previous_history: &[Value],
) -> Result<AfterTurnReport, String> {
    let driver = build_driver(session, model, api_key, last_request_messages)?;
    driver.recover_after_overflow(&session.session_id, previous_history)
}

/// 内核侧证据恢复：按当前有效摘要授权读取会话事件与 artifact。
///
/// 与 Python `_tool_recall_session_evidence` 同口径：没有活动会话、或会话暂时读不出来，
/// 都给一份失败信封；返回 (ok, 输出文本)，文本是紧凑 JSON（Python `json.dumps(..., separators=(",", ":"))`）。
pub fn recall_session_evidence(
    session: Option<&KernelSession>,
    arguments: &Value,
) -> (bool, String) {
    let Some(session) = session else {
        return failure_envelope("session_unavailable", "当前没有可读取的活动 Session。");
    };
    let events = match session.store.read_active_events(&session.session_id) {
        Ok(events) => events,
        Err(_) => {
            return failure_envelope("evidence_unavailable", "当前 Session 证据暂时不可读取。")
        }
    };
    let source_events: Vec<SourceEvent> = events
        .into_iter()
        .map(|event| SourceEvent {
            event_id: event.event_id,
            event_type: event.event_type,
            payload: Value::Object(event.payload),
        })
        .collect();
    let root = session.store.root().to_path_buf();
    let artifacts = SessionArtifactStore::new(root.clone(), root.join("artifacts"));
    let session_id = session.session_id.clone();
    let reader = move |path: &str| -> Result<String, ArtifactReadFailure> {
        artifacts
            .read_text(&session_id, path)
            .map_err(|_| ArtifactReadFailure::Unreadable)
    };
    let result = SessionEvidenceRecallService::default().recall(
        &source_events,
        arguments.get("event_ids").unwrap_or(&Value::Null),
        &reader,
    );
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    (ok, python_dumps_compact(&result))
}

/// 证据恢复的失败信封：形状与 Python 侧一致。
fn failure_envelope(code: &str, message: &str) -> (bool, String) {
    let value = json!({
        "schema_version": 1,
        "ok": false,
        "items": [],
        "diagnostics": [{"code": code, "message": message}],
        "truncated": false,
    });
    (false, python_dumps_compact(&value))
}
