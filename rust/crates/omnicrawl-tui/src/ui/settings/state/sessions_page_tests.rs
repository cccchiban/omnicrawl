//! 会话管理页的状态机：删除必须连按两次、当前会话不可删、移动即取消待确认。

use super::*;

fn row(id: &str, title: &str, messages: usize, is_current: bool) -> SessionRow {
    SessionRow {
        session_id: id.to_string(),
        title: title.to_string(),
        message_count: messages,
        is_current,
    }
}

fn state_with(rows: Vec<SessionRow>) -> SessionsState {
    let mut state = SessionsState::default();
    state.set_rows(rows);
    state
}

#[test]
fn first_delete_press_only_enters_confirmation() {
    let mut state = state_with(vec![row("a", "甲", 3, false)]);
    assert_eq!(state.request_delete(), None, "第一次按不应发出删除");
    assert_eq!(
        state.confirming().map(|pending| pending.session_id.as_str()),
        Some("a")
    );
}

#[test]
fn second_delete_press_on_the_same_row_emits_the_id() {
    let mut state = state_with(vec![row("a", "甲", 3, false)]);
    state.request_delete();
    assert_eq!(state.request_delete(), Some("a".to_string()));
    assert!(state.confirming().is_none(), "确认后应清掉待确认态");
}

#[test]
fn moving_the_selection_cancels_the_pending_delete() {
    // 否则「确认」会落到另一个会话上：这是最危险的一种错配。
    let mut state = state_with(vec![row("a", "甲", 3, false), row("b", "乙", 1, false)]);
    state.request_delete();
    state.move_selection(1);
    assert!(state.confirming().is_none());
    assert_eq!(state.request_delete(), None, "移动后需要重新确认");
}

#[test]
fn current_session_is_refused_without_entering_confirmation() {
    let mut state = state_with(vec![row("cur", "当前", 9, true)]);
    assert_eq!(state.request_delete(), None);
    assert!(state.confirming().is_none(), "当前会话不该进确认态");
    assert!(
        state.status.contains("当前会话"),
        "应给出拒绝原因：{}",
        state.status
    );
}

#[test]
fn confirm_target_switch_needs_a_second_press_again() {
    // 先对 A 确认，再移到 B 按一次：不能直接删 B。
    let mut state = state_with(vec![row("a", "甲", 3, false), row("b", "乙", 1, false)]);
    state.request_delete();
    state.move_selection(1);
    assert_eq!(state.request_delete(), None);
    assert_eq!(
        state.confirming().map(|pending| pending.session_id.as_str()),
        Some("b")
    );
}

#[test]
fn empty_list_reports_instead_of_silently_doing_nothing() {
    let mut state = state_with(Vec::new());
    assert_eq!(state.request_delete(), None);
    assert!(state.status.contains("没有可删除"), "{}", state.status);
}

#[test]
fn set_rows_keeps_the_selection_on_the_same_session() {
    let mut state = state_with(vec![row("a", "甲", 3, false), row("b", "乙", 1, false)]);
    state.move_selection(1);
    // 列表刷新后顺序变化（b 排到前面），选中仍应跟住 b。
    state.set_rows(vec![row("b", "乙", 1, false), row("a", "甲", 3, false)]);
    assert_eq!(state.selected_row().map(|row| row.session_id.as_str()), Some("b"));
}

#[test]
fn removing_the_selected_row_clamps_the_selection() {
    // 删掉最后一行后选中越界会让渲染与按键都拿不到行。
    let mut state = state_with(vec![row("a", "甲", 3, false), row("b", "乙", 1, false)]);
    state.move_selection(1);
    state.set_rows(vec![row("a", "甲", 3, false)]);
    assert_eq!(state.selected(), 0);
    assert!(state.selected_row().is_some());
}

#[test]
fn summary_falls_back_for_untitled_sessions() {
    assert_eq!(row("a", "  ", 0, false).summary(), "（未命名）  0 条");
    assert_eq!(row("a", "有标题 ", 2, false).summary(), "有标题  2 条");
}
