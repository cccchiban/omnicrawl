//! `omnicrawl-fetch`：网页抓取接口（无鉴权的本机 REST）。
//!
//! `fetcher` 工具本来只在 Agent 回合内可用：模型抓网页要走一次工具调用。本 crate 把同一份
//! 抓取能力以 REST 形式暴露给**本机其它程序**（Python 等批量脚本、CLI、Skill），批量脚本
//! 因此不必经过主模型与对话回合就能抓网页。
//!
//! 边界：
//! * 只监听回环地址，且**不做鉴权、不做参数校验**——任何能访问该地址的调用方都能直接抓取。
//!   因此配置拒绝非回环地址（`FetchApiConfig::normalize`），这是唯一的准入控制。
//! * 抓取语义与 `fetcher` 工具完全同源：入参归一化、robots 校验、指纹传输、静态提取与
//!   本地 JS 沙箱重建全部复用 `omnicrawl_host::tools::fetch_batch`，不写第二套。
//! * 每次请求一份独立 cookie 罐：不同调用方的会话不能互相串。
//!
//! 契约与用法见 `omnicrawl://docs/fetch_api.md`。

pub mod app;
pub mod config;
pub mod routes;
pub mod server;

pub use app::{build_app, build_router, ApiError, AppState};
pub use config::{load_settings, FetchSettings};
pub use server::{ensure_running, service_ready, stop, EnsureOutcome, ServiceState};
