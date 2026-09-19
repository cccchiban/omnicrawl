//! `omnicrawl-tui`：内核协议 v1 的 Rust 宿主前端（全屏终端工作台）。
//!
//! 本 crate 同时产出库与二进制：库部分（协议宿主、工具执行体、状态机、渲染）供集成
//! 测试直接驱动，二进制只负责终端生命周期与事件循环。协议见 `rust/docs/protocol-v1.md`。

pub mod app;
pub mod args;
pub mod host;
pub mod kernel;
pub mod state;
pub mod tools;
pub mod ui;
