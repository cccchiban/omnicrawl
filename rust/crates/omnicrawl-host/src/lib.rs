//! `omnicrawl-host`：协议 v1 宿主侧的执行层，被 TUI 与本地 API 共用。
//!
//! 这一层是「内核之外的那半个 Agent」：起内核子进程、收发协议帧、把整批工具调用
//! 定调成审批/提问/执行、再按模型顺序回观察，以及每个工具的执行体（文件、搜索、
//! git、后台任务、知识库、记忆、网页、图像、语音与桌面自动化）。
//!
//! 与 TUI 的分工：这里没有任何终端渲染与按键处理，界面状态留在 TUI。
//! 语义基准是 Python 侧的 `omnicrawl/agent/toolkit/` 与 `omnicrawl/workspace/`。
//!
//! [`plugins`] 是插件 Hook 的宿主编排层：进程级 `PluginRuntime` 的持有者、
//! 会话/回合/工具各节点的载荷拼装，以及安装器与运行期热更新的入口。
//!
//! [`review`] 是审查模型（`approval.mode = review`）的运行期：把删除类、下载并执行类与
//! 高风险 Git 调用交给独立审查模型，失败一律 fail-closed。

pub mod approval;
pub mod host;
pub mod kernel;
pub mod plugins;
pub mod process_control;
pub mod prompt;
// 稳定 prompt 前缀的身份指纹：`initialize.model.prompt_cache_identity` 的组装方。
pub mod prompt_cache;
pub mod review;
pub mod tools;
pub mod turn;
