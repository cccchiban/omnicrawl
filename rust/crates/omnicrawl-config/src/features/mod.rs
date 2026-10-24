//! Agent 功能特性配置（审批、压缩、工具开关、运行护栏、子代理、图像、TTS、脱敏…）。
//!
//! 每个模块对应 `omnicrawl/config/features/` 下的一个 Python 文件；读盘与写回都经
//! [`crate::core::runtime`]，进程外信息由 [`crate::core::runtime::ConfigEnvironment`] 注入。

pub mod advisor;
pub mod agent_workspace;
pub mod approval;
pub mod context_compaction;
// Rust 侧新增：Computer Use 桌面操作的开关与预算（Python 无对映）。
pub mod computer_use;
// Rust 侧新增：结构化决策模型（Python 无对映），独立成文件与模块，避开 parity 数据集。
pub mod decision_model;
pub mod desensitization;
// Rust 侧新增：向量模型（云端嵌入接口，Python 无对映）。
pub mod embedding;
pub mod image_gen;
pub mod run_guard;
pub mod skills;
pub mod subagents;
pub mod tls_resumption;
pub mod tool_output_compression;
pub mod tools;
pub mod tts;
// 云端 TTS 的服务商路由与两家接口参数（Rust 侧新增，Python 无对映）。
pub mod tts_cloud;
// 联网搜索的通路与凭据（Rust 侧新增，Python 无对映）。
pub mod web_search;
