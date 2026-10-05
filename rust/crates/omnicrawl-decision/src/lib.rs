//! `omnicrawl-decision`：结构化决策模型的本地 REST 接口。
//!
//! 结构化决策模型（`decision_models.toml`）本来只在宿主内部使用：工具调用审查、检索重排、
//! 提问托管各走各的出站调用。本 crate 把同一份决策能力以 REST 形式暴露给**本机其它程序**
//! （CLI 工具、Skill、外部脚本），使它们不必经主模型就能拿到决策结果——大脑（主模型）负责
//! 规划，小脑（决策模型）负责快速判定，两者通过这个接口协同。
//!
//! 边界：
//! * 只监听回环地址，且**不做鉴权**——任何能访问该地址的调用方都能直接调用，因此配置
//!   拒绝非回环地址（`DecisionApiConfig::normalize`），这是唯一的准入控制。
//! * 决策语义与宿主内部完全同源：请求构造、响应解析、脱敏旁路、审查提问都复用
//!   `omnicrawl_host::decision_wire` 与 `omnicrawl_host::review`，不写第二套。
//! * 本 crate 不碰 Agent 运行态：它只做「把 state + questions 转发给决策服务并读回答案」。
//!
//! 契约与用法见 `omnicrawl://docs/decision_api.md`。

pub mod app;
pub mod config;
pub mod routes;
pub mod server;
pub mod upstream;

pub use app::{build_app, build_router, AppState};
pub use config::{load_settings, DecisionSettings};
pub use routes::runtime_from_config;
pub use server::{ensure_running, health, stop, EnsureOutcome, ServiceState};
pub use upstream::{
    decide, decide_choice, decide_rank, decide_review, ChoiceOutcome, DecideOutcome,
    DecisionContext, UpstreamError, DEFAULT_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS,
};
