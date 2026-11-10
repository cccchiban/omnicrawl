//! 向量模型页：两个开关、两组渠道列表与表单。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。
//!
//! 页面顺序固定为「嵌入模型开关 → 重排序模型开关 → 嵌入渠道 → 重排渠道」：两个开关对应
//! 检索的两个阶段，渠道各自跟在自己的开关之下。

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
                    EmbeddingModelsState::EMBEDDING_SWITCH_ROW => self.toggle_embedding_switch(),
                    EmbeddingModelsState::RERANK_SWITCH_ROW => self.toggle_rerank_switch(),
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

    /// 嵌入模型开关：就地翻转并交给宿主写盘（选中即保存，与决策页的开关同口径）。
    pub(super) fn toggle_embedding_switch(&mut self) -> Option<SettingsEvent> {
        Some(SettingsEvent::Apply(SettingsChange::EmbeddingEnabled {
            enabled: !self.embedding_models.enabled,
        }))
    }

    /// 重排序模型开关：口径同上，写另一段配置。
    pub(super) fn toggle_rerank_switch(&mut self) -> Option<SettingsEvent> {
        Some(SettingsEvent::Apply(SettingsChange::RerankEnabled {
            enabled: !self.embedding_models.rerank_enabled,
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

    /// 行游标移动：前两行是两个开关，之后依次是嵌入渠道、重排渠道。
    pub(super) fn move_embedding_row(&mut self, delta: isize) {
        let count = self.embedding_models.total_rows() as isize;
        if count <= 0 {
            return;
        }
        let current = self.embedding_models.selected as isize;
        self.embedding_models.selected = (current + delta).rem_euclid(count) as usize;
    }

    pub(super) fn edit_embedding_channel(&mut self) {
        let Some((kind, index)) = self
            .embedding_models
            .channel_at(self.embedding_models.selected)
        else {
            return;
        };
        let row = {
            let group = self.embedding_models.group(kind);
            group.rows.get(index).cloned()
        };
        let Some(row) = row else {
            self.embedding_models.status = "还没有可编辑的渠道；按 N 新建一条。".to_string();
            return;
        };
        self.embedding_models.status = format!(
            "正在编辑{}渠道 {}；Ctrl+S 保存，Esc 返回列表。",
            kind.section(),
            row.key
        );
        self.embedding_models.form = Some(EmbeddingForm {
            row,
            field: EmbeddingField::Name,
            input: None,
            is_new: false,
            index: Some(index),
            kind,
        });
    }

    /// 新建渠道：草稿只在表单里，`Ctrl+S` 才落进列表（`Esc` 直接丢弃）。
    ///
    /// 落在哪一组由游标决定：在重排组里按 N 新建的就是重排渠道。
    pub(super) fn new_embedding_channel(&mut self) {
        let kind = self.embedding_models.kind_for_new();
        let group = self.embedding_models.group(kind);
        let mut row = group.template.clone();
        let base = if row.key.trim().is_empty() {
            group
                .rows
                .first()
                .map(|item| item.key.clone())
                .unwrap_or_else(|| "渠道".to_string())
        } else {
            row.key.trim().to_string()
        };
        let mut candidate = base.clone();
        let mut index = 2;
        // key 在整组的命名空间里唯一即可（两组各写各的配置段）。
        while group.rows.iter().any(|item| item.key == candidate) {
            candidate = format!("{base}{index}");
            index += 1;
        }
        row.key = candidate.clone();
        if row.name.trim().is_empty() {
            row.name = candidate.clone();
        }
        self.embedding_models.status =
            format!("新建{}渠道；填好后按 Ctrl+S 保存，Esc 放弃。", kind.section());
        self.embedding_models.form = Some(EmbeddingForm {
            row,
            field: EmbeddingField::Name,
            input: None,
            is_new: true,
            index: None,
            kind,
        });
    }

    pub(super) fn delete_embedding_channel(&mut self) {
        let Some((kind, index)) = self
            .embedding_models
            .channel_at(self.embedding_models.selected)
        else {
            self.embedding_models.status =
                "开关行不可删除；把游标移到渠道行再按 D。".to_string();
            return;
        };
        if self.embedding_models.group(kind).rows.len() <= 1 {
            self.embedding_models.status =
                format!("{}至少要保留一个渠道。", kind.section());
            return;
        }
        let removed = self.embedding_models.group_mut(kind).rows.remove(index);
        // 删完可能落到「只剩开关」的位置，游标要收回合法范围。
        let last = self.embedding_models.total_rows().saturating_sub(1);
        self.embedding_models.selected = self.embedding_models.selected.min(last);
        let group = self.embedding_models.group(kind);
        let next_default = if group.default_key == removed.key {
            pick_default_embedding_channel(&group.rows, &group.default_key)
        } else {
            group.default_key.clone()
        };
        self.embedding_models.group_mut(kind).default_key = next_default;
        self.embedding_models.status = format!(
            "已删除{}渠道 {}；Ctrl+S 保存后才会写盘。",
            kind.section(),
            removed.key
        );
    }

    pub(super) fn move_embedding_field(&mut self, delta: isize) {
        let Some(form) = self.embedding_models.form.as_ref() else {
            return;
        };
        let fields = visible_embedding_fields(form.kind);
        let count = fields.len() as isize;
        if count <= 0 {
            return;
        }
        let current = fields
            .iter()
            .position(|field| *field == form.field)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        let field = fields[next];
        if let Some(form) = self.embedding_models.form.as_mut() {
            form.field = field;
        }
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
            "已放弃新建渠道（未写盘）。".to_string()
        } else {
            "已返回渠道列表（未保存的改动已丢弃）。".to_string()
        };
    }

    /// 保存整页：两组列表一起写回，草稿落进它自己那一组。
    pub(super) fn save_embedding_channels(&mut self) -> Option<SettingsEvent> {
        let form = self.embedding_models.form.clone()?;
        let current_default = self.embedding_models.group(form.kind).default_key.clone();
        let mut rows = self.embedding_models.group(form.kind).rows.clone();
        match form.index {
            Some(index) if index < rows.len() => rows[index] = form.row.clone(),
            _ => rows.push(form.row.clone()),
        }
        if let Err(message) = validate_embedding_rows(&rows) {
            self.embedding_models.status = message;
            return None;
        }
        let default_key = pick_default_embedding_channel(&rows, &current_default);
        let group = self.embedding_models.group_mut(form.kind);
        group.rows = rows;
        group.default_key = default_key;
        self.embedding_models.form = None;
        self.embedding_models.status = "正在保存向量模型配置…".to_string();
        let embedding = self.embedding_models.groups[0].clone();
        let rerank = self.embedding_models.groups[1].clone();
        Some(SettingsEvent::Apply(SettingsChange::EmbeddingModels {
            rows: embedding.rows,
            default_key: embedding.default_key,
            rerank_rows: rerank.rows,
            rerank_default_key: rerank.default_key,
        }))
    }
}
