//! `omnicrawl-tui`：内核协议 v1 的 Rust 宿主前端（全屏终端工作台）。
//!
//! 本 crate 同时产出库与二进制：库部分（状态机与渲染）供集成测试直接驱动，
//! 二进制只负责终端生命周期与事件循环。协议见 `rust/docs/protocol-v1.md`。
//!
//! 宿主执行层（内核客户端、工具批次与工具执行体）已抽到 `omnicrawl-host`，
//! 供 TUI 与本地 API 共用；这里按原路径再导出，历史调用点与集成测试的
//! `omnicrawl_tui::tools::...` 仍然可用。

pub use omnicrawl_host::{approval, host, kernel, tools};

pub mod app;
pub mod args;
pub mod clipboard;
pub mod commands;
pub mod diagnostics;
pub mod image_path;
pub mod monitor;
pub mod paste;
pub mod state;
pub mod ui;
