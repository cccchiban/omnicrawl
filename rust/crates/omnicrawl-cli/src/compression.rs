//! 工具输出压缩的旁路调用：把超长工具观察交给独立小模型压成精简文本。
//!
//! 语义基准是 Python `agent/controllers/tools/compression.py` 的批量编排与
//! `agent/runtime/tool_output_compressor.py` 的请求构造：只有合格结果才压缩、模型没真正压小
//! 就不采纳、失败保留原文。
//!
//! 模型连接沿用内核 `initialize.model` 给的那条（与 `subagent::child_model_config` 同一做法），
//! 只把 model 名换成 `[tool_output_compression].model_key`；请求是旁路调用：不流式、不写会话。
//!
//! 一批里有多条合格结果时**并发**压缩（一个任务一个线程、各自建运行时），并逐条把
//! `started` / `finished` / `failed` 报给宿主；失败（含超时）也上报，宿主会在工具卡上
//! 显示「压缩超时…」/「压缩失败…」。

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

/// 压缩的阶段事件（内核 → 宿主通知用）。
///
/// `started` / `finished` 只带计量事实（界面文案由宿主决定），`failed` 直接带**已备好的
/// 展示文案**（超时/错误只有内核知道超时秒数与上游错误文本，宿主拿到就能显示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionPhase<'a> {
    Started {
        call_id: &'a str,
        tool: &'a str,
        before_chars: usize,
    },
    Finished {
        call_id: &'a str,
        tool: &'a str,
        before_chars: usize,
        after_chars: usize,
        /// 压缩后的正文：宿主拿它替换卡片里的原始输出（用户要求「压缩后替换原内容」）。
        text: &'a str,
    },
    /// 压缩失败（含超时）：保留原始输出，宿主把 `message` 显示在工具卡上。
    Failed {
        call_id: &'a str,
        tool: &'a str,
        before_chars: usize,
        message: &'a str,
    },
}

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
    system_prompt: String,
    min_chars: usize,
    max_input_chars: usize,
    max_output_chars: usize,
    timeout_seconds: f64,
    options: GenerationOptions,
}

impl CompressionJob {
    /// 该工具结果是否值得压缩（与 Python `_should_compact` 同规则）。
    fn should_compress(&self, tool_name: &str, output: &str) -> bool {
        compression_logic::should_compact(tool_name, output, self.min_chars)
    }

    /// 在工作线程里自建运行时后跑一次压缩（运行时不是 `Send`，不能跨线程搬）。
    fn run(
        &self,
        tool_name: &str,
        arguments_summary: &str,
        task_hint: &str,
        output: &str,
    ) -> Result<String, String> {
        let runtime = build_model_runtime_with_key(&self.config, self.api_key.clone())
            .map_err(|error| error.message().to_string())?;
        self.request(runtime.as_ref(), tool_name, arguments_summary, task_hint, output)
    }

    /// 单次请求的请求体构造与响应校验；模型返回工具调用或空文本、请求失败都返回 `Err`。
    fn request(
        &self,
        runtime: &dyn ModelRuntime,
        tool_name: &str,
        arguments_summary: &str,
        task_hint: &str,
        output: &str,
    ) -> Result<String, String> {
        if output.trim().is_empty() {
            return Err("工具输出为空，无需压缩。".to_string());
        }
        let sampled = compression_logic::sample_output(output, self.max_input_chars);
        let messages =
            compression_logic::build_messages(tool_name, arguments_summary, task_hint, &sampled);
        let conversation: Vec<ConversationMessage> = conversation_from_openai_messages(&messages);
        let identity: BTreeMap<String, String> = BTreeMap::from([
            (
                "scope".to_string(),
                "omnicrawl-tool-output-compression".to_string(),
            ),
            ("profile".to_string(), self.profile.clone()),
            ("model".to_string(), self.model.clone()),
        ]);
        let input = ChatRequestInput {
            model: &self.model,
            system_prompt: &self.system_prompt,
            messages: &conversation,
            tools: &[],
            options: &self.options,
            profile_request_timeout_seconds: self.timeout_seconds,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let reply = runtime
            .run_turn(&input, &mut DiscardSink)
            .map_err(|error| error.message.clone())?;
        if !reply.tool_calls.is_empty() {
            return Err("压缩模型返回了工具调用。".to_string());
        }
        let text = compression_logic::clean_reply_text(reply.content.as_str());
        if text.is_empty() {
            return Err("压缩模型返回了空文本。".to_string());
        }
        Ok(compression_logic::bound_text(&text, self.max_output_chars))
    }
}

/// 压缩失败的展示文案：超时单独说「压缩超时」，其余按「压缩失败」（用户要求）。
fn compression_failure_message(error: &str, timeout_seconds: f64) -> String {
    let lowered = error.to_lowercase();
    let timeout =
        error.contains("超时") || lowered.contains("timeout") || lowered.contains("timed out");
    if timeout {
        format!("压缩超时（{timeout_seconds:.0} 秒），保留原始输出：{error}")
    } else {
        format!("压缩失败，保留原始输出：{error}")
    }
}

pub struct KernelCompressor {
    job: CompressionJob,
}

/// 解析压缩模型：返回（要用的内核模型连接, Profile id, API Key）。
///
/// 主路径与模型切换同一套口径：`apply_model_selection` 支持自定义 key/alias、
/// `profile/model_id` 与裸 model_id，能一并把基地址/协议/凭据换成被选中的那条。
/// 以前是把整个 key 当模型名直接发给上游，`channel-2/Qwen/…` 这种带 Profile 前缀的
/// key 就会被上游回「模型不存在或当前账号无权使用该模型」。
///
/// 配置里没有可读的 `[llm]`（极简配置/测试）或解析结果不可用时，退回「帧里那条连接 +
/// 选择当模型名」，与改动前的行为一致。
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
    let api_key = std::env::var(&child.api_key_env).unwrap_or_default();
    (child, String::new(), api_key)
}

/// 被选中 Profile 自己写在 config.toml 里的明文 key（没有则 `None`）。
///
/// 为什么不直接用 `apply_model_selection` 给出的 `resolved.api_key`：
/// `ProviderProfile::resolve_api_key` 是**环境变量优先**，而内核进程里的 `OPENAI_API_KEY`
/// 是宿主为「当前主渠道」注入的（`omnicrawl-host/src/kernel.rs::kernel_credentials_env`）。
/// 压缩渠道往往是另一家服务（例如主渠道是聚合网关、压缩渠道是硅基流动），它同样写着
/// `api_key_env = "OPENAI_API_KEY"`，于是会拿主渠道的 key 去打压缩渠道 → HTTP 401。
/// Python 侧直接读进程环境（用户自己的 shell），纯净环境里同名变量通常根本没设，
/// 取到的就是明文 key——所以「明文优先」在常见配置下与 Python 同结果（差异只在
/// 「仅在环境变量里轮换密钥、config.toml 里留着旧明文」时会用明文，写进 README 了）。
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
                model: child.model.clone(),
                config: child,
                api_key,
                profile,
                system_prompt: compression_logic::system_prompt_text(),
                min_chars: config.min_chars.max(0) as usize,
                max_input_chars: config.max_input_chars.max(0) as usize,
                max_output_chars: config.max_output_chars.max(0) as usize,
                timeout_seconds: config.timeout_seconds.max(0) as f64,
                options,
            },
        })
    }


    /// 把压缩结果写回观察：模型没压小就保留原文（与 Python `_compress_one` 同规则）。
    ///
    /// 一批里有多条合格结果时**并发**压缩（每条一个线程），完成顺序即回报顺序；
    /// `on_phase` 只在调用线程上被调用——宿主在回调里借用 `Conn` 发通知，而 `Conn` 不是
    /// `Send`，所以通知只能留在调用线程。返回真正采纳的下标，供调用方记录/展示。
    pub fn apply_observations(
        &self,
        observations: &mut [omnicrawl_core::AgentLoopObservation],
        task_hint: &str,
        cancelled: &dyn Fn() -> bool,
        on_phase: &dyn Fn(CompressionPhase<'_>),
    ) -> Vec<usize> {
        // 1) 挑出合格结果，并立刻把「正在压缩…」发给宿主（每条调用各自的进度）。
        let mut jobs: Vec<(usize, String, String, usize, String)> = Vec::new();
        for (index, observation) in observations.iter().enumerate() {
            if cancelled() {
                return Vec::new();
            }
            let tool_name = observation.tool_call.name.clone();
            let output = observation.result.output.clone();
            if !self.job.should_compress(&tool_name, &output) {
                continue;
            }
            let summary = compression_logic::arguments_summary(&serde_json::Value::Object(
                observation.tool_call.arguments.clone(),
            ));
            let before_chars = output.chars().count();
            on_phase(CompressionPhase::Started {
                call_id: &observation.tool_call.id,
                tool: &tool_name,
                before_chars,
            });
            jobs.push((index, tool_name, summary, before_chars, output));
        }
        if jobs.is_empty() {
            return Vec::new();
        }

        // 2) 一个任务一个线程并发跑：多个工具调用就是多个压缩请求同时在飞。
        //    线程是**分离**的（不 join）：回合被取消时主线程立刻返回，慢请求在后台自生自灭；
        //    它只往通道里塞结果、不碰 `Conn`（通知只在主线程发）。
        let (sender, receiver) = std::sync::mpsc::channel();
        let hint = task_hint.to_string();
        for (index, tool_name, summary, before_chars, output) in jobs {
            let job = self.job.clone();
            let sender = sender.clone();
            let hint = hint.clone();
            std::thread::spawn(move || {
                let result = job.run(&tool_name, &summary, &hint, &output);
                let _ = sender.send((index, tool_name, before_chars, result));
            });
        }
        drop(sender);

        // 3) 主线程按完成顺序收结果并回报（先回来的先显示，不必等最慢的那个）。
        let mut applied = Vec::new();
        for (index, tool_name, before_chars, result) in receiver {
            if cancelled() {
                break;
            }
            let call_id = observations[index].tool_call.id.clone();
            match result {
                Ok(compacted) => {
                    let after_chars = compacted.chars().count();
                    if after_chars >= before_chars {
                        let message = format!(
                            "压缩未缩小结果（{before_chars} → {after_chars} 字符），保留原始输出。"
                        );
                        eprintln!("[kernel] 工具输出压缩未缩小结果，保留原始输出：{tool_name}");
                        on_phase(CompressionPhase::Failed {
                            call_id: &call_id,
                            tool: &tool_name,
                            before_chars,
                            message: &message,
                        });
                        continue;
                    }
                    on_phase(CompressionPhase::Finished {
                        call_id: &call_id,
                        tool: &tool_name,
                        before_chars,
                        after_chars,
                        text: &compacted,
                    });
                    let full = compression_logic::compacted_display(
                        &compacted,
                        after_chars,
                        before_chars,
                        &self.job.model,
                    );
                    let observation = &mut observations[index];
                    let tool_call = observation.tool_call.clone();
                    observation.result.output = compacted.clone();
                    observation.result.full_output = full;
                    observation.message = omnicrawl_session::tool_result_message(
                        &tool_call.name,
                        observation.result.ok,
                        &compacted,
                        &tool_call.id,
                    );
                    applied.push(index);
                }
                Err(error) => {
                    let message = compression_failure_message(&error, self.job.timeout_seconds);
                    eprintln!("[kernel] 工具输出压缩失败，保留原始输出：{tool_name}：{error}");
                    on_phase(CompressionPhase::Failed {
                        call_id: &call_id,
                        tool: &tool_name,
                        before_chars,
                        message: &message,
                    });
                }
            }
        }
        applied
    }
}
