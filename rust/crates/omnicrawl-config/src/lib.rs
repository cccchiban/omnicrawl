//! 配置域（`omnicrawl/config/`）的 Rust 移植。
//!
//! 内核要脱离宿主独立运行时，必须自己回答「用哪个模型、走哪个协议、开哪些功能」，
//! 因此这里按 Python 侧的三层结构原样搬运：
//!
//! - [`core`]：配置仓库基础（TOML 读写、路径解析、原子写回）、启动初始化与通用设置；
//! - [`models`]：模型、LLM、渠道与视觉相关配置；
//! - [`features`]：Agent 功能特性配置（审批、压缩、图像、子代理、工具）。
//!
//! 与 Python 的差别集中在两处：进程外信息由 [`core::runtime::ConfigEnvironment`] 注入；
//! 需要网络或子进程的判定（模型发现、凭据探测）只搬判定与文案，不做 I/O。
//! 逐项边界见 `README.md`。

pub mod core;
pub mod error;
pub mod features;
pub mod models;
pub mod toml;
pub mod value;

pub use error::ConfigError;
