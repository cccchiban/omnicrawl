//! MQTT 自动化回路：遥测进、动作出。
//!
//! 这一组模块把「消息平台连接器」那套形态换成了「事件源 + 执行器」：
//!
//! * 入口不是人发的消息，而是传感器报文（[policy] 里的窗口聚合与规则）；
//! * 出口不是给人看的回复，而是一条带 request_id 与 TTL 的下发指令（[prompt::command_payload]）；
//! * 模型只在规则命中时被调一次（[planner] 的宿主实现），拿完动作就结束，不常驻、不沉睡。
//!
//! 分层：config（配置）→ codec/client（协议与传输）→ policy（确定性策略）→ prompt（模板与校验）
//! → bot（主循环）。只有 bot 把这几层串起来，其余都能单独测试。

pub mod audit;
pub mod bot;
pub mod client;
pub mod codec;
pub mod config;
pub mod planner;
pub mod policy;
pub mod prompt;

pub use audit::AuditLog;
pub use bot::{AckOutcome, MqttBot};
pub use client::{parse_broker_url, BrokerAddress, MqttClient};
pub use config::{
    check_config, load_mqtt_config, validate_config, ActionSpec, CompareOp, MqttConfig, ParamSpec,
    PlannerMode, RuleSpec, DEFAULT_AGENT_TOOLS, DEFAULT_TURN_TIMEOUT_SECONDS,
};
pub use planner::Planner;
pub use policy::{parse_readings, Engine, Reading, Trigger, Window};
pub use prompt::{
    command_payload, decision_questions, parse_plan_text, validate_plan, PlanRequest, RawPlan,
    ValidatedPlan, SYSTEM_PROMPT,
};
