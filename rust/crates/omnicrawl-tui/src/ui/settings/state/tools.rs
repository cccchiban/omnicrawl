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


    /// 首行改审查模式（循环候选），第二行进入联网搜索表单页，其余行切换工具开关。
    pub(super) fn toggle_tool(&mut self, direction: isize) -> Option<SettingsEvent> {
        if self.tools.selected == TOOLS_APPROVAL_ROW {
            let mode = cycle_approval_mode(&self.tools.approval, direction);
            return Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: mode.to_string(),
            }));
        }
        if self.tools.selected == TOOLS_WEB_SEARCH_ROW {
            return self.open_web_search_form();
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


    /// 进入联网搜索表单页：一条设置在工具页里，编辑界面借用表单页结构。
    ///
    /// 只切面板、不写配置：与左栏切页同义，`Esc` 回到工具页。
    pub(super) fn open_web_search_form(&mut self) -> Option<SettingsEvent> {
        // 先取值再取可变借用：草稿来自工具页那一行的当前状态。
        let engine = self.tools.web_search.engine.clone();
        let tavily = secret_draft_for(self.tools.web_search.tavily_configured);
        let exa = secret_draft_for(self.tools.web_search.exa_configured);
        let firecrawl = secret_draft_for(self.tools.web_search.firecrawl_configured);
        let anysearch = secret_draft_for(self.tools.web_search.anysearch_configured);
        let tinyfish = secret_draft_for(self.tools.web_search.tinyfish_configured);
        let github = secret_draft_for(self.tools.web_search.github_configured);
        self.pane = Pane::Form(FormKind::WebSearch);
        self.dropdown = None;
        if let Some(form) = self.form_mut() {
            // 每次进入都从当前配置重起，避免上一次未保存的草稿残留。
            form.set_values(vec![
                FieldValue::Text(engine),
                FieldValue::Text(tavily),
                FieldValue::Text(exa),
                FieldValue::Text(firecrawl),
                FieldValue::Text(anysearch),
                FieldValue::Text(tinyfish),
                FieldValue::Text(github),
            ]);
            form.set_focused(0);
            form.set_status(String::new());
        }
        None
    }


    /// 联网搜索表单页的 `Esc`：回到工具页（而不是左栏）。
    pub(super) fn close_web_search_form(&mut self) {
        self.pane = Pane::Tools;
        self.dropdown = None;
        self.focus = Focus::Pane;
    }


    /// 联网搜索表单页的保存：把三个字段拼成一页表单变更。
    ///
    /// 与其余表单页同构（走 `SettingsChange::Form`），这样界面的「保存 → 回填 → 回工具页」
    /// 与失败保留草稿的语义都复用同一条路径。
    pub(super) fn save_web_search_form(&mut self) -> Option<SettingsEvent> {
        let form = self.form()?;
        let values: Vec<FieldValue> = (0..form.specs().len())
            .map(|index| {
                let raw = form.value(index).map(|value| value.text()).unwrap_or("");
                FieldValue::Text(secret_draft(raw))
            })
            .collect();
        Some(SettingsEvent::Apply(SettingsChange::Form {
            kind: FormKind::WebSearch,
            values,
        }))
    }
}
