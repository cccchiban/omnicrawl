//! Agent 功能特性配置（审批、压缩、工具开关、运行护栏、子代理、图像、TTS、脱敏…）。
//!
//! 每个模块对应 `omnicrawl/config/features/` 下的一个 Python 文件；读盘与写回都经
//! [`crate::core::runtime`]，进程外信息由 [`crate::core::runtime::ConfigEnvironment`] 注入。

pub mod advisor;
pub mod agent_workspace;
pub mod approval;
pub mod context_compaction;
pub mod desensitization;
pub mod image_gen;
pub mod run_guard;
pub mod subagents;
pub mod tool_output_compression;
pub mod tools;
pub mod tts;
