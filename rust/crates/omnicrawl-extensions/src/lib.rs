//! 扩展子系统：`omnicrawl/extensions/` 的 Rust 内核移植。
//!
//! 语义基准是 Python 侧的真实现，两侧靠 `rust/tools/gen_extensions_*_fixture.py` 生成的
//! 对照数据集对齐，不靠人读代码。
//!
//! | Python | Rust |
//! | --- | --- |
//! | `extensions/plugin_models.py` | [`models`] |
//! | `extensions/plugin_registry.py` | [`registry`] |
//! | `extensions/skill.py` | [`skill`] |
//! | `extensions/plugin_protocol.py` | [`protocol`] |
//! | `extensions/plugin_manager.py` | [`manager`] |
//! | `extensions/plugin_install.py` | [`install`] |
//!
//! 边界：`node_runner.mjs` 是插件 Worker 的 JS 端点，插件生态在 JS 侧，它不进内核；
//! 内核持有的是宿主侧的全部逻辑（注册表、执行计划、Worker 生命周期、安装与 Skill 生命周期）。

pub mod error;
pub mod install;
pub mod manager;
pub mod models;
mod path;
pub mod protocol;
pub mod registry;
pub mod skill;
