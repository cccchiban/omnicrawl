//! Skill 子系统：`omnicrawl/extensions/skill.py` 的 Rust 内核移植。
//!
//! 覆盖 Skill 全生命周期「发现 → 索引 → 匹配 → 注入」；对照数据集由
//! `rust/tools/gen_extensions_*_fixture.py` 生成，两侧靠它对齐，不靠人读代码。
//!
//! | Python | Rust |
//! | --- | --- |
//! | `extensions/skill.py` | [`skill`] |
//!
//! 边界：本 crate 只承载 Skill。插件子系统已于 2026-10-30 从产品中移除，
//! 相关的模型、注册表、Worker 协议、安装器与 Hook 分发一并删除。

pub mod bm25;
mod path;
pub mod skill;
