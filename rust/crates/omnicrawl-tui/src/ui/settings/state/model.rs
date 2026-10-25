//! 模型管理页：渠道列表与渠道表单、模型发现接线。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    // ---------- 模型管理页：渠道列表与表单 ----------

    /// 进入表单：编辑选中的渠道。
    pub(super) fn edit_channel(&mut self) {
        let selected = self.model_management.selected;
        let Some(row) = self.model_management.rows.get(selected).cloned() else {
            self.model_management.status = "还没有渠道可编辑；按 N 新建一条。".to_string();
            return;
        };
        self.model_management.status =
            format!("正在编辑渠道 {}；Ctrl+S 保存，Esc 返回列表。", row.key);
        self.model_management.form = Some(ChannelForm {
            row,
            field: ChannelField::Name,
            input: None,
            dropdown: None,
            is_new: false,
            index: Some(selected),
        });
    }

    /// 新建渠道：草稿只存在于表单里，`Ctrl+S` 才落进列表（`Esc` 直接丢弃）。
    pub(super) fn new_channel(&mut self) {
        let mut row = self.model_management.template.clone();
        let base = if row.key.trim().is_empty() {
            "新渠道".to_string()
        } else {
            row.key.trim().to_string()
        };
        let mut candidate = base.clone();
        let mut index = 2;
        while self
            .model_management
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
        if row.profile_id.trim().is_empty() {
            row.profile_id = format!("{candidate}-profile");
        }
        self.model_management.status =
            format!("正在新建渠道 {candidate}；填好后按 Ctrl+S 保存，Esc 放弃。");
        self.model_management.form = Some(ChannelForm {
            row,
            field: ChannelField::Name,
            input: None,
            dropdown: None,
            is_new: true,
            index: None,
        });
    }

    /// 删除选中渠道；至少保留一条（与配置侧的校验同口径）。
    pub(super) fn delete_channel(&mut self) {
        if self.model_management.rows.len() <= 1 {
            self.model_management.status = "至少要保留一个模型渠道。".to_string();
            return;
        }
        let selected = self.model_management.selected;
        let removed = self.model_management.rows.remove(selected);
        self.model_management.selected = selected.min(self.model_management.rows.len() - 1);
        if self.model_management.default_key == removed.key {
            self.model_management.default_key = self
                .model_management
                .rows
                .iter()
                .find(|row| row.enabled)
                .or_else(|| self.model_management.rows.first())
                .map(|row| row.key.clone())
                .unwrap_or_default();
        }
        // 删掉的正好是当前模型渠道时，落到新的默认渠道。
        if self.model_management.model_key == removed.key {
            self.model_management.model_key = self.model_management.default_key.clone();
        }
        self.model_management.status =
            format!("已删除渠道 {}；Ctrl+S 保存后才会写盘。", removed.key);
    }

    /// 表单里切换字段（`↑`/`↓` 与 `Tab` 同义）。
    pub(super) fn move_channel_field(&mut self, delta: isize) {
        let Some(form) = self.model_management.form.as_mut() else {
            return;
        };
        let count = ChannelField::ORDER.len() as isize;
        let current = form.field.index() as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        form.field = ChannelField::ORDER[next];
    }

    /// `Enter`/空格：文本字段进编辑态、枚举展开候选、开关就地翻转。
    pub(super) fn activate_channel_field(&mut self) -> Option<SettingsEvent> {
        let Some(form) = self.model_management.form.as_mut() else {
            return None;
        };
        let mut event = None;
        match form.field {
            ChannelField::Enabled => form.row.enabled = !form.row.enabled,
            ChannelField::Provider => {
                let options: Vec<String> = provider_options()
                    .iter()
                    .map(|provider| (*provider).to_string())
                    .collect();
                let selected = options
                    .iter()
                    .position(|option| option == &form.row.provider)
                    .unwrap_or(0);
                form.input = None;
                form.dropdown = Some(ChannelDropdown {
                    field: ChannelField::Provider,
                    options,
                    selected,
                });
            }
            ChannelField::Protocol => {
                let options: Vec<String> = protocols_for_provider(&form.row.provider)
                    .iter()
                    .map(|protocol| (*protocol).to_string())
                    .collect();
                let selected = options
                    .iter()
                    .position(|option| option == &form.row.protocol)
                    .unwrap_or(0);
                form.input = None;
                form.dropdown = Some(ChannelDropdown {
                    field: ChannelField::Protocol,
                    options,
                    selected,
                });
            }
            // 「模型 ID」不再是纯手输：交宿主按渠道的 Provider/协议/基地址/凭据在
            // 后台拉取模型列表，结果经 `set_channel_models` 回填成可选的候选。
            ChannelField::ModelId => {
                form.dropdown = None;
                form.input = None;
                event = Some(SettingsEvent::DiscoverChannelModels {
                    profile_id: form.row.profile_id.clone(),
                    provider: form.row.provider.clone(),
                    protocol: form.row.protocol.clone(),
                    base_url: form.row.base_url.clone(),
                    api_key: form.row.api_key.clone(),
                    api_key_env: form.row.api_key_env.clone(),
                    user_agent: form.row.user_agent.clone(),
                });
                self.model_management.status =
                    "正在检测模型列表…（Esc 可保留手动输入，稍候可重试）".to_string();
            }
            field => {
                // API Key 的输入态从空开始：既不回显明文，也让「留空=不改动」成为默认动作。
                form.dropdown = None;
                form.input = Some(channel_input_composer(&form.row, field));
            }
        }
        event
    }

    /// 自动检测结果回填：有候选就展开下拉，否则退回手动输入并说明原因。
    ///
    /// 结果先回给「正在等它的表单页」（压缩页与顾问页的模型列）：由该页的
    /// [`ModelDiscovery`] 收下，下一次展开模型列就有候选；否则给渠道表单的模型 ID 列。
    pub fn set_channel_models(&mut self, models: Vec<String>, message: String) {
        if let Some(kind) = self.pending_discovery_form() {
            if let Some(discovery) = self.model_discoveries.get_mut(kind.index()) {
                discovery.set_models(models, &message);
                let status = discovery.status().to_string();
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    form.set_status(status);
                }
            }
            return;
        }
        let Some(form) = self.model_management.form.as_mut() else {
            return;
        };
        // 检测期间用户可能已经离开这个字段：结果直接丢弃。
        if form.field != ChannelField::ModelId {
            return;
        }
        if models.is_empty() {
            let reason = if message.trim().is_empty() {
                "未返回可用模型".to_string()
            } else {
                message
            };
            let mut composer = Composer::default();
            composer.insert(&form.row.model_id);
            form.dropdown = None;
            form.input = Some(composer);
            self.model_management.status =
                format!("未检测到模型列表（{reason}）；可直接输入模型 ID 后按 Enter。");
            return;
        }
        let selected = models
            .iter()
            .position(|id| id == &form.row.model_id)
            .unwrap_or(0);
        form.input = None;
        form.dropdown = Some(ChannelDropdown {
            field: ChannelField::ModelId,
            options: models,
            selected,
        });
        self.model_management.status =
            "已检测到模型列表：↑↓ 选择，Enter 确认，Esc 收起。".to_string();
    }

    /// 哪个表单页正在等这次发现结果；没有就返回 `None`（说明是渠道表单发起的）。
    pub(super) fn pending_discovery_form(&self) -> Option<FormKind> {
        self.model_discoveries
            .iter()
            .position(ModelDiscovery::handles_discovery)
            .and_then(|index| FORM_KINDS.get(index).copied())
    }

    /// 枚举候选展开态：`↑`/`↓` 移动、`Enter` 确认、`Esc` 收起。
    pub(super) fn handle_channel_dropdown_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.model_management.form.as_mut()?;
        let dropdown = form.dropdown.as_mut()?;
        match key {
            KeyCode::Up => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize - 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Down => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize + 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Enter => {
                let value = dropdown.options.get(dropdown.selected).cloned();
                let field = dropdown.field;
                form.dropdown = None;
                if let Some(value) = value {
                    set_channel_field(&mut form.row, field, value);
                    if field == ChannelField::Provider {
                        // 换 Provider 后原协议可能不再适用：跟到新 Provider 的第一个协议。
                        if let Some(protocol) = protocols_for_provider(&form.row.provider).first() {
                            form.row.protocol = (*protocol).to_string();
                        }
                    }
                }
                None
            }
            KeyCode::Esc => {
                form.dropdown = None;
                None
            }
            _ => None,
        }
    }

    /// 文本编辑态：字符插入、光标移动、`Enter` 提交、`Esc` 取消。
    pub(super) fn handle_channel_input_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.model_management.form.as_mut()?;
        let field = form.field;
        let composer = form.input.as_mut()?;
        match key {
            KeyCode::Enter => {
                let value = composer.take();
                form.input = None;
                set_channel_field(&mut form.row, field, value);
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

    /// `Esc`：退出表单回列表（未保存的改动丢弃，草稿直接扔掉）。
    pub(super) fn leave_channel_form(&mut self) {
        let drafting = self
            .model_management
            .form
            .as_ref()
            .map(|form| form.is_new)
            .unwrap_or(false);
        self.model_management.form = None;
        self.model_management.status = if drafting {
            "已放弃新建渠道（未写盘）。".to_string()
        } else {
            "已返回渠道列表（未保存的改动已丢弃）。".to_string()
        };
    }

    /// `Ctrl+S`：把表单落进列表、校验后交给宿主写盘。
    pub(super) fn save_channels(&mut self) -> Option<SettingsEvent> {
        let form = self.model_management.form.clone()?;
        let mut rows = self.model_management.rows.clone();
        match form.index {
            Some(index) if index < rows.len() => rows[index] = form.row.clone(),
            _ => rows.push(form.row.clone()),
        }
        if let Err(message) = validate_channels(&rows) {
            self.model_management.status = message;
            return None;
        }
        let default_key = pick_default_channel(&rows, &self.model_management.default_key);
        self.model_management.status = "正在保存渠道配置…".to_string();
        self.model_management.rows = rows.clone();
        self.model_management.default_key = default_key.clone();
        self.model_management.form = None;
        Some(SettingsEvent::Apply(SettingsChange::Channels {
            rows,
            default_key,
        }))
    }


    /// 当前表单页聚焦字段的序号与规格。
    pub(super) fn focused_form_field(&self) -> Option<(usize, FieldSpec)> {
        let form = self.form()?;
        let index = form.focused();
        Some((index, form.field(index)?))
    }


    /// 某个表单页的「渠道 + 模型」两段草稿值（渠道 key 与模型 ID）。
    ///
    /// 只在「渠道」与「模型」两个字段都存在的表单页（工具输出压缩、顾问设置）上有意义。
    pub(super) fn model_draft(&self, kind: FormKind) -> Option<(String, String)> {
        let form = self.forms.get(kind.index())?;
        let channel_index = form
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelChannel))?;
        let model_index = form
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelId))?;
        Some((
            form.value(channel_index)?.text().to_string(),
            form.value(model_index)?.text().to_string(),
        ))
    }


    /// 该表单页的模型发现状态（只有带模型选择的表单页有）。
    pub(super) fn model_discovery(&self, kind: FormKind) -> Option<&ModelDiscovery> {
        self.model_discoveries.get(kind.index())
    }


    /// 该表单页的「模型」字段草稿值；页面没有渠道字段时渠道取当前生效的那条。
    ///
    /// 「渠道 + 模型」两列只有在同页并存时才成对（工具输出压缩、顾问设置）。桌面操作页
    /// 只有「桌面模型」一列，渠道按当前生效渠道算——否则下面所有候选都会退化成空，
    /// 下拉展开后什么也看不到。
    pub(super) fn model_selection_draft(&self, kind: FormKind) -> Option<(String, String)> {
        if let Some(draft) = self.model_draft(kind) {
            return Some(draft);
        }
        let form = self.forms.get(kind.index())?;
        let model_index = form
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelId))?;
        let channel = {
            let current = self.model_management.model_key.trim();
            if current.is_empty() {
                self.model_management.default_key.trim().to_string()
            } else {
                current.to_string()
            }
        };
        Some((channel, form.value(model_index)?.text().to_string()))
    }


    /// 某个表单页当前选中的渠道记录。
    pub(super) fn selected_model_channel(&self, kind: FormKind) -> Option<&ChannelRow> {
        let (channel, _) = self.model_selection_draft(kind)?;
        self.model_management
            .rows
            .iter()
            .find(|row| row.key == channel)
    }


    /// 模型下拉的候选：渠道自带模型 + 该渠道的发现结果 + 当前值。
    pub fn model_candidates_for(&self, kind: FormKind) -> Vec<String> {
        let Some((_, model)) = self.model_selection_draft(kind) else {
            return Vec::new();
        };
        let channel = self.selected_model_channel(kind);
        match self.model_discovery(kind) {
            Some(discovery) => discovery.candidates(channel, &model),
            None => {
                let mut models = Vec::new();
                if let Some(channel) = channel {
                    if !channel.model_id.trim().is_empty() {
                        models.push(channel.model_id.trim().to_string());
                    }
                }
                if !model.trim().is_empty() {
                    models.push(model.trim().to_string());
                }
                models
            }
        }
    }


    /// 需要时请宿主发现「当前渠道」的可用模型；不需要时返回 `None`。
    pub(super) fn ensure_model_discovery(&mut self, index: usize) -> Option<SettingsEvent> {
        let kind = self.form()?.kind();
        let (channel_key, _) = self.model_selection_draft(kind)?;
        if channel_key.trim().is_empty() {
            return None;
        }
        let discovery = self.model_discoveries.get_mut(kind.index())?;
        if !discovery.wants_discovery(&channel_key) {
            return None;
        }
        let channel = self
            .model_management
            .rows
            .iter()
            .find(|row| row.key == channel_key)?
            .clone();
        discovery.mark_discovering(&channel_key);
        if let Some(form) = self.forms.get_mut(kind.index()) {
            form.set_status("正在发现可用模型…");
        }
        let _ = index;
        Some(SettingsEvent::DiscoverChannelModels {
            profile_id: channel.profile_id,
            provider: channel.provider,
            protocol: channel.protocol,
            base_url: channel.base_url,
            api_key: channel.api_key,
            api_key_env: channel.api_key_env,
            user_agent: channel.user_agent,
        })
    }


    /// 表单页里「模型」字段的下标（字段表里唯一的 [`FieldKind::ModelId`]）。
    pub(super) fn model_id_field(&self, kind: FormKind) -> Option<usize> {
        self.forms
            .get(kind.index())?
            .specs()
            .iter()
            .position(|spec| matches!(spec.kind, FieldKind::ModelId))
    }


    /// 换渠道后把模型列清空（沿用新渠道自带的模型），并请宿主重新发现。
    pub(super) fn reset_model_for_channel(&mut self, kind: FormKind, channel: &str) {
        if let Some(index) = self.model_id_field(kind) {
            if let Some(form) = self.forms.get_mut(kind.index()) {
                form.set_value(index, FieldValue::Text(String::new()));
            }
        }
        if let Some(discovery) = self.model_discoveries.get_mut(kind.index()) {
            discovery.set_status(format!("已切换到渠道 {channel}；按 Enter 展开模型候选。"));
        }
    }
}
