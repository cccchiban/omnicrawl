//! Textual 全屏工作台的 Rust 对映层。
//!
//! 目录与 Python `omnicrawl/ui/fullscreen/` 对齐：`terminal/` 终端协议与外观、
//! `app/` 装配、`conversation/` 会话视图、`input/` 输入区、`status/` HUD 与遥测、
//! `rendering/` 渲染管线、`turn/` 回合执行、`support/` 非框架支持层。
//!
//! Textual 专属机制（CSS 主题解析、Screen 栈、monkeypatch 门面、挂载竞态补丁）
//! 不逐行照搬，由本层的组件状态、布局函数、事件分派与显式按键处理承接；
//! 文案、取色、按键与可视行为以 Python 侧逐条对齐。

pub mod input;
pub mod random;
pub mod rendering;
pub mod status;
pub mod terminal;
pub mod text;
pub mod tool_labels;

/// 对映 Python 内建 `round`：浮点半数进偶数（banker's rounding）。
///
/// Rust 的 `f64::round` 是半数远离零，与 Python 语义不同，而 HUD 的百分比与
/// 动画总帧数都由 Python `round` 落定，需保持同一取整规则。
pub fn round_half_even(value: f64) -> i64 {
    let floor = value.floor();
    let fraction = value - floor;
    let round_up = fraction > 0.5 || (fraction == 0.5 && (floor as i64) % 2 != 0);
    if round_up {
        (floor + 1.0) as i64
    } else {
        floor as i64
    }
}
