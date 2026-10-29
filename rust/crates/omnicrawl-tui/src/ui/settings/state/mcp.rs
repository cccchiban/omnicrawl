//! MCP 设置页：全局策略行、Server 列表与服务器编辑器。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    /// MCP 设置页：全局行（改即保存）、Server 列表与编辑器三个子模式。
    pub(super) fn handle_mcp_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.mcp.editor.is_some() {
            return self.handle_mcp_editor_key(key);
        }
        if self.mcp.servers_view {
            return self.handle_mcp_servers_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_mcp(-1);
                None
            }
            KeyCode::Down => {
                self.move_mcp(1);
                None
            }
            KeyCode::Left => self.activate_mcp_global(-1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.activate_mcp_global(1),
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.enter_mcp_servers();
                None
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    pub(super) fn move_mcp(&mut self, delta: isize) {
        let count = MCP_ROW_COUNT as isize;
        self.mcp.selected = ((self.mcp.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 改一行全局设置：布尔切换、超时换档、`servers` 行进入列表。
    pub(super) fn activate_mcp_global(&mut self, direction: isize) -> Option<SettingsEvent> {
        match self.mcp.selected {
            MCP_ROW_ENABLED => self.mcp.enabled = !self.mcp.enabled,
            MCP_ROW_NETWORK => self.mcp.network = !self.mcp.network,
            MCP_ROW_WRITE => self.mcp.write = !self.mcp.write,
            MCP_ROW_COMMAND => self.mcp.command = !self.mcp.command,
            MCP_ROW_AUDIT => self.mcp.audit = !self.mcp.audit,
            MCP_ROW_TIMEOUT => {
                self.mcp.timeout = cycle_mcp_timeout(self.mcp.timeout, direction);
            }
            MCP_ROW_SERVERS => {
                self.enter_mcp_servers();
                return None;
            }
            _ => return None,
        }
        Some(SettingsEvent::Apply(SettingsChange::Mcp(
            self.mcp.globals_change(),
        )))
    }

    pub(super) fn enter_mcp_servers(&mut self) {
        self.mcp.servers_view = true;
        self.mcp.server_selected = self
            .mcp
            .server_selected
            .min(self.mcp.servers.len().saturating_sub(1));
    }

    pub(super) fn move_mcp_server(&mut self, delta: isize) {
        let count = self.mcp.servers.len() as isize;
        if count == 0 {
            self.mcp.server_selected = 0;
            return;
        }
        self.mcp.server_selected =
            ((self.mcp.server_selected as isize + delta).rem_euclid(count)) as usize;
    }

    pub(super) fn handle_mcp_servers_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_mcp_server(-1);
                None
            }
            KeyCode::Down => {
                self.move_mcp_server(1);
                None
            }
            KeyCode::Enter => {
                let row = self.mcp.servers.get(self.mcp.server_selected).cloned()?;
                self.mcp.editor = Some(McpEditor {
                    draft: McpServerDraft::from_row(&row),
                    focused: MCP_EDITOR_NAME,
                    input: None,
                    is_new: false,
                });
                None
            }
            KeyCode::Char(' ') => {
                let row = self.mcp.servers.get(self.mcp.server_selected).cloned()?;
                Some(SettingsEvent::Apply(SettingsChange::Mcp(
                    McpChange::SetServerEnabled {
                        name: row.name,
                        enabled: !row.enabled,
                    },
                )))
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.mcp.editor = Some(McpEditor {
                    draft: McpServerDraft {
                        transport: MCP_TRANSPORT_OPTIONS[0].to_string(),
                        risk_level: MCP_RISK_OPTIONS[1].to_string(),
                        timeout_seconds: self.mcp.timeout,
                        ..McpServerDraft::default()
                    },
                    focused: MCP_EDITOR_NAME,
                    input: None,
                    is_new: true,
                });
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                let row = self.mcp.servers.get(self.mcp.server_selected).cloned()?;
                Some(SettingsEvent::Apply(SettingsChange::Mcp(
                    McpChange::DeleteServer { name: row.name },
                )))
            }
            KeyCode::Esc | KeyCode::Left => {
                self.mcp.servers_view = false;
                Some(SettingsEvent::Apply(SettingsChange::Mcp(
                    self.mcp.globals_change(),
                )))
            }
            _ => None,
        }
    }

    pub(super) fn move_mcp_editor(&mut self, delta: isize) {
        if let Some(editor) = self.mcp.editor.as_mut() {
            editor.input = None;
            let count = MCP_EDITOR_FIELD_COUNT as isize;
            editor.focused = ((editor.focused as isize + delta).rem_euclid(count)) as usize;
        }
    }

    /// 编辑器里的 transport / risk 字段按方向换档；文本字段忽略。
    pub(super) fn cycle_mcp_editor_field(&mut self, direction: isize) {
        let Some(editor) = self.mcp.editor.as_mut() else {
            return;
        };
        match editor.focused {
            MCP_EDITOR_TRANSPORT => {
                editor.draft.transport =
                    cycle_static(&MCP_TRANSPORT_OPTIONS, &editor.draft.transport, direction)
            }
            MCP_EDITOR_RISK => {
                editor.draft.risk_level =
                    cycle_static(&MCP_RISK_OPTIONS, &editor.draft.risk_level, direction)
            }
            MCP_EDITOR_TIMEOUT => {
                editor.draft.timeout_seconds =
                    cycle_mcp_timeout(editor.draft.timeout_seconds, direction)
            }
            _ => {}
        }
    }

    pub(super) fn handle_mcp_editor_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self
            .mcp
            .editor
            .as_ref()
            .is_some_and(|editor| editor.input.is_some())
        {
            if let Some(editor) = self.mcp.editor.as_mut() {
                editor.input_key(key);
            }
            return None;
        }
        match key {
            KeyCode::Up => {
                self.move_mcp_editor(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_mcp_editor(1);
                None
            }
            KeyCode::Left => {
                self.cycle_mcp_editor_field(-1);
                None
            }
            KeyCode::Right => {
                self.cycle_mcp_editor_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let focused = self.mcp.editor.as_ref().map(|editor| editor.focused)?;
                match focused {
                    MCP_EDITOR_TRANSPORT | MCP_EDITOR_RISK | MCP_EDITOR_TIMEOUT => {
                        self.cycle_mcp_editor_field(1);
                    }
                    _ => {
                        if let Some(editor) = self.mcp.editor.as_mut() {
                            editor.begin_input();
                        }
                    }
                }
                None
            }
            KeyCode::Esc => {
                self.mcp.editor = None;
                None
            }
            _ => None,
        }
    }

    /// 编辑器里的文本型字段（名称/命令/参数/URL/环境变量/请求头）。
    pub(super) fn handle_mcp_editor_save(&mut self) -> Option<SettingsEvent> {
        let editor = self.mcp.editor.as_mut()?;
        editor.commit_input();
        Some(SettingsEvent::Apply(SettingsChange::Mcp(
            McpChange::SaveServer(Box::new(editor.draft.clone())),
        )))
    }
}
