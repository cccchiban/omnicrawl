//! 设置面板：全屏两区（左侧一级项 + 右侧二级面板），对映
//! `omnicrawl/ui/fullscreen/screens/` 的 `parse` 与各 `*SettingsPane`。
//!
//! 界面层不碰配置与内核：状态机只产出 [`SettingsEvent`]，由 `app.rs` 去写盘、
//! 推内核、按需重建工具表，再把结果写回界面（成功/失败都只是一行状态文本）。
//! 这样设置面板可以脱离内核单独用 `TestBackend` 驱动。
//!
//! 已落地 `settings.py` 的一级菜单，以及模型管理、工具开关、子任务设置、视觉、
//! 三个单选页、顾问设置、工具输出压缩、消息脱敏、持续运转、隔离工作区、图像生成
//! 六个表单页、TTS 页与 MCP 设置；「通过对话修改设置」是一个动作行，`Enter` 直接把
//! 控制权交给配置对话弹层（见 `crate::ui::config_chat`），因此它没有右侧面板。

pub mod form;
pub mod hit;
pub mod picker;
pub mod render;
pub mod state;

use omnicrawl_config::models::llm::normalize_reasoning_effort;

pub use form::{FieldKind, FieldSpec, FieldValue, FormKind, FormState, FORM_KINDS};
pub use hit::{HitAction, HitRegion};
pub use picker::{split_token, ModelDiscovery};

pub use state::{
    channel_field_value, decision_field_value, display_value, ChannelDropdown, ChannelField,
    ChannelFormView, ChannelRow, ChoiceKind, DecisionDropdown, DecisionField,
    DecisionFormView, DecisionLocalChange, DecisionLocalRow, DecisionLocalValues, DecisionRow,
    DecisionSwitchRow, Dropdown, DropdownField, Focus, McpChange,
    McpEditorRowView, McpRowView, McpServerDraft, McpServerRow, McpSettingsValues, ModelParamRow,
    OptionValue,
    Pane, SettingsChange, SettingsEvent, SettingsState, SettingsValues, SubagentChange, SubagentRow,
    ToolSwitchRow, TtsChange, TtsDraft, TtsRowView, TtsValues, VisionChange, VisionModelRef,
    DECISION_LOCAL_ROW_COUNT, DECISION_LOCAL_SECTION, DECISION_MODE_OPTIONS, DECISION_SWITCH_SECTION,
    MCP_EDITOR_FIELD_COUNT, MCP_EDITOR_LABELS, MCP_ROW_COUNT,
    MCP_ROW_LABELS, MCP_TIMEOUT_OPTIONS, MODEL_PARAM_SECTION, TOOLS_APPROVAL_LABEL,
    TOOLS_APPROVAL_OPTIONS,
    TOOLS_APPROVAL_ROW, TTS_DEVICE_OPTIONS, TTS_FALLBACK_VOICES, TTS_ROW_COUNT, TTS_ROW_LABELS,
    TTS_THREAD_COUNTS,
};

/// 一级设置项的自上而下顺序（对映 Python 的 `_SETTING_ORDER`）。
///
/// `mcp` 是 Rust 宿主额外暴露的一级项：Python 把 MCP 设置做成独立弹层
/// （`MCPSettingsScreen`）并由 `SettingsAction("mcp_settings")` 打开，Rust 的设置面板
/// 没有独立导航层，因此把它并入左侧列表，键名与 Python 的 `_build_complex_pane("mcp")` 一致。
pub const ROW_ORDER: [&str; 17] = [
    "config_chat",
    "model_management",
    "decision_models",
    "advisor",
    "tool_output_compression",
    "tools",
    "mcp",
    "vision",
    "image_gen",
    "tts",
    "run_guard",
    "agent_workspace",
    "desensitization",
    "memory",
    "plugins",
    "subagents",
    "show_thinking",
];

/// 一级项的界面文案（对映 Python 的 `_row_labels`）；未知键回落为键名本身。
pub fn row_label(key: &str) -> String {
    let label = match key {
        "config_chat" => "通过对话修改设置",
        "model_management" => "模型管理",
        "decision_models" => "结构化决策模型",
        "advisor" => "顾问设置",
        "tool_output_compression" => "工具输出压缩",
        "tools" => "工具设置",
        "mcp" => "MCP 设置",
        "vision" => "视觉",
        "image_gen" => "图像生成",
        "tts" => "TTS 语音合成",
        "run_guard" => "持续运转",
        "agent_workspace" => "隔离工作区",
        "desensitization" => "消息脱敏",
        "memory" => "记忆功能",
        "plugins" => "插件功能",
        "subagents" => "子任务设置",
        "show_thinking" => "思考显示",
        other => other,
    };
    label.to_string()
}

/// 子任务设置页的一个高级参数（对映 Python 的 `_SUBAGENT_ADVANCED_LABELS` 与
/// `_SUBAGENT_ADVANCED_OPTIONS`，键顺序与 `SUBAGENT_ADVANCED_SETTING_KEYS` 一致）。
pub struct SubagentAdvancedSpec {
    pub key: &'static str,
    pub label: &'static str,
    /// 可循环的档位。
    pub options: &'static [i64],
    /// 该键在配置里是浮点数：写盘时要用 `Float`，否则类型校验会拒绝。
    pub is_float: bool,
}

/// 子任务设置页的高级参数表。
pub const SUBAGENT_ADVANCED_SPECS: [SubagentAdvancedSpec; 6] = [
    SubagentAdvancedSpec {
        key: "max_concurrency",
        label: "最大并发数",
        options: &[1, 2, 3, 4],
        is_float: false,
    },
    SubagentAdvancedSpec {
        key: "max_tasks_per_batch",
        label: "每批最大任务数",
        options: &[1, 2, 3, 4],
        is_float: false,
    },
    SubagentAdvancedSpec {
        key: "default_timeout_seconds",
        label: "子任务超时（秒）",
        options: &[30, 60, 120, 300, 600, 1200, 3600],
        is_float: true,
    },
    SubagentAdvancedSpec {
        key: "model_request_concurrency",
        label: "模型请求并发数",
        options: &[1, 2, 3, 4],
        is_float: false,
    },
    SubagentAdvancedSpec {
        key: "verify_command_timeout_seconds",
        label: "验证检查超时（秒）",
        options: &[30, 60, 120, 180, 240, 360],
        is_float: false,
    },
    SubagentAdvancedSpec {
        key: "task_retention_minutes",
        label: "任务保留时间（分钟）",
        options: &[15, 30, 60, 120, 360, 1440, 10080],
        is_float: false,
    },
];

/// 按档位表折算当前值并移动 `direction` 档（对映 Python 的档位循环：
/// 值不在候选里时先取最接近的一档，再移动）。
pub fn cycle_subagent_option(options: &[i64], current: i64, direction: isize) -> i64 {
    if options.is_empty() {
        return current;
    }
    let index = options
        .iter()
        .position(|option| *option == current)
        .unwrap_or_else(|| {
            options
                .iter()
                .enumerate()
                .min_by_key(|(_, option)| (**option - current).abs())
                .map(|(index, _)| index)
                .unwrap_or(0)
        });
    let count = options.len() as isize;
    options[((index as isize + direction).rem_euclid(count)) as usize]
}

/// 上下文长度候选（单位 K Token）。
pub const CONTEXT_WINDOW_OPTIONS_K: [i64; 7] = [32, 64, 128, 256, 512, 1024, 2048];

/// 上下文压缩阈值候选（百分比，5 为一个单位）。
pub const CONTEXT_COMPACTION_PERCENT_OPTIONS: [i64; 19] = [
    5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85, 90, 95,
];

/// 把任意上下文窗口折算到候选档位，取最接近的一档（单位 Token）。
pub fn nearest_context_window_tokens(window_tokens: i64) -> i64 {
    CONTEXT_WINDOW_OPTIONS_K
        .iter()
        .copied()
        .min_by_key(|option| (option * 1000 - window_tokens).abs())
        .unwrap_or(128)
        * 1000
}

/// 把任意百分比折算到候选档位（对映 `_context_compaction_percent`）。
pub fn nearest_compaction_percent(percent: i64) -> i64 {
    CONTEXT_COMPACTION_PERCENT_OPTIONS
        .iter()
        .copied()
        .min_by_key(|option| (option - percent).abs())
        .unwrap_or(80)
}

/// 推理强度的候选（对映 Python 的 `_REASONING_OPTIONS` 与 `_REASONING_LABELS`）。
pub const REASONING_OPTIONS: [(&str, &str); 6] = [
    ("none", "关闭"),
    ("low", "低"),
    ("medium", "中"),
    ("high", "高"),
    ("xhigh", "超高"),
    ("max", "最大"),
];

/// 把任意推理强度值折算到候选（`Select` 必须命中候选，与 Python 同义）。
///
/// 别名表（`x_high` / `extra_high` / `off` / `med` …）来自配置域
/// [`normalize_reasoning_effort`]，与 Python 的 `_REASONING_EFFORT_ALIASES` 同源；
/// 未识别或折算出 `disabled` 的值一律落到候选首项 `none`（Python 里设置页只在
/// 六个候选之间选择，`disabled` 不出现）。
pub fn normalize_reasoning(value: &str) -> String {
    let canonical = normalize_reasoning_effort(value).unwrap_or("none");
    REASONING_OPTIONS
        .iter()
        .map(|(option, _)| *option)
        .find(|option| *option == canonical)
        .unwrap_or("none")
        .to_string()
}

/// 推理强度的中文文案。
///
/// 已识别的档位（含别名）给出中文文案；配置域不认得的值回落为候选首项的取值
/// `none`——与设置页只展示六档候选、不会凭空造出一个未知档位同义。
pub fn reasoning_label(effort: &str) -> String {
    match normalize_reasoning_effort(effort) {
        Ok(canonical) => REASONING_OPTIONS
            .iter()
            .find(|(option, _)| *option == canonical)
            .map(|(_, label)| (*label).to_string())
            .unwrap_or_else(|| canonical.to_string()),
        Err(_) => "none".to_string(),
    }
}

/// 某个单选页的候选（文案 + 取值）。
///
/// 三个单选页都是开关，候选与 Python 的 SelectPane 一致；候选随环境变化的字段
/// （渠道与模型、上下文与推理档位）挂在模型管理页与表单页上，不在静态表里。
pub fn choice_field_options(kind: ChoiceKind) -> Vec<(String, OptionValue)> {
    match kind {
        // 「思考显示 / 记忆 / 插件」都是开关，候选与 Python 的 SelectPane 一致。
        ChoiceKind::ShowThinking | ChoiceKind::Memory | ChoiceKind::Plugins => vec![
            ("开启".to_string(), OptionValue::Flag(true)),
            ("关闭".to_string(), OptionValue::Flag(false)),
        ],
    }
}

/// 上下文长度的候选（文案 + Token）。
pub fn context_window_options() -> Vec<(String, i64)> {
    CONTEXT_WINDOW_OPTIONS_K
        .iter()
        .map(|k| (format!("{k}K"), k * 1000))
        .collect()
}

/// 上下文压缩阈值的候选（文案 + 百分比）。
pub fn context_compaction_options() -> Vec<(String, i64)> {
    CONTEXT_COMPACTION_PERCENT_OPTIONS
        .iter()
        .map(|percent| (format!("{percent}%"), *percent))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_order_covers_every_label() {
        for key in ROW_ORDER {
            assert_ne!(row_label(key), key, "一级项 {key} 缺少界面文案");
        }
        assert_eq!(row_label("config_chat"), "通过对话修改设置");
    }

    #[test]
    fn nearest_options_snap_to_candidates() {
        assert_eq!(nearest_context_window_tokens(128_000), 128_000);
        assert_eq!(nearest_context_window_tokens(1), 32_000);
        assert_eq!(nearest_context_window_tokens(300_000), 256_000);
        assert_eq!(nearest_compaction_percent(80), 80);
        assert_eq!(nearest_compaction_percent(83), 85);
        assert_eq!(nearest_compaction_percent(0), 5);
    }

    #[test]
    fn reasoning_options_match_python_table() {
        assert_eq!(normalize_reasoning("X-High"), "xhigh");
        assert_eq!(normalize_reasoning("  MAX "), "max");
        assert_eq!(
            normalize_reasoning("ultra"),
            "none",
            "未知档位折算到候选首项"
        );
        assert_eq!(reasoning_label("xhigh"), "超高");
        assert_eq!(reasoning_label("none"), "关闭");
        assert_eq!(reasoning_label("没见过的值"), "none");
    }

    #[test]
    fn choice_options_cover_every_kind() {
        for kind in [
            ChoiceKind::ShowThinking,
            ChoiceKind::Memory,
            ChoiceKind::Plugins,
        ] {
            assert_eq!(
                choice_field_options(kind),
                vec![
                    ("开启".to_string(), OptionValue::Flag(true)),
                    ("关闭".to_string(), OptionValue::Flag(false)),
                ],
                "开关类单选页的候选与 Python 的 SelectPane 一致"
            );
        }
    }

    #[test]
    fn subagent_cycle_snaps_and_wraps() {
        let options = [1, 2, 3, 4];
        assert_eq!(cycle_subagent_option(&options, 2, 1), 3);
        assert_eq!(cycle_subagent_option(&options, 4, 1), 1, "到顶回第一档");
        assert_eq!(cycle_subagent_option(&options, 1, -1), 4, "到底回最后一档");
        assert_eq!(
            cycle_subagent_option(&options, 9, 0),
            4,
            "值不在候选里时先取最接近的一档"
        );
        assert_eq!(cycle_subagent_option(&[], 5, 1), 5, "空档位表原样返回");

        let timeouts = SUBAGENT_ADVANCED_SPECS[2].options;
        assert_eq!(SUBAGENT_ADVANCED_SPECS[2].key, "default_timeout_seconds");
        assert!(SUBAGENT_ADVANCED_SPECS[2].is_float, "子任务超时是浮点配置");
        assert_eq!(cycle_subagent_option(timeouts, 30, -1), 3600);
    }

    #[test]
    fn field_options_match_python_candidates() {
        let window = context_window_options();
        assert_eq!(window.len(), CONTEXT_WINDOW_OPTIONS_K.len());
        assert_eq!(window[0], ("32K".to_string(), 32_000));
        assert_eq!(window[6], ("2048K".to_string(), 2_048_000));

        let percent = context_compaction_options();
        assert_eq!(percent.len(), 19);
        assert_eq!(percent[0], ("5%".to_string(), 5));
        assert_eq!(percent[18], ("95%".to_string(), 95));
    }
}
