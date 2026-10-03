//! 宿主侧工具批次：审批/提问判定、执行派发与观察构造。
//!
//! 批次是不可分割的边界：先按模型调用顺序把每个调用定调（自持工具就地执行、敏感工具
//! 逐个确认、提问工具等用户作答），审批全部完成后把待执行调用一次性交给执行层并发执行，
//! 最后按模型顺序回观察——顺序与数量都不能变（内核会校验）。

use omnicrawl_controllers::approval::{
    decide, git_action_tier, is_git_tool_call, is_shell_command_tool_call, user_cancelled_reason,
    ApprovalDecision, GIT_TIER_READONLY,
};
use omnicrawl_controllers::output::vision_observation_messages;
use omnicrawl_controllers::shared::tool_timeout_result;
use omnicrawl_controllers::types::ToolImageAttachment;
use omnicrawl_core::{AgentLoopObservation, ToolCall, ToolResult};
use omnicrawl_ipc::Id;
use serde_json::{json, Map, Value};

use crate::approval::ApprovalMode;
use crate::tools::ToolRegistry;

pub const TODO_TOOL: &str = "update_todos";
pub const ASK_USER_TOOL: &str = "ask_user";
pub const PAUSE_WORK_TOOL: &str = "pause_work";

/// 工具没有可用执行器时的错误码，与内核连接器模式同名。
pub const TOOL_UNAVAILABLE: &str = "tool_unavailable";
pub const DENIED: &str = omnicrawl_ipc::DENIED_ERROR_CODE;

/// 审批判定需要的工具事实来源（工具说明 + 参数 schema 文本）。
///
/// 删除意图判定要用到工具说明与参数 schema（`is_delete_behavior_tool_call`），
/// 而批次本身只拿到调用名与参数，因此由调用方（宿主 / TUI）把工具表的事实注进来。
pub trait ToolFacts: Send + Sync {
    fn facts(&self, tool: &str) -> Option<(String, String)>;
}

/// 调用方上下文：审批模式与需要就地更新的宿主状态。
pub struct BatchContext<'a> {
    pub approval: ApprovalMode,
    pub todos: &'a mut Vec<TodoItem>,
    pub paused: &'a mut bool,
    /// 工具事实：审批判定（尤其是 `review` 模式的删除意图识别）要用；没有时按空串处理。
    pub tools: Option<&'a dyn ToolFacts>,
}

/// 一次推进的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum BatchStep {
    /// 需要用户决定，面板在 [`PendingBatch::waiting`]。
    Awaiting,
    /// 审批已全部完成：并发执行这些调用（批次内下标 → 调用）。
    Execute(Vec<(usize, ToolCall)>),
    /// 整批结果齐备，可以回观察。
    Complete,
}

/// 界面要展示的待决交互；由批次自己持有，避免状态在两处各存一份。
#[derive(Debug, Clone, PartialEq)]
pub enum Waiting {
    Question(QuestionPanel),
    Approval(ApprovalPanel),
}

/// `ask_user` 面板末尾统一追加的自定义作答入口：选中后解锁输入框自己写答案。
pub const QUESTION_CUSTOM_LABEL: &str = "我有自己的想法...";

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionPanel {
    pub prompt: String,
    pub options: Vec<String>,
    pub selected: usize,
}

impl QuestionPanel {
    /// 有选项时是单选列表，否则由输入框补答案。
    pub fn is_select(&self) -> bool {
        !self.options.is_empty()
    }

    /// 可选行数：模型给的选项 + 末尾的自定义作答入口。
    pub fn option_count(&self) -> usize {
        if self.options.is_empty() {
            0
        } else {
            self.options.len() + 1
        }
    }

    /// 自定义作答入口的行号（没有选项时为 `None`）。
    pub fn custom_row(&self) -> Option<usize> {
        self.is_select().then(|| self.options.len())
    }

    /// 选中项是不是自定义作答入口。
    pub fn is_custom(&self) -> bool {
        self.custom_row() == Some(self.selected)
    }

    /// 选中行的显示文案（自定义入口给出固定文案）。
    pub fn selected_label(&self) -> &str {
        if self.is_custom() {
            QUESTION_CUSTOM_LABEL
        } else {
            self.options.get(self.selected).map(String::as_str).unwrap_or("")
        }
    }

    pub fn answer(&self) -> String {
        match self.options.get(self.selected) {
            Some(option) => option.clone(),
            None => String::new(),
        }
    }

    /// 上下键移动选择，循环（含末尾的自定义作答入口）。
    pub fn select(&mut self, delta: isize) {
        let count = self.option_count();
        if count == 0 {
            return;
        }
        self.selected = ((self.selected as isize + delta).rem_euclid(count as isize)) as usize;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalPanel {
    pub tool: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TodoItem {
    pub id: String,
    pub step: String,
    pub completed: bool,
}

/// 一个待回复的 `tool.batch` 请求。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingBatch {
    request_id: Id,
    calls: Vec<ToolCall>,
    /// 按模型顺序排列的定调结果；`None` 表示已批准、待执行。
    results: Vec<Option<ToolResult>>,
    /// 与 `results` 平行的视觉附件（`read_image` 这类工具才有）。
    vision: Vec<Option<VisionPayload>>,
    cursor: usize,
    waiting: Option<Waiting>,
    planned: bool,
}

/// 随工具结果注入下一条观察的内容：分析提示词与图片附件。
#[derive(Debug, Clone, PartialEq)]
pub struct VisionPayload {
    pub prompt: String,
    pub images: Vec<ToolImageAttachment>,
}

impl PendingBatch {
    pub fn new(request_id: Id, calls: Vec<ToolCall>) -> Self {
        let results = vec![None; calls.len()];
        let vision = vec![None; calls.len()];
        Self {
            request_id,
            calls,
            results,
            vision,
            cursor: 0,
            waiting: None,
            planned: false,
        }
    }

    pub fn request_id(&self) -> &Id {
        &self.request_id
    }

    pub fn waiting(&self) -> Option<&Waiting> {
        self.waiting.as_ref()
    }

    pub fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    /// 当前处理到的调用：审批或提问面板都属于它，下标供事件发射使用。
    pub fn current(&self) -> Option<(usize, &ToolCall)> {
        self.calls.get(self.cursor).map(|call| (self.cursor, call))
    }

    /// 已定调的调用结果；`None` 表示这个调用还没有结果（等执行或等用户）。
    pub fn results(&self) -> &[Option<ToolResult>] {
        &self.results
    }

    /// 上下键移动待决提问的选择；没有待决提问时无操作。
    pub fn select_question(&mut self, delta: isize) {
        if let Some(Waiting::Question(panel)) = self.waiting.as_mut() {
            panel.select(delta);
        }
    }

    /// 推进到下一个等待点、执行点或整批结束。
    pub fn advance(&mut self, ctx: &mut BatchContext<'_>) -> BatchStep {
        while let Some(call) = self.calls.get(self.cursor).cloned() {
            match call.name.as_str() {
                TODO_TOOL => {
                    *ctx.todos = parse_todos(&call.arguments);
                    let summary = format!("已更新任务清单（{} 项）。", ctx.todos.len());
                    self.results[self.cursor] = Some(text_result(summary));
                    self.cursor += 1;
                }
                PAUSE_WORK_TOOL => {
                    *ctx.paused = true;
                    self.results[self.cursor] = Some(text_result(
                        "已请求暂停：本回合结束后不再自动继续。".to_string(),
                    ));
                    self.cursor += 1;
                }
                ASK_USER_TOOL => {
                    self.waiting = Some(Waiting::Question(question_from(&call)));
                    return BatchStep::Awaiting;
                }
                _ if needs_confirmation(ctx, &call) => {
                    self.waiting = Some(Waiting::Approval(ApprovalPanel {
                        tool: call.name.clone(),
                        summary: summarize_arguments(&call.arguments),
                    }));
                    return BatchStep::Awaiting;
                }
                _ => self.cursor += 1,
            }
        }
        self.waiting = None;
        self.plan()
    }

    /// 用户回答了提问；没有待决提问时返回 `None`。
    pub fn answer(&mut self, answer: String, ctx: &mut BatchContext<'_>) -> Option<BatchStep> {
        self.calls.get(self.cursor)?;
        if !matches!(self.waiting, Some(Waiting::Question(_))) {
            return None;
        }
        let empty = answer.trim().is_empty();
        let output = if empty {
            "提问没有取得答案（用户未作答）。".to_string()
        } else {
            answer
        };
        self.results[self.cursor] = Some(ToolResult {
            ok: !empty,
            full_output: output.clone(),
            output,
            error_code: empty.then_some("ask_user_no_answer".to_string()),
            retryable: false,
        });
        self.cursor += 1;
        self.waiting = None;
        Some(self.advance(ctx))
    }

    /// 用户对审批作出决定；没有待决审批时返回 `None`。
    pub fn decide(&mut self, approved: bool, ctx: &mut BatchContext<'_>) -> Option<BatchStep> {
        if self.calls.get(self.cursor).is_none()
            || !matches!(self.waiting, Some(Waiting::Approval(_)))
        {
            return None;
        }
        if !approved {
            let tool = self.calls[self.cursor].name.clone();
            self.results[self.cursor] = Some(denied_result(&tool));
        }
        self.cursor += 1;
        self.waiting = None;
        Some(self.advance(ctx))
    }

    /// 当前推进位置（下一个待决定或待执行的调用下标）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 插件改写调用参数（`tool.call.before` 的 transform 结局）：索引按批次调用顺序编号。
    ///
    /// 返回是否命中；未命中（越界）时静默忽略，分发本身不做假设。
    pub fn rewrite_arguments(&mut self, index: usize, arguments: Map<String, Value>) -> bool {
        match self.calls.get_mut(index) {
            Some(call) => {
                call.arguments = arguments;
                true
            }
            None => false,
        }
    }

    /// 执行层回填一个调用的结果；返回是否整批就绪。
    pub fn record_result(
        &mut self,
        index: usize,
        result: ToolResult,
        vision: Option<VisionPayload>,
    ) -> bool {
        if let Some(slot) = self.results.get_mut(index) {
            *slot = Some(result);
        }
        if let Some(slot) = self.vision.get_mut(index) {
            *slot = vision;
        }
        self.results.iter().all(Option::is_some)
    }

    /// 仍未回填结果的调用（执行层正在跑，或因超时被丢弃）。
    pub fn pending_jobs(&self) -> Vec<(usize, ToolCall)> {
        self.calls
            .iter()
            .enumerate()
            .filter(|(index, _)| self.results[*index].is_none())
            .map(|(index, call)| (index, call.clone()))
            .collect()
    }

    /// 执行超时：把仍未回填的调用写成超时结果（与 Python 的批次绝对截止时间语义一致，
    /// 后台线程继续运行但结果被丢弃）；返回被写成超时的调用下标。
    pub fn fill_timeout(&mut self, timeout_seconds: i64) -> Vec<(usize, ToolCall)> {
        let pending = self.pending_jobs();
        for (index, _) in &pending {
            let result = tool_timeout_result(timeout_seconds, "");
            self.results[*index] = Some(ToolResult {
                ok: result.ok,
                output: result.output,
                full_output: result.full_output,
                error_code: result.error_code,
                retryable: result.retryable,
            });
        }
        pending
    }

    /// 整批是否已有结果（执行层回填完毕）。
    pub fn is_ready(&self) -> bool {
        !self.calls.is_empty() && self.results.iter().all(Option::is_some)
    }

    /// 按模型调用顺序产出观察（整批就绪后调用）。
    ///
    /// `attach_images` 为真时，带图片的调用会把图片作为下一条观察注入（与 Python 的
    /// `vision_observation_messages` 同义）；为假时不注入，图片不会进入模型请求。
    pub fn observations(&self, attach_images: bool) -> Vec<AgentLoopObservation> {
        self.calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let result = self.results[index]
                    .clone()
                    .unwrap_or_else(|| unavailable_result(&call.name));
                let vision = self.vision.get(index).and_then(Option::as_ref);
                observation_from_result(call, result, vision, attach_images)
            })
            .collect()
    }

    fn plan(&mut self) -> BatchStep {
        let to_run: Vec<(usize, ToolCall)> = self
            .calls
            .iter()
            .enumerate()
            .filter(|(index, _)| self.results[*index].is_none())
            .map(|(index, call)| (index, call.clone()))
            .collect();
        if to_run.is_empty() {
            BatchStep::Complete
        } else if self.planned {
            // 已经派发过执行：等执行层回填，不再重复派发。
            BatchStep::Awaiting
        } else {
            self.planned = true;
            BatchStep::Execute(to_run)
        }
    }
}

/// 该调用的审批归属（与 Python `_approve_tool_call` 同源，判定在 `omnicrawl-controllers`）。
///
/// - `Approve`：直接执行；
/// - `Confirm`：弹面板等用户决定（批次停在 `Waiting::Approval`）；
/// - `Review`：交审查模型，由宿主的审查步骤处理（[`crate::review`]），批次不在此停等。
pub fn approval_decision(ctx: &BatchContext<'_>, call: &ToolCall) -> ApprovalDecision {
    if is_self_hosted(&call.name) {
        return ApprovalDecision::Approve;
    }
    let (description, schema) = match ctx.tools.and_then(|facts| facts.facts(&call.name)) {
        Some((description, schema)) => (description, schema),
        None => (String::new(), String::new()),
    };
    decide(
        &call.name,
        &description,
        &schema,
        &call.arguments,
        ctx.approval.decision_mode(),
    )
}

/// 是否需要人工确认：只有 `Confirm` 才停等用户（`review` 不弹面板）。
pub fn needs_confirmation(ctx: &BatchContext<'_>, call: &ToolCall) -> bool {
    matches!(approval_decision(ctx, call), ApprovalDecision::Confirm)
}

/// 旧签名的手工确认判定：等价于 `decide` 的 `Confirm`（无工具事实）。
pub fn approval_required(mode: ApprovalMode, tool: &str, arguments: &Map<String, Value>) -> bool {
    if is_self_hosted(tool) {
        return false;
    }
    if is_git_tool_call(tool) {
        // `git` 在 review 模式下走审查模型，这里只回答「要不要问人」。
        if mode == ApprovalMode::Review {
            return false;
        }
        return git_action_tier(arguments) != GIT_TIER_READONLY;
    }
    if mode == ApprovalMode::Auto {
        return false;
    }
    if mode == ApprovalMode::Review {
        return false;
    }
    is_shell_command_tool_call(tool)
}

pub fn is_self_hosted(tool: &str) -> bool {
    matches!(tool, TODO_TOOL | ASK_USER_TOOL | PAUSE_WORK_TOOL)
}

/// 工具结果 → 循环观察：`message` 是回填给模型的原始 tool 消息。
///
/// 视觉附件不进 `message`（工具消息只有文本），而是作为**下一条**观察消息追加，与 Python
/// 的 `vision_observation_messages` 一致：图片是临时观察，不进入会话转录。
pub fn observation_from_result(
    call: &ToolCall,
    result: ToolResult,
    vision: Option<&VisionPayload>,
    attach_images: bool,
) -> AgentLoopObservation {
    let content = result.output.clone();
    let followup_messages = match vision {
        Some(payload) => {
            vision_observation_messages(result.ok, attach_images, &payload.prompt, &payload.images)
        }
        None => Vec::new(),
    };
    AgentLoopObservation {
        tool_call: call.clone(),
        result,
        message: json!({
            "role": "tool",
            "tool_call_id": call.id.clone(),
            "content": content,
        }),
        followup_messages,
    }
}

/// 本宿主没有这个工具的执行器时的结果。
pub fn unavailable_result(tool: &str) -> ToolResult {
    ToolResult {
        ok: false,
        output: format!("工具 {tool} 在当前宿主没有可用执行器。"),
        full_output: format!("工具 {tool} 在当前宿主没有可用执行器。"),
        error_code: Some(TOOL_UNAVAILABLE.to_string()),
        retryable: false,
    }
}

/// 执行线程内部 panic 时的兜底结果：工具没有产出可用结果，但回合必须继续推进，
/// 不能让内核永远等一个已经消失的线程。
pub fn panicked_result(tool: &str) -> ToolResult {
    let output =
        format!("工具 {tool} 执行异常（宿主内部错误），没有可用结果；请改用其它方式继续。");
    ToolResult {
        ok: false,
        full_output: output.clone(),
        output,
        error_code: Some("tool_panicked".to_string()),
        retryable: false,
    }
}

pub fn denied_result(tool: &str) -> ToolResult {
    denied_with_reason(&user_cancelled_reason(tool))
}

/// 审批（人工确认或审查模型）拒绝一次调用时落一条 MCP 审计。
///
/// 对映 Python `_approve_tool_call` 里的
/// `if mcp_manager is not None and tool.name in mcp_manager.registry.tools:
/// mcp_manager.record_denied_tool_call(tool.name, arguments, reason)`。
///
/// 粒度刻意与 Python 对齐：只在「未批准」这条路径上调用——被插件在
/// `tool.call.before` / `approval.before` / `execute.before` 挡下的调用在 Python 里走
/// 提前返回，不写这条审计；非 MCP 工具与未配置 MCP 时是空操作。
/// `reason` 用与拒绝结果同一句文案，审计记录与模型看到的观察因此一致。
pub fn record_mcp_denial(
    registry: &ToolRegistry,
    tool: &str,
    arguments: &Map<String, Value>,
    reason: &str,
) {
    let Some(manager) = registry.mcp() else {
        return;
    };
    if !registry.is_mcp_tool(tool) {
        return;
    }
    manager.record_denied_tool_call(tool, arguments, reason);
}

/// 指定原因的拒绝结果：插件守卫在 `tool.call.before` / `approval.before` /
/// `execute.before` 挡下调用时用它，文案由调用方给出（与 Python 的三条分支一致）。
pub fn denied_with_reason(reason: &str) -> ToolResult {
    ToolResult {
        ok: false,
        full_output: reason.to_string(),
        output: reason.to_string(),
        error_code: Some(DENIED.to_string()),
        retryable: false,
    }
}

fn text_result(output: String) -> ToolResult {
    ToolResult {
        ok: true,
        full_output: output.clone(),
        output,
        error_code: None,
        retryable: false,
    }
}

/// 把 `update_todos` 的参数解成清单；缺字段回落为空值，非法项跳过。
pub fn parse_todos(arguments: &Map<String, Value>) -> Vec<TodoItem> {
    let Some(items) = arguments.get("todos").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let step = object.get("step").and_then(Value::as_str)?.to_string();
            Some(TodoItem {
                id: object
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                step,
                completed: object
                    .get("completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// `ask_user` 的参数 → 提问面板；没有选项时由输入框补答案。
pub fn question_from(call: &ToolCall) -> QuestionPanel {
    let prompt = call
        .arguments
        .get("question")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "模型请求补充信息，但没有给出问题正文。".to_string());
    let options = call
        .arguments
        .get("options")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|text| text.to_string())
                .collect()
        })
        .unwrap_or_default();
    QuestionPanel {
        prompt,
        options,
        selected: 0,
    }
}

/// 参数的单行摘要：整块 JSON 压成一行并限长，供审批面板展示。
pub fn summarize_arguments(arguments: &Map<String, Value>) -> String {
    const LIMIT: usize = 160;
    let text = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string());
    if text.chars().count() <= LIMIT {
        return text;
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            id: format!("{name}-1"),
            function_name: name.to_string(),
        }
    }

    fn context<'a>(
        approval: ApprovalMode,
        todos: &'a mut Vec<TodoItem>,
        paused: &'a mut bool,
    ) -> BatchContext<'a> {
        BatchContext {
            approval,
            todos,
            paused,
            tools: None,
        }
    }

    #[test]
    fn self_hosted_tools_run_without_approval() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Manual, &mut todos, &mut paused);
        let mut batch = PendingBatch::new(
            Id::Number(1),
            vec![
                call(
                    TODO_TOOL,
                    json!({"todos": [
                        {"id": "a", "step": "写骨架", "completed": true},
                        {"id": "b", "step": "接审批", "completed": false},
                        {"not_a_step": 1}
                    ]}),
                ),
                call(PAUSE_WORK_TOOL, json!({})),
            ],
        );

        assert_eq!(batch.advance(&mut ctx), BatchStep::Complete);
        assert_eq!(todos.len(), 2, "非法项应被跳过");
        assert!(todos[0].completed);
        assert!(paused);

        let observations = batch.observations(false);
        assert_eq!(observations.len(), 2);
        assert!(observations[0].result.ok);
        assert!(observations[0].result.output.contains("2 项"));
        assert_eq!(observations[1].tool_call.name, PAUSE_WORK_TOOL);
    }

    #[test]
    fn manual_mode_only_confirms_shell_and_git_writes() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Manual, &mut todos, &mut paused);

        // 文件与搜索类工具在 manual 下直接放行（与 Python 一致）。
        for quiet in [
            call("read", json!({"path": "a.py"})),
            call("write_file", json!({"path": "a.py", "content": "x"})),
            call("list", json!({"path": "."})),
            call("grep", json!({"pattern": "x"})),
            call("git", json!({"action": "status"})),
        ] {
            let mut batch = PendingBatch::new(Id::Number(20), vec![quiet.clone()]);
            assert_eq!(
                batch.advance(&mut ctx),
                BatchStep::Execute(vec![(0, quiet.clone())]),
                "{} 不该要求人工确认",
                quiet.name
            );
        }

        // 非只读 git 操作要确认。
        let mut batch = PendingBatch::new(
            Id::Number(21),
            vec![call("git", json!({"action": "commit", "message": "x"}))],
        );
        assert_eq!(batch.advance(&mut ctx), BatchStep::Awaiting);
    }

    #[test]
    fn manual_mode_approves_then_executes_and_denial_is_reported() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Manual, &mut todos, &mut paused);
        let mut batch = PendingBatch::new(
            Id::Number(2),
            vec![call("bash", json!({"command": "echo hi"}))],
        );

        assert_eq!(batch.advance(&mut ctx), BatchStep::Awaiting);
        match batch.waiting() {
            Some(Waiting::Approval(panel)) => {
                assert_eq!(panel.tool, "bash");
                assert!(panel.summary.contains("echo hi"));
            }
            other => panic!("应当停在审批面板，实际：{other:?}"),
        }

        // 批准：进入执行阶段。
        let step = batch.decide(true, &mut ctx).expect("有待决审批");
        match step {
            BatchStep::Execute(jobs) => {
                assert_eq!(jobs.len(), 1);
                assert_eq!(jobs[0].1.name, "bash");
                assert!(batch.record_result(jobs[0].0, text_result("文件内容".to_string()), None));
            }
            other => panic!("批准后应派发执行，实际：{other:?}"),
        }
        let observations = batch.observations(false);
        assert_eq!(observations[0].result.output, "文件内容");
        assert!(observations[0].result.ok);

        // 拒绝：整批不再需要执行。
        let mut batch = PendingBatch::new(
            Id::Number(3),
            vec![call("bash", json!({"command": "rm -rf /"}))],
        );
        assert_eq!(batch.advance(&mut ctx), BatchStep::Awaiting);
        assert_eq!(batch.decide(false, &mut ctx), Some(BatchStep::Complete));
        let observations = batch.observations(false);
        assert_eq!(observations[0].result.error_code.as_deref(), Some(DENIED));
    }

    #[test]
    fn auto_mode_executes_without_approval() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Auto, &mut todos, &mut paused);
        let mut batch = PendingBatch::new(
            Id::Number(4),
            vec![
                call("bash", json!({"command": "pytest -q"})),
                call("read", json!({"path": "a.py"})),
            ],
        );

        match batch.advance(&mut ctx) {
            BatchStep::Execute(jobs) => {
                assert_eq!(jobs.len(), 2, "整批并发执行，顺序保持模型调用顺序");
                assert_eq!(jobs[0].1.name, "bash");
                assert_eq!(jobs[1].1.name, "read");
                assert!(!batch.record_result(jobs[0].0, text_result("done".to_string()), None));
                assert!(batch.record_result(jobs[1].0, text_result("ok".to_string()), None));
            }
            other => panic!("auto 模式应直接派发执行，实际：{other:?}"),
        }
        let observations = batch.observations(false);
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].result.output, "done");
        assert_eq!(observations[1].result.output, "ok");
    }

    #[test]
    fn ask_user_waits_then_continues_to_execution() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Auto, &mut todos, &mut paused);
        let mut batch = PendingBatch::new(
            Id::Number(5),
            vec![
                call(
                    ASK_USER_TOOL,
                    json!({"question": "选哪个方案？", "kind": "select", "options": ["A", "B"]}),
                ),
                call("read", json!({"path": "a.py"})),
            ],
        );

        assert_eq!(batch.advance(&mut ctx), BatchStep::Awaiting);
        let question = match batch.waiting() {
            Some(Waiting::Question(panel)) => panel.clone(),
            other => panic!("应当停在提问面板，实际：{other:?}"),
        };
        assert!(question.is_select());
        assert_eq!(question.prompt, "选哪个方案？");

        match batch.answer("B".to_string(), &mut ctx).expect("有待决提问") {
            BatchStep::Execute(jobs) => {
                assert_eq!(jobs.len(), 1);
                assert_eq!(jobs[0].1.name, "read");
                batch.record_result(jobs[0].0, text_result("内容".to_string()), None);
            }
            other => panic!("回答后应派发剩余执行，实际：{other:?}"),
        }
        let observations = batch.observations(false);
        assert_eq!(observations.len(), 2);
        assert!(observations[0].result.output.contains('B'));
        assert_eq!(observations[1].result.output, "内容");
    }

    #[test]
    fn empty_answer_is_reported_as_no_answer() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Auto, &mut todos, &mut paused);
        let mut batch = PendingBatch::new(
            Id::Number(6),
            vec![call(ASK_USER_TOOL, json!({"question": "还有别的吗？"}))],
        );
        batch.advance(&mut ctx);
        match batch.answer("   ".to_string(), &mut ctx) {
            Some(BatchStep::Complete) => {}
            other => panic!("空答案应当立刻收尾，实际：{other:?}"),
        }
        let observations = batch.observations(false);
        assert!(!observations[0].result.ok);
        assert_eq!(
            observations[0].result.error_code.as_deref(),
            Some("ask_user_no_answer")
        );
    }

    #[test]
    fn panic_fallback_keeps_turn_alive() {
        let result = panicked_result("read");
        assert!(!result.ok);
        assert_eq!(result.error_code.as_deref(), Some("tool_panicked"));
        assert!(result.output.contains("read"), "{}", result.output);
        assert!(!result.retryable, "重试大概率再次 panic，标记为不可重试");
    }

    #[test]
    fn timeout_fills_only_pending_calls() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Auto, &mut todos, &mut paused);
        let mut batch = PendingBatch::new(
            Id::Number(7),
            vec![
                call("read", json!({"path": "a.py"})),
                call("bash", json!({"command": "sleep 5"})),
            ],
        );
        let jobs = match batch.advance(&mut ctx) {
            BatchStep::Execute(jobs) => jobs,
            other => panic!("应派发执行，实际：{other:?}"),
        };
        assert_eq!(batch.pending_jobs().len(), 2, "两个调用都还没结果");
        assert!(
            !batch.record_result(jobs[0].0, text_result("已有结果".to_string()), None),
            "还有一个调用没回填，整批不该就绪"
        );

        let timed_out = batch.fill_timeout(600);
        let timed_out_index: Vec<usize> = timed_out.iter().map(|(index, _)| *index).collect();
        assert_eq!(timed_out_index, vec![jobs[1].0], "只回填还没结果的调用");
        assert_eq!(timed_out[0].1.name, "bash");
        assert!(batch.pending_jobs().is_empty());
        assert!(batch.is_ready());

        let observations = batch.observations(false);
        assert_eq!(
            observations[0].result.output, "已有结果",
            "已有结果不被覆盖"
        );
        assert!(!observations[1].result.ok);
        assert!(
            observations[1].result.output.contains("工具执行超时"),
            "{:?}",
            observations[1]
        );
        assert!(
            observations[1].result.output.contains("600"),
            "{:?}",
            observations[1]
        );
    }

    #[test]
    fn selection_wraps_and_free_text_has_no_options() {
        let mut panel = QuestionPanel {
            prompt: "p".to_string(),
            options: vec!["A".to_string(), "B".to_string()],
            selected: 0,
        };
        panel.select(1);
        assert_eq!(panel.selected, 1);
        assert_eq!(panel.answer(), "B");
        // 第三行是自定义作答入口：选中它时 answer() 不再是选项原文。
        panel.select(1);
        assert_eq!(panel.selected, 2);
        assert!(panel.is_custom());
        assert_eq!(panel.selected_label(), QUESTION_CUSTOM_LABEL);
        assert_eq!(panel.answer(), "", "自定义入口没有预置答案");
        panel.select(1);
        assert_eq!(panel.selected, 0, "循环回第一个选项");

        let free = question_from(&call(ASK_USER_TOOL, json!({"question": "写点什么"})));
        assert!(!free.is_select());
        assert!(free.answer().is_empty());
        assert_eq!(free.option_count(), 0, "没有选项时也没有自定义入口");
    }

    #[test]
    fn question_without_body_falls_back_to_notice() {
        let panel = question_from(&call(ASK_USER_TOOL, json!({"options": ["A"]})));
        assert!(panel.prompt.contains("没有给出问题正文"));
    }

    #[test]
    fn vision_attachments_are_injected_only_when_enabled() {
        let mut todos = Vec::new();
        let mut paused = false;
        let mut ctx = context(ApprovalMode::Auto, &mut todos, &mut paused);
        let mut batch = PendingBatch::new(
            Id::Number(9),
            vec![call(
                "read_image",
                json!({"path": "p.png", "prompt": "描述这张图"}),
            )],
        );
        let jobs = match batch.advance(&mut ctx) {
            BatchStep::Execute(jobs) => jobs,
            other => panic!("auto 模式应派发执行，实际：{other:?}"),
        };
        let payload = VisionPayload {
            prompt: "描述这张图".to_string(),
            images: vec![ToolImageAttachment {
                media_type: "image/png".to_string(),
                data_base64: "AAAA".to_string(),
                filename: "p.png".to_string(),
                detail: "auto".to_string(),
            }],
        };
        batch.record_result(
            jobs[0].0,
            text_result("{\"vision_attachment\":true}".to_string()),
            Some(payload),
        );

        // 未开启原生视觉：图片不注入，模型只看到工具文本。
        let without = batch.observations(false);
        assert!(without[0].followup_messages.is_empty());

        let with = batch.observations(true);
        assert_eq!(with[0].followup_messages.len(), 1);
        let message = &with[0].followup_messages[0];
        assert_eq!(message["role"], "user");
        assert_eq!(message["content"][0]["text"], "描述这张图");
        assert_eq!(message["content"][1]["type"], "image_url");
        let url = message["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap_or_default();
        assert!(url.starts_with("data:image/png;base64,AAAA"), "{url}");
        assert_eq!(message["content"][1]["image_url"]["detail"], "auto");
    }
}
