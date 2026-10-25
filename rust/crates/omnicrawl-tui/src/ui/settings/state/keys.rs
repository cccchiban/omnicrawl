//! 键位总机：按当前面板把按键分派给各页处理器。
//!
//! 分派顺序（输入态 → 子视图 → 面板）是设置面板的唯一入口契约。

use super::*;

impl SettingsState {


    // ---------- 粘贴 ----------

    /// 把一次粘贴落进当前面板的输入态；没有正在编辑的字段时返回 `false`。
    ///
    /// 终端发来 `Event::Paste`（bracketed paste）时按键路径整条不走，必须在这里接住：
    /// 否则凭据这类长文本在设置面板里只能靠逐字符击键（多数终端也不会把整段粘贴
    /// 拆成单个按键送来）。
    ///
    /// 焦点还在左侧一级列表时一律拒收：那边没有输入框，落进去只会偷偷改掉右侧
    /// 正在预览的字段草稿。
    pub fn paste(&mut self, text: &str) -> bool {
        if self.focus != Focus::Pane {
            return false;
        }
        match self.pane {
            Pane::Form(_) => self
                .form_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::ModelManagement => self
                .model_management
                .form
                .as_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::DecisionModels => self
                .decision_models
                .form
                .as_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::EmbeddingModels => self
                .embedding_models
                .form
                .as_mut()
                .map(|form| form.paste(text))
                .unwrap_or(false),
            Pane::Mcp => self
                .mcp
                .editor
                .as_mut()
                .map(|editor| editor.paste(text))
                .unwrap_or(false),
            Pane::Tts => self.tts.paste(text),
            _ => false,
        }
    }


    // ---------- 键盘 ----------

    pub fn handle_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match self.focus {
            Focus::List => self.handle_list_key(key),
            Focus::Pane => self.handle_pane_key(key),
        }
    }


    pub(super) fn handle_list_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.move_selection(1);
                None
            }
            KeyCode::Enter | KeyCode::Right => {
                self.enter_pane();
                None
            }
            KeyCode::Esc => Some(SettingsEvent::Close),
            _ => None,
        }
    }


    pub(super) fn handle_pane_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        // 展开态的三个键位对所有面板一致，先统一处理。
        if self.dropdown.is_some() {
            return self.handle_dropdown_key(key);
        }
        match self.pane {
            Pane::Tools => self.handle_tools_key(key),
            Pane::Mcp => self.handle_mcp_key(key),
            Pane::Subagents => self.handle_subagents_key(key),
            Pane::Vision => self.handle_vision_key(key),
            Pane::ModelManagement => self.handle_model_management_key(key),
            Pane::DecisionModels => self.handle_decision_models_key(key),
            Pane::EmbeddingModels => self.handle_embedding_models_key(key),
            Pane::Choice(kind) => self.handle_choice_key(key, kind),
            Pane::Form(_) => self.handle_form_key(key),
            Pane::Tts => self.handle_tts_key(key),
            Pane::Pending => match key {
                KeyCode::Esc | KeyCode::Left => {
                    self.back_to_list();
                    None
                }
                _ => None,
            },
        }
    }


    /// 模型管理页：列表态与表单态各有一组键位。
    ///
    /// 列表态的行 = 渠道行 + 「模型与参数」分区的四行（标题行不参与选中）；渠道行上
    /// `Enter` 进表单，参数行上 `Enter`/`←`/`→` 展开候选，选中即保存。
    pub(super) fn handle_model_management_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.model_management.form.is_some() {
            return self.handle_channel_form_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_model_row(-1);
                None
            }
            KeyCode::Down => {
                self.move_model_row(1);
                None
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                if self.model_management.param_selected().is_some() {
                    self.open_dropdown();
                    return None;
                }
                if key == KeyCode::Char(' ') {
                    return None;
                }
                self.edit_channel();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if self.model_management.param_selected().is_some() {
                    self.model_management.status =
                        "只有渠道行参与新建；按 ↑ 选中渠道再按 N。".to_string();
                    return None;
                }
                self.new_channel();
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if self.model_management.param_selected().is_some() {
                    self.model_management.status =
                        "只有渠道行参与删除；按 ↑ 选中渠道再按 D。".to_string();
                    return None;
                }
                self.delete_channel();
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }


    /// 渠道表单：输入态 → 候选展开态 → 字段导航，三层各管各的键位。
    pub(super) fn handle_channel_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let input_active = self
            .model_management
            .form
            .as_ref()
            .map(|form| form.input.is_some())
            .unwrap_or(false);
        if input_active {
            return self.handle_channel_input_key(key);
        }
        let dropdown_open = self
            .model_management
            .form
            .as_ref()
            .map(|form| form.dropdown.is_some())
            .unwrap_or(false);
        if dropdown_open {
            return self.handle_channel_dropdown_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_channel_field(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_channel_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_channel_field(),
            KeyCode::Esc | KeyCode::Left => {
                self.leave_channel_form();
                None
            }
            _ => None,
        }
    }


    /// 候选展开态：`↑`/`↓` 移动、`Enter` 确认并保存、`Esc` 收起。
    pub(super) fn handle_dropdown_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_candidate(-1);
                None
            }
            KeyCode::Down => {
                self.move_candidate(1);
                None
            }
            KeyCode::Enter => self.confirm_dropdown(),
            KeyCode::Esc => {
                self.dropdown = None;
                None
            }
            _ => None,
        }
    }


    /// 单选页：只有一个下拉，键位与 Textual `Select` 一致（与模型管理页的参数行同源）。
    pub(super) fn handle_choice_key(&mut self, key: KeyCode, _kind: ChoiceKind) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Char(' ') => {
                self.open_dropdown();
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }


    /// 表单页：输入态 → 字段导航两层各管各的键位；`Ctrl+S` 保存由宿主转发进来。
    ///
    /// 「渠道」与「模型」两个字段都是下拉：渠道列出配置里的渠道，模型列在选中渠道的
    /// 候选里选（进模型列时会按需发起一次发现，结果由宿主回填）。
    pub(super) fn handle_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.form().is_some_and(|form| form.input().is_some()) {
            if let Some(form) = self.form_mut() {
                form.input_key(key);
            }
            return None;
        }
        let (index, spec) = self.focused_form_field()?;
        match key {
            KeyCode::Up => {
                if let Some(form) = self.form_mut() {
                    form.move_focus(-1);
                }
                None
            }
            // 换字段与 Textual 表单一致：`Tab` 与 `↓` 同义。
            KeyCode::Down | KeyCode::Tab => {
                if let Some(form) = self.form_mut() {
                    form.move_focus(1);
                }
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                match spec.kind {
                    FieldKind::Flag => {
                        if let Some(form) = self.form_mut() {
                            form.toggle_flag(index);
                        }
                    }
                    // 候选字段选完不即时保存：仍要 `Ctrl+S`（与 Python 的表单页同义）。
                    FieldKind::Enum(_) | FieldKind::ModelChannel => self.open_dropdown(),
                    // 进模型列时先按需拉一次该渠道的模型列表，再展开候选。
                    FieldKind::ModelId => {
                        let event = self.ensure_model_discovery(index);
                        self.open_dropdown();
                        return event;
                    }
                    FieldKind::Int | FieldKind::Float | FieldKind::Text | FieldKind::Secret => {
                        if let Some(form) = self.form_mut() {
                            form.begin_input(index);
                        }
                    }
                }
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                // 联网搜索页是从工具页进来的，`Esc` 回去的地方也是工具页。
                if self.pane == Pane::Form(FormKind::WebSearch) {
                    self.close_web_search_form();
                } else {
                    self.back_to_list();
                }
                None
            }
            _ => None,
        }
    }


    /// `Ctrl` 组合键：渠道表单与表单页的保存都走 `Ctrl+S`。
    pub fn handle_ctrl_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match self.pane {
            Pane::EmbeddingModels if self.embedding_models.form.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_embedding_channels(),
                _ => None,
            },
            Pane::ModelManagement if self.model_management.form.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_channels(),
                _ => None,
            },
            Pane::DecisionModels if self.decision_models.form.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.save_decision_channels(),
                _ => None,
            },
            Pane::Form(kind) => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    if kind == FormKind::WebSearch {
                        return self.save_web_search_form();
                    }
                    let values = self
                        .form()
                        .map(|form| form.values().to_vec())
                        .unwrap_or_default();
                    Some(SettingsEvent::Apply(SettingsChange::Form { kind, values }))
                }
                _ => None,
            },
            Pane::Mcp if self.mcp.editor.is_some() => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => self.handle_mcp_editor_save(),
                _ => None,
            },
            Pane::Tts => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => Some(SettingsEvent::Apply(
                    SettingsChange::Tts(TtsChange::Save(self.tts.draft())),
                )),
                _ => None,
            },
            Pane::Vision => match key {
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // 只有真动过模型原生视觉才连带写回（与 Python 同义）。
                    let native = if self.vision.native != self.vision.native_previous {
                        Some(self.vision.native)
                    } else {
                        None
                    };
                    Some(SettingsEvent::Apply(SettingsChange::Vision(VisionChange {
                        enabled: self.vision.enabled,
                        models: self.vision.models.clone(),
                        native,
                    })))
                }
                KeyCode::Up => {
                    self.move_vision_priority(-1);
                    None
                }
                KeyCode::Down => {
                    self.move_vision_priority(1);
                    None
                }
                _ => None,
            },
            _ => None,
        }
    }
}
