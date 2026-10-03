//! 压缩旁路调用：工具输出压缩与模型回复压缩两段请求都走主模型渠道。
//!
//! 两段压缩只在**阈值触发**时并发发起（见 `crate::dual_compaction`）：工具输出压缩把窗口里的
//! 工具调用压成一段，模型回复压缩把窗口里的助手回复压成一段，拼起来接在系统提示词之后。
//!
//! 模型连接沿用内核 `initialize.model` 给的那条（与 `subagent::child_model_config` 同一做法），
//! 只把 model 名换成 `[tool_output_compression].model_key`（未设置时即主模型）；请求是旁路调用：
//! 不写会话、用量不计入主对话统计。

use omnicrawl_config::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use omnicrawl_config::features::tool_output_compression::load_tool_output_compression_config;
use omnicrawl_config::models::llm::load_llm_config;
use omnicrawl_config::models::llm_multi::{apply_model_selection, parse_profiles};
use omnicrawl_controllers::compression as compression_logic;
use omnicrawl_ipc::bridge::KernelModelConfig;
use omnicrawl_llm::{ChatRequestInput, DiscardSink, ModelRuntime};
use omnicrawl_protocol::{
    conversation_from_openai_messages, ConversationMessage, GenerationOptions,
};
use std::collections::BTreeMap;

use crate::session::build_model_runtime_with_key;

/// 单次压缩所需的全部输入（`Clone` + `Send`）：并发压缩时每个任务各自一份。
///
/// 为什么不共用一条 `ModelRuntime`：运行时是**顺序**会话（`run_turn` 一次跑一个），
/// 共用就只能一个个排队，而用户要求「多个工具调用＝多个压缩请求同时在飞」。
/// 因此每个任务在自己的工作线程里按同一份连接配置各建一个运行时
/// （Python 侧同样是「每轮 acquire 一个 runtime」的池化模型）。
#[derive(Clone)]
struct CompressionJob {
    config: KernelModelConfig,
    api_key: String,
    /// 实际发给上游的模型名（`apply_model_selection` 解析后的）。
    model: String,
    /// 解析后的 Profile：只用于 prompt-cache 身份（与 Python 同一份字段）。
    profile: String,
    max_input_chars: usize,
    max_output_chars: usize,
    timeout_seconds: f64,
    options: GenerationOptions,
}

pub struct KernelCompressor {
    job: CompressionJob,
}

impl Clone for KernelCompressor {
    fn clone(&self) -> Self {
        Self {
            job: self.job.clone(),
        }
    }
}

/// 解析压缩模型：返回（要用的内核模型连接, Profile id, API Key）。
///
/// 主路径与模型切换同一套口径：`apply_model_selection` 支持自定义 key/alias、
/// `profile/model_id` 与裸 model_id，能一并把基地址/协议/凭据换成被选中的那条。
/// 以前是把整个 key 当模型名直接发给上游，`channel-2/Qwen/…` 这种带 Profile 前缀的
/// key 就会被上游回「模型不存在或当前账号无权使用该模型」。
///
/// 配置里没有可读的 `[llm]`（极简配置/测试）或解析结果不可用时，退回「帧里那条连接 +
/// 选择当模型名」；凭据同样从 config.toml 读明文。
fn resolve_compression_model(
    env: &ConfigEnvironment,
    model: &KernelModelConfig,
    selection: &str,
) -> (KernelModelConfig, String, String) {
    let resolved = load_llm_config(env).and_then(|base| apply_model_selection(env, &base, selection));
    if let Ok(resolved) = resolved {
        if !resolved.model.trim().is_empty() && !resolved.base_url.trim().is_empty() {
            let mut child = model.clone(); // 超时/思考开关等旁路参数沿用主模型通道
            child.model = resolved.model.clone();
            child.provider = resolved.provider.clone();
            child.protocol = resolved.protocol.clone();
            child.base_url = resolved.base_url.clone();
            child.api_key_env = resolved.api_key_env.clone();
            child.user_agent = resolved.user_agent.clone();
            child.system_prompt = String::new();
            child.tools = Vec::new();
            let api_key = profile_literal_key(env, &resolved.profile_id)
                .unwrap_or_else(|| resolved.api_key.clone());
            return (child, resolved.profile_id.clone(), api_key);
        }
    }
    let mut child = model.clone();
    child.model = selection.to_string();
    child.system_prompt = String::new();
    child.tools = Vec::new();
    // 配置读不出来时没有可用凭据：返回空串，调用方按「缺少凭据」跳过压缩。
    (child, String::new(), String::new())
}

/// Profile `api_key_env` 对应的「明文 key 优先」查找：只认 Profile 里写的 `api_key`。
///
/// 审查/压缩渠道往往是另一家服务（例如主渠道是聚合网关、压缩渠道是硅基流动），
/// 用主渠道的 key 去打压缩渠道会 HTTP 401。
fn profile_literal_key(env: &ConfigEnvironment, profile_id: &str) -> Option<String> {
    let profile_id = profile_id.trim();
    if profile_id.is_empty() {
        return None;
    }
    let data = load_config_data(env, None).ok()?;
    let section = get_section(&data, "llm").ok()?;
    parse_profiles(&section)
        .remove(profile_id)
        .map(|profile| profile.api_key.trim().to_string())
        .filter(|key| !key.is_empty())
}

impl KernelCompressor {
    /// 从配置与主模型连接装配压缩器；未启用或装配失败返回 `None`（压缩是可选旁路）。
    pub fn load(model: &KernelModelConfig) -> Option<Self> {
        let env = ConfigEnvironment::from_process();
        let config = load_tool_output_compression_config(&env, None).ok()?;
        if !config.active() {
            return None;
        }

        let selection = config.model_key.trim().to_string();
        let (child, profile, api_key) = resolve_compression_model(&env, model, &selection);
        if api_key.trim().is_empty() {
            eprintln!("[kernel] 工具输出压缩缺少凭据（{selection}），已跳过压缩。");
            return None;
        }

        let effort = compression_logic::effective_reasoning_effort(
            config.thinking_enabled,
            &config.reasoning_effort,
        );
        let options = GenerationOptions {
            reasoning_effort: effort,
            ..GenerationOptions::default()
        };

        Some(Self {
            job: CompressionJob {
                max_input_chars: config.max_input_chars.max(0) as usize,
                model: child.model.clone(),
                config: child,
                api_key,
                profile,
                max_output_chars: config.max_output_chars.max(0) as usize,
                timeout_seconds: config.timeout_seconds.max(0) as f64,
                options,
            },
        })
    }

    /// 双段压缩统一使用 Low 思考深度（用户要求），其余参数沿用主模型通道。
    pub fn with_low_reasoning(mut self) -> Self {
        self.job.options.reasoning_effort = "low".to_string();
        self
    }

    /// 回合结束的整轮概括：把本轮全部工具调用压成一段正文，原文另存文件。
    ///
    /// 与逐条压缩的差别只有粒度：一次请求覆盖整轮全部调用（不再按工具名与长度筛），
    /// 结果由调用方写进上下文替换原来的逐条请求与结果。失败一律返回 `Err`，调用方保留原文。
    pub fn summarize_turn(
        &self,
        calls: &[compression_logic::TurnCallRecord],
        task_hint: &str,
    ) -> Result<String, String> {
        if calls.is_empty() {
            return Err("本轮没有工具调用，无需概括。".to_string());
        }
        let runtime = build_model_runtime_with_key(&self.job.config, self.job.api_key.clone())
            .map_err(|error| error.message().to_string())?;
        self.request_turn_summary(runtime.as_ref(), calls, task_hint)
    }

    /// 整轮概括的请求体构造与响应校验（与逐条路径共用清洗与截断口径）。
    fn request_turn_summary(
        &self,
        runtime: &dyn ModelRuntime,
        calls: &[compression_logic::TurnCallRecord],
        task_hint: &str,
    ) -> Result<String, String> {
        let sampled: Vec<compression_logic::TurnCallRecord> = calls
            .iter()
            .map(|call| compression_logic::TurnCallRecord {
                output: compression_logic::sample_output(&call.output, self.job.max_input_chars),
                ..call.clone()
            })
            .collect();
        let messages = compression_logic::build_turn_summary_messages(&sampled, task_hint);
        let conversation: Vec<ConversationMessage> = conversation_from_openai_messages(&messages);
        let identity: BTreeMap<String, String> = BTreeMap::from([
            (
                "scope".to_string(),
                "omnicrawl-tool-output-compression".to_string(),
            ),
            ("profile".to_string(), self.job.profile.clone()),
            ("model".to_string(), self.job.model.clone()),
        ]);
        let system_prompt = compression_logic::turn_summary_system_prompt();
        let input = ChatRequestInput {
            model: &self.job.model,
            system_prompt: &system_prompt,
            messages: &conversation,
            tools: &[],
            options: &self.job.options,
            profile_request_timeout_seconds: self.job.timeout_seconds,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let reply = runtime
            .run_turn(&input, &mut DiscardSink)
            .map_err(|error| error.message.clone())?;
        if !reply.tool_calls.is_empty() {
            return Err("概括模型返回了工具调用。".to_string());
        }
        let text = compression_logic::clean_reply_text(reply.content.as_str());
        if text.is_empty() {
            return Err("概括模型返回了空文本。".to_string());
        }
        Ok(compression_logic::bound_text(
            &text,
            self.job.max_output_chars,
        ))
    }

    /// 阈值触发的模型回复压缩：把历史里的助手回复压成一段正文。
    ///
    /// 与工具调用压缩**并发**发起（由调用方各起一个线程），因此这里只做单次请求；
    /// 失败一律返回 `Err`，调用方回退为不注入该段文本。
    pub fn compact_replies(&self, replies: &[String], task_hint: &str) -> Result<String, String> {
        if replies.is_empty() {
            return Err("没有可压缩的模型回复。".to_string());
        }
        let runtime = build_model_runtime_with_key(&self.job.config, self.job.api_key.clone())
            .map_err(|error| error.message().to_string())?;
        let joined = replies.join("

");
        let bounded = compression_logic::sample_output(&joined, self.job.max_input_chars);
        let messages = compression_logic::build_reply_compaction_messages(&[bounded], task_hint);
        let conversation: Vec<ConversationMessage> = conversation_from_openai_messages(&messages);
        let identity: BTreeMap<String, String> = BTreeMap::from([
            (
                "scope".to_string(),
                "omnicrawl-tool-output-compression".to_string(),
            ),
            ("profile".to_string(), self.job.profile.clone()),
            ("model".to_string(), self.job.model.clone()),
        ]);
        let system_prompt = compression_logic::reply_compaction_system_prompt();
        let input = ChatRequestInput {
            model: &self.job.model,
            system_prompt: &system_prompt,
            messages: &conversation,
            tools: &[],
            options: &self.job.options,
            profile_request_timeout_seconds: self.job.timeout_seconds,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let reply = runtime
            .run_turn(&input, &mut DiscardSink)
            .map_err(|error| error.message.clone())?;
        if !reply.tool_calls.is_empty() {
            return Err("回复压缩模型返回了工具调用。".to_string());
        }
        let text = compression_logic::clean_reply_text(reply.content.as_str());
        if text.is_empty() {
            return Err("回复压缩模型返回了空文本。".to_string());
        }
        Ok(compression_logic::bound_text(
            &text,
            self.job.max_output_chars,
        ))
    }
}
