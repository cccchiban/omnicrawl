//! Windows 沙盒执行体（P1）：AppContainer 身份 + Job Object 限额 + 裸 `CreateProcessW`。
//!
//! 设计见 `rust/assets/docs/sandbox_windows_design.md` §4.2.1、§6、§8。这里只做执行，
//! 「该不该拦」的判定仍由 [`super::Sandbox`] 负责——两者分工与 P0 一致。
//!
//! 三条约束决定了本模块的形状：
//!
//! 1. **不能用 `CreateProcessAsUserW`**：它要求 `SE_INCREASE_QUOTA_NAME` /
//!    `SE_ASSIGNPRIMARYTOKEN_NAME`，普通交互式进程会以 `ERROR_PRIVILEGE_NOT_HELD` 失败。
//!    正确做法是用 `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 修饰一次普通的
//!    `CreateProcessW`（零特权）。
//! 2. **`std::process::Child` 无法从 `PROCESS_INFORMATION` 构造**（字段私有），所以本模块
//!    自持 `hProcess`/`hThread`/管道句柄，提供 [`SandboxChild`]；`tools/command.rs` 的轮询
//!    循环对两种子进程共用。
//! 3. **AppContainer 是受限令牌**：访问检查要求常规 SID 链与受限 SID 链（只含 package SID
//!    与能力 SID）**同时**放行。因此不仅要给工作区授权，还要给通往它的每一级父目录加
//!    遍历权——否则工作区的 ACE 根本走不到。这是真机上最容易踩空的一处。
//!
//! 模块按「变更理由」划分：`base` 是与 Win32 无直接关系的常量与错误映射；`acl` 管目录
//! 授权；`environment` 决定子进程能看到什么；`toolchain` 探测解释器目录；`process` 掌管
//! 句柄、管道与子进程；`network` 是回环豁免与能力 SID；`isolation` 串起上述各块的容器
//! 生命周期。`toolchain` 特意不叫 `runtime`，避免与 [`crate::sandbox::runtime`] 混淆。

mod acl;
mod base;
mod environment;
mod isolation;
mod network;
mod process;
mod toolchain;

pub use process::{append_quoted, build_command_line};
pub use environment::{build_environment, debug_environment};
pub use isolation::{Container, Isolation};
pub use network::LoopbackExemption;
pub use process::{SandboxChild, Spawned, Streamed, Terminator};
