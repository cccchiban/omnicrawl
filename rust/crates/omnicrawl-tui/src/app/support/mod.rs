//! `App` 之外的自由函数。按变更理由分三块，避免同一个文件被三批人同时改。
//!
//! 这些函数不碰 `App` 状态，因此从 `app.rs` 里独立出来；各子模块用 `use super::super::*`
//! 复用同一批导入，不必重复依赖清单。

mod forms;
mod rows;
mod runtime;

pub(crate) use forms::*;
pub(crate) use rows::*;
pub(crate) use runtime::*;

// 这一项是 crate 外可见的 API（`app` 模块对外导出），不能被 `pub(crate) use` 收窄。
pub use runtime::is_ctrl_v_release;
