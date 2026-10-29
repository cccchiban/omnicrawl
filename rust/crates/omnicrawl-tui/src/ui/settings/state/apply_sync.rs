//! 宿主回填与跨页同步：把宿主应用结果、内核拒绝与模型清单回填进各页状态。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    // ---------- 宿主回填 ----------

    /// 应用成功：写入新值并显示成功文本。
    pub fn apply_succeeded(&mut self, change: &SettingsChange, message: String) {
        match change {
            SettingsChange::Channels { rows, default_key } => {
                self.model_management.rows = rows.clone();
                self.model_management.default_key = default_key.clone();
                self.model_management.selected = self
                    .model_management
                    .selected
                    .min(self.model_management.row_count().saturating_sub(1));
                self.model_management.form = None;
                self.model_management.status = message;
            }
            SettingsChange::DecisionModels { rows, default_key } => {
                self.decision_models.rows = rows.clone();
                self.decision_models.default_key = default_key.clone();
                self.decision_models.selected = self
                    .decision_models
                    .selected
                    .min(self.decision_row_count().saturating_sub(1));
                self.decision_models.form = None;
                self.decision_models.status = message;
            }
            SettingsChange::EmbeddingModels { rows, default_key } => {
                self.embedding_models.rows = rows.clone();
                self.embedding_models.default_key = default_key.clone();
                self.embedding_models.selected = self
                    .embedding_models
                    .selected
                    .min(self.embedding_models.total_rows().saturating_sub(1));
                self.embedding_models.form = None;
                self.embedding_models.status = message;
            }
            SettingsChange::EmbeddingEnabled { enabled } => {
                self.embedding_models.enabled = *enabled;
                self.embedding_models.status = message;
            }
            SettingsChange::DecisionLocal(change) => {
                match change {
                    DecisionLocalChange::Save { size, device } => {
                        self.decision_models.local.size = size.clone();
                        self.decision_models.local.device = device.clone();
                    }
                    DecisionLocalChange::PrepareEnvironment
                    | DecisionLocalChange::Download { .. }
                    | DecisionLocalChange::StartService
                    | DecisionLocalChange::StopService => {
                        // 长任务：界面先按住重复动作；宿主随后用 `refresh_decision_local`
                        // 的 `task` 字段持续对齐（任务结束自然放开）。
                        self.decision_models.local.busy = true;
                    }
                    DecisionLocalChange::Delete { .. } => {}
                    DecisionLocalChange::RemoveEnvironment => {
                        // 同步动作，但环境状态变了：就绪标记由宿主随后的刷新覆盖。
                        self.decision_models.local.env_ready = false;
                    }
                    DecisionLocalChange::UseChannel { size } => {
                        // 渠道行由宿主写盘后回填（`sync_decision_models`），这里只记住尺寸。
                        self.decision_models.local.size = size.clone();
                    }
                }
                self.decision_models.status = message;
            }
            SettingsChange::DecisionSwitch { key, enabled } => {
                if let Some(row) = self
                    .decision_models
                    .switches
                    .iter_mut()
                    .find(|row| &row.key == key)
                {
                    row.enabled = *enabled;
                }
                // 该开关画在决策页，状态行跟着焦点所在页走。
                if self.pane == Pane::EmbeddingModels {
                    self.embedding_models.status = message;
                } else {
                    self.decision_models.status = message;
                }
            }
            SettingsChange::Model { key } => {
                self.model_management.model_key = key.clone();
                self.model_management.status = message;
            }
            SettingsChange::ContextWindow { tokens } => {
                self.model_management.window_tokens = *tokens;
                self.model_management.status = message;
            }
            SettingsChange::CompactionPercent { percent } => {
                self.model_management.percent = *percent;
                self.model_management.status = message;
            }
            SettingsChange::ToolSwitch { name, enabled } => {
                if let Some(row) = self.tools.rows.iter_mut().find(|row| &row.name == name) {
                    row.enabled = *enabled;
                }
                self.tools.status = message;
            }
            SettingsChange::ToolApproval { mode } => {
                self.tools.approval = nearest_approval_mode(mode).to_string();
                self.tools.status = message;
            }
            SettingsChange::Form { kind, values } => {
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    // 配置侧可能规范化过取值，用宿主回填的值覆盖草稿。
                    form.set_values(values.clone());
                    form.set_status(message.clone());
                }
            }
            SettingsChange::Subagent(change) => {
                match change {
                    SubagentChange::Enabled(enabled) => {
                        if let Some(row) = self
                            .subagents
                            .rows
                            .iter_mut()
                            .find(|row| row.key == "enabled")
                        {
                            row.active = *enabled;
                            row.value = if *enabled { "已开启" } else { "已关闭" }.to_string();
                        }
                    }
                    SubagentChange::Advanced { key, value } => {
                        if let Some(row) =
                            self.subagents.rows.iter_mut().find(|row| &row.key == key)
                        {
                            row.value = value.to_string();
                        }
                    }
                }
                self.subagents.status = message;
            }
            SettingsChange::Vision(change) => {
                self.vision.enabled = change.enabled;
                self.vision.models = change.models.clone();
                self.vision.selected = self
                    .vision
                    .selected
                    .min(self.vision.models.len().saturating_sub(1));
                if let Some(native) = change.native {
                    self.vision.native = native;
                    self.vision.native_previous = native;
                }
                self.vision.status = message;
            }
            SettingsChange::Reasoning { effort } => {
                self.model_management.reasoning = effort.clone();
                self.model_management.status = message;
            }
            SettingsChange::ShowThinking { enabled } => {
                self.choices.show_thinking = *enabled;
                self.choices.status[ChoiceKind::ShowThinking.index()] = message;
            }
            SettingsChange::Feature { key, enabled } => {
                let kind = if key == "memory" {
                    self.choices.memory = *enabled;
                    ChoiceKind::Memory
                } else {
                    self.choices.plugins = *enabled;
                    ChoiceKind::Plugins
                };
                self.choices.status[kind.index()] = message;
            }
            SettingsChange::Tts(TtsChange::Save(draft)) => {
                // 保存成功：把草稿的明文换成刚写盘的生效值（另一家的配置由宿主另行推回）。
                self.tts.apply_saved(draft);
                self.tts.status = message;
            }
            SettingsChange::Mcp(change) => self.apply_mcp_succeeded(change, message),
        }
        self.dropdown = None;
    }

    /// MCP 变更成功：把界面状态对齐到刚保存的配置。
    pub(super) fn apply_mcp_succeeded(&mut self, change: &McpChange, message: String) {
        match change {
            McpChange::Globals {
                enabled,
                allow_external_network_tools,
                require_confirmation_for_write,
                require_confirmation_for_command,
                audit_log_enabled,
                timeout_seconds,
            } => {
                self.mcp.enabled = *enabled;
                self.mcp.network = *allow_external_network_tools;
                self.mcp.write = *require_confirmation_for_write;
                self.mcp.command = *require_confirmation_for_command;
                self.mcp.audit = *audit_log_enabled;
                self.mcp.timeout = *timeout_seconds;
            }
            McpChange::SaveServer(draft) => {
                let row = McpServerRow {
                    name: draft.name.clone(),
                    enabled: draft.enabled,
                    transport: draft.transport.clone(),
                    risk_level: draft.risk_level.clone(),
                };
                if let Some(original) = draft.original_name.as_deref() {
                    if let Some(slot) = self
                        .mcp
                        .servers
                        .iter_mut()
                        .find(|candidate| candidate.name == original)
                    {
                        *slot = row;
                    } else {
                        self.mcp.servers.push(row);
                    }
                } else if !self
                    .mcp
                    .servers
                    .iter()
                    .any(|candidate| candidate.name == draft.name)
                {
                    self.mcp.servers.push(row);
                }
                self.mcp.server_selected = self
                    .mcp
                    .server_selected
                    .min(self.mcp.servers.len().saturating_sub(1));
                self.mcp.editor = None;
            }
            McpChange::SetServerEnabled { name, enabled } => {
                if let Some(row) = self
                    .mcp
                    .servers
                    .iter_mut()
                    .find(|candidate| &candidate.name == name)
                {
                    row.enabled = *enabled;
                }
            }
            McpChange::DeleteServer { name } => {
                self.mcp.servers.retain(|candidate| &candidate.name != name);
                self.mcp.server_selected = self
                    .mcp
                    .server_selected
                    .min(self.mcp.servers.len().saturating_sub(1));
            }
        }
        self.mcp.status = message;
    }

    /// 应用失败：值保持不变（界面回落到原值），只显示失败文本。
    pub fn apply_failed(&mut self, message: String) {
        match self.pane {
            Pane::Tools => self.tools.status = message,
            Pane::Mcp => self.mcp.status = message,
            Pane::Subagents => self.subagents.status = message,
            Pane::Vision => self.vision.status = message,
            // 模型管理页失败时保留表单，方便就地改错再按 Ctrl+S。
            Pane::ModelManagement => self.model_management.status = message,
            // 决策页同理：保留表单。
            Pane::DecisionModels => self.decision_models.status = message,
            Pane::EmbeddingModels => self.embedding_models.status = message,
            Pane::Choice(kind) => self.choices.status[kind.index()] = message,
            // 表单页失败时同样保留草稿。
            Pane::Form(_) => {
                if let Some(form) = self.form_mut() {
                    form.set_status(message);
                }
            }
            Pane::Tts => self.tts.status = message,
            Pane::Pending => {}
        }
        self.dropdown = None;
    }

    /// 内核拒绝即时更新时，在对应面板的状态文本后追加说明。
    ///
    /// 配置已经写盘、宿主侧已经生效，所以这里只补一句「下次会话生效」的事实，
    /// 不把值回滚（与「写盘失败」是两件事）。
    pub fn note_kernel_rejection(&mut self, change: &SettingsChange, note: &str) {
        match change {
            SettingsChange::Channels { .. } => self.model_management.status.push_str(note),
            SettingsChange::DecisionModels { .. }
            | SettingsChange::DecisionSwitch { .. }
            | SettingsChange::DecisionLocal(_) => {
                self.decision_models.status.push_str(note);
                if self.pane == Pane::EmbeddingModels {
                    self.embedding_models.status.push_str(note);
                }
            }
            SettingsChange::EmbeddingModels { .. } | SettingsChange::EmbeddingEnabled { .. } => {
                self.embedding_models.status.push_str(note);
            }
            SettingsChange::Model { .. }
            | SettingsChange::ContextWindow { .. }
            | SettingsChange::CompactionPercent { .. }
            | SettingsChange::Reasoning { .. } => self.model_management.status.push_str(note),
            SettingsChange::ToolSwitch { .. } => self.tools.status.push_str(note),
            SettingsChange::ToolApproval { .. } => self.tools.status.push_str(note),
            SettingsChange::Mcp(_) => self.mcp.status.push_str(note),
            SettingsChange::Subagent(_) => self.subagents.status.push_str(note),
            SettingsChange::Vision(_) => self.vision.status.push_str(note),
            SettingsChange::ShowThinking { .. } => {
                self.choices
                    .status_mut(ChoiceKind::ShowThinking)
                    .push_str(note);
            }
            SettingsChange::Feature { key, .. } => {
                let kind = if key == "memory" {
                    ChoiceKind::Memory
                } else {
                    ChoiceKind::Plugins
                };
                self.choices.status_mut(kind).push_str(note);
            }
            SettingsChange::Form { kind, .. } => {
                if let Some(form) = self.forms.get_mut(kind.index()) {
                    let mut status = form.status().to_string();
                    status.push_str(note);
                    form.set_status(status);
                }
            }
            SettingsChange::Tts(_) => self.tts.status.push_str(note),
        }
    }

    /// 模型管理页的渠道列表（渲染层用）。
    pub fn channel_rows(&self) -> &[ChannelRow] {
        &self.model_management.rows
    }

    /// 模型管理页列表态的选中行（渠道行 + 分区行一起编号）。
    pub fn channel_selected(&self) -> usize {
        self.model_management.selected
    }

    pub fn channel_default_key(&self) -> &str {
        &self.model_management.default_key
    }

    /// 当前模型渠道 key（分区首行的取值）。
    pub fn model_key(&self) -> &str {
        &self.model_management.model_key
    }

    /// 模型管理页「模型与参数」分区的行视图：`(标签, 取值, 是否选中, 是否可选)`。
    pub fn model_param_rows(&self) -> Vec<(ModelParamRow, String, bool)> {
        let selected = self.model_management.param_selected();
        ModelParamRow::ORDER
            .iter()
            .copied()
            .map(|row| {
                let value = match row {
                    ModelParamRow::Channel => {
                        let key = self.model_management.model_key.trim();
                        if key.is_empty() {
                            "（未选择）".to_string()
                        } else {
                            self.model_management
                                .rows
                                .iter()
                                .find(|item| item.key == key)
                                .map(|item| {
                                    if item.name.trim().is_empty() {
                                        item.key.clone()
                                    } else {
                                        item.name.clone()
                                    }
                                })
                                .unwrap_or_else(|| key.to_string())
                        }
                    }
                    ModelParamRow::Window => {
                        format!("{}K", self.model_management.window_tokens / 1000)
                    }
                    ModelParamRow::Compaction => {
                        format!("{}%", self.model_management.percent)
                    }
                    ModelParamRow::Reasoning => reasoning_label(&self.model_management.reasoning),
                };
                (row, value, selected == Some(row))
            })
            .collect()
    }

    /// 正在编辑的渠道（渲染层用）；不在表单时为 `None`。
    pub fn channel_form(&self) -> Option<ChannelFormView<'_>> {
        let form = self.model_management.form.as_ref()?;
        Some(ChannelFormView {
            row: &form.row,
            field: form.field,
            input: form.input.as_ref(),
            dropdown: form.dropdown.as_ref(),
            is_new: form.is_new,
        })
    }

    /// 工具开关页的注册标记刷新之外，模型管理页在保存后也要与磁盘对齐。
    pub fn sync_channels(&mut self, rows: Vec<ChannelRow>, default_key: &str) {
        self.model_management.selected = self
            .model_management
            .selected
            .min(self.model_management.row_count().saturating_sub(1));
        self.model_management.default_key = if default_key.trim().is_empty() {
            rows.first().map(|row| row.key.clone()).unwrap_or_default()
        } else {
            default_key.to_string()
        };
        // 当前模型渠道不在新列表里时，落到默认渠道（删除渠道后保存的同一口径）。
        if !rows
            .iter()
            .any(|row| row.key == self.model_management.model_key)
        {
            self.model_management.model_key = self.model_management.default_key.clone();
        }
        self.model_management.rows = rows;
        self.model_management.form = None;
    }

    /// 工具表重建后刷新行的注册标记与开关状态（审查模式不随之变化）。
    pub fn refresh_tools(&mut self, rows: Vec<ToolSwitchRow>) {
        let selected = self.tools.selected.min(rows.len());
        self.tools.rows = rows;
        self.tools.selected = selected;
    }

    /// 联网搜索设置保存后，用磁盘上的真实配置重排本页草稿（含凭据是否已配置）。
    ///
    /// 草稿里的凭据一并换成掩码：刚录入的明文不应该继续留在界面上，
    /// 「留空＝不改动」的语义也从这一刻起成立。
    pub fn set_web_search(&mut self, row: WebSearchRow) {
        let values = web_search::web_search_form_values(&row);
        if let Some(form) = self.forms.get_mut(FormKind::WebSearch.index()) {
            form.set_values(values);
        }
    }

    /// 宿主回填：图像生成草稿按磁盘的生效值重排（归一化可能折算过端点与模型）。
    ///
    /// 只覆盖服务商与之相关的两行（`base_url` / `model`），其余字段以草稿为准——
    /// 保存刚写盘的就是草稿，重排只是把配置侧的规范化结果同步回界面。
    pub fn set_image_gen_provider_defaults(&mut self, provider: &str, base_url: &str, model: &str) {
        let Some(form) = self.forms.get_mut(FormKind::ImageGen.index()) else {
            return;
        };
        form.set_value(
            IMAGE_GEN_PROVIDER_INDEX,
            FieldValue::Text(provider.to_string()),
        );
        form.set_value(
            IMAGE_GEN_BASE_URL_INDEX,
            FieldValue::Text(base_url.to_string()),
        );
        form.set_value(IMAGE_GEN_MODEL_INDEX, FieldValue::Text(model.to_string()));
        let gemini = provider.trim().eq_ignore_ascii_case("gemini");
        form.set_gemini(gemini);
    }

    /// 决策渠道列表（渲染层用）。
    pub fn decision_rows(&self) -> &[DecisionRow] {
        &self.decision_models.rows
    }

    pub fn decision_selected(&self) -> usize {
        self.decision_models.selected
    }

    /// 决策模型页的功能开关行（渲染层用）。
    pub fn decision_switch_rows(&self) -> &[DecisionSwitchRow] {
        &self.decision_models.switches
    }

    /// 决策页列表态的总行数（渠道 + 开关 + 自部署分区）；渲染层的窗口滚动与行号都读它。
    pub fn decision_row_count(&self) -> usize {
        self.decision_models.rows.len()
            + self.decision_models.switches.len()
            + DECISION_LOCAL_ROW_COUNT
    }

    /// 自部署分区的行（渲染层用）。
    pub fn decision_local_rows(&self) -> Vec<DecisionLocalRow> {
        self.decision_models.local.rows()
    }

    /// 自部署分区是否有后台任务在跑。
    pub fn decision_local_busy(&self) -> bool {
        self.decision_models.local.busy
    }

    /// 自部署任务进度行的渲染输入：`(阶段文案, 比例, 量化细节)`。
    ///
    /// 比例是 `None` 时渲染层画循环滑块（任务在跑但这一步没有可量化的进度）。
    pub fn decision_progress_line(&self) -> (String, Option<f64>, String) {
        let local = &self.decision_models.local;
        let ratio = local.progress.ratio();
        let value = local.progress_text();
        (value, ratio, local.progress.detail())
    }

    pub fn decision_default_key(&self) -> &str {
        &self.decision_models.default_key
    }

    /// 正在编辑的决策渠道（渲染层用）；不在表单时为 `None`。
    pub fn decision_form(&self) -> Option<DecisionFormView<'_>> {
        let form = self.decision_models.form.as_ref()?;
        Some(DecisionFormView {
            row: &form.row,
            field: form.field,
            input: form.input.as_ref(),
            dropdown: form.dropdown.as_ref(),
            is_new: form.is_new,
        })
    }

    /// 宿主回填：向量模型页的状态变化（保存失败时的提示等）。
    pub fn refresh_embedding_models(
        &mut self,
        rows: Vec<EmbeddingRow>,
        default_key: &str,
        enabled: bool,
        message: &str,
    ) {
        self.embedding_models.rows = rows;
        self.embedding_models.enabled = enabled;
        if !default_key.trim().is_empty() {
            self.embedding_models.default_key = default_key.trim().to_string();
        }
        let last = self.embedding_models.total_rows().saturating_sub(1);
        if self.embedding_models.selected > last {
            self.embedding_models.selected = last;
        }
        if !message.trim().is_empty() {
            self.embedding_models.status = message.trim().to_string();
        }
    }

    /// 宿主回填：顶部总开关的现值（开关翻转由状态层先画，再以磁盘结果为准）。
    pub fn set_embedding_enabled(&mut self, enabled: bool) {
        self.embedding_models.enabled = enabled;
    }

    pub fn embedding_rows(&self) -> &[EmbeddingRow] {
        &self.embedding_models.rows
    }

    /// 顶部总开关的当前值（渲染层用）。
    pub fn embedding_enabled(&self) -> bool {
        self.embedding_models.enabled
    }

    /// 联网搜索走向量模型排序的当前值（渲染层用）。
    /// 行游标是否停在开关区（向量召回 / 联网搜索向量排序）。
    pub fn embedding_switch_selected(&self) -> bool {
        self.embedding_models.selected < EmbeddingModelsState::SWITCH_ROWS
    }

    /// 游标在开关区时的行号（0 向量召回、1 联网搜索向量排序）。
    pub fn embedding_switch_row(&self) -> usize {
        self.embedding_models.selected
    }

    /// 选中的**渠道**下标；游标停在总开关时为 `None`。
    pub fn embedding_selected(&self) -> Option<usize> {
        EmbeddingModelsState::channel_index(self.embedding_models.selected)
            .filter(|index| *index < self.embedding_models.rows.len())
    }

    pub fn embedding_default_key(&self) -> &str {
        &self.embedding_models.default_key
    }

    /// 正在编辑的向量渠道（渲染层用）；不在表单时为 `None`。
    pub fn embedding_form(&self) -> Option<EmbeddingFormView<'_>> {
        let form = self.embedding_models.form.as_ref()?;
        Some(EmbeddingFormView {
            row: &form.row,
            field: form.field,
            input: form.input.as_ref(),
            dropdown: None,
            is_new: form.is_new,
        })
    }

    /// 宿主回填：自部署分区的状态变化（下载完成 / 环境就绪 / 服务启停 / 任务进度）。
    ///
    /// 只覆盖分区的只读值，不动当前选中的行与渠道列表；`message` 非空时同时更新状态行，
    /// 空串表示「只刷新事实」。
    ///
    /// `busy` 完全由宿主给出的 `task` 决定：任务结束（`task` 为 `None`）时这里必然复位，
    /// 不会再出现「后台早已结束、界面还按着『进行中』不放」的死锁。
    pub fn refresh_decision_local(&mut self, values: DecisionLocalValues, message: String) {
        self.decision_models.local = DecisionLocalState::new(values);
        if !message.trim().is_empty() {
            self.decision_models.status = message;
        }
    }

    /// 保存后把决策页与磁盘对齐（与 `sync_channels` 同义）。
    pub fn sync_decision_models(&mut self, rows: Vec<DecisionRow>, default_key: &str) {
        self.decision_models.selected = self.decision_models.selected.min(
            (rows.len() + self.decision_models.switches.len() + DECISION_LOCAL_ROW_COUNT)
                .saturating_sub(1),
        );
        self.decision_models.default_key = if default_key.trim().is_empty() {
            rows.first().map(|row| row.key.clone()).unwrap_or_default()
        } else {
            default_key.to_string()
        };
        self.decision_models.rows = rows;
        self.decision_models.form = None;
    }
}
