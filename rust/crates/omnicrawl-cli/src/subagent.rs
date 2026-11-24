//! 内核自持的 SubAgent 工具族：把受限子任务跑成独立子回合，并管理任务与隔离工作树。
//!
//! 与 `recall_session_evidence` 同类——命中即由内核作答，不占宿主的批次；区别是子回合里的
//! 模型请求与工具批次照旧经协议外发：子模型请求走内核自带的 provider runtime，子工具批次走
//! 宿主的 `tool.batch`（`turn_id` 用子任务号，便于宿主把审批与生命周期分开）。
//!
//! 本模块只放装配与投影，不碰连接：`Conn` 相关的驱动留在 `session.rs`。

use std::path::PathBuf;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::subagents::{load_subagent_config, SubAgentConfig};
use omnicrawl_controllers::subagents::coordinator::{
    json_result_text, select_profile_tools, top_level_error, validate_task_arguments, PreparedTaskView,
};
use omnicrawl_controllers::subagents::definitions::{AgentDefinition, AgentDefinitionRegistry};
use omnicrawl_controllers::subagents::orchestration::fork_task_message;
use omnicrawl_ipc::bridge::KernelModelConfig;
use omnicrawl_session::redaction::redact_sensitive_text;
use serde_json::{json, Map, Value};

/// 子回合实际使用的上下文窗口：该角色声明了窗口就用它，否则沿用父会话。
pub fn child_context_window(config: &SubAgentConfig, agent_type: &str, parent_window: i64) -> i64 {
    match config.context_window_overrides.get(agent_type).copied() {
        Some(window) if window > 0 => window,
        _ => parent_window,
    }
}

/// 一个已通过校验、权限已收窄的子任务。
#[derive(Debug, Clone)]
pub struct PreparedTask {
    pub batch_id: String,
    pub task_id: String,
    pub description: String,
    pub prompt: String,
    pub agent_type: String,
    pub definition: AgentDefinition,
    pub tool_names: Vec<String>,
    pub context: String,
    pub background: bool,
}

impl PreparedTask {
    pub fn view(&self) -> PreparedTaskView {
        PreparedTaskView {
            batch_id: self.batch_id.clone(),
            task_id: self.task_id.clone(),
            description: self.description.clone(),
            agent_type: self.agent_type.clone(),
            definition_source: self.definition.source.clone(),
        }
    }
}

/// 一次子回合的有界结果。
#[derive(Debug, Clone, Default)]
pub struct SubAgentExecution {
    pub final_text: String,
    pub model_turns: usize,
    pub tool_calls: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

/// 装入内核对 SubAgent 的运行期视图。
#[derive(Clone)]
pub struct SubAgentRuntime {
    pub config: SubAgentConfig,
    pub registry: AgentDefinitionRegistry,
}

impl SubAgentRuntime {
    /// 从进程环境读数（与 Python 侧一致：独立 `subagents.toml`，环境变量只能收紧）。
    pub fn load() -> Result<Self, String> {
        let env = ConfigEnvironment::from_process();
        let config =
            load_subagent_config(&env, None).map_err(|error| error.message().to_string())?;
        let home = env.home().to_path_buf();
        let mut registry = AgentDefinitionRegistry::with_located_builtin(Some(home));
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        registry.discover(&workspace);
        Ok(Self { config, registry })
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// 父工具名：从宿主交来的静态声明里取（OpenAI functions 形状）。
    pub fn parent_tool_names(model: &KernelModelConfig) -> Vec<String> {
        model
            .tools
            .iter()
            .filter_map(|item| {
                item.pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    /// 校验并准备一批任务；失败时返回 `(code, message)` 形式的顶层错误。
    ///
    /// 一次调用可携带多个任务，它们各自成子回合、共用同一个批次号；并发上限由任务
    /// 管理器的 worker 数决定。
    pub fn prepare(
        &self,
        arguments: &Map<String, Value>,
        parent_tools: &[String],
        batch_id: &str,
    ) -> Result<Vec<PreparedTask>, (String, String)> {
        let value = Value::Object(arguments.clone());
        if let Some(error) = validate_task_arguments(&value, &self.config, &self.registry) {
            return Err(error);
        }
        let entries = arguments
            .get("tasks")
            .and_then(Value::as_array)
            .expect("校验已保证 tasks 是非空数组");
        let mut tasks = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let task = entry.as_object().expect("校验已保证任务项是对象");
            let description = task
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let prompt = task
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let agent_type = task
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            let context = task
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or("fresh")
                .to_string();
            let Some(definition) = self.registry.get(&agent_type) else {
                return Err((
                    "AGENT_TYPE_NOT_FOUND".to_string(),
                    format!("未找到 Agent 定义：{agent_type}。"),
                ));
            };
            let verify_tools = vec![
                omnicrawl_controllers::subagents::verify::VERIFY_COMMAND_TOOL_NAME.to_string(),
            ];
            let selection = select_profile_tools(definition, parent_tools, &verify_tools);
            tasks.push(PreparedTask {
                batch_id: batch_id.to_string(),
                // 任务号固定 `task-` + 12 位小写十六进制：这个长度与字符集是
                // `is_subagent_task_id` 的硬判据。批次散列取前 10 位，末两位留给同批序号，
                // 一批多任务因此各自唯一，又都与同一个批次号对应。
                task_id: format!("task-{}{:02x}", batch_suffix(&batch_id), index),
                description,
                prompt,
                agent_type,
                definition: definition.clone(),
                tool_names: selection.names,
                context,
                background: arguments
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(definition.background),
            });
        }
        Ok(tasks)
    }

    /// 子任务的初始消息：fresh 只有任务指令（系统提示走模型配置），fork 在父快照后追加指令。
    pub fn task_messages(&self, task: &PreparedTask, fork_messages: &[Value]) -> Vec<Value> {
        if task.context == "fork" && !fork_messages.is_empty() {
            let mut messages = fork_messages.to_vec();
            messages.push(fork_task_message(&task.description, &task.prompt));
            return messages;
        }
        vec![json!({
            "role": "user",
            "content": format!(
                "<subagent_task>\n描述：{}\n任务：\n{}\n</subagent_task>",
                task.description, task.prompt
            ),
        })]
    }

    /// 子回合的模型配置：只声明该任务被允许的工具，系统提示换成角色定义（fork 继承父提示）。
    pub fn child_model_config(
        &self,
        model_config: &KernelModelConfig,
        task: &PreparedTask,
    ) -> KernelModelConfig {
        let mut config = model_config.clone();
        config.tools = model_config
            .tools
            .iter()
            .filter(|item| {
                item.pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(|name| task.tool_names.iter().any(|item| item == name))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        config.system_prompt = if task.context == "fork" {
            model_config.system_prompt.clone()
        } else {
            task.definition.system_prompt.clone()
        };
        if let Some(selection) = self.config.model_overrides.get(&task.agent_type) {
            if !selection.trim().is_empty() {
                config.model = selection.clone();
            }
        }
        // 窗口覆盖与模型覆盖各自独立：换模型不一定换窗口（同一模型可配不同上限），
        // 只声明窗口时子代理仍用父模型、但按声明的窗口折算压缩阈值。
        config.context_window_tokens = child_context_window(
            &self.config,
            &task.agent_type,
            model_config.context_window_tokens,
        );
        config
    }

    /// 子任务完成后的公开结果投影。
    pub fn completed_result(&self, task: &PreparedTask, execution: &SubAgentExecution) -> Value {
        let summary = self.summarize(&execution.final_text);
        json!({
            "task_id": task.task_id,
            "description": task.description,
            "agent_type": task.agent_type,
            "definition_source": task.definition.source,
            "status": "completed",
            "summary": summary,
            "evidence": [],
            "artifacts": [],
            "usage": {
                "input_tokens": execution.input_tokens,
                "output_tokens": execution.output_tokens,
                "cached_input_tokens": execution.cached_input_tokens,
                "model_turns": execution.model_turns,
                "tool_calls": execution.tool_calls,
            },
            "error": Value::Null,
        })
    }

    /// 结果摘要：脱敏后按配置上限截断。
    pub fn summarize(&self, final_text: &str) -> String {
        let summary = redact_sensitive_text(final_text.trim());
        let limit = self.config.result_summary_chars.max(1) as usize;
        if summary.chars().count() > limit {
            let mut truncated: String = summary.chars().take(limit).collect();
            truncated.push_str("\n... 子任务结果已截断。");
            return truncated;
        }
        summary
    }

    /// 功能未启用时的顶层错误文本。
    pub fn disabled_text(&self) -> String {
        json_result_text(&top_level_error(
            "SUBAGENT_DISABLED",
            "SubAgent 功能未启用。请在配置中显式设置 subagents.enabled=true。",
        ))
    }
}

/// 批次散列里用于拼任务号的前 10 位（末两位留给同批序号）。
fn batch_suffix(batch_id: &str) -> String {
    let suffix = batch_id.strip_prefix("batch-").unwrap_or(batch_id);
    suffix.chars().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(window: Option<i64>) -> SubAgentConfig {
        let mut config = SubAgentConfig::default();
        if let Some(value) = window {
            config
                .context_window_overrides
                .insert("explore".to_string(), value);
        }
        config
    }

    /// 父会话 1M、子代理声明 32K：必须按 32K 走。
    ///
    /// 留父会话的大值会让压缩阈值落在 800K，而子模型远早于该点就撞上游硬上限——
    /// 这条正是「子代理换小窗口模型后请求直接失败」的成因。
    #[test]
    fn declared_window_replaces_parent() {
        assert_eq!(
            child_context_window(&config_with(Some(32_000)), "explore", 1_024_000),
            32_000
        );
        assert_eq!(
            child_context_window(&config_with(Some(1_000_000)), "explore", 128_000),
            1_000_000
        );
    }

    #[test]
    fn absent_role_inherits_parent() {
        assert_eq!(
            child_context_window(&config_with(None), "explore", 1_024_000),
            1_024_000
        );
        assert_eq!(
            child_context_window(&config_with(Some(32_000)), "plan", 1_024_000),
            1_024_000
        );
    }

    /// 解析层已拒绝非正数，运行时仍要守住：0 或负数不能把子代理的窗口抹成 0。
    #[test]
    fn non_positive_never_wins() {
        assert_eq!(
            child_context_window(&config_with(Some(0)), "explore", 1_024_000),
            1_024_000
        );
        assert_eq!(
            child_context_window(&config_with(Some(-1)), "explore", 1_024_000),
            1_024_000
        );
    }
}
