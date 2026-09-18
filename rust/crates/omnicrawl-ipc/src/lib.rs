//! 宿主桥接协议（协议 v1）。
//!
//! 内核与宿主是两个进程：内核持有协议层、回合循环与 Provider 流解析，宿主持有传输、
//! 工具、审批、会话与界面。两侧只用一条 NDJSON 流通信，一行一个 JSON-RPC 2.0 帧；
//! 形状与插件通路（`omnicrawl/extensions/node_runner.mjs`）一致，宿主不必为内核另写解帧。
//!
//! - 帧与错误码：`frame`
//! - 版本协商：`version`
//! - 方法语义与负载：`bridge`
//!
//! 方法清单、负载字段与错误约定见 `rust/docs/protocol-v1.md`。

pub mod bridge;
pub mod frame;
pub mod version;

pub use bridge::{
    method, BridgeError, Command, HostEvent, InitializeParams, KernelCompactionConfig,
    KernelSessionConfig, ModelRequest, PendingCommand, ToolBatch, ToolBatchResult,
    TurnCancelParams, TurnSubmitParams,
};
pub use frame::{error_code, ErrorObject, Frame, FrameError, Id, JSONRPC_VERSION};
pub use version::{negotiate_version, VersionError, PROTOCOL_VERSION, SUPPORTED_MAJOR};
