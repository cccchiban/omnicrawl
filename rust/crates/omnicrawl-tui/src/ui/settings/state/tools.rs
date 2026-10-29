//! 工具开关页与子任务页：开关、审批模式与高级参数。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    pub(super) fn handle_tools_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_tool(-1);
                None
            }
            KeyCode::Down => {
                self.move_tool(1);
                None
            }
            // 审查模式行按方向换档；工具开关行无视方向，一律切换。
            KeyCode::Left => self.toggle_tool(-1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.toggle_tool(1),
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    /// 子任务设置页：`↑`/`↓` 选行；`←`/`→`/`Enter`/空格 改选中行（选中即保存）。
    pub(super) fn handle_subagents_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_subagent(-1);
                None
            }
            KeyCode::Down => {
                self.move_subagent(1);
                None
            }
            // `←` 退一档、`→`/`Enter`/空格 进一档；总开关行两个方向都是切换。
            KeyCode::Left => self.activate_subagent(-1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.activate_subagent(1),
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }
    pub(super) fn move_subagent(&mut self, delta: isize) {
        let count = self.subagents.rows.len() as isize;
        if count == 0 {
            return;
        }
        self.subagents.selected =
            ((self.subagents.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 改选中行：总开关就地翻转，高级参数按档位表循环；写盘由宿主负责。
    pub(super) fn activate_subagent(&mut self, direction: isize) -> Option<SettingsEvent> {
        let row = self.subagents.rows.get(self.subagents.selected)?;
        if row.toggle {
            return Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Enabled(!row.active),
            )));
        }
        let spec = SUBAGENT_ADVANCED_SPECS
            .iter()
            .find(|spec| spec.key == row.key.as_str())?;
        let current: i64 = row.value.parse().unwrap_or(spec.options[0]);
        let value = cycle_subagent_option(spec.options, current, direction);
        Some(SettingsEvent::Apply(SettingsChange::Subagent(
            SubagentChange::Advanced {
                key: row.key.clone(),
                value,
            },
        )))
    }


    /// 工具页首行是审查模式，其余行是逐工具开关；上下键在最外层一起循环。
    pub(super) fn move_tool(&mut self, delta: isize) {
        let count = self.tool_row_count() as isize;
        if count <= 0 {
            return;
        }
        self.tools.selected = ((self.tools.selected as isize + delta).rem_euclid(count)) as usize;
    }


    /// 首行改审查模式（循环候选），其余行切换工具开关。
    ///
    /// 「联网搜索」已抽成左栏的独立设置页，不再是本页的一行。
    pub(super) fn toggle_tool(&mut self, direction: isize) -> Option<SettingsEvent> {
        if self.tools.selected == TOOLS_APPROVAL_ROW {
            let mode = cycle_approval_mode(&self.tools.approval, direction);
            return Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: mode.to_string(),
            }));
        }
        let row = self
            .tools
            .rows
            .get(self.tools.selected - TOOLS_SWITCH_ROW_OFFSET)?;
        Some(SettingsEvent::Apply(SettingsChange::ToolSwitch {
            name: row.name.clone(),
            enabled: !row.enabled,
        }))
    }
}
