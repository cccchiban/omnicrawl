//! 向量模型页：开关、渠道列表与表单。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    // ---------- 向量模型页 ----------

    pub(super) fn handle_embedding_models_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.embedding_models.form.is_some() {
            return self.handle_embedding_form_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_embedding_row(-1);
                None
            }
            KeyCode::Down => {
                self.move_embedding_row(1);
                None
            }
            // 开关行选中时 Enter/空格/→ 翻转并立刻写盘；渠道行才进表单。
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                match self.embedding_models.selected {
                    EmbeddingModelsState::MEMORY_SWITCH_ROW => self.toggle_embedding_switch(),
                    _ => {
                        self.edit_embedding_channel();
                        None
                    }
                }
            }
            KeyCode::Left | KeyCode::Esc => {
                self.back_to_list();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.new_embedding_channel();
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.delete_embedding_channel();
                None
            }
            _ => None,
        }
    }

    /// 总开关行：就地翻转并交给宿主写盘（选中即保存，与决策页的开关同口径）。
    pub(super) fn toggle_embedding_switch(&mut self) -> Option<SettingsEvent> {
        Some(SettingsEvent::Apply(SettingsChange::EmbeddingEnabled {
            enabled: !self.embedding_models.enabled,
        }))
    }

    pub(super) fn handle_embedding_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let input_active = self
            .embedding_models
            .form
            .as_ref()
            .map(|form| form.input.is_some())
            .unwrap_or(false);
        if input_active {
            return self.handle_embedding_input_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_embedding_field(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_embedding_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_embedding_field(),
            KeyCode::Esc | KeyCode::Left => {
                self.leave_embedding_form();
                None
            }
            _ => None,
        }
    }

    pub(super) fn handle_embedding_input_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.embedding_models.form.as_mut()?;
        let field = form.field;
        let composer = form.input.as_mut()?;
        match key {
            KeyCode::Enter => {
                let value = composer.take();
                form.input = None;
                set_embedding_field(&mut form.row, field, value);
            }
            KeyCode::Esc => form.input = None,
            KeyCode::Char(character) => composer.insert(&character.to_string()),
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.move_left(),
            KeyCode::Right => composer.move_right(),
            KeyCode::Home => composer.move_home(),
            KeyCode::End => composer.move_end(),
            _ => {}
        }
        None
    }

    /// 行游标移动：第 0 行是总开关，之后依次是渠道。
    pub(super) fn move_embedding_row(&mut self, delta: isize) {
        let count = self.embedding_models.total_rows() as isize;
        if count <= 0 {
            return;
        }
        let current = self.embedding_models.selected as isize;
        self.embedding_models.selected = (current + delta).rem_euclid(count) as usize;
    }

    pub(super) fn edit_embedding_channel(&mut self) {
        let Some(index) = EmbeddingModelsState::channel_index(self.embedding_models.selected)
        else {
            return;
        };
        let Some(row) = self.embedding_models.rows.get(index).cloned() else {
            self.embedding_models.status = "还没有向量渠道可编辑；按 N 新建一条。".to_string();
            return;
        };
        self.embedding_models.status =
            format!("正在编辑向量渠道 {}；Ctrl+S 保存，Esc 返回列表。", row.key);
        self.embedding_models.form = Some(EmbeddingForm {
            row,
            field: EmbeddingField::Name,
            input: None,
            is_new: false,
            index: Some(index),
        });
    }

    /// 新建向量渠道：草稿只在表单里，`Ctrl+S` 才落进列表（`Esc` 直接丢弃）。
    pub(super) fn new_embedding_channel(&mut self) {
        let mut row = self.embedding_models.template.clone();
        let base = if row.key.trim().is_empty() {
            "向量渠道".to_string()
        } else {
            row.key.trim().to_string()
        };
        let mut candidate = base.clone();
        let mut index = 2;
        while self
            .embedding_models
            .rows
            .iter()
            .any(|item| item.key == candidate)
        {
            candidate = format!("{base}{index}");
            index += 1;
        }
        row.key = candidate.clone();
        if row.name.trim().is_empty() {
            row.name = candidate.clone();
        }
        self.embedding_models.status = "新建向量渠道；填好后按 Ctrl+S 保存，Esc 放弃。".to_string();
        self.embedding_models.form = Some(EmbeddingForm {
            row,
            field: EmbeddingField::Name,
            input: None,
            is_new: true,
            index: None,
        });
    }

    pub(super) fn delete_embedding_channel(&mut self) {
        let Some(index) = EmbeddingModelsState::channel_index(self.embedding_models.selected)
        else {
            self.embedding_models.status = "开关行不可删除；把游标移到渠道行再按 D。".to_string();
            return;
        };
        if self.embedding_models.rows.len() <= 1 {
            self.embedding_models.status = "至少要保留一个向量渠道。".to_string();
            return;
        }
        let removed = self.embedding_models.rows.remove(index);
        // 删完可能落到「只剩开关」的位置，游标要收回合法范围。
        let last = self.embedding_models.total_rows().saturating_sub(1);
        self.embedding_models.selected = self.embedding_models.selected.min(last);
        if self.embedding_models.default_key == removed.key {
            self.embedding_models.default_key = pick_default_embedding_channel(
                &self.embedding_models.rows,
                &self.embedding_models.default_key,
            );
        }
        self.embedding_models.status =
            format!("已删除向量渠道 {}；Ctrl+S 保存后才会写盘。", removed.key);
    }

    pub(super) fn move_embedding_field(&mut self, delta: isize) {
        let Some(form) = self.embedding_models.form.as_mut() else {
            return;
        };
        let count = EmbeddingField::ORDER.len() as isize;
        let current = form.field.index() as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        form.field = EmbeddingField::ORDER[next];
    }

    pub(super) fn activate_embedding_field(&mut self) -> Option<SettingsEvent> {
        let Some(form) = self.embedding_models.form.as_mut() else {
            return None;
        };
        match form.field {
            EmbeddingField::Enabled => form.row.enabled = !form.row.enabled,
            field => {
                form.input = Some(embedding_input_composer(&form.row, field));
            }
        }
        None
    }

    pub(super) fn leave_embedding_form(&mut self) {
        let drafting = self
            .embedding_models
            .form
            .as_ref()
            .map(|form| form.is_new)
            .unwrap_or(false);
        self.embedding_models.form = None;
        self.embedding_models.status = if drafting {
            "已放弃新建向量渠道（未写盘）。".to_string()
        } else {
            "已返回向量渠道列表（未保存的改动已丢弃）。".to_string()
        };
    }

    pub(super) fn save_embedding_channels(&mut self) -> Option<SettingsEvent> {
        let form = self.embedding_models.form.clone()?;
        let mut rows = self.embedding_models.rows.clone();
        match form.index {
            Some(index) if index < rows.len() => rows[index] = form.row.clone(),
            _ => rows.push(form.row.clone()),
        }
        if let Err(message) = validate_embedding_rows(&rows) {
            self.embedding_models.status = message;
            return None;
        }
        let default_key = pick_default_embedding_channel(&rows, &self.embedding_models.default_key);
        self.embedding_models.status = "正在保存向量模型配置…".to_string();
        self.embedding_models.rows = rows.clone();
        self.embedding_models.default_key = default_key.clone();
        self.embedding_models.form = None;
        Some(SettingsEvent::Apply(SettingsChange::EmbeddingModels {
            rows,
            default_key,
        }))
    }
}
