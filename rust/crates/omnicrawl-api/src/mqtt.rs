//! MQTT 自动化回路的宿主侧装配：选规划器、接审计，然后交给连接器主循环。
//!
//! 两种规划器，按 `[mqtt] planner` 选：
//!
//! * `agent`（默认）：拉起内核子进程跑一个回合。模型可以用白名单里的工具查资料、翻记忆、
//!   多步推理，最后输出动作 JSON。审批固定为自动审查，工具表收窄到 `[mqtt] tools`——
//!   「完整工具面」和「不被传感器报文牵着跑」两件事靠白名单同时成立。
//! * `decision`：一次结构化决策。决策模型（`decision_models.toml`）可用时走 choice，
//!   未配置时回落主模型单轮补全。不碰工具、不拉内核，便宜且快。
//!
//! 两条路径最终都汇到同一份 `validate_plan`：动作必须在动作表内，参数按 min/max 夹取。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_config::models::llm::load_llm_config;
use omnicrawl_connectors::agent::{AgentDriver, TurnError, TurnEvent};
use omnicrawl_connectors::mqtt::{
    decision_questions, load_mqtt_config, parse_plan_text, validate_config, AuditLog, MqttBot,
    MqttConfig, PlanRequest, Planner, PlannerMode, RawPlan, SYSTEM_PROMPT,
};
use omnicrawl_host::approval::ApprovalMode;
use omnicrawl_host::decision_wire;
use omnicrawl_host::review::{decision_channel_from_config, DecisionReviewOptions};
use omnicrawl_llm::{ChatEndpoint, ChatRequestInput, DiscardSink, OpenAiChatRuntime};
use omnicrawl_protocol::{conversation_from_openai_messages, GenerationOptions, ToolSpec};
use omnicrawl_version::USER_AGENT;
use serde_json::{json, Value};

use crate::connector::HostConnectorDriver;
use crate::service::{connector_options_from_process, resolve_kernel_program};

/// 决策请求的提问 ID：与 decision_questions 里的键一致。
const ACTION_QUESTION: &str = "action";
/// 决策请求的超时上限：现场控制不能等太久，宁可回落规则兜底。
const DECISION_TIMEOUT_CAP_SECONDS: u64 = 60;

/// omnicrawl-host connector mqtt：起回路并阻塞到退出。
pub fn run() -> Result<(), String> {
    let environment = ConfigEnvironment::from_process();
    let data = crate::service::connector_config_data(&environment);
    let config = load_mqtt_config(data.get("mqtt"))?;
    if !config.enabled {
        return Err("mqtt.enabled 为 false：连接器已在配置里关闭。".to_string());
    }
    validate_config(&config)?;
    let audit = AuditLog::open(audit_path(&environment, &config))?;

    match config.planner {
        PlannerMode::Decision => {
            eprintln!("[mqtt] 规划器：单次结构化决策");
            let planner = build_decision_planner(&environment)?;
            let mut bot = MqttBot::new(config, planner, audit)?;
            bot.run()
        }
        PlannerMode::Agent => run_agent(&environment, config, audit),
    }
}

/// 回合循环：内核子进程 + 白名单工具表 + 自动审查。
fn run_agent(
    environment: &ConfigEnvironment,
    config: MqttConfig,
    audit: AuditLog,
) -> Result<(), String> {
    let mut options = connector_options_from_process(environment)?;
    // 无人值守：没有人在终端上点确认，审批只能靠自动审查（审查闸仍会拦下危险调用），
    // 同时把工具表收窄到白名单——白名单外的工具既不声明给模型，宿主也不执行。
    options.approval = ApprovalMode::Review;
    options.registry_options.allowed_tools = Some(config.tools.clone());
    // 等待人工确认的上限压到几秒：无人值守没人会点「批准」，等满默认的几分钟只会
    // 把回合拖到超时。等不到就按拒绝处理（fail-closed），模型改用其它手段或返回 do_nothing。
    options.confirmation_timeout_seconds = options.confirmation_timeout_seconds.min(5.0);
    let program = resolve_kernel_program(environment, None);
    eprintln!(
        "[mqtt] 规划器：内核回合（工具白名单 {} 项：{}）",
        config.tools.len(),
        if config.tools.is_empty() {
            "空".to_string()
        } else {
            config.tools.join(", ")
        }
    );
    let timeout = Duration::from_secs(config.turn_timeout_seconds);
    let driver = Arc::new(
        HostConnectorDriver::start_with(options, &program).map_err(|error| {
            format!(
                "{error}\n提示：确认内核可执行文件与宿主同目录（或设置 OMNICRAWL_BINARY），\
                 或把 [mqtt] planner 改成 decision 走单次决策。"
            )
        })?,
    );
    let planner: Arc<dyn Planner> = Arc::new(AgentPlanner {
        driver: Arc::clone(&driver) as Arc<dyn TurnDriver>,
        timeout,
    });
    let mut bot = MqttBot::new(config, planner, audit)?;
    let result = bot.run();
    driver.shutdown();
    result
}

/// 审计文件：配置给了就用配置（支持 ~ 前缀），否则落在用户配置目录下。
fn audit_path(environment: &ConfigEnvironment, config: &MqttConfig) -> PathBuf {
    let configured = config.audit_path.trim();
    if configured.is_empty() {
        return user_config_dir(environment)
            .join("mqtt")
            .join("audit.jsonl");
    }
    if let Some(rest) = configured.strip_prefix("~/") {
        return environment.home().join(rest);
    }
    PathBuf::from(configured)
}

/// 回合驱动面：把真实驱动与测试桩隔开（测试不需要内核进程）。
trait TurnDriver: Send + Sync {
    fn run_turn(&self, text: &str, events: &mut dyn FnMut(TurnEvent)) -> Result<String, TurnError>;
    fn reset_conversation(&self) -> Result<(), String>;
    fn request_cancel(&self);
}

impl TurnDriver for HostConnectorDriver {
    fn run_turn(&self, text: &str, events: &mut dyn FnMut(TurnEvent)) -> Result<String, TurnError> {
        AgentDriver::run_turn(self, text, events)
    }

    fn reset_conversation(&self) -> Result<(), String> {
        AgentDriver::reset_conversation(self)
    }

    fn request_cancel(&self) {
        AgentDriver::request_cancel(self)
    }
}

/// 回合循环规划器：一次唤醒 = 一个新会话 + 一个回合 + 一次动作解析。
struct AgentPlanner {
    driver: Arc<dyn TurnDriver>,
    timeout: Duration,
}

impl Planner for AgentPlanner {
    fn plan(&self, request: &PlanRequest) -> Result<RawPlan, String> {
        // 每次唤醒都从干净会话开始：上一次遥测的推理对本次没有价值，留着只会累积上下文，
        // 也让「为什么这么判」只取决于本次现场数据与最近动作。
        self.driver.reset_conversation()?;
        let prompt = agent_prompt(request);
        let (sender, receiver) = mpsc::channel();
        let driver = Arc::clone(&self.driver);
        let handle = thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            let result = driver.run_turn(&prompt, &mut sink).map_err(|error| match error {
                TurnError::Cancelled => "回合被取消。".to_string(),
                TurnError::Failed(message) => message,
            });
            let _ = sender.send(result);
        });
        match receiver.recv_timeout(self.timeout) {
            Ok(Ok(text)) => parse_plan_text(&text, "agent"),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                // 超时先取消、再等回合收口：运行记录还占着单活动名额，不等会让下一次触发
                // 直接撞上 RUN_ACTIVE。
                self.driver.request_cancel();
                let _ = handle.join();
                Err(format!(
                    "回合超时（{} 秒），已请求取消。",
                    self.timeout.as_secs()
                ))
            }
        }
    }

    fn describe(&self) -> &'static str {
        "agent"
    }
}

/// 回合输入 = 固定指令 + 现场 JSON。内核的 system prompt 是通用助手人设，
/// 动作格式这类硬约束必须随消息进，否则模型会按对话习惯回答。
fn agent_prompt(request: &PlanRequest) -> String {
    format!(
        "{SYSTEM_PROMPT}\n\n现场数据（JSON）：\n{}",
        request.user_content()
    )
}

/// 决策通道装配：决策渠道优先，其次主模型。
fn build_decision_planner(
    environment: &ConfigEnvironment,
) -> Result<Arc<dyn Planner>, String> {
    let llm = load_llm_config(environment).map_err(|error| error.message().to_string())?;
    let timeout_seconds = if llm.request_timeout_seconds > 0 {
        llm.request_timeout_seconds
            .clamp(5, DECISION_TIMEOUT_CAP_SECONDS as i64) as u64
    } else {
        20
    };

    if let Some(channel) = decision_channel_from_config(environment) {
        eprintln!(
            "[mqtt] 决策通道：结构化决策模型（{} / {}）",
            channel.mode, channel.model
        );
        return Ok(Arc::new(DecisionPlanner {
            channel,
            timeout_seconds,
        }));
    }

    if llm.model.trim().is_empty() || llm.api_key.trim().is_empty() {
        return Err(concat!(
            "没有可用的决策通道：decision_models.toml 未配置决策渠道，主模型也缺少 model 或 api_key。",
            "请至少配置其中一个——无人值守回路不能没有决策者。"
        )
        .to_string());
    }
    eprintln!(
        "[mqtt] 决策通道：主模型 {}（未配置决策模型，按回落规则使用）",
        llm.model
    );
    Ok(Arc::new(MainModelPlanner {
        model: llm.model,
        base_url: llm.base_url,
        api_key: llm.api_key,
        timeout_seconds,
    }))
}

/// 决策模型通道：一次 choice 问题，答案被约束在动作表内。
struct DecisionPlanner {
    channel: DecisionReviewOptions,
    timeout_seconds: u64,
}

impl Planner for DecisionPlanner {
    fn plan(&self, request: &PlanRequest) -> Result<RawPlan, String> {
        let state = request.state();
        let questions = decision_questions(&request.actions);
        let body =
            decision_wire::request_body(&self.channel.mode, &self.channel.model, &state, &questions);
        let body_text =
            serde_json::to_string(&body).map_err(|error| format!("构造决策请求失败：{error}"))?;
        let text = decision_wire::post_decision(
            &self.channel.mode,
            &self.channel.base_url,
            &self.channel.resolve_api_key(),
            &body_text,
            self.timeout_seconds,
            USER_AGENT,
            self.channel.retry_count,
        )?;
        let index = decision_wire::parse_choice_index(
            &self.channel.mode,
            &text,
            ACTION_QUESTION,
            request.actions.len(),
            decision_wire::OPTION_KEY_PREFIX,
        )?;
        let action = request
            .actions
            .get(index)
            .ok_or_else(|| format!("决策模型返回的动作下标越界：{index}"))?;
        Ok(RawPlan {
            action: action.id.clone(),
            params: json!({}),
            reason: String::new(),
            confidence: choice_confidence(&self.channel.mode, &text),
            source: "decision",
        })
    }

    fn describe(&self) -> &'static str {
        "decision"
    }
}

/// 主模型通道：一次单轮补全，输出即动作 JSON。
struct MainModelPlanner {
    model: String,
    base_url: String,
    api_key: String,
    timeout_seconds: u64,
}

impl Planner for MainModelPlanner {
    fn plan(&self, request: &PlanRequest) -> Result<RawPlan, String> {
        let timeout = self.timeout_seconds as f64;
        let generation = GenerationOptions {
            // 现场判定不需要长推理：压到最低，省时也省 token。
            reasoning_effort: "low".to_string(),
            request_timeout_seconds: timeout,
            request_retry_count: 1,
            ..GenerationOptions::default()
        };
        let mut identity: BTreeMap<String, String> = BTreeMap::new();
        identity.insert("mqtt_zone".to_string(), request.zone.clone());
        identity.insert("mqtt_rule".to_string(), request.rule_id.clone());
        identity.insert("model".to_string(), self.model.clone());

        let messages = vec![json!({"role": "user", "content": request.user_content()})];
        let conversation = conversation_from_openai_messages(&messages);
        let tools: Vec<ToolSpec> = Vec::new();
        let endpoint = ChatEndpoint {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            user_agent: USER_AGENT.to_string(),
        };
        let input = ChatRequestInput {
            model: self.model.as_str(),
            system_prompt: SYSTEM_PROMPT,
            messages: &conversation,
            tools: &tools,
            options: &generation,
            profile_request_timeout_seconds: timeout,
            prompt_cache_capable: false,
            prompt_cache_identity: &identity,
        };
        let runtime = OpenAiChatRuntime::new(endpoint);
        let mut sink = DiscardSink;
        let reply = runtime
            .run_turn(&input, &mut sink)
            .map_err(|error| format!("主模型调用失败：{error}"))?;
        parse_plan_text(&reply.content, "main_model")
    }

    fn describe(&self) -> &'static str {
        "main_model"
    }
}

/// 决策响应里的置信度：没有就按缺失处理，不做补零。
fn choice_confidence(mode: &str, text: &str) -> Option<f64> {
    let answers: Value = decision_wire::extract_answers(mode, text).ok()?;
    answers
        .get(ACTION_QUESTION)
        .and_then(|answer| answer.get("confidence"))
        .and_then(Value::as_f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_connectors::mqtt::{ActionSpec, CompareOp};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 桩驱动：模拟内核回合的耗时与输出，不碰真实进程。
    struct StubDriver {
        reply: String,
        delay: Duration,
        resets: AtomicUsize,
        cancelled: AtomicBool,
    }

    impl StubDriver {
        fn new(reply: &str, delay: Duration) -> Self {
            Self {
                reply: reply.to_string(),
                delay,
                resets: AtomicUsize::new(0),
                cancelled: AtomicBool::new(false),
            }
        }
    }

    impl TurnDriver for StubDriver {
        fn run_turn(
            &self,
            _text: &str,
            _events: &mut dyn FnMut(TurnEvent),
        ) -> Result<String, TurnError> {
            thread::sleep(self.delay);
            Ok(self.reply.clone())
        }

        fn reset_conversation(&self) -> Result<(), String> {
            self.resets.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn request_cancel(&self) {
            self.cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn request() -> PlanRequest {
        PlanRequest {
            zone: "3".to_string(),
            rule_id: "hot".to_string(),
            metric: "temp".to_string(),
            value: 31.2,
            threshold: 28.0,
            op: CompareOp::Gt,
            snapshot: json!({"temp": {"last": 31.2}}),
            actions: vec![
                ActionSpec {
                    id: "open_vent".to_string(),
                    label: "打开通风口".to_string(),
                    description: "通风降温".to_string(),
                    params: Vec::new(),
                },
                ActionSpec {
                    id: "do_nothing".to_string(),
                    label: "维持现状".to_string(),
                    description: String::new(),
                    params: Vec::new(),
                },
            ],
            recent: Vec::new(),
            now: "2026-10-29T10:00:00+08:00".to_string(),
        }
    }

    fn planner_with(driver: Arc<StubDriver>, timeout: Duration) -> AgentPlanner {
        AgentPlanner {
            driver: Arc::clone(&driver) as Arc<dyn TurnDriver>,
            timeout,
        }
    }

    #[test]
    fn agent_planner_resets_the_session_then_parses_the_reply() {
        let driver = Arc::new(StubDriver::new(
            "{\"action\":\"open_vent\",\"reason\":\"31.2 度\"}",
            Duration::ZERO,
        ));
        let plan = planner_with(Arc::clone(&driver), Duration::from_secs(5))
            .plan(&request())
            .expect("应当解析出动作");
        assert_eq!(plan.action, "open_vent");
        assert_eq!(plan.source, "agent");
        assert_eq!(
            driver.resets.load(Ordering::Relaxed),
            1,
            "每次唤醒都要从干净会话开始"
        );
    }

    #[test]
    fn agent_planner_rejects_prose_without_action() {
        let driver = Arc::new(StubDriver::new("建议开窗通风", Duration::ZERO));
        let error = planner_with(driver, Duration::from_secs(5))
            .plan(&request())
            .unwrap_err();
        assert!(error.contains("JSON"), "{error}");
    }

    #[test]
    fn agent_planner_cancels_on_timeout() {
        let driver = Arc::new(StubDriver::new("{}", Duration::from_millis(400)));
        let error = planner_with(Arc::clone(&driver), Duration::from_millis(30))
            .plan(&request())
            .unwrap_err();
        assert!(error.contains("超时"), "{error}");
        assert!(
            driver.cancelled.load(Ordering::Relaxed),
            "超时必须请求取消，否则下一次触发会撞上 RUN_ACTIVE"
        );
    }

    #[test]
    fn prompt_carries_the_output_contract() {
        let prompt = agent_prompt(&request());
        assert!(prompt.starts_with(SYSTEM_PROMPT));
        assert!(prompt.contains("\"zone\":\"3\""));
        assert!(prompt.contains("open_vent"));
    }
}
