//! `advisor`：把当前工作分支转发给顾问模型，返回 plan / correction / stop 三类指导。
//!
//! 判定与消息构造复用内核已搬好的 `omnicrawl-controllers::advisor`（可用性、黑名单、分支裁剪、
//! 工具清单、超时与全部错误文案）；本模块只负责运行期：独立 Runtime 引导、单轮无工具补全与信封。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_controllers::advisor::{
    advisor_blacklisted, advisor_request_timeout, build_advisor_branch, executor_tool_inventory,
    ADVISOR_BLACKLISTED_ERROR, ADVISOR_DISABLED_ERROR, ADVISOR_EMPTY_ERROR,
    ADVISOR_NO_CONTEXT_ERROR,
};
use omnicrawl_llm::{ChatEndpoint, ChatRequestInput, DiscardSink, OpenAiChatRuntime};
use omnicrawl_protocol::{conversation_from_openai_messages, GenerationOptions, ToolSpec};
use serde_json::{json, Map, Value};

use super::error::{ToolError, ToolOutcome};

pub const ADVISOR_TOOL_NAME: &str = "advisor";
pub const ADVISOR_SYSTEM_PROMPT: &str =
    include_str!("../../../../../rust/assets/templates/advisor_system.md");
const USER_AGENT: &str = "omnicrawl-tui-advisor/0.0.1";

/// 顾问运行期配置：模型与凭据来自 `--advisor-*` / `OMNICRAWL_ADVISOR_*`，工作分支与工具面由宿主注入。
#[derive(Clone)]
pub struct AdvisorOptions {
    pub enabled: bool,
    /// 顾问模型名；空串等价未选择（`AdvisorConfig.active` 的语义）。
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    /// 推理强度（对应 Python 的 `advisor.effort`）。
    pub effort: String,
    /// 当前执行者模型，用于黑名单命中判定。
    pub executor_model: String,
    pub executor_catalog_key: String,
    pub executor_profile_id: String,
    pub disabled_for_models: Vec<String>,
    pub timeout_seconds: i64,
    pub workspace_root: PathBuf,
    /// 当前工作分支的消息（由宿主提供；拿不到时返回空）。
    pub messages: Arc<dyn Fn() -> Vec<Value> + Send + Sync>,
    /// 执行者可用工具面（工具名 + 说明），供顾问了解执行者可用的手段。
    pub tools: Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
}

impl Default for AdvisorOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            effort: String::new(),
            executor_model: String::new(),
            executor_catalog_key: String::new(),
            executor_profile_id: String::new(),
            disabled_for_models: Vec::new(),
            timeout_seconds: 180,
            workspace_root: PathBuf::from("."),
            messages: Arc::new(Vec::new),
            tools: Arc::new(Vec::new),
        }
    }
}

impl AdvisorOptions {
    /// 生效的顾问凭据：显式配置优先，其次读环境变量。
    pub fn resolve_api_key(&self) -> String {
        if !self.api_key.trim().is_empty() {
            return self.api_key.clone();
        }
        std::env::var(&self.api_key_env).unwrap_or_default()
    }

    /// 是否真正可用（显式启用且已选模型）。
    pub fn active(&self) -> bool {
        self.enabled && !self.model.trim().is_empty()
    }
}

pub fn advisor(options: &AdvisorOptions, _arguments: &Map<String, Value>) -> ToolOutcome {
    if !options.active() {
        return Err(ToolError::new(ADVISOR_DISABLED_ERROR));
    }
    if advisor_blacklisted(
        &options.disabled_for_models,
        &options.executor_catalog_key,
        &options.executor_profile_id,
        &options.executor_model,
    ) {
        return Err(ToolError::new(ADVISOR_BLACKLISTED_ERROR));
    }

    let working_messages = (options.messages)();
    if working_messages.is_empty() {
        return Err(ToolError::new(ADVISOR_NO_CONTEXT_ERROR));
    }
    let branch = build_advisor_branch(working_messages);

    // 工具清单前置：顾问不持有工具，但需要知道执行者可用工具面。
    let inventory = executor_tool_inventory(&(options.tools)());
    let mut prompt_messages = Vec::with_capacity(branch.len() + 1);
    prompt_messages.push(json!({"role": "user", "content": inventory}));
    prompt_messages.extend(branch);

    let api_key = options.resolve_api_key();
    if api_key.trim().is_empty() {
        return Err(ToolError::new(format!(
            "顾问模型 Runtime 初始化失败：缺少 API Key：请设置环境变量 {}。",
            options.api_key_env
        )));
    }

    let timeout_seconds =
        advisor_request_timeout(options.timeout_seconds, options.timeout_seconds) as f64;
    let generation = GenerationOptions {
        reasoning_effort: options.effort.clone(),
        request_timeout_seconds: timeout_seconds,
        // 空响应由 Runtime 有界重试一次；其余错误短路。
        request_retry_count: 1,
        ..GenerationOptions::default()
    };
    let mut identity: BTreeMap<String, String> = BTreeMap::new();
    identity.insert(
        "workspace".to_string(),
        options.workspace_root.to_string_lossy().to_string(),
    );
    identity.insert("advisor".to_string(), "system".to_string());
    identity.insert("model".to_string(), options.model.clone());

    let conversation = conversation_from_openai_messages(&prompt_messages);
    let endpoint = ChatEndpoint {
        base_url: options.base_url.clone(),
        api_key,
        user_agent: USER_AGENT.to_string(),
    };
    let runtime = OpenAiChatRuntime::new(endpoint);
    let tools: Vec<ToolSpec> = Vec::new();
    let input = ChatRequestInput {
        model: options.model.as_str(),
        system_prompt: ADVISOR_SYSTEM_PROMPT,
        messages: &conversation,
        tools: &tools,
        options: &generation,
        profile_request_timeout_seconds: timeout_seconds,
        prompt_cache_capable: false,
        prompt_cache_identity: &identity,
    };
    let mut sink = DiscardSink;
    let reply = runtime
        .run_turn(&input, &mut sink)
        .map_err(|error| ToolError::new(format!("顾问请求失败：{error}")))?;

    let text = reply.content.trim().to_string();
    if text.is_empty() {
        return Err(ToolError::new(ADVISOR_EMPTY_ERROR));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn options_with(
        messages: Vec<Value>,
        configure: impl FnOnce(&mut AdvisorOptions),
    ) -> AdvisorOptions {
        let mut options = AdvisorOptions {
            enabled: true,
            model: "advisor-model".to_string(),
            api_key: "test-key".to_string(),
            messages: Arc::new(move || messages.clone()),
            tools: Arc::new(|| vec![("read".to_string(), "读取文件".to_string())]),
            ..AdvisorOptions::default()
        };
        configure(&mut options);
        options
    }

    #[test]
    fn availability_and_blacklist_short_circuit_errors() {
        let disabled = AdvisorOptions::default();
        assert_eq!(
            advisor(&disabled, &arguments(json!({})))
                .expect_err("未启用应当被拒绝")
                .message,
            ADVISOR_DISABLED_ERROR
        );

        let blacklisted = options_with(vec![json!({"role": "user", "content": "x"})], |options| {
            options.disabled_for_models = vec!["advisor-model".to_string()];
            options.executor_model = "advisor-model".to_string();
        });
        assert_eq!(
            advisor(&blacklisted, &arguments(json!({})))
                .expect_err("黑名单命中应当被拒绝")
                .message,
            ADVISOR_BLACKLISTED_ERROR
        );

        let empty = options_with(Vec::new(), |_| {});
        assert_eq!(
            advisor(&empty, &arguments(json!({})))
                .expect_err("无上下文应当被拒绝")
                .message,
            ADVISOR_NO_CONTEXT_ERROR
        );
    }

    #[test]
    fn missing_credentials_report_the_environment_variable() {
        let options = options_with(vec![json!({"role": "user", "content": "x"})], |options| {
            options.api_key = String::new();
            options.api_key_env = "OMNICRAWL_TUI_ADVISOR_MISSING".to_string();
        });
        let error = advisor(&options, &arguments(json!({}))).expect_err("缺凭据应当被拒绝");
        assert!(
            error.message.contains("OMNICRAWL_TUI_ADVISOR_MISSING"),
            "{}",
            error.message
        );
        assert!(
            error
                .message
                .starts_with("顾问模型 Runtime 初始化失败：缺少 API Key"),
            "{}",
            error.message
        );
    }
}
