//! 只读视图：渲染层要读的行、状态文本与表单快照。
//!
//! 渲染只读这一层，改版式不需要碰键位与写盘。

use super::*;

impl SettingsState {


    // ---------- 只读访问（渲染层用） ----------

    pub fn rows(&self) -> [&'static str; ROW_ORDER.len()] {
        ROW_ORDER
    }


    pub fn dropdown(&self) -> Option<Dropdown> {
        self.dropdown
    }


    /// 单选页当前值的显示文本；不在单选页时返回空串。
    pub fn choice_value(&self) -> String {
        match self.pane {
            Pane::Choice(kind) => display_value(&kind.value(&self.choices)),
            _ => String::new(),
        }
    }


    pub fn tool_rows(&self) -> &[ToolSwitchRow] {
        &self.tools.rows
    }


    pub fn tool_selected(&self) -> usize {
        self.tools.selected
    }


    /// 工具页首行的审查模式显示文案（`工具调用审查：自动审查`）。
    pub fn tool_approval_value(&self) -> String {
        approval_mode_label(&self.tools.approval)
    }


    /// 工具页「联网搜索」行的显示文案（默认引擎 + 已配置的凭据）。
    pub fn web_search_value(&self) -> String {
        self.tools.web_search.label()
    }


    /// 工具页的可选行数：审查模式 + 联网搜索 + 逐工具开关。
    pub fn tool_row_count(&self) -> usize {
        self.tools.rows.len() + TOOLS_SWITCH_ROW_OFFSET
    }


    pub fn subagent_rows(&self) -> &[SubagentRow] {
        &self.subagents.rows
    }


    pub fn subagent_selected(&self) -> usize {
        self.subagents.selected
    }


    /// MCP 全局设置行（渲染层用）。
    pub fn mcp_rows(&self) -> Vec<McpRowView> {
        let values = self.mcp.global_values();
        MCP_ROW_LABELS
            .iter()
            .enumerate()
            .map(|(index, label)| McpRowView {
                label,
                value: values[index].clone(),
                selected: !self.mcp.servers_view
                    && self.mcp.editor.is_none()
                    && index == self.mcp.selected,
            })
            .collect()
    }


    /// MCP 面板当前是否停在 Server 列表。
    pub fn mcp_servers_view(&self) -> bool {
        self.mcp.servers_view
    }


    /// MCP Server 列表（渲染层用）。
    pub fn mcp_server_rows(&self) -> &[McpServerRow] {
        &self.mcp.servers
    }


    pub fn mcp_server_selected(&self) -> usize {
        self.mcp.server_selected
    }


    /// 正在编辑的 Server 标题；不在编辑器时为 `None`。
    pub fn mcp_editor_title(&self) -> Option<String> {
        self.mcp.editor.as_ref().map(|editor| {
            if editor.is_new {
                "添加 MCP Server".to_string()
            } else {
                "编辑 MCP Server".to_string()
            }
        })
    }


    /// Server 编辑器的字段行；不在编辑器时为空。
    pub fn mcp_editor_rows(&self) -> Vec<McpEditorRowView> {
        let Some(editor) = self.mcp.editor.as_ref() else {
            return Vec::new();
        };
        let draft = &editor.draft;
        let values: [String; MCP_EDITOR_FIELD_COUNT] = [
            draft.name.clone(),
            draft.transport.clone(),
            draft.command.clone(),
            draft.args.clone(),
            draft.url.clone(),
            draft.timeout_seconds.to_string(),
            draft.risk_level.clone(),
            draft.env.clone(),
            draft.headers.clone(),
        ];
        MCP_EDITOR_LABELS
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let focused = index == editor.focused;
                let editing = focused && editor.input.is_some();
                let shown = if editing {
                    editor
                        .input
                        .as_ref()
                        .and_then(|composer| composer.wrapped_lines(FORM_INPUT_WIDTH).pop())
                        .map(|text| format!("{text}▌"))
                        .unwrap_or_default()
                } else {
                    values[index].clone()
                };
                McpEditorRowView {
                    label,
                    value: shown,
                    editing,
                    focused,
                    has_menu: index == MCP_EDITOR_TRANSPORT || index == MCP_EDITOR_RISK,
                }
            })
            .collect()
    }



    /// 聚焦字段在**可见行**里的序号（渲染层按它滚动窗口）。
    pub fn form_focus_position(&self) -> usize {
        self.form()
            .map(|form| {
                form.visible_indices()
                    .iter()
                    .position(|index| *index == form.focused())
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }


    /// 表单页当前显示的字段个数（隐藏字段不计入滚动窗口）。
    pub fn form_field_count(&self) -> usize {
        self.form()
            .map(|form| form.visible_indices().len())
            .unwrap_or(0)
    }



    /// 表单页的字段行（渲染层用）；不在表单页时为空。隐藏字段不出现在这份清单里。
    pub fn form_rows(&self) -> Vec<FormFieldView> {
        let Pane::Form(_) = self.pane else {
            return Vec::new();
        };
        let Some(form) = self.form() else {
            return Vec::new();
        };
        form.visible_indices()
            .into_iter()
            .filter_map(|index| {
                let spec = form.field(index)?;
                Some(FormFieldView {
                    label: spec.label,
                    value: self.form_field_value(form, index, spec),
                    has_menu: matches!(
                        spec.kind,
                        FieldKind::Enum(_) | FieldKind::ModelChannel | FieldKind::ModelId
                    ),
                    editing: index == form.focused() && form.input().is_some(),
                    focused: index == form.focused(),
                })
            })
            .collect()
    }


    /// 表单字段的显示文本：开关用两态文案，候选与模型项换成界面文案，输入态带光标块。
    pub(super) fn form_field_value(&self, form: &FormState, index: usize, spec: FieldSpec) -> String {
        if index == form.focused() {
            if let Some(composer) = form.input() {
                return composer
                    .wrapped_lines(FORM_INPUT_WIDTH)
                    .pop()
                    .map(|text| format!("{text}▌"))
                    .unwrap_or_default();
            }
        }
        let raw = form.value(index).map(|value| value.text()).unwrap_or("");
        match spec.kind {
            FieldKind::Flag => {
                let (off, on) = spec.flag_labels;
                if form.value(index).map(|value| value.flag()).unwrap_or(false) {
                    on.to_string()
                } else {
                    off.to_string()
                }
            }
            FieldKind::Enum(options) => options
                .iter()
                .find(|(_, option)| *option == raw)
                .map(|(label, _)| (*label).to_string())
                .unwrap_or_else(|| raw.to_string()),
            FieldKind::ModelChannel | FieldKind::ModelId => self.model_select_value(spec, raw),
            // 凭据从不回显：已配置显示掩码，未配置显示明确文案（草稿里存的是掩码或新值）。
            FieldKind::Secret => {
                if raw.trim().is_empty() {
                    "（未配置）".to_string()
                } else {
                    MASKED_SECRET.to_string()
                }
            }
            FieldKind::Int | FieldKind::Float | FieldKind::Text => raw.to_string(),
        }
    }


    /// 模型选择字段的显示文本：渠道列显示渠道名，模型列显示模型 ID（或「渠道默认」）。
    ///
    /// 草稿里存的是原始值（渠道 key 与模型 ID），这里换成界面文案。
    pub(super) fn model_select_value(&self, field: FieldSpec, raw: &str) -> String {
        match field.kind {
            FieldKind::ModelChannel => self
                .model_management
                .rows
                .iter()
                .find(|row| row.key == raw)
                .map(|row| {
                    if row.name.trim().is_empty() {
                        row.key.clone()
                    } else {
                        row.name.clone()
                    }
                })
                .unwrap_or_else(|| {
                    if raw.trim().is_empty() {
                        "（未选择）".to_string()
                    } else {
                        raw.to_string()
                    }
                }),
            FieldKind::ModelId => {
                if raw.trim().is_empty() {
                    "（渠道默认）".to_string()
                } else {
                    raw.to_string()
                }
            }
            _ => raw.to_string(),
        }
    }


    /// 当前面板底部的一行状态文本；没有状态文本的面板返回空串。
    pub fn status(&self) -> &str {
        match self.pane {
            Pane::Tools => &self.tools.status,
            Pane::Mcp => &self.mcp.status,
            Pane::Subagents => &self.subagents.status,
            Pane::Vision => &self.vision.status,
            Pane::ModelManagement => &self.model_management.status,
            Pane::DecisionModels => &self.decision_models.status,
            Pane::EmbeddingModels => &self.embedding_models.status,
            Pane::Choice(kind) => &self.choices.status[kind.index()],
            Pane::Form(_) => self.form().map(|form| form.status()).unwrap_or(""),
            Pane::Tts => &self.tts.status,
            Pane::Pending => "",
        }
    }


    /// 面板内的操作提示行。
    pub fn pane_hint(&self) -> &'static str {
        match self.pane {
            Pane::Tools => TOOLS_HINT,
            Pane::Mcp => {
                if self.mcp.editor.is_some() {
                    MCP_EDITOR_HINT
                } else if self.mcp.servers_view {
                    MCP_SERVERS_HINT
                } else {
                    MCP_HINT
                }
            }
            Pane::Subagents => SUBAGENTS_HINT,
            Pane::Vision => VISION_HINT,
            Pane::ModelManagement => {
                if self.model_management.form.is_some() {
                    MODEL_FORM_HINT
                } else {
                    MODEL_LIST_HINT
                }
            }
            Pane::DecisionModels => {
                if self.decision_models.form.is_some() {
                    DECISION_FORM_HINT
                } else {
                    DECISION_LIST_HINT
                }
            }
            Pane::EmbeddingModels => {
                if self.embedding_models.form.is_some() {
                    EMBEDDING_FORM_HINT
                } else {
                    EMBEDDING_LIST_HINT
                }
            }
            // Python 的单选面板只有下拉与状态行，没有提示行。
            Pane::Choice(_) => "",
            Pane::Form(FormKind::ToolOutputCompression) => COMPRESSION_HINT,
            Pane::Form(FormKind::WebSearch) => WEB_SEARCH_HINT,
            Pane::Form(_) => FORM_HINT,
            Pane::Tts => TTS_HINT,
            Pane::Pending => "",
        }
    }


    /// 当前表单页的草稿；不在表单页时为 `None`。
    pub(super) fn form(&self) -> Option<&FormState> {
        match self.pane {
            Pane::Form(kind) => self.forms.get(kind.index()),
            _ => None,
        }
    }


    pub(super) fn form_mut(&mut self) -> Option<&mut FormState> {
        match self.pane {
            Pane::Form(kind) => self.forms.get_mut(kind.index()),
            _ => None,
        }
    }



    /// TTS 页的行视图（渲染层用；顺序即当前可见行，索引即屏幕行号）。
    pub fn tts_rows(&self) -> Vec<TtsRowView> {
        self.tts.rows()
    }


    /// TTS 页某一行号在可见行里的下标（不可见时返回 `None`）。
    pub fn tts_row_index(&self, row: usize) -> Option<usize> {
        self.tts.visible_rows().iter().position(|item| *item == row)
    }


    pub fn tts_focused(&self) -> usize {
        self.tts.focused
    }


    /// TTS 页选中行在可见行里的下标（渲染层用它比屏幕行号）。
    pub fn tts_focused_index(&self) -> usize {
        self.tts
            .visible_rows()
            .iter()
            .position(|row| *row == self.tts.focused)
            .unwrap_or(0)
    }
}
