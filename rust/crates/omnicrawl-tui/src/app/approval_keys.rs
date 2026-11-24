//! 工具审批确认面板的按键与决策落盘。
//!
//! 面板本身是 `Waitting::Approval`，三行选项在 `ui/panels.rs` 渲染；这里只负责
//! 把按键翻译成批准/拒绝，并在拒绝路径上补 MCP 审计。

use super::*;

impl App {
    /// 审批面板吃按键：上下移选项，回车执行高亮项，`Y` 单次批准，`A` 记名批准。
    ///
    /// 只收选中行号而不是整个面板：面板借自 `AppState`，而批准/拒绝要可变借用它。
    pub(super) fn handle_approval_key(&mut self, selected: usize, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                self.state.select_approval(delta);
            }
            // 两个批准键与三行选项一一对应：回车按当前高亮行执行（默认落在「确认执行」上），
            // `Y` 只批准这一次，`A` 连本会话的同类工具一起批准。
            KeyCode::Char('y') | KeyCode::Char('Y') => self.approve_pending(false),
            KeyCode::Char('a') | KeyCode::Char('A') => self.approve_pending(true),
            KeyCode::Enter => match selected {
                host::APPROVAL_EXECUTE => self.approve_pending(false),
                host::APPROVAL_EXECUTE_ALWAYS => self.approve_pending(true),
                _ => self.reject_pending(),
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.reject_pending(),
            _ => {}
        }
        true
    }

    /// 批准待决调用：`remember` 为真时同时把该工具记进本会话的「不再过问」名单。
    pub(super) fn approve_pending(&mut self, remember: bool) {
        let Some(batch_id) = self.state.batch_request_id() else {
            return;
        };
        if let Some(step) = self.state.decide_approval(true, remember) {
            self.handle_batch_step(batch_id, step);
        }
    }

    /// 拒绝待决调用。拒绝的若是 MCP Tool，先落审计再推进批次（与 Python 的
    /// `record_denied_tool_call` 同一时点、同一文案）。
    pub(super) fn reject_pending(&mut self) {
        let Some(batch_id) = self.state.batch_request_id() else {
            return;
        };
        if let Some(call) = self.state.pending_approval_call() {
            host::record_mcp_denial(
                &self.registry,
                &call.name,
                &call.arguments,
                &omnicrawl_controllers::approval::user_cancelled_reason(&call.name),
            );
        }
        if let Some(step) = self.state.decide_approval(false, false) {
            self.handle_batch_step(batch_id, step);
        }
    }
}