//! 控制台输入模式自愈（对映 Python `ui/fullscreen/terminal/handling.py` 的
//! `_restore_windows_vt_input_mode_if_needed` 与周期看门狗）。
//!
//! 背景：Windows 上锁屏 / 息屏 / 从休眠恢复时，系统会把控制台模式重置回普通
//! 模式；`cmd`、`python` 等工具子进程也会把共享控制台的鼠标输入位清掉。模式一旦
//! 被重置，crossterm 就再也收不到鼠标与功能键记录（变成未处理的
//! `MOUSE_EVENT` / 空字符虚拟键），界面看起来「鼠标失灵、方向键没反应」，
//! 而且不会收到任何可识别事件来触发恢复。
//!
//! 对策：周期核对控制台模式，被改写时按同一套规则改回去，并重新发出鼠标报告
//! 与焦点报告的终端协议序列。核对失败（重定向输入、非控制台宿主、终端正在关闭）
//! 一律按「未恢复」处理，不把异常抛给事件循环——Python 侧同样把这里定义为自愈
//! 边界而非主流程。
//!
//! 目标模式必须与 crossterm 自己设的值**整值一致**，这是本模块最容易写错的地方：
//! crossterm 0.29 的 Windows 事件源走 win32 控制台记录路径（`INPUT_RECORD` 交给
//! `handle_key_event` / `handle_mouse_event`），**不需要** VT 输入位；而且它的
//! `EnableMouseCapture` 是把模式整体覆盖成 `0x0010 | 0x0080 | 0x0008`
//! （`event/sys/windows.rs` 的 `ENABLE_MOUSE_MODE`）。若这里要求的目标里多出任何一个位
//! （例如按 Python 的 VT 驱动那样要求 `ENABLE_VIRTUAL_TERMINAL_INPUT`），就会出现
//! 「自愈补上该位 → 重发鼠标捕获又把它抹掉 → 下一秒再次判定被重置」的一秒一次自激
//! （曾真实报障：界面上刷满「控制台输入模式被外部重置」）。
//!
//! 快速编辑位、回显位、行输入位、处理输入位都不在目标值里，因此同样被压掉。

/// 控制台模式自愈的平台实现；非 Windows 平台是空实现。
#[cfg(windows)]
mod platform {
    use std::ffi::c_void;

    use super::HealOutcome;

    /// `GetStdHandle` 的标准设备编号（值为 `-10` / `-11` 的无符号表示）。
    const STD_INPUT_HANDLE: u32 = 0u32.wrapping_sub(10);
    const STD_OUTPUT_HANDLE: u32 = 0u32.wrapping_sub(11);
    /// `INVALID_HANDLE_VALUE`。
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    // 控制台模式位（`wincon.h`）。
    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
    /// 「非 raw」位（回显 / 行输入 / 处理输入 / 快速编辑）：目标值里必须一个都没有。
    const NON_RAW_MASK: u32 =
        ENABLE_QUICK_EDIT_MODE | ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT;
    /// crossterm `event/sys/windows.rs` 的 `ENABLE_MOUSE_MODE`：鼠标捕获时的整值目标，
    /// 也是本模块自愈后必须落到的值（改这里等于改 crossterm 的行为，必须同步核对）。
    const CROSSTERM_MOUSE_MODE: u32 =
        ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT;
    /// 编译期自证：整值目标确实压掉了那几个位（数字写错就编不过，不靠运行期测试兜）。
    const _: () = assert!(CROSSTERM_MOUSE_MODE & NON_RAW_MASK == 0);
    /// 输出句柄的 VT 处理位（清掉它 ANSI 序列会被当普通文本打印）。
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }

    /// 「健康」的输入模式：与 crossterm 鼠标捕获设的整值完全一致，因此是自愈的不动点。
    ///
    /// 必须是**整值**而不是「在现有值上补位」：crossterm 的 `EnableMouseCapture` 会把模式
    /// 整体覆盖成 [`CROSSTERM_MOUSE_MODE`]，只要目标里多出任何一个位（例如 VT 输入位），
    /// 每次重发鼠标捕获都会把模式推回 `0x0098`，下一秒又被判定成「被外部重置」，形成
    /// 一秒一次的提示刷屏。参数保留是为了让调用方与测试能表达「当前模式是什么」，
    /// 返回值故意与它无关。
    pub fn required_input_mode(_current: u32) -> u32 {
        CROSSTERM_MOUSE_MODE
    }

    /// 读取句柄的控制台模式。
    fn console_mode(handle: *mut c_void) -> Option<u32> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut mode = 0u32;
        // SAFETY: 句柄来自 `GetStdHandle`，`mode` 是本函数的栈变量。
        let ok = unsafe { GetConsoleMode(handle, &mut mode) };
        (ok != 0).then_some(mode)
    }

    fn set_console_mode(handle: *mut c_void, mode: u32) -> bool {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        // SAFETY: 同 `console_mode`。
        unsafe { SetConsoleMode(handle, mode) != 0 }
    }

    /// 核对并恢复控制台输入模式。
    pub fn heal_console_input_mode() -> HealOutcome {
        // SAFETY: 只读取标准句柄，不做所有权转移。
        let (input, output) = unsafe {
            (
                GetStdHandle(STD_INPUT_HANDLE),
                GetStdHandle(STD_OUTPUT_HANDLE),
            )
        };
        let Some(input_mode) = console_mode(input) else {
            return HealOutcome::Unavailable;
        };
        let required = required_input_mode(input_mode);
        if input_mode == required {
            return HealOutcome::NoChange;
        }
        if !set_console_mode(input, required) {
            return HealOutcome::Unavailable;
        }
        // 输出侧同样可能被重置：VT 处理关闭后所有 ANSI 序列都会原样打印成乱码。
        if let Some(output_mode) = console_mode(output) {
            let wanted = output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
            if wanted != output_mode {
                set_console_mode(output, wanted);
            }
        }
        HealOutcome::Restored
    }
}

#[cfg(not(windows))]
mod platform {
    use super::HealOutcome;

    /// 非 Windows 平台永远不需要自愈（终端模式由 termios 与本进程共同维护）。
    pub fn heal_console_input_mode() -> HealOutcome {
        HealOutcome::NoChange
    }
}

/// 一次核对的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealOutcome {
    /// 模式未被改写；不需要任何动作（非 Windows 平台恒为此值）。
    NoChange,
    /// 模式被外部重置，已恢复（调用方要重发终端协议序列）。
    Restored,
    /// 当前不是控制台（重定向输入 / 管道 / 终端已关闭）；本次跳过。
    Unavailable,
}

impl HealOutcome {
    /// 是否真的恢复过——只有 `true` 时才需要重发鼠标与焦点协议序列。
    pub fn is_restored(self) -> bool {
        matches!(self, Self::Restored)
    }
}

pub use platform::heal_console_input_mode;
#[cfg(windows)]
pub use platform::required_input_mode;

#[cfg(test)]
mod tests {
    use super::*;

    /// 自愈目标必须与 crossterm 的 `ENABLE_MOUSE_MODE` 逐位一致。
    ///
    /// 这条断言是防回归的关键：目标一旦多出 VT 输入位（0x0200），自愈就会和
    /// crossterm 的鼠标捕获互相覆盖，界面上会刷满「控制台输入模式被外部重置」。
    #[test]
    #[cfg(windows)]
    fn required_mode_matches_crossterm_mouse_mode() {
        // 数值抄自 crossterm 0.29 `event/sys/windows.rs` 的 ENABLE_MOUSE_MODE。
        const EXPECTED: u32 = 0x0010 | 0x0080 | 0x0008;
        let required = required_input_mode(0x0001 | 0x0040); // 任意「被重置」的当前值
        assert_eq!(required, EXPECTED, "目标值必须等于 crossterm 的鼠标捕获模式");
        assert_eq!(required & 0x0200, 0, "不得要求 VT 输入位（crossterm 会覆盖掉它）");
        assert_eq!(required & 0x0001, 0, "处理输入必须关闭");
        assert_eq!(required & 0x0004, 0, "回显必须关闭");
        assert_eq!(required & 0x0002, 0, "行输入必须关闭");
        assert_eq!(required & 0x0040, 0, "快速编辑必须关闭");
    }

    /// 模式已经是目标值时自愈必须判定为「无需改动」，避免每周期都重设一次。
    #[test]
    #[cfg(windows)]
    fn already_healthy_mode_is_not_touched() {
        let healthy = required_input_mode(0x0200);
        assert_eq!(required_input_mode(healthy), healthy, "已健康的模式是自愈不动点");
        assert_eq!(healthy, 0x0098, "健康值就是 crossterm 鼠标捕获后的值");
    }

    /// 非 Windows 平台没有可恢复的模式。
    #[test]
    #[cfg(not(windows))]
    fn other_platforms_never_report_a_restore() {
        assert_eq!(heal_console_input_mode(), HealOutcome::NoChange);
        assert!(!heal_console_input_mode().is_restored());
    }
}
