//! 由 `session.rs` 按职责拆出，语义未变。

use super::*;



use super::tools::{execute_tasks, next_subagent_batch_id, subagent_failure_code};
use super::turn::{Conn};

/// `subagent.run`：派生单个子 Agent 并等它跑完，回子 Agent 的**原始输出文本**。
///
/// 与 `subagent_run` 工具同一条执行路径（定义发现、工具白名单、worktree 隔离都一致），
/// 区别只在于：本方法是宿主显式请求的单个任务，结果不回公开摘要，而是把子 Agent 的
/// 收尾文本原样交给宿主（`/review` 要自己解析 JSON 报告）。
pub(super) fn respond_subagent_run(conn: &Rc<RefCell<Conn>>, id: Id, params: &SubagentRunParams) {
    let agent_type = params.agent_type.trim();
    if agent_type.is_empty() {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_PARAMS, "subagent.run 需要 agent_type。"),
        );
        return;
    }
    if params.prompt.trim().is_empty() {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_PARAMS, "subagent.run 需要 prompt。"),
        );
        return;
    }
    match run_subagent_request(conn, agent_type, params.description.trim(), &params.prompt) {
        Ok((output, task)) => {
            conn.borrow_mut().respond(
                id,
                json!({
                    "agent_type": agent_type,
                    "output": output,
                    "task": task,
                }),
            );
        }
        Err((code, message)) => {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::with_data(
                    error_code::INVALID_REQUEST,
                    message,
                    json!({ "kind": code }),
                ),
            );
        }
    }
}

/// 派生单个子 Agent 并等它结束：返回（子 Agent 收尾文本，公开结果）。
///
/// 宿主入口始终等待完整报告，等待期间继续转发子任务工具与控制面查询。
pub(super) fn run_subagent_request(
    conn: &Rc<RefCell<Conn>>,
    agent_type: &str,
    description: &str,
    prompt: &str,
) -> Result<(String, Value), (String, String)> {
    let runtime =
        SubAgentRuntime::load().map_err(|detail| ("SUBAGENT_DISABLED".to_string(), detail))?;
    if !runtime.enabled() {
        return Err((
            "SUBAGENT_DISABLED".to_string(),
            "SubAgent 功能未启用。请在配置中显式设置 subagents.enabled=true。".to_string(),
        ));
    }
    let (model_config, fork_messages) = {
        let borrowed = conn.borrow();
        let Some(model) = borrowed.model.clone() else {
            return Err((
                "SUBAGENT_MODEL_ERROR".to_string(),
                "内核未持有模型配置，无法执行子任务。".to_string(),
            ));
        };
        let history = borrowed
            .session
            .as_ref()
            .map(|session| session.history.clone())
            .unwrap_or_default();
        (model, history)
    };
    let parent_tools = SubAgentRuntime::parent_tool_names(&model_config);
    let batch_id = next_subagent_batch_id();
    // 复用 `subagent_run` 工具的入参形状：校验、工具白名单与错误文案因此完全同源。
    let arguments: Map<String, Value> = json!({
        "tasks": [{
            "subagent_type": agent_type,
            "description": description,
            "prompt": prompt,
        }],
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    let tasks = runtime.prepare(&arguments, &parent_tools, &batch_id)?;
    // 宿主入口始终等完整报告：定义里的 `background: true` 不能把它变成后台批次。
    let tasks: Vec<PreparedTask> = tasks
        .into_iter()
        .map(|mut task| {
            task.background = false;
            task
        })
        .collect();

    let result = execute_tasks(runtime, model_config, tasks, fork_messages, conn, true, "")?;
    // 结果投影复用协调器的同一条路径：失败描述、全文优先与脱敏在这里一次做对。
    let results = result["results"].as_array().cloned().unwrap_or_default();
    let (ok, summary) = batch_summary(&batch_id, results.clone());
    let encoded = json_result_text(&summary);
    let output = require_completed_result(ok, &encoded)
        .map_err(|error| (subagent_failure_code(&summary), error.message().to_string()))?;
    let task = results.first().cloned().unwrap_or(Value::Null);
    Ok((output, task))
}
