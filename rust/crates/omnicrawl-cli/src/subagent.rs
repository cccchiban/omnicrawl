//! 内核自持的 `subagent` 工具：把一批受限子任务跑成独立子回合。
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
    json_result_text, select_profile_tools, top_level_error, validate_arguments, PreparedTaskView,
};
use omnicrawl_controllers::subagents::definitions::{AgentDefinition, AgentDefinitionRegistry};
use omnicrawl_controllers::subagents::orchestration::fork_task_message;
use omnicrawl_ipc::bridge::KernelModelConfig;
use omnicrawl_session::redaction::redact_sensitive_text;
use serde_json::{json, Map, Value};

/// 工具名：与工具目录里的 `subagent` 一致。
pub const SUBAGENT_TOOL_NAME: &str = "subagent";
/// 定义目录的环境变量；未设置时回落到用户 Agent 目录。
pub const SUBAGENTS_DIR_ENV: &str = "OMNICRAWL_SUBAGENTS_DIR";

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
        let builtin = builtin_directory(&env);
        let home = env.home().to_path_buf();
        let mut registry = AgentDefinitionRegistry::new(builtin, Some(home));
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        registry.discover(&workspace, &[]);
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
    pub fn prepare(
        &self,
        arguments: &Map<String, Value>,
        parent_tools: &[String],
        batch_id: &str,
    ) -> Result<Vec<PreparedTask>, (String, String)> {
        let value = Value::Object(arguments.clone());
        if let Some(error) = validate_arguments(&value, &self.config, &self.registry) {
            return Err(error);
        }
        let tasks = arguments
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut prepared = Vec::with_capacity(tasks.len());
        for (index, task) in tasks.iter().enumerate() {
            let task = task.as_object().expect("校验已保证 tasks 元素是对象");
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
            prepared.push(PreparedTask {
                batch_id: batch_id.to_string(),
                task_id: format!("task-{:012x}", index as u64 + 1),
                description,
                prompt,
                agent_type,
                definition: definition.clone(),
                tool_names: selection.names,
                context,
            });
        }
        Ok(prepared)
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

/// 定义目录：优先环境变量，否则用用户 Agent 目录（与 four-layer 发现里的 user 层同址，
/// 重复扫描会被 real path 去重跳过）。
pub fn builtin_directory(env: &ConfigEnvironment) -> PathBuf {
    match env.get(SUBAGENTS_DIR_ENV) {
        Some(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => env.home().join(".OmniCrawl").join("agents"),
    }
}
