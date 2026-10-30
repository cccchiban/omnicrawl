//! 由 `session.rs` 按职责拆出，语义未变。

use super::*;

use super::model_port::{ActiveAssistantSlot, build_model_runtime, parse_options, parse_tools, provider_warning_event, prune_candidate_of, to_agent_reply};

use super::protocol::{PortFailure};

use super::turn::Conn;
use omnicrawl_controllers::tool_args::is_subagent_tool;

/// 工具端口：整批交给宿主执行，按调用顺序取回观察。
pub(super) struct RemoteTools {
    pub(super) conn: Rc<RefCell<Conn>>,
    pub(super) turn_id: String,
    /// 隔离根：子任务把工具批次指到 worktree 里执行；主回合与共享子任务为 `None`。
    pub(super) workspace_root: Option<String>,
    /// 独立视觉模型代理的连接配置：真有带图观察时才装配（`[vision]` 未启用时 `load` 返回 `None`）。
    pub(super) vision_model: Option<KernelModelConfig>,
    /// 本轮 undo 账本：记录工具副作用，回合收尾时落 `turn_snapshot`。后台批次不记账。
    pub(super) undo: Option<Rc<RefCell<crate::undo::TurnUndo>>>,
    /// 本轮执行清单：`update_todos` 调用按内核投影写进来，供 run_guard 续跑判定读。
    pub(super) todos: Option<Rc<RefCell<Vec<TodoItem>>>>,
    /// 工具事件落盘开关与来源：`Some` 时本批写 `tool_call_requested` / `tool_result`，
    /// 格子里是本批的 assistant 原文。**`None` 表示这批不落事件**——后台子任务批次走的就是
    /// 这条路，对应 Python 子代理循环里的 `persist_session_events=False`。
    pub(super) active_assistant: Option<ActiveAssistantSlot>,
    /// 本回合任务文本：淘汰裁决与逐批压缩用它判断「这次调用的结果还有没有用」。
    pub(super) task_hint: String,
    /// **上一批**调用（刚变老的那批）的送审形状；本批开始时若判为无用就整组移出上下文。
    ///
    /// 只保存最近一批：淘汰点因此始终贴着尾部，最新一批（还在被使用）从不被淘汰。
    pub(super) previous_batch: Vec<PruneCandidate>,
    /// 与 [`Self::previous_batch`] 同序的事件 ID（`tool_call_requested` / `tool_result`）：
    /// 逐批工具调用压缩按它精确覆盖上一批，不波及更早的批次。
    pub(super) previous_batch_event_ids: Vec<String>,
}

impl ToolBatchHost for RemoteTools {
    /// 执行本批之前：把**上一批**（刚变老的那批）交宿主裁决，判为无用的**标注**下来。
    ///
    /// 钩子点由回合循环给（新的 assistant 工具消息入列之后、本批执行之前），因此：
    /// * 送审只盯上下文尾部，前面已发过的前缀逐字不变（前缀缓存不失效）；
    /// * 最新一批（本批，还没执行）永不送审——它正是被后续步骤使用的「小登」；
    /// * 一组的粒度是「一次调用」：调用项与其 `tool` 结果一起走，协议不会留下半截。
    ///
    /// **两条路径同构**：决策模型淘汰与非决策模型工具调用压缩都是「逐批对刚变老的那一批下判断」，
    /// 不再有一点汇合到回合收尾的全量压缩。决策优先，失败自动回退：
    /// * 宿主持有 `tool_call_prune` 能力 → 走裁决；裁决不可用（开关关、无渠道、失败、不可解析）
    ///   就落到下面的逐批压缩（`evicted_call_ids_for` 返回 `None` 即不可用）；
    /// * 宿主不持该能力 → 直接走逐批压缩。
    ///
    /// 处理方式（用户指定的「先标注-后压缩」）：两条路径都只当场**标注**（落到
    /// [`EVICTED_EVENT_TYPE`] / `tool_call_summary` 事件与本轮账本），**不改写本回合的上下文**；
    /// 整组剔除统一在回合收尾做（见 `run_session_tail`）。
    fn before_tool_batch(&mut self, _messages: &mut Vec<Value>) -> Result<(), LoopError> {
        if self.previous_batch.is_empty() {
            return Ok(());
        }
        let groups = std::mem::take(&mut self.previous_batch);
        let event_ids = std::mem::take(&mut self.previous_batch_event_ids);
        let supported = self.conn.borrow().tool_call_prune;
        let evicted = if supported {
            self.prune_batch(&groups)?
        } else {
            None
        };
        match evicted {
            // 决策模型裁出了无用批次：标注后由回合收尾整组剔除。
            Some(evicted) if !evicted.is_empty() => {
                self.mark_evicted(&evicted, &groups);
                Ok(())
            }
            // 决策模型可用但一组都没判无用：不再叠一层压缩，本批原样留着。
            Some(_) => Ok(()),
            // 裁决不可用（或无该能力）：回退到非决策模型的逐批压缩。
            None => self.compact_batch(&groups, &event_ids),
        }
    }

    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
        // 本轮 undo 账本：工作区是有 HEAD 的 Git 仓库时才记账（探测只做一次）。
        if let Some(undo) = &self.undo {
            crate::undo::record_calls(&mut undo.borrow_mut(), calls);
        }

        // 执行清单的最新一版：投影规则与 `update_todos` 的工具实现同一份
        // （`project_todos`），续跑判定与 `run_guard_*` 事件载荷都读它。
        if let Some(todos) = &self.todos {
            for call in calls {
                if call.name != TODO_TOOL_NAME {
                    continue;
                }
                let raw = call
                    .arguments
                    .get("todos")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let projected = project_todos(&raw);
                let mut tracked = todos.borrow_mut();
                tracked.clear();
                tracked.extend(projected);
            }
        }

        // 工具事件落盘（Python `_normalize_tool_calls` 的落盘点）：模型每次请求工具都先
        // 落 `tool_call_requested`，参数走公开投影、带上本批 assistant 协议字段。
        // 后台子任务批次不落（`active_assistant` 为 None ≡ `persist_session_events=False`）；
        // 协议原文只在本批有效，读走即清空，免得残留到下一个模型回合。
        let persist_events = self.active_assistant.is_some();
        if let Some(slot) = self.active_assistant.as_ref() {
            let assistant_message = slot.borrow_mut().take();
            let connection = self.conn.borrow();
            if let Some(session) = connection.session.as_ref() {
                let _ = crate::tool_events::record_requested_calls(
                    calls,
                    assistant_message.as_ref(),
                    session.store.as_ref(),
                    &session.session_id,
                );
            }
        }

        // 内核自己持有会话时，「只读当前会话」的工具由内核直接答，不占宿主的批次；
        // 其余工具照旧整批交给宿主执行。
        let mut answers: Vec<Option<AgentLoopObservation>> =
            (0..calls.len()).map(|_| None).collect();
        let mut remote_calls: Vec<ToolCall> = Vec::new();
        let mut remote_slots: Vec<usize> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if let Some(observation) = self.local_tool(call) {
                answers[index] = Some(observation);
                continue;
            }
            remote_calls.push(call.clone());
            remote_slots.push(index);
        }

        if !remote_calls.is_empty() {
            let batch = ToolBatch {
                turn_id: self.turn_id.clone(),
                step: first_step,
                calls: remote_calls,
                workspace_root: self.workspace_root.clone(),
            };
            let params = serde_json::to_value(&batch)
                .expect("tool.batch 负载是serde_json::Value字段，必须可序列化");
            let value = self
                .conn
                .borrow_mut()
                .request(method::TOOL_BATCH, params)
                .map_err(PortFailure::into_tool_error)?;
            let observations = ToolBatchResult::from_result(&value)
                .map(|parsed| parsed.observations)
                .map_err(|error| {
                    LoopError::ToolBatch(format!("宿主返回的观察无法解析：{error}"))
                })?;
            for (slot, observation) in remote_slots.into_iter().zip(observations) {
                answers[slot] = Some(observation);
            }
        }

        let mut ordered: Vec<AgentLoopObservation> = Vec::with_capacity(answers.len());
        for answer in answers {
            match answer {
                Some(observation) => ordered.push(observation),
                None => {
                    return Err(LoopError::ToolBatch(format!(
                        "工具批次缺少第 {} 个调用的观察。",
                        ordered.len() + 1
                    )))
                }
            }
        }

        // 宿主拒绝的调用先落进会话转录：投影与历史据此还原「用户拒绝执行」。
        // 与请求事件同一个开关：子代理的工具调用不落父会话（Python 同规则）。
        if persist_events {
            let conn = self.conn.borrow();
            if let Some(session) = conn.session.as_ref() {
                crate::approval_audit::record_denied_calls(
                    &ordered,
                    session.store.as_ref(),
                    &session.session_id,
                );
            }
        }

        // 超长输出先落盘：模型上下文只留头尾预览与落盘路径（与 Python 的批次预算同口径）。
        {
            let conn = self.conn.borrow();
            let session = conn
                .session
                .as_ref()
                .map(|session| (session.store.as_ref(), session.session_id.as_str()));
            crate::output_budget::apply_batch_output_budget(&mut ordered, session);
        }

        // 图片观察的去向与 Python `route_image_result` 同优先级：原生视觉直送主模型，
        // 否则交给独立视觉模型代理。两条路都走不通时必须把图片摄掉——主模型看不懂图，
        // 图片留在请求里只会被 Provider 拒掉；宿主侧已在未配置代理时不交图，这里是兵底。
        //
        // 原生视觉判定走三态：显式开关优先，未配置（`None`）时按模型自身声明的能力回落，
        // 不能把「未配置」当「关闭」——那会让支持视觉的模型白走一趟代理。
        if crate::vision_proxy::has_vision_observation(&ordered) {
            let native_vision = self
                .vision_model
                .as_ref()
                .map(|model| {
                    omnicrawl_controllers::turn::turn_loop::native_vision_enabled(
                        model.native_vision,
                        omnicrawl_controllers::turn::turn_loop::model_supports_vision(
                            model.vision_capability,
                        ),
                    )
                })
                .unwrap_or(false);
            if !native_vision {
                match self.vision_model.as_ref().and_then(KernelVisionProxy::load) {
                    Some(proxy) => {
                        let cancel_source = Rc::clone(&self.conn);
                        let cancelled = || cancel_source.borrow().cancel.load(Ordering::SeqCst);
                        if !cancelled() {
                            proxy.apply_observations(&mut ordered, &cancelled);
                        }
                    }
                    None => {
                        omnicrawl_controllers::vision_proxy::strip_vision_followups(&mut ordered);
                    }
                }
            }
        }

        // 工具输出压缩改成「回合结束整体概括」：批次内不再逐条压缩，这里原样保留观察，
        // 由回合收尾统一处理（见 `run_session_tail`）。
        // `tool_result` 落盘（Python `_execute_tool_batch` 的落盘点在同一个位置）：写的是
        // **处理过**的观察，事件里的 output / model_output 与回填模型的文本同源；超长输出的
        // artifact 化由会话存储负责。被拒绝的调用也有结果事件（观察本身就是拒绝结果）。
        if persist_events {
            let conn = self.conn.borrow();
            if let Some(session) = conn.session.as_ref() {
                let _ = crate::tool_events::record_results(
                    &ordered,
                    session.store.as_ref(),
                    &session.session_id,
                );
            }
        }
        // 本批执行完就记成「上一批」：下一次 `before_tool_batch` 时它正是刚变老的那批，
        // 送审裁决就在那里发生（本批此刻还是「小登」，从不被淘汰）。
        // 只有落事件的批次参与：子代理批次的调用不进父会话，也不该按父会话的历史淘汰。
        if persist_events {
            // 淘汰范围在这里定：记忆 / 知识库 / `read` / `grep` 永不进送审（`prunable_tool`）。
            // 放在内核而不是宿主，是为了让这些原文根本不离开本进程——送审负载是出网内容，
            // 「判了也大概率保留」的调用没必要先带出去再请宿主说保留。
            let batch: Vec<&AgentLoopObservation> = ordered
                .iter()
                .filter(|observation| prunable_tool(&observation.tool_call.name))
                .collect();
            self.previous_batch = batch.iter().map(|item| prune_candidate_of(item)).collect();
            // 逐批压缩要按事件 ID 精确覆盖这一批：与上面同一口径，否则会把没进概括的
            // 调用事件一起剔除。
            let call_ids: Vec<String> =
                batch.iter().map(|item| item.tool_call.id.clone()).collect();
            self.previous_batch_event_ids = self.batch_event_ids(&call_ids);
        }
        Ok(ordered)
    }
}

impl RemoteTools {
    /// 本批调用对应的事件 ID：从会话事件流里按 `tool_call_id` 过滤。
    ///
    /// 事件写入接口不返回 ID，因此这里回读一次事件流；读失败时返回空集合，逐批压缩
    /// 会因缺少可覆盖事件而跳过（保留原文），不会误删更早批次。
    fn batch_event_ids(&self, call_ids: &[String]) -> Vec<String> {
        if call_ids.is_empty() {
            return Vec::new();
        }
        let conn = self.conn.borrow();
        let Some(session) = conn.session.as_ref() else {
            return Vec::new();
        };
        let events = match session.store.read_active_events(&session.session_id) {
            Ok(events) => events,
            Err(error) => {
                eprintln!(
                    "[kernel] 读取会话事件失败，跳过逐批工具调用压缩：{}",
                    error.message()
                );
                return Vec::new();
            }
        };
        let source: Vec<SourceEvent> = events
            .into_iter()
            .map(|event| SourceEvent {
                event_id: event.event_id,
                event_type: event.event_type,
                payload: Value::Object(event.payload),
            })
            .collect();
        compression_logic::tool_event_ids_for_calls(&source, call_ids)
    }

    /// 决策模型裁决上一批：返回应予淘汰的调用 ID。
    ///
    /// `None` 表示裁决不可用（无渠道、缺凭据、请求失败、响应不可解析），调用方据此回退到
    /// 非决策模型的逐批压缩；`Some(空)` 表示裁决可用但一组都没判无用。
    fn prune_batch(&self, groups: &[PruneCandidate]) -> Result<Option<Vec<String>>, LoopError> {
        let request = ContextPruneRequest {
            task: self.task_hint.clone(),
            groups: groups
                .iter()
                .map(|group| ContextPruneGroup {
                    call_id: group.call_id.clone(),
                    tool: group.tool.clone(),
                    arguments: group.arguments.clone(),
                    ok: group.ok,
                    output: group.output.clone(),
                })
                .collect(),
        };
        let params = serde_json::to_value(&request)
            .expect("context.prune 负载是serde_json::Value字段，必须可序列化");
        let value = self
            .conn
            .borrow_mut()
            .request(method::CONTEXT_PRUNE, params)
            .map_err(PortFailure::into_tool_error)?;
        let result = ContextPruneResult::from_result(&value).map_err(|error| {
            LoopError::ToolBatch(format!("context.prune 响应无法解析：{error}"))
        })?;
        // 宿主侧把「裁决不可用」与「全部保留」都回成空列表：协议上没有区分位，因此空列表
        // 一律按可用处理（不额外压缩），避免在决策渠道正常时又叠一层压缩请求。
        Ok(Some(result.evicted_call_ids))
    }

    /// 标注决策模型判为无用的调用：只落事件与账本，不动本回合上下文。
    ///
    /// 真正的整组剔除留到回合收尾统一做（`run_session_tail`）——那里它先于工具调用压缩与
    /// 上下文压缩；被淘汰的原文因此不会再经由压缩摘要回流。
    fn mark_evicted(&self, evicted: &[String], groups: &[PruneCandidate]) {
        let payload = evicted_event_payload(evicted, groups);
        if let Some(session) = self.conn.borrow().session.as_ref() {
            if let Err(detail) = session.append(EVICTED_EVENT_TYPE, payload) {
                eprintln!("[kernel] 会话写入工具调用淘汰失败：{detail}");
            }
        }
        {
            let conn = self.conn.borrow();
            let mut marks = conn.evict_marks.borrow_mut();
            for call_id in evicted {
                if !marks.contains(call_id) {
                    marks.push(call_id.clone());
                }
            }
        }
        eprintln!(
            "[kernel] 工具调用淘汰标注：{} 组待淘汰（保留 {} 组原文），回合收尾统一处理。",
            evicted.len(),
            groups.len().saturating_sub(evicted.len())
        );
    }

    /// 非决策模型的逐批压缩：把上一批压成一段，落 `tool_call_summary` 标注。
    ///
    /// 与决策路径同构——只处理「刚变老的那一批」，压缩点贴着上下文尾部，最新一批不受影响。
    /// 压缩失败只记日志、保留原文（与淘汰同样 fail-open）。
    fn compact_batch(
        &self,
        groups: &[PruneCandidate],
        event_ids: &[String],
    ) -> Result<(), LoopError> {
        if groups.is_empty() || event_ids.is_empty() {
            return Ok(());
        }
        let model = self.conn.borrow().model.clone();
        let Some(model) = model else {
            return Ok(());
        };
        // 压缩器不可用（未启用、无压缩模型、缺凭据）时静默跳过：逐批压缩是可选项。
        let Some(compressor) = KernelCompressor::load(&model) else {
            return Ok(());
        };
        let calls: Vec<compression_logic::TurnCallRecord> = groups
            .iter()
            .map(|group| compression_logic::TurnCallRecord {
                tool: group.tool.clone(),
                arguments: group.arguments.clone(),
                ok: group.ok,
                output: group.output.clone(),
            })
            .collect();
        // 落盘位置与回合末概括同一份（工作区临时目录），概括文本末尾的路径供模型回读原文。
        let (workspace_root, session_id) = {
            let conn = self.conn.borrow();
            match conn.session.as_ref() {
                Some(session) => (
                    session.workspace.clone().unwrap_or_default(),
                    session.session_id.clone(),
                ),
                None => (String::new(), String::new()),
            }
        };
        match crate::turn_summary::summarize_batch(
            &compressor,
            &calls,
            &self.task_hint,
            &workspace_root,
            &session_id,
        ) {
            Some(outcome) => {
                let payload = json!({
                    "content": outcome.content,
                    "covered_event_ids": event_ids,
                    "archive_path": outcome.archive_path,
                    "tool_call_count": outcome.tool_calls,
                    "raw_chars": outcome.raw_chars,
                    "summary_chars": outcome.summary_chars,
                    "turn_id": self.turn_id,
                });
                if let Some(session) = self.conn.borrow().session.as_ref() {
                    if let Err(detail) = session.append("tool_call_summary", payload) {
                        eprintln!("[kernel] 会话写入逐批工具调用压缩失败：{detail}");
                    }
                }
                // 事件已落盘：按投影重建运行期历史，让**下一批**请求真正拿到概括文本。
                // 少了这一步，压缩只在转录里「看起来」生效，模型仍然收到全文。
                let mut session = self.conn.borrow_mut().session.take();
                if let Some(session) = session.as_mut() {
                    if let Err(detail) = session.reload_history() {
                        eprintln!("[kernel] 逐批工具调用压缩后重建运行期历史失败：{detail}");
                    }
                }
                self.conn.borrow_mut().session = session;
                // 会话区计量不在批次中途落：逐批压缩发生在回合运行中，提示会一行行抖在
                // 工具卡后面。这里只累加净省，回合收尾（`run_session_tail`）与淘汰合并成
                // 一行「已压缩：N Token」；回放仍按事件的字符数重建同一个数字。
                self.conn.borrow_mut().tool_compaction_saved +=
                    compression_logic::saved_tokens(outcome.raw_chars, outcome.summary_chars);
                self.conn.borrow_mut().tool_compaction_batches += 1;
                eprintln!(
                    "[kernel] 逐批工具调用压缩标注：{} 次调用（{} → {} 字符），回合收尾统一处理。",
                    outcome.tool_calls, outcome.raw_chars, outcome.summary_chars
                );
            }
            None => {
                // 压缩失败保留原文：这里不报错，避免把可选的省上下文手段升级成回合失败。
                eprintln!("[kernel] 逐批工具调用压缩不可用，保留原文。");
            }
        }
        Ok(())
    }

    /// 内核侧工具：命中即本地作答；返回 None 表示交给宿主。
    fn local_tool(&self, call: &ToolCall) -> Option<AgentLoopObservation> {
        if call.name == crate::computer_use::COMPUTER_USE_TOOL_NAME {
            let (ok, output) = run_computer_use(&self.conn, &call.arguments, &call.id);
            return Some(self.local_observation(call, ok, &output));
        }
        if is_subagent_tool(&call.name) {
            let (ok, output) =
                run_subagent_tool(&call.name, &self.conn, &call.arguments, &call.id);
            return Some(self.local_observation(call, ok, &output));
        }
        if call.name != RECALL_SESSION_EVIDENCE_TOOL_NAME {
            return None;
        }
        let (ok, output) = {
            let conn = self.conn.borrow();
            conn.session.as_ref()?;
            let arguments = Value::Object(call.arguments.clone());
            recall_session_evidence(conn.session.as_ref(), &arguments)
        };
        Some(self.local_observation(call, ok, &output))
    }

    /// 内核自持工具的观察结果，连同工具生命周期事件一起交出去。
    ///
    /// 宿主只从 `tool.started` / `tool.finished` 收口工具卡：内核自己办掉的调用若只返回
    /// 观察，卡片会一直停在「调用中」，直到回合结束被强制标成「未收到结果」。
    fn local_observation(&self, call: &ToolCall, ok: bool, output: &str) -> AgentLoopObservation {
        let result = ToolResult {
            ok,
            output: output.to_string(),
            full_output: output.to_string(),
            error_code: None,
            retryable: false,
        };
        let mut connection = self.conn.borrow_mut();
        connection.notify(HostEvent::ToolStarted(ToolStartedPayload {
            step: 1,
            call: call.clone(),
        }));
        connection.notify(HostEvent::ToolFinished(ToolEventPayload {
            call: call.clone(),
            result: result.clone(),
        }));
        AgentLoopObservation {
            tool_call: call.clone(),
            result,
            message: tool_result_message(&call.name, ok, output, &call.id),
            followup_messages: Vec::new(),
        }
    }
}

mod background;
mod subagent_tools;

pub(crate) use background::BackgroundModelPort;
pub(crate) use background::BackgroundTools;
pub(crate) use background::run_background_task;
pub(crate) use subagent_tools::emit_batch_finished;
pub(crate) use subagent_tools::next_subagent_batch_id;
pub(crate) use subagent_tools::run_computer_use;
pub(crate) use subagent_tools::run_subagent_tool;
pub(crate) use subagent_tools::run_task_in_isolation;
pub(crate) use subagent_tools::subagent_failure_code;
pub(crate) use subagent_tools::subagent_query_value;

