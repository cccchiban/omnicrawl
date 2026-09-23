//! 渲染层（对映 Python `ui/fullscreen/rendering/`）。
//!
//! Python 侧这里放渲染管线与对话区组件：`pipeline.py`（消息流渲染装配）、
//! `widgets.py`（消息/工具卡/任务清单/确认屏等组件）、`tool_diff.py`（工具 diff
//! 着色）、`latex.py`（LaTeX 排版）、`logo_anim.py` 与 `welcome_logo.py`（启动
//! 标志与逐帧动画）。
//!
//! Rust 侧按文件顺序迁移，公共的样式文本由 [`crate::ui::fullscreen::text`] 承载；
//! `widgets` 已落子任务进度树、任务清单与子任务会话面板，`difflib` 提供
//! `tool_diff` 依赖的 `SequenceMatcher` 等价实现，`welcome_logo` 与 `logo_anim`
//! 提供首屏欢迎 Logo 的静态字形与解密扫描入场动画，`latex` 把数学公式转成
//! Unicode 近似文本，其余组件随后续批次补入。

pub mod difflib;
pub mod latex;
pub mod logo_anim;
pub mod markdown;
pub mod tool_diff;
pub mod welcome_logo;
pub mod widgets;
