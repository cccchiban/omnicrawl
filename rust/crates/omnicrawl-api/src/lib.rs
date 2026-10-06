//! OmniCrawl 本地 HTTP/SSE API 服务端（`omnicrawl/api/` 的 Rust 移植）。
//!
//! 边界：本 crate 只负责 HTTP 面——配置装载与校验、Bearer 鉴权、统一响应信封、
//! CORS 与路由装配。Agent 运行时（回合、工具批次、会话与后台任务）由宿主层提供，
//! 与 Python 侧 `service.py` 只做编排、不碰 HTTP 细节的分工一致。
//!
//! 契约来源：内置文档 `omnicrawl://docs/API.md`；
//! 逐项对照关系与已知差异见 `README.md`。

pub mod app;
pub mod config;
pub mod error;
pub mod model_discovery;
pub mod openapi;
pub mod routes;
pub mod runs;
pub mod service;
pub mod shared_store;

pub use app::{
    api_routes, build_app, build_router, build_router_with_state, merge_guarded,
    reuse_port_supported, serve, token_matches, ApiState,
};
pub use config::{
    api_config_from_section, load_api_config, ApiConfig, ApiConfigError, API_PREFIX,
    WORKER_CHILD_ENV,
};
pub use error::ApiError;
pub use runs::{RunBackend, RunEvent, RunRecord, RunStatus, RunStore, RunStoreError, TodoSnapshot};
pub use service::{AgentService, ServiceOptions};
pub use shared_store::{
    shared_run_store_filename, Decision, SharedRunStore, DECISION_ANSWER, DECISION_CANCEL,
    DECISION_CONFIRM,
};
