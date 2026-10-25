//! 下拉候选项：展开、移项与确认。
//!
//! 枚举型字段的候选项来源与确认后的写回集中在一处。

use super::*;

impl SettingsState {


    /// 当前面板正在编辑的字段；没有可展开字段（工具页/配置对话入口页，以及表单页
    /// 的开关与整数项）时为 `None`。
    pub(super) fn active_field(&self) -> Option<DropdownField> {
        match self.pane {
            Pane::Choice(kind) => Some(DropdownField::Choice(kind)),
            Pane::ModelManagement if self.model_management.form.is_none() => self
                .model_management
                .param_selected()
                .map(DropdownField::ModelParam),
            Pane::Form(_) => match self.focused_form_field()?.1.kind {
                FieldKind::Enum(_) | FieldKind::ModelChannel | FieldKind::ModelId => {
                    Some(DropdownField::Form(self.form()?.focused()))
                }
                _ => None,
            },
            Pane::Tools
            | Pane::Mcp
            | Pane::Subagents
            | Pane::ModelManagement
            | Pane::DecisionModels
            | Pane::EmbeddingModels
            | Pane::Vision
            | Pane::Tts
            | Pane::Pending => None,
        }
    }


    pub(super) fn open_dropdown(&mut self) {
        let Some(field) = self.active_field() else {
            return;
        };
        let current = match field {
            DropdownField::ModelParam(ModelParamRow::Channel) => {
                OptionValue::Model(self.model_management.model_key.clone())
            }
            DropdownField::ModelParam(ModelParamRow::Window) => {
                OptionValue::Int(self.model_management.window_tokens)
            }
            DropdownField::ModelParam(ModelParamRow::Compaction) => {
                OptionValue::Int(self.model_management.percent)
            }
            DropdownField::ModelParam(ModelParamRow::Reasoning) => {
                OptionValue::Text(self.model_management.reasoning.clone())
            }
            DropdownField::Choice(kind) => kind.value(&self.choices),
            // 视觉页的候选没有「当前值」：游标停在第一项。
            DropdownField::VisionAdd => OptionValue::Text(String::new()),
            // 表单页的模型项与文本项都按字段值定位游标。
            DropdownField::Form(index) => {
                let Some(form) = self.form() else {
                    return;
                };
                let Some(spec) = form.field(index) else {
                    return;
                };
                let raw = form.value(index).map(|value| value.text()).unwrap_or("");
                match spec.kind {
                    FieldKind::ModelChannel => OptionValue::Model(raw.to_string()),
                    _ => OptionValue::Text(raw.to_string()),
                }
            }
        };
        let selected = self
            .options_for(field)
            .iter()
            .position(|(_, option)| *option == current)
            .unwrap_or(0);
        self.dropdown = Some(Dropdown { field, selected });
    }


    pub(super) fn move_candidate(&mut self, delta: isize) {
        // 先算出候选数量再改动游标：`options_for` 借整个 `self`，不能与游标的可变借用重叠。
        let Some(field) = self.dropdown.map(|dropdown| dropdown.field) else {
            return;
        };
        let count = self.options_for(field).len() as isize;
        if count == 0 {
            return;
        }
        let Some(dropdown) = self.dropdown.as_mut() else {
            return;
        };
        dropdown.selected = ((dropdown.selected as isize + delta).rem_euclid(count)) as usize;
    }


    /// 某个字段的候选：模型与渠道项随配置变化，表单页的候选取自字段表，其余是静态表。
    pub(super) fn options_for(&self, field: DropdownField) -> Vec<(String, OptionValue)> {
        match field {
            DropdownField::ModelParam(row) => self.model_param_options(row),
            // 视觉页的候选是渠道列表，与「A 添加」的语义一致。
            DropdownField::VisionAdd => self.channel_options(),
            DropdownField::Form(index) => match self.form().and_then(|form| form.field(index)) {
                Some(FieldSpec {
                    kind: FieldKind::Enum(options),
                    ..
                }) => options
                    .iter()
                    .map(|(label, value)| {
                        (
                            (*label).to_string(),
                            OptionValue::Text((*value).to_string()),
                        )
                    })
                    .collect(),
                Some(FieldSpec {
                    kind: FieldKind::ModelChannel,
                    ..
                }) => self.channel_options(),
                Some(FieldSpec {
                    kind: FieldKind::ModelId,
                    ..
                }) => {
                    let kind = self.form().map(|form| form.kind());
                    match kind {
                        Some(kind) => self
                            .model_candidates_for(kind)
                            .into_iter()
                            .map(|model| (model.clone(), OptionValue::Text(model)))
                            .collect(),
                        None => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
            other => other.static_options().unwrap_or_default(),
        }
    }


    /// 渠道候选（文案 = 渠道名，取值 = 渠道 key）。
    pub(super) fn channel_options(&self) -> Vec<(String, OptionValue)> {
        self.model_management
            .rows
            .iter()
            .map(|row| {
                let label = if row.name.trim().is_empty() {
                    row.key.clone()
                } else {
                    row.name.clone()
                };
                (label, OptionValue::Model(row.key.clone()))
            })
            .collect()
    }


    /// 模型管理页参数行的候选。
    pub(super) fn model_param_options(&self, row: ModelParamRow) -> Vec<(String, OptionValue)> {
        match row {
            ModelParamRow::Channel => self.channel_options(),
            ModelParamRow::Window => context_window_options()
                .into_iter()
                .map(|(label, value)| (label, OptionValue::Int(value)))
                .collect(),
            ModelParamRow::Compaction => context_compaction_options()
                .into_iter()
                .map(|(label, value)| (label, OptionValue::Int(value)))
                .collect(),
            ModelParamRow::Reasoning => REASONING_OPTIONS
                .iter()
                .map(|(option, label)| {
                    (
                        (*label).to_string(),
                        OptionValue::Text((*option).to_string()),
                    )
                })
                .collect(),
        }
    }


    /// 当前展开的下拉的候选；没有展开时为空（渲染层用）。
    pub fn dropdown_options(&self) -> Vec<(String, OptionValue)> {
        match self.dropdown {
            Some(dropdown) => self.options_for(dropdown.field),
            None => Vec::new(),
        }
    }


    /// 确认候选：值未变则不动作（Textual `Select` 只在值真正变化时触发保存）。
    pub(super) fn confirm_dropdown(&mut self) -> Option<SettingsEvent> {
        let dropdown = self.dropdown.take()?;
        let options = self.options_for(dropdown.field);
        let (_, value) = options.get(dropdown.selected)?.clone();
        match (dropdown.field, value) {
            // 模型管理页的参数行：与单选页同义，选中即保存。
            (DropdownField::ModelParam(ModelParamRow::Channel), OptionValue::Model(key)) => {
                if key == self.model_management.model_key {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::Model { key }))
            }
            (DropdownField::ModelParam(ModelParamRow::Window), OptionValue::Int(tokens)) => {
                if tokens == self.model_management.window_tokens {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::ContextWindow {
                    tokens,
                }))
            }
            (DropdownField::ModelParam(ModelParamRow::Compaction), OptionValue::Int(percent)) => {
                if percent == self.model_management.percent {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::CompactionPercent {
                    percent,
                }))
            }
            (DropdownField::ModelParam(ModelParamRow::Reasoning), OptionValue::Text(effort)) => {
                if effort == self.model_management.reasoning {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::Reasoning { effort }))
            }
            (DropdownField::Choice(ChoiceKind::ShowThinking), OptionValue::Flag(enabled)) => {
                if enabled == self.choices.show_thinking {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::ShowThinking {
                    enabled,
                }))
            }
            (DropdownField::Choice(kind), OptionValue::Flag(enabled)) => {
                let key = match kind {
                    ChoiceKind::Memory => "memory",
                    ChoiceKind::Plugins => "plugins",
                    // 其余单选页没有布尔开关；走到这里说明面板映射写错了。
                    ChoiceKind::ShowThinking => return None,
                };
                let current = match kind {
                    ChoiceKind::Memory => self.choices.memory,
                    _ => self.choices.plugins,
                };
                if enabled == current {
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::Feature {
                    key: key.to_string(),
                    enabled,
                }))
            }
            // 表单页的候选只落进草稿，不即时保存（等 `Ctrl+S`）。
            (DropdownField::Form(index), value) => {
                let text = match value {
                    OptionValue::Text(text) | OptionValue::Model(text) => text,
                    _ => return None,
                };
                let kind = self.form().map(|form| form.kind())?;
                let is_channel = matches!(
                    self.form()
                        .and_then(|form| form.field(index))
                        .map(|spec| spec.kind),
                    Some(FieldKind::ModelChannel)
                );
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    form.set_value(index, FieldValue::Text(text.clone()));
                    form.set_status(format!("已选择：{text}；按 Ctrl+S 保存。"));
                }
                // 图像生成页换服务商：另一条通道的行藏起来（取值留在草稿里）。
                if kind == FormKind::ImageGen && index == IMAGE_GEN_PROVIDER_INDEX {
                    if let Some(form) = self.forms.get_mut(kind.index()) {
                        let gemini = text.trim().eq_ignore_ascii_case("gemini");
                        form.set_gemini(gemini);
                    }
                }
                // 换渠道后模型列清空，并让下一次进模型列重新发现。
                if is_channel {
                    self.reset_model_for_channel(kind, &text);
                }
                None
            }
            // 视觉页的「A 添加」：把选中的渠道作为一条 custom 引用加进列表。
            (DropdownField::VisionAdd, OptionValue::Model(key)) => {
                self.add_vision_model(&key);
                None
            }
            _ => None,
        }
    }
}
