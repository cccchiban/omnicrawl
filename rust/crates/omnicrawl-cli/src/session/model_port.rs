//! 由 `session.rs` 按职责拆出，语义未变。

use super::*;
use super::protocol::{PortFailure};



use super::turn::{Conn, ProtocolSink, drain_inbound_during_turn};

/// 一个回合内的模型用量累计与最近一次请求的逐字消息。
#[derive(Debug, Clone, Default)]
pub(super) struct TurnUsage {
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cached_input_tokens: i64,
    pub(super) last_request_input_tokens: i64,
    pub(super) last_request_messages: Vec<Value>,
    /// 本回合是否被上游判定为上下文超限：命中即走压缩恢复再重试一次。
    pub(super) context_overflow: bool,
}

impl TurnUsage {
    pub(super) fn snapshot(&self) -> TokenUsageSample {
        TokenUsageSample::new(
            self.input_tokens,
            self.output_tokens,
            self.cached_input_tokens,
        )
        .unwrap_or_default()
    }

    pub(super) fn reset(&mut self) {
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cached_input_tokens = 0;
        self.last_request_input_tokens = 0;
        self.last_request_messages.clear();
        self.context_overflow = false;
    }
}

/// 模型端口（兼容路径）：把模型请求交给宿主代答，供未提供模型配置的宿主使用。
pub(super) struct RemoteModelPort {
    pub(super) conn: Rc<RefCell<Conn>>,
    pub(super) turn_id: String,
    /// 本批 assistant 原文：代答路径下宿主回的回复同样带工具调用，事件里也要有协议字段。
    pub(super) active_assistant: ActiveAssistantSlot,
}

impl ReplySource for RemoteModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        let request = ModelRequest {
            turn_id: self.turn_id.clone(),
            messages: messages.clone(),
        };
        let params = serde_json::to_value(&request)
            .expect("model.reply 负载是serde_json::Value字段，必须可序列化");
        let value = self
            .conn
            .borrow_mut()
            .request(method::MODEL_REPLY, params)
            .map_err(PortFailure::into_reply_error)?;
        let reply = serde_json::from_value(value).map_err(|error| {
            LoopError::ReplySource(format!("宿主返回的模型回复无法解析：{error}"))
        })?;
        remember_active_assistant(&self.active_assistant, &reply);
        Ok(reply)
    }
}

/// 模型端口（内核自带）：由 `omnicrawl-llm` 自己发请求，增量与用量经协议外发。
///
/// 重试位置在这里而不是循环里：Python 侧同样把 `request_retry_count` 施加在主循环，
/// 有界重试空响应与可重试错误；文案与重试节奏保持与 Python 一致。
pub(super) struct KernelModelPort {
    pub(super) conn: Rc<RefCell<Conn>>,
    pub(super) config: KernelModelConfig,
    pub(super) usage: Rc<RefCell<TurnUsage>>,
    /// 本批 assistant 原文：工具批次落 `tool_call_requested` 时取协议字段。
    pub(super) active_assistant: ActiveAssistantSlot,
    /// 本回合的模型运行时句柄：从 [`Conn`] 的跨回合缓存里取（首帧惰性取一次）。
    ///
    /// 语义基准是 Python `ModelRuntimeManager.acquire_turn()`：一个回合只取一次快照，
    /// 之后「首次请求 + 每次工具观察后的后续请求」共用同一份运行时。若每个请求都重建，
    /// 出网脱敏装饰器的逐消息缓存（`memo`）与屏蔽计划缓存都会清零，同一段历史会被
    /// 反复扫描；ureq 连接池也会被丢掉。
    pub(super) runtime: RefCell<Option<Rc<KernelModelRuntime>>>,
    /// 本回合的请求侧派生数据（工具声明、生成选项）：两者在回合内不变，构建一次即可。
    pub(super) request: RefCell<Option<TurnRequestParts>>,
}

/// 一个可跨回合复用的运行时，连同它的构建键。
///
/// `key` 覆盖影响运行时构造的全部字段（含凭据），配置变了就地重建。
pub(super) struct KernelModelRuntime {
    pub(super) key: RuntimeBuildKey,
    pub(super) runtime: Box<dyn ModelRuntime>,
}

/// 运行时构建键：这些字段任一变化都必须重建运行时（凭据也在内，换 Key 要立刻生效）。
///
/// `session_id` 是会话级脱敏序号映射的绑定线索（`/new`、`/resume` 换会话时不能把上一条
/// 会话的「序号 → 原文」映射带过去），因此它也是键的一部分。
///
/// `desensitization` 是 `[desensitization]` 段的指纹：脱敏装饰器在**构建模型运行时**时包装，
/// 因此只有把它纳入键，改配置才能在下一回合换掉装饰器（否则要等到换模型或重起内核）。
#[derive(Clone, PartialEq)]
pub(super) struct RuntimeBuildKey {
    pub(super) session_id: String,
    pub(super) provider: String,
    pub(super) protocol: String,
    pub(super) base_url: String,
    pub(super) model: String,
    pub(super) user_agent: String,
    pub(super) context_window_tokens: i64,
    pub(super) api_key: String,
    pub(super) desensitization: DesensitizationFingerprint,
}

/// 只用于测试断言的可读输出：凭据字段一律打码，绝不把 Key 写进日志或 panic 信息。
impl std::fmt::Debug for RuntimeBuildKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeBuildKey")
            .field("session_id", &self.session_id)
            .field("provider", &self.provider)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("user_agent", &self.user_agent)
            .field("context_window_tokens", &self.context_window_tokens)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    ""
                } else {
                    "<已隐藏>"
                },
            )
            .field("desensitization", &self.desensitization)
            .finish()
    }
}

impl RuntimeBuildKey {
    fn of(config: &KernelModelConfig, session_id: &str, api_key: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            provider: config.provider.clone(),
            protocol: config.protocol.clone(),
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            user_agent: config.user_agent.clone(),
            context_window_tokens: config.context_window_tokens,
            api_key: api_key.to_string(),
            desensitization: DesensitizationFingerprint::read(),
        }
    }
}

/// `[desensitization]` 的生效值指纹：整段配置就是等价判定键。
///
/// 脱敏装饰器只在构建运行时包装一次，所以用「读到的整段配置」当键——任何字段变了都重建运行时，
/// 不必逐个字段列出来。配置读不出来时退化为 `None`（与 `maybe_wrap_desensitization` 的
/// 「按未启用处理」同义）。
#[derive(Clone, PartialEq)]
pub(super) struct DesensitizationFingerprint(pub(super) Option<DesensitizationConfig>);

/// 同上：只给测试断言用，输出的是「有没有读到配置 + 启用状态」，不打印整段配置。
impl std::fmt::Debug for DesensitizationFingerprint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.as_ref() {
            Some(config) => formatter
                .debug_struct("DesensitizationFingerprint")
                .field("enabled", &config.enabled)
                .finish(),
            None => formatter.write_str("DesensitizationFingerprint(unreadable)"),
        }
    }
}

impl DesensitizationFingerprint {
    pub(super) fn read() -> Self {
        let environment = ConfigEnvironment::from_process();
        Self(load_desensitization_config(&environment, None).ok())
    }
}

/// 回合内不变的请求侧派生数据。
pub(super) struct TurnRequestParts {
    pub(super) options: GenerationOptions,
    pub(super) tools: Vec<ToolSpec>,
    pub(super) name_map: ToolNameMap,
}

impl TurnRequestParts {
    fn build(config: &KernelModelConfig) -> Result<Self, LoopError> {
        let options = parse_options(config)?;
        let (tools, name_map) = conform_tool_names(&parse_tools(config));
        Ok(Self {
            options,
            tools,
            name_map,
        })
    }
}

impl ReplySource for KernelModelPort {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        // 工具调用 id 的线上体检：同一次请求里 assistant `tool_calls` 的 id 必须互不重复、
        // `tool` 结果必须与之逐一双向配对。网关会把「同一请求重复提交相同的 call_id」直接判成
        // HTTP 400（`type: invalid_tool_state`），整段会话就此断掉。修复只做在**发出去的副本**上：
        // 运行期消息与转录事件保持原样，按原始 id 匹配的淘汰标注与重启后的历史投影都不受影响。
        let mut wire_messages = messages.clone();
        let id_report = normalize_call_ids(&mut wire_messages);
        if id_report.has_violation() {
            let detail = id_report.describe();
            eprintln!("[kernel] 工具调用 id 线上体检：{detail}");
            if id_report.repaired() {
                self.conn
                    .borrow_mut()
                    .notify(HostEvent::Notice(MessagePayload {
                        message: format!("工具调用 id 线上修复：{detail}"),
                    }));
            }
        }
        // 运行时在本回合内只构建一次，整段循环（含失败重试）都借用同一实例；
        // 跨回合仍命中同一实例时连构建都省掉（见 `Conn::runtime_cache`）。
        let runtime = self.runtime_handle()?;
        let runtime: &dyn ModelRuntime = runtime.runtime.as_ref();
        let parts = self.request_parts()?;
        let (tools, name_map) = (&parts.tools, &parts.name_map);
        let options = &parts.options;
        let identity = self.config.prompt_cache_identity.clone();
        let limit = self.config.request_retry_count.max(1);
        let mut attempt = 0;
        // 重试前的 call_id 轮换开关：只有网关明确判过「重复提交相同 call_id」才打开。
        let mut rotate_ids = false;

        loop {
            // 轮换只发生在重试路径：网关的重复提交判定按 id 认，原样再发等于再犯一次。
            // 只轮换尾部一批（最近一条 assistant `tool_calls` 及其结果），历史前缀逐字保留。
            let attempt_messages = if rotate_ids {
                rotate_ids = false;
                let mut list = wire_messages.clone();
                let rotated = rotate_tail_call_ids(&mut list, &format!("r{attempt}"));
                self.notify_retry(format!(
                    "网关判定工具调用重复提交，重试前已轮换 {rotated} 个 call_id"
                ));
                list
            } else {
                wire_messages.clone()
            };
            // 摘要请求要逐字复用最近一次真实发出的消息（含重试轮换后的 id）。
            {
                let mut usage = self.usage.borrow_mut();
                usage.last_request_messages = attempt_messages.clone();
                usage.last_request_input_tokens = 0;
            }
            let conversation = conversation_from_openai_messages(&attempt_messages);
            let input = ChatRequestInput {
                model: &self.config.model,
                system_prompt: &self.config.system_prompt,
                messages: &conversation,
                tools,
                options,
                profile_request_timeout_seconds: self
                    .config
                    .request_timeout_seconds
                    .filter(|seconds| *seconds > 0.0)
                    .unwrap_or_default(),
                prompt_cache_capable: self.config.prompt_cache_capable,
                prompt_cache_identity: &identity,
            };
            let mut sink = ProtocolSink {
                conn: Rc::clone(&self.conn),
                usage: Rc::clone(&self.usage),
            };
            match runtime.run_turn(&input, &mut sink) {
                Ok(reply) => {
                    if is_empty_reply(&reply) {
                        attempt += 1;
                        if attempt < limit {
                            self.notify_retry(format!("正在重试(第{attempt}次)"));
                            continue;
                        }
                        return Err(self.model_error(format!(
                            "Agent 连续 {limit} 次返回空响应，已停止本轮请求。"
                        )));
                    }
                    let mut reply = reply;
                    name_map.restore_reply(&mut reply);
                    let reply = to_agent_reply(reply)?;
                    self.notify_model_response(&reply);
                    remember_active_assistant(&self.active_assistant, &reply);
                    return Ok(reply);
                }
                Err(error) => {
                    if error.kind == RuntimeErrorKind::Cancelled {
                        return Err(LoopError::Cancelled(error.message));
                    }
                    if error.message.contains(CONTEXT_LENGTH_EXCEEDED_MESSAGE) {
                        self.usage.borrow_mut().context_overflow = true;
                    }
                    attempt += 1;
                    // 内核策略（刻意偏离 Python 的 `is_retryable_model_request_error`）：
                    // 任何**非取消**的请求失败都先退避重试——网络抖动、网关 5xx、限流，甚至
                    // 瞬时被拒，都不该让整段会话直接断掉；确定性错误重试几次后仍会如实上报。
                    // 上下文超限例外：它要走压缩恢复路径，不能在这里耗重试。
                    let overflow = error.message.contains(CONTEXT_LENGTH_EXCEEDED_MESSAGE)
                        || self.usage.borrow().context_overflow;
                    let retryable = !overflow;
                    if retryable && attempt < limit {
                        // 已经推给界面的半截流要先撤销，否则重试会叠在旧文本上。
                        if error.kind == RuntimeErrorKind::StreamInterrupted {
                            self.conn.borrow_mut().notify(HostEvent::StreamRollback);
                        }
                        // 退避前先把入站帧抽干：这段时间里用户按 ESC 也能立刻止痛。
                        if drain_inbound_during_turn(&self.conn) {
                            return Err(LoopError::Cancelled("回合已取消。".to_string()));
                        }
                        self.notify_retry(format!("请求失败，正在自动重试（第{attempt}次）"));
                        // 这次失败是网关在说「同一个 call_id 又提交了一次」：原样重发只会
                        // 再被拒，下一轮用轮换过的尾部 id 发，这份提交就不再是同一份。
                        rotate_ids = is_tool_state_rejection(&error.message);
                        let backoff_ms = (400u64 << (attempt - 1).min(3)).min(2_000);
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                        continue;
                    }
                    if retryable {
                        return Err(self.model_error(format!(
                            "上游错误已达到本回合自动重试上限，已停止本次请求。{}",
                            error.message
                        )));
                    }
                    return Err(self.model_error(error.message));
                }
            }
        }
    }
}

impl KernelModelPort {
    /// 取本回合的模型运行时：命中本连接的缓存则直接复用，缺失或配置变了才重建。
    ///
    /// 凭据只从环境读；端点与协议缺省时用内核工厂的默认值。
    fn runtime_handle(&self) -> Result<Rc<KernelModelRuntime>, LoopError> {
        if let Some(cached) = self.runtime.borrow().clone() {
            return Ok(cached);
        }
        let api_key = read_api_key(&self.config)?;
        let session_id = self
            .conn
            .borrow()
            .session
            .as_ref()
            .map(|session| session.session_id.clone())
            .unwrap_or_default();
        let key = RuntimeBuildKey::of(&self.config, &session_id, &api_key);
        let cached = self
            .conn
            .borrow_mut()
            .model_runtime(&key, || build_model_runtime_with_key(&self.config, api_key))?;
        *self.runtime.borrow_mut() = Some(Rc::clone(&cached));
        Ok(cached)
    }

    /// 取本回合的请求侧派生数据（工具声明、生成选项）。
    ///
    /// 两者在回合内不变，而热路径上一轮会调用多次（每次工具观察之后都会再来一次）：
    /// `KernelModelPort` 持有的是回合开始时定下的配置快照，回合中途不会被改写，
    /// 因此构建一次即可。
    fn request_parts(&self) -> Result<std::cell::Ref<'_, TurnRequestParts>, LoopError> {
        if self.request.borrow().is_none() {
            let built = TurnRequestParts::build(&self.config)?;
            *self.request.borrow_mut() = Some(built);
        }
        Ok(std::cell::Ref::map(self.request.borrow(), |slot| {
            slot.as_ref().expect("回合请求部件刚被写入")
        }))
    }

    fn notify_retry(&self, message: String) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::RetryStatus(MessagePayload { message }));
    }

    /// `model.request.error`：一次模型请求以协议错误终结时的通知（取消不算）。
    fn notify_model_error(&self, message: &str) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::ModelRequestError(ModelRequestErrorPayload {
                error: message.to_string(),
                model: self.config.model.clone(),
            }));
    }

    /// 记一条错误通知并把它原样折成循环错误。
    fn model_error(&self, message: String) -> LoopError {
        self.notify_model_error(&message);
        LoopError::ReplySource(message)
    }

    /// `model.response.after`：一次模型请求成功返回后的观察通知。
    fn notify_model_response(&self, reply: &AgentModelReply) {
        self.conn
            .borrow_mut()
            .notify(HostEvent::ModelResponseAfter(ModelResponseAfterPayload {
                model: self.config.model.clone(),
                content: reply.content.clone(),
                tool_call_count: reply.tool_calls.len(),
            }));
    }
}


/// Provider 告警 → 宿主事件。
///
/// 出网脱敏的还原类告警（`desensitization_*`）说的是一段**已经落到对话里**的内容
/// （畸形/未注册占位符被原样保留），属于会话流里的一条提示，不是「正在做什么」的
/// 运行状态；映射成 `turn.status` 会让它顶掉输入框上边框的状态行，甚至盖住
/// 「正在调用」而看不出回合还在跑。因此它走 `turn.notice`，由宿主写进会话流。
/// 其余告警（网关降级重试等）本就是进行中的状态，继续走状态行。
pub(super) fn provider_warning_event(warning: &omnicrawl_protocol::ProviderWarning) -> HostEvent {
    let payload = MessagePayload {
        message: warning.message.clone(),
    };
    if warning.code.starts_with("desensitization_") {
        HostEvent::Notice(payload)
    } else {
        HostEvent::Status(payload)
    }
}

/// 按协议组装模型运行时：Provider 的默认 API 根、能力合并与运行时选择都在内核工厂里做。
///
/// 内核没有 Profile id（宿主才有），因此这里的 `profile_id` 留空。
pub(crate) fn build_model_runtime(
    config: &KernelModelConfig,
) -> Result<Box<dyn ModelRuntime>, LoopError> {
    let api_key = read_api_key(config)?;
    build_model_runtime_with_key(config, api_key)
}

/// 同上，但复用调用方已经解析好的凭据（压缩摘要那条链自己读一次环境变量）。
pub(crate) fn build_model_runtime_with_key(
    config: &KernelModelConfig,
    api_key: String,
) -> Result<Box<dyn ModelRuntime>, LoopError> {
    let provider = if config.provider.trim().is_empty() {
        "openai"
    } else {
        config.provider.trim()
    };
    let profile = ProviderProfile {
        id: String::new(),
        provider: provider.to_string(),
        base_url: config.base_url.clone(),
        api_key,
        user_agent: config.user_agent.clone(),
        default_protocol: config.protocol.clone(),
    };
    let descriptor = ModelDescriptor {
        model_id: config.model.clone(),
        protocol: config.protocol.clone(),
        capabilities: None,
        context_window_tokens: config.context_window_tokens,
        max_output_tokens: None,
    };
    let bundle = build_runtime(&profile, &descriptor)
        .map_err(|error| LoopError::ReplySource(error.message))?;
    Ok(maybe_wrap_desensitization(bundle.runtime))
}

/// 出网脱敏装饰器（Python `maybe_wrap_runtime`）：按进程配置的 `[desensitization]` 决定是否包装。
///
/// 语义基准是 Python `registry.py::build_runtime` 在返回处调用 `maybe_wrap_runtime`——
/// 配置读取失败按「未启用」处理（零成本原样返回），不影响模型运行时构建。
/// 内核自带 provider 运行时后，脱敏也必须跟到内核侧，否则内核独立运行时会丢失出网脱敏。
pub(super) fn maybe_wrap_desensitization(runtime: Box<dyn ModelRuntime>) -> Box<dyn ModelRuntime> {
    let environment = ConfigEnvironment::from_process();
    let Ok(config) = load_desensitization_config(&environment, None) else {
        return runtime;
    };
    let options = desensitization_options(&config);
    DesensitizationRuntime::maybe_wrap(runtime, &options)
}

/// `[desensitization]` 配置 → 内核装饰器选项（值类型规则层按 `detect_*` 开关裁剪）。
pub(super) fn desensitization_options(config: &DesensitizationConfig) -> DesensitizationOptions {
    let rule_categories: Vec<String> = [
        (CATEGORY_PEM_PRIVATE_KEY, config.detect_pem_private_key),
        (
            CATEGORY_DB_CONNECTION_STRING,
            config.detect_db_connection_string,
        ),
        (CATEGORY_EMAIL, config.detect_email),
        (CATEGORY_BANK_CARD, config.detect_bank_card),
        (CATEGORY_INTERNAL_IP, config.detect_internal_ip),
        (CATEGORY_EXTERNAL_IP, config.detect_external_ip),
        (CATEGORY_URL, config.detect_url),
        (CATEGORY_MAC_ADDRESS, config.detect_mac_address),
        (CATEGORY_LICENSE_PLATE, config.detect_license_plate),
    ]
    .into_iter()
    .filter(|(_, enabled)| *enabled)
    .map(|(category, _)| category.to_string())
    .collect();
    let gitleaks_config_path = config.gitleaks_config_path.trim();
    DesensitizationOptions {
        enabled: config.enabled,
        strict_restore: config.strict_restore,
        fail_closed: config.fail_closed,
        entropy_enabled: config.entropy_enabled,
        entropy_min_length: config.entropy_min_length.max(0) as usize,
        entropy_min_bits: config.entropy_min_bits,
        entropy_pure_letters: config.entropy_pure_letters,
        entropy_pure_digits: config.entropy_pure_digits,
        rule_categories,
        gitleaks_enabled: config.gitleaks_enabled,
        gitleaks_config_path: if gitleaks_config_path.is_empty() {
            None
        } else {
            Some(gitleaks_config_path.to_string())
        },
        extra_sensitive_keys: config.extra_sensitive_keys.clone(),
        exempt_keys: config.exempt_keys.clone(),
    }
}

/// 凭据直接取自帧里的明文字段，不再读环境变量。
pub(crate) fn read_api_key(config: &KernelModelConfig) -> Result<String, LoopError> {
    Ok(config.api_key.trim().to_string())
}

pub(crate) fn parse_options(config: &KernelModelConfig) -> Result<GenerationOptions, LoopError> {
    if config.options.is_null() {
        return Ok(GenerationOptions::default());
    }
    serde_json::from_value(config.options.clone())
        .map_err(|error| LoopError::ReplySource(format!("模型配置里的 options 无法解析：{error}")))
}

pub(super) fn parse_tools(config: &KernelModelConfig) -> Vec<ToolSpec> {
    config
        .tools
        .iter()
        .filter_map(tool_spec_from_openai_item)
        .collect()
}

pub(super) fn is_empty_reply(reply: &ModelReply) -> bool {
    reply.content.trim().is_empty() && reply.tool_calls.is_empty()
}

/// 网关是否在说「同一请求重复提交了相同的 call_id」。
///
/// 判定与解释都在 [`omnicrawl_llm::looks_like_tool_state_rejection`]：这类错误面里带着网关
/// 自己的原话，重试时把尾部工具调用的 id 换一批就不算同一份提交了。
pub(super) fn is_tool_state_rejection(message: &str) -> bool {
    omnicrawl_llm::looks_like_tool_state_rejection(message)
}

/// 归并后的回复 → 循环要的形状；assistant 消息按 OpenAI 形状回填进上下文。
pub(super) fn to_agent_reply(reply: ModelReply) -> Result<AgentModelReply, LoopError> {
    let mut assistant = to_openai_messages("", std::slice::from_ref(&reply.assistant_message));
    let message = assistant
        .pop()
        .unwrap_or_else(|| json!({"role": "assistant", "content": reply.content}));
    let tool_calls = reply
        .tool_calls
        .iter()
        .map(|call| ToolCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            id: call.call_id.clone(),
            function_name: call.name.clone(),
        })
        .collect();
    Ok(AgentModelReply {
        message,
        content: reply.content,
        tool_calls,
        reasoning: reply.reasoning,
        content_streamed: reply.content_streamed,
    })
}

/// 本批「含工具调用的 assistant 原文」暂存格（Python 的 `_active_assistant_tool_message`）。
///
/// 模型回复带工具调用时写进来，同一批的工具批次读走——`tool_call_requested` 事件要带回
/// `assistant_content` / 思考回传字段 / `function_name`，否则恢复后同一段历史换一种写法，
/// 前缀缓存必然失效（Python 侧的原话）。**取走即清空**：批与批之间不残留，免得下个模型
/// 回合的事件带上错位的 `assistant_content`。
pub(super) type ActiveAssistantSlot = Rc<RefCell<Option<Value>>>;

/// 记下这一批的 assistant 协议原文；只有「带工具调用」的回复才需要暂存。
pub(super) fn remember_active_assistant(slot: &ActiveAssistantSlot, reply: &AgentModelReply) {
    if reply.tool_calls.is_empty() || !reply.message.is_object() {
        return;
    }
    *slot.borrow_mut() = Some(reply.message.clone());
}

/// 从 `initialize` 参数里取会话配置；给了就给，缺字段或形状不对按「没给」处理。
pub(super) fn session_config_of(params: Option<&Value>) -> Option<Box<KernelSessionConfig>> {
    let value = params?.get("session")?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<KernelSessionConfig>(value.clone())
        .ok()
        .map(Box::new)
}

/// 从 `initialize` 参数里取「宿主支持工具调用淘汰裁决」的能力声明；缺省 false。
pub(super) fn tool_call_prune_of(params: Option<&Value>) -> bool {
    params
        .and_then(|value| value.get("tool_call_prune"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 这份模型设置除了 `tool_call_prune` 之外是否没有任何字段（纯能力更新）。
///
/// 纯能力更新不该走模型配置的校验：它不碰模型，也就不能在「内核没有模型配置」时被拒
/// （报告模式的嵌入宿主同样要能改这个开关）。
pub(super) fn settings_without_prune_is_empty(settings: &SessionModelSettings) -> bool {
    let mut rest = settings.clone();
    rest.tool_call_prune = None;
    rest == SessionModelSettings::default()
}

/// 从 `initialize` 参数里取模型配置；给了就给，缺字段或形状不对按「没给」处理。
pub(super) fn model_config_of(params: Option<&Value>) -> Option<Box<KernelModelConfig>> {
    let value = params?.get("model")?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<KernelModelConfig>(value.clone())
        .ok()
        .map(Box::new)
}

/// 一次已执行的调用 → 淘汰裁决的送审形状。
///
/// 参数走公开投影（`public_tool_arguments`，与事件落盘同一份）：进决策请求的正文因此不含
/// shell 全文与密钥原文；结果取**模型可见输出**（`ToolResult::output`，与回填模型的文本同源）。
pub(super) fn prune_candidate_of(observation: &AgentLoopObservation) -> PruneCandidate {
    let tool = observation.tool_call.name.as_str();
    let arguments = public_tool_arguments(tool, &observation.tool_call.arguments);
    PruneCandidate::bounded(
        observation.tool_call.id.clone(),
        tool,
        omnicrawl_controllers::json::python_dumps_compact(&arguments),
        observation.result.ok,
        &observation.result.output,
    )
}
