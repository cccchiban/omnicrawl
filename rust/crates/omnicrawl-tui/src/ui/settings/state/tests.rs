//! 设置面板状态机的用例：键位语义、行/页推进与表单桥的字段口径。
//!
//! 从 `state.rs` 内联迁出（原文件测试占四成，生产代码被挤到检索视野之外）。

use super::*;

    fn tool_rows() -> Vec<ToolSwitchRow> {
        vec![
            ToolSwitchRow {
                name: "read".to_string(),
                label: "读取文件内容".to_string(),
                enabled: true,
                registered: true,
            },
            ToolSwitchRow {
                name: "powershell".to_string(),
                label: "执行 PowerShell 命令".to_string(),
                enabled: false,
                registered: true,
            },
            ToolSwitchRow {
                name: "tts_synthesize".to_string(),
                label: "TTS 语音合成".to_string(),
                enabled: true,
                registered: false,
            },
        ]
    }

    fn state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()))
    }

    fn embedding_row(key: &str, enabled: bool) -> EmbeddingRow {
        EmbeddingRow {
            key: key.to_string(),
            name: key.to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            api_key: String::new(),
            model: "text-embedding-3-small".to_string(),
            dimensions: 0,
            timeout_seconds: 30,
            enabled,
        }
    }

    fn embedding_state(enabled: bool) -> SettingsState {
        let mut state = state();
        state.embedding_models = EmbeddingModelsState {
            rows: vec![embedding_row("first", true), embedding_row("second", true)],
            default_key: "first".to_string(),
            enabled,
            selected: 0,
            form: None,
            template: EmbeddingRow::default(),
            status: String::new(),
        };
        // 让按键直接落到向量页（与用户进入该页后的状态一致）。
        state.pane = Pane::EmbeddingModels;
        state.focus = Focus::Pane;
        state
    }

    /// 第 0 行是向量召回总开关：Enter 翻转并交宿主写盘，不动渠道。
    #[test]
    fn embedding_top_row_toggles_the_switch() {
        let mut state = embedding_state(false);
        assert!(state.embedding_switch_selected());
        assert_eq!(
            state.embedding_selected(),
            None,
            "游标在开关上时没有选中渠道"
        );

        let event = state.handle_key(KeyCode::Enter);
        match event {
            Some(SettingsEvent::Apply(SettingsChange::EmbeddingEnabled { enabled })) => {
                assert!(enabled, "关闭时回车应当请求开启");
            }
            other => panic!("应当请求翻转总开关，实际：{other:?}"),
        }
    }

    /// 上下移动跨越开关与渠道，且不会停在越界位置。
    #[test]
    fn embedding_cursor_walks_switch_then_channels() {
        let mut state = embedding_state(true);
        assert!(state.embedding_switch_selected());

        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.embedding_selected(),
            Some(0),
            "下移一格应选中第一条渠道"
        );
        state.handle_key(KeyCode::Down);
        assert_eq!(state.embedding_selected(), Some(1));
        // 再下移回到顶部开关（环形）。
        state.handle_key(KeyCode::Down);
        assert!(state.embedding_switch_selected());
        assert_eq!(state.embedding_switch_row(), 0);
        // 上移一格又到最后一条渠道。
        state.handle_key(KeyCode::Up);
        assert_eq!(state.embedding_selected(), Some(1));
    }

    /// 游标在开关上时 D 不删渠道。
    #[test]
    fn embedding_delete_is_ignored_on_switch_row() {
        let mut state = embedding_state(true);
        state.handle_key(KeyCode::Char('d'));
        assert_eq!(state.embedding_rows().len(), 2, "开关行上的 D 不该删除渠道");
        assert!(state.embedding_switch_selected());
    }

    /// 单选页的初始值：推理强度 medium、思考显示开启、记忆开启、插件关闭。
    fn choice_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_choices("medium", true, true, false),
        )
    }

    /// 模型管理页的初始值：两个渠道，当前模型是第一条。
    fn model_state(current: &str) -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model(current)
                .with_choices("medium", true, true, false),
        )
    }

    fn goto(state: &mut SettingsState, key: &str) {
        while state.selected_key() != key {
            state.handle_key(KeyCode::Down);
        }
    }

    /// 构造一条渠道行（协议按 Provider 的第一个候选）。
    fn channel_row(key: &str, name: &str, provider: &str, model_id: &str) -> ChannelRow {
        ChannelRow {
            key: key.to_string(),
            profile_id: format!("{key}-profile"),
            name: name.to_string(),
            provider: provider.to_string(),
            protocol: protocols_for_provider(provider)
                .first()
                .map(|protocol| (*protocol).to_string())
                .unwrap_or_default(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-test-key".to_string(),
            model_id: model_id.to_string(),
            user_agent: String::new(),
            enabled: true,
        }
    }

    /// 渠道页的初始值：两条渠道（当前是第一条）与一份新建模板。
    fn model_management_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_channels(
            vec![
                channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                channel_row("claude-backup", "备用渠道", "anthropic", "claude-4"),
            ],
            "gpt-main",
            channel_row("新渠道", "新渠道", "openai", "gpt-5.2"),
        ))
    }

    #[test]
    fn starts_on_first_row_with_its_pane() {
        let state = state();
        assert_eq!(state.selected_key(), "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);
        assert_eq!(state.focus(), Focus::List);
        assert_eq!(state.title(), "模型管理");
    }

    #[test]
    fn list_navigation_wraps_and_switches_panes() {
        let mut state = state();
        // 走到「工具设置」行。
        goto(&mut state, "tools");
        assert_eq!(state.selected_key(), "tools");
        assert_eq!(state.pane(), Pane::Tools);
        // 上翻依次经过「工具输出压缩」「顾问设置」「向量模型」「结构化决策模型」到「模型管理」。
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "tool_output_compression");
        assert_eq!(state.pane(), Pane::Form(FormKind::ToolOutputCompression));
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "advisor");
        assert_eq!(state.pane(), Pane::Form(FormKind::Advisor));
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "embedding_models");
        assert_eq!(state.pane(), Pane::EmbeddingModels);
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "decision_models");
        assert_eq!(state.pane(), Pane::DecisionModels);
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);

        // 循环：模型管理已是首行，再上翻直接回到末行，下翻再回首行。
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "show_thinking");
        state.handle_key(KeyCode::Down);
        assert_eq!(state.selected_key(), "model_management");
    }

    #[test]
    fn escape_exits_only_from_list() {
        let mut state = state();
        assert_eq!(state.handle_key(KeyCode::Esc), Some(SettingsEvent::Close));

        // 进入右侧后 Esc 先回左侧，再按一次才退出。
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.focus(), Focus::Pane);
        state.handle_key(KeyCode::Esc);
        assert_eq!(state.focus(), Focus::List);
        assert_eq!(state.handle_key(KeyCode::Esc), Some(SettingsEvent::Close));
    }

    #[test]
    fn model_param_rows_show_channel_and_context_values() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);
        assert_eq!(state.channel_rows().len(), 2);
        assert_eq!(state.channel_default_key(), "gpt-main");
        let rows = state.model_param_rows();
        assert_eq!(rows.len(), ModelParamRow::ORDER.len());
        assert_eq!(rows[0].0, ModelParamRow::Channel);
        assert_eq!(rows[0].1, "主渠道");
        assert_eq!(rows[1].1, "128K");
        assert_eq!(rows[2].1, "80%");
        assert_eq!(rows[3].1, "中");
        assert!(!rows[0].2, "初始选中的是渠道行，不是参数行");
    }

    #[test]
    fn model_param_channel_switch_applies_and_moves_cursor() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter); // 进右侧面板
        assert_eq!(state.focus(), Focus::Pane);

        // ↓ 两次到「当前模型渠道」行。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        assert_eq!(state.channel_selected(), 2, "渠道 2 行 + 分区首行");
        let rows = state.model_param_rows();
        assert!(rows[0].2, "分区首行被选中");

        // 展开候选：游标停在当前渠道（第 1 项）。
        assert_eq!(state.handle_key(KeyCode::Enter), None);
        let dropdown = state.dropdown().expect("应展开渠道候选");
        assert_eq!(
            dropdown.field,
            DropdownField::ModelParam(ModelParamRow::Channel)
        );
        assert_eq!(dropdown.selected, 0);
        assert_eq!(state.dropdown_options().len(), 2);

        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Model {
                key: "gpt-backup".to_string()
            }))
        );
        assert_eq!(state.dropdown(), None, "确认后收起下拉");

        state.apply_succeeded(
            &SettingsChange::Model {
                key: "gpt-backup".to_string(),
            },
            "模型已切到 备用渠道（gpt-4o），已保存到 config.toml。".to_string(),
        );
        assert_eq!(state.model_key(), "gpt-backup");
        assert_eq!(
            state.status(),
            "模型已切到 备用渠道（gpt-4o），已保存到 config.toml。"
        );
    }

    #[test]
    fn model_param_context_rows_apply_and_report() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        for _ in 0..3 {
            state.handle_key(KeyCode::Down); // 到「上下文长度」
        }
        assert!(state.model_param_rows()[1].2);

        // 折叠态按 Enter 展开候选，游标停在当前值（128K 是第 3 个候选）。
        state.handle_key(KeyCode::Enter);
        let dropdown = state.dropdown().expect("应展开候选");
        assert_eq!(
            dropdown.field,
            DropdownField::ModelParam(ModelParamRow::Window)
        );
        assert_eq!(dropdown.selected, 2, "128K 是第 3 个候选");

        state.handle_key(KeyCode::Up);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::ContextWindow {
                tokens: 64_000
            }))
        );
        state.apply_succeeded(
            &SettingsChange::ContextWindow { tokens: 64_000 },
            "上下文长度已设为 64K。".to_string(),
        );
        assert_eq!(state.context_window_tokens(), 64_000);
        assert_eq!(state.status(), "上下文长度已设为 64K。");

        // 压缩阈值：↓ 到该行，展开后下移一格到 85%。
        state.handle_key(KeyCode::Down);
        assert!(state.model_param_rows()[2].2);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.compaction_percent(), 80);
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::CompactionPercent {
                percent: 85
            })),
            "从 80% 下移一格到 85%"
        );
    }

    #[test]
    fn model_param_reasoning_row_applies() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        for _ in 0..5 {
            state.handle_key(KeyCode::Down); // 到「推理强度」
        }
        assert!(state.model_param_rows()[3].2);
        state.handle_key(KeyCode::Enter);
        let dropdown = state.dropdown().expect("应展开候选");
        assert_eq!(
            dropdown.field,
            DropdownField::ModelParam(ModelParamRow::Reasoning)
        );
        assert_eq!(dropdown.selected, 2, "medium 是第 3 个候选");
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Reasoning {
                effort: "high".to_string()
            }))
        );
        state.apply_succeeded(
            &SettingsChange::Reasoning {
                effort: "high".to_string(),
            },
            "推理强度已设为 高。".to_string(),
        );
        assert_eq!(state.model_param_rows()[3].1, "高");
    }

    #[test]
    fn model_param_escape_collapses_and_keeps_value() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        for _ in 0..3 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter); // 展开
        state.handle_key(KeyCode::Down);
        assert_eq!(state.handle_key(KeyCode::Esc), None);
        assert_eq!(state.dropdown(), None);
        assert_eq!(state.context_window_tokens(), 128_000);
    }

    #[test]
    fn re_picking_same_value_does_not_apply() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter); // 展开「当前模型渠道」
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            None,
            "选中同一档不触发保存"
        );
    }

    #[test]
    fn failure_keeps_previous_value_and_reports() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.apply_failed("设置未完成：磁盘只读。".to_string());
        assert_eq!(state.context_window_tokens(), 128_000);
        assert_eq!(state.status(), "设置未完成：磁盘只读。");
    }

    #[test]
    fn tools_pane_toggles_selected_row() {
        let mut state = state();
        while state.selected_key() != "tools" {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.pane(), Pane::Tools);
        state.handle_key(KeyCode::Enter); // 进入面板
        assert_eq!(state.tool_selected(), TOOLS_APPROVAL_ROW);
        assert_eq!(
            state.handle_key(KeyCode::Char(' ')),
            Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: "auto".to_string(),
            })),
            "首行是工具调用审查，默认 review 右移一档到 auto"
        );

        // 第二行起是逐工具开关（联网搜索已是左栏的独立设置页）。
        state.handle_key(KeyCode::Down);
        assert_eq!(state.tool_selected(), TOOLS_SWITCH_ROW_OFFSET);
        assert_eq!(
            state.handle_key(KeyCode::Char(' ')),
            Some(SettingsEvent::Apply(SettingsChange::ToolSwitch {
                name: "read".to_string(),
                enabled: false,
            }))
        );
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::ToolSwitch {
                name: "powershell".to_string(),
                enabled: true,
            })),
            "关闭的开关再按一次是启用"
        );
    }

    #[test]
    fn tools_pane_escape_returns_to_list() {
        let mut state = state();
        while state.selected_key() != "tools" {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Right);
        assert_eq!(state.focus(), Focus::Pane);
        state.handle_key(KeyCode::Esc);
        assert_eq!(state.focus(), Focus::List);
        assert_eq!(state.selected_key(), "tools");
    }

    #[test]
    fn select_key_jumps_straight_to_the_requested_page() {
        let mut state = state();
        state.select_key("mcp");
        assert_eq!(state.selected_key(), "mcp");
        assert_eq!(state.pane(), Pane::Mcp);
        assert_eq!(state.focus(), Focus::Pane, "落点直接进右栏");

        state.select_key("tools");
        assert_eq!(state.selected_key(), "tools");
        assert_eq!(state.pane(), Pane::Tools);

        state.select_key("model_management");
        assert_eq!(state.selected_key(), "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);

        // 未知键不动。
        state.select_key("没有这一项");
        assert_eq!(state.selected_key(), "model_management");
    }

    #[test]
    fn apply_success_flips_tool_state_and_refresh_keeps_selection() {
        let mut state = state();
        while state.selected_key() != "tools" {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        state.apply_succeeded(
            &SettingsChange::ToolSwitch {
                name: "read".to_string(),
                enabled: false,
            },
            "读取文件内容已关闭。".to_string(),
        );
        assert!(!state.tool_rows()[0].enabled);
        assert_eq!(state.status(), "读取文件内容已关闭。");

        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        state.refresh_tools(vec![]);
        assert_eq!(state.tool_selected(), 0, "空表时游标回落到首行的审查模式");
        assert!(state.tool_rows().is_empty());
    }

    #[test]
    fn tools_pane_approval_row_cycles_and_applies() {
        let mut state = state();
        goto(&mut state, "tools");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.tool_selected(), TOOLS_APPROVAL_ROW);
        assert_eq!(state.tool_approval_value(), "自动审查", "默认档位是 review");

        assert_eq!(
            state.handle_key(KeyCode::Right),
            Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: "auto".to_string(),
            })),
            "右移一档"
        );
        // 事件尚未回填（状态机要等 `apply_succeeded`），游标仍停在 review。
        assert_eq!(
            state.handle_key(KeyCode::Left),
            Some(SettingsEvent::Apply(SettingsChange::ToolApproval {
                mode: "manual".to_string(),
            })),
            "左移一档"
        );

        state.apply_succeeded(
            &SettingsChange::ToolApproval {
                mode: "manual".to_string(),
            },
            "工具调用审查已设为 人工确认。".to_string(),
        );
        assert_eq!(state.tool_approval_value(), "人工确认");
        assert_eq!(state.status(), "工具调用审查已设为 人工确认。");
    }

    #[test]
    fn approval_cycle_matches_python_approval_options() {
        assert_eq!(cycle_approval_mode("review", 1), "auto");
        assert_eq!(cycle_approval_mode("review", -1), "manual");
        assert_eq!(cycle_approval_mode("auto", 1), "manual", "到顶回第一档");
        assert_eq!(cycle_approval_mode("manual", -1), "auto", "到底回最后一档");
        assert_eq!(
            nearest_approval_mode("去死"),
            "review",
            "不认得的值回落默认档"
        );
        assert_eq!(nearest_approval_mode("always"), "auto", "别名照常折算");
    }

    #[test]
    fn shown_choice_pages_keep_their_option_lists() {
        for kind in [
            ChoiceKind::ShowThinking,
            ChoiceKind::Memory,
            ChoiceKind::Plugins,
        ] {
            let options = choice_field_options(kind);
            assert_eq!(
                options,
                vec![
                    ("开启".to_string(), OptionValue::Flag(true)),
                    ("关闭".to_string(), OptionValue::Flag(false)),
                ],
                "开关类单选页的候选与 Python 的 SelectPane 一致"
            );
        }
    }

    #[test]
    fn show_thinking_page_toggles_to_disabled() {
        let mut state = choice_state();
        goto(&mut state, "show_thinking");
        assert_eq!(state.choice_value(), "开启");

        state.handle_key(KeyCode::Enter); // 进面板
        state.handle_key(KeyCode::Enter); // 展开：游标落在「开启」
        assert_eq!(state.dropdown().map(|dropdown| dropdown.selected), Some(0));
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::ShowThinking {
                enabled: false
            }))
        );
    }

    #[test]
    fn memory_toggle_emits_feature_change() {
        let mut state = choice_state();
        goto(&mut state, "memory");
        assert_eq!(state.pane(), Pane::Choice(ChoiceKind::Memory));
        assert_eq!(state.choice_value(), "开启");

        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char(' ')); // 空格与 Enter 同义：展开候选
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Feature {
                key: "memory".to_string(),
                enabled: false,
            }))
        );
    }

    #[test]
    fn plugins_toggle_starts_from_current_value() {
        let mut state = choice_state();
        goto(&mut state, "plugins");
        assert_eq!(state.choice_value(), "关闭", "初始关闭");

        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter);
        assert_eq!(
            state.dropdown().map(|dropdown| dropdown.selected),
            Some(1),
            "关闭是第 2 个候选"
        );
        state.handle_key(KeyCode::Up);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Feature {
                key: "plugins".to_string(),
                enabled: true,
            }))
        );
    }

    #[test]
    fn choice_pages_keep_their_own_status() {
        let mut state = choice_state();
        goto(&mut state, "memory");
        assert_eq!(state.pane_hint(), "", "单选页没有提示行");
        state.apply_failed("设置未完成：磁盘只读。".to_string());
        assert_eq!(state.status(), "设置未完成：磁盘只读。");

        state.handle_key(KeyCode::Down); // 焦点仍在左侧：换到插件行
        assert_eq!(state.selected_key(), "plugins");
        assert_eq!(state.status(), "", "插件页还没有状态文本");
        state.handle_key(KeyCode::Up);
        assert_eq!(state.selected_key(), "memory");
        assert_eq!(
            state.status(),
            "设置未完成：磁盘只读。",
            "回到记忆页仍是自己的文本"
        );
    }

    #[test]
    fn model_management_lists_and_opens_form() {
        let mut state = model_state("gpt-main");
        goto(&mut state, "model_management");
        assert_eq!(state.pane(), Pane::ModelManagement);
        assert_eq!(state.channel_rows().len(), 2);
        assert_eq!(state.channel_default_key(), "gpt-main");
        assert!(state.pane_hint().contains("N 新建"), "列表提示");

        state.handle_key(KeyCode::Enter); // 进右侧面板
        state.handle_key(KeyCode::Enter); // 编辑选中渠道
        let form = state.channel_form().expect("应进入表单");
        assert_eq!(form.row.key, "gpt-main");
        assert!(!form.is_new);
        assert_eq!(form.field, ChannelField::Name);
        assert!(form.input.is_none());
        assert!(state.pane_hint().contains("Ctrl+S 保存"), "表单提示");
    }

    #[test]
    fn channel_form_edits_text_field_in_place() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 编辑
        state.handle_key(KeyCode::Enter); // 进名称输入态（缓冲以当前值为起点）

        assert!(state.channel_form().and_then(|form| form.input).is_some());
        state.handle_key(KeyCode::Backspace); // 删掉「道」
        state.handle_key(KeyCode::Char('心'));
        state.handle_key(KeyCode::Enter); // 提交

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.name, "主渠心");
        assert!(form.input.is_none(), "提交后退出输入态");
    }

    #[test]
    fn channel_input_escape_discards_edit() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 输入态
        state.handle_key(KeyCode::Char('x'));
        state.handle_key(KeyCode::Esc); // 取消本次字段编辑

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.name, "主渠道", "取消不改动原值");
        assert!(form.input.is_none());
    }

    #[test]
    fn channel_form_provider_dropdown_resets_protocol() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 编辑：字段停在渠道名
        state.handle_key(KeyCode::Down); // → Provider
        state.handle_key(KeyCode::Enter); // 展开候选

        let options = state
            .channel_form()
            .and_then(|form| form.dropdown)
            .map(|dropdown| dropdown.options.clone())
            .expect("Provider 应展开候选");
        assert!(options.len() >= 2, "候选来自 provider_options");
        assert_eq!(options[0], "openai");

        state.handle_key(KeyCode::Down); // openai → anthropic
        state.handle_key(KeyCode::Enter); // 确认

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.provider, "anthropic");
        assert_eq!(
            form.row.protocol,
            protocols_for_provider("anthropic")[0],
            "换 Provider 后协议跟到新 Provider 的第一个候选"
        );
        assert!(form.dropdown.is_none(), "确认后收起候选");
    }

    #[test]
    fn channel_new_draft_kept_out_of_list_and_discarded_on_escape() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('n'));

        let form = state.channel_form().expect("应进入新建表单");
        assert!(form.is_new);
        assert_eq!(form.row.key, "新渠道");
        assert_eq!(state.channel_rows().len(), 2, "草稿还没落进列表");

        state.handle_key(KeyCode::Esc);
        assert!(state.channel_form().is_none());
        assert_eq!(state.channel_rows().len(), 2);
        assert!(
            state.status().contains("放弃"),
            "提示已放弃：{}",
            state.status()
        );
    }

    #[test]
    fn channel_save_validates_then_emits_change() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 编辑主渠道

        // 字段顺序：渠道名 / Provider / 请求协议 / Base URL / API Key / 模型 ID / UA / 启用。
        for _ in 0..5 {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(
            state.channel_form().map(|form| form.field),
            Some(ChannelField::ModelId)
        );
        // 「模型 ID」按 Enter 发起自动检测，而不是直接进输入态。
        match state.handle_key(KeyCode::Enter) {
            Some(SettingsEvent::DiscoverChannelModels { provider, .. }) => {
                assert_eq!(provider, "openai");
            }
            other => panic!("应发起模型检测：{other:?}"),
        }
        let form = state.channel_form().expect("仍在表单");
        assert!(form.input.is_none(), "检测期间不进入手输");
        assert_eq!(form.field, ChannelField::ModelId);

        // 检测失败：退回手输，清空模型 ID 后保存应被校验挡下。
        state.set_channel_models(Vec::new(), "缺少 API Key".to_string());
        for _ in 0..24 {
            state.handle_key(KeyCode::Backspace); // 清空模型 ID
        }
        state.handle_key(KeyCode::Enter); // 提交空值
        assert_eq!(
            state.handle_ctrl_key(KeyCode::Char('s')),
            None,
            "校验失败不产出保存事件"
        );
        assert!(state.status().contains("模型 ID 不能为空"));
        assert!(state.channel_form().is_some(), "校验失败时保留表单");

        // 检测成功：展开候选，选中第二个再保存。
        state.set_channel_models(
            vec!["gpt-5.2".to_string(), "gpt-4o".to_string()],
            String::new(),
        );
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        match state.handle_ctrl_key(KeyCode::Char('s')) {
            Some(SettingsEvent::Apply(SettingsChange::Channels { rows, default_key })) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].model_id, "gpt-4o");
                assert_eq!(default_key, "gpt-main");
            }
            other => panic!("应产出保存事件：{other:?}"),
        }
        assert!(state.channel_form().is_none(), "保存后退出表单");

        state.apply_succeeded(
            &SettingsChange::Channels {
                rows: state.channel_rows().to_vec(),
                default_key: state.channel_default_key().to_string(),
            },
            "渠道配置已保存。".to_string(),
        );
        assert_eq!(state.status(), "渠道配置已保存。");
    }

    #[test]
    fn channel_validation_rejects_duplicates_and_empty_fields() {
        let mut rows = vec![
            channel_row("dup", "一号", "openai", "m1"),
            channel_row("dup", "二号", "openai", "m2"),
        ];
        assert!(validate_channels(&rows)
            .expect_err("重复 key 必须被拒")
            .contains("重复"));

        rows[1].key = "other".to_string();
        rows[1].model_id = String::new();
        assert!(validate_channels(&rows)
            .expect_err("空模型 ID 必须被拒")
            .contains("模型 ID 不能为空"));

        rows[1].model_id = "m2".to_string();
        rows[0].enabled = false;
        rows[1].enabled = false;
        assert!(validate_channels(&rows)
            .expect_err("全关必须被拒")
            .contains("至少需要启用一个"));

        rows[0].enabled = true;
        assert!(validate_channels(&rows).is_ok());
    }

    #[test]
    fn default_channel_selection_prefers_enabled() {
        let mut rows = vec![
            channel_row("a", "A", "openai", "m-a"),
            channel_row("b", "B", "openai", "m-b"),
        ];
        assert_eq!(
            pick_default_channel(&rows, "b"),
            "b",
            "原默认还在且启用就沿用"
        );
        assert_eq!(pick_default_channel(&rows, "gone"), "a");
        rows[1].enabled = false;
        assert_eq!(
            pick_default_channel(&rows, "b"),
            "a",
            "原默认被关掉就换第一条启用的"
        );
    }

    #[test]
    fn channel_delete_keeps_at_least_one() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('d'));
        assert_eq!(state.channel_rows().len(), 1);
        assert_eq!(
            state.channel_default_key(),
            "claude-backup",
            "默认渠道让位给剩下那条"
        );

        state.handle_key(KeyCode::Char('d'));
        assert_eq!(state.channel_rows().len(), 1, "只剩一条时拒绝删除");
        assert!(state.status().contains("至少要保留一个模型渠道"));
    }

    #[test]
    fn channel_sync_from_disk_replaces_rows() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 进表单

        state.sync_channels(
            vec![channel_row("only", "唯一渠道", "gemini", "gemini-3")],
            "only",
        );
        assert_eq!(state.channel_rows().len(), 1);
        assert_eq!(state.channel_default_key(), "only");
        assert!(state.channel_form().is_none(), "同步后回到列表");
    }

    #[test]
    fn empty_reasoning_value_falls_back_to_none() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model("gpt-main")
                .with_choices("", true, false, false),
        );
        goto(&mut state, "model_management");
        assert_eq!(state.model_param_rows()[3].1, "关闭", "空串折算到 none");

        state.handle_key(KeyCode::Enter);
        for _ in 0..5 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter); // 展开推理档位
        assert_eq!(state.dropdown().map(|dropdown| dropdown.selected), Some(0));
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            None,
            "选中同一档不触发保存"
        );
    }

    // ---------- 表单页（顾问设置 / 工具输出压缩） ----------

    /// 顾问设置页的初始值：停用、effort=high、选主渠道、未选模型。
    fn advisor_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model("gpt-main")
                .with_form(
                    FormKind::Advisor,
                    vec![
                        FieldValue::Flag(false),
                        FieldValue::Text("high".to_string()),
                        FieldValue::Text("gpt-main".to_string()),
                        FieldValue::Text(String::new()),
                    ],
                ),
        )
    }

    /// 压缩页 + 两个渠道：用来驱动「渠道 + 模型」两段选择与模型发现。
    fn compression_state_with_channels() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("channel", "主渠道", "openai", "deepseek-v4.1-flash"),
                        channel_row("channel-2", "硅基流动", "openai", ""),
                    ],
                    "channel",
                    ChannelRow::default(),
                )
                .with_form(
                    FormKind::ToolOutputCompression,
                    vec![
                        FieldValue::Flag(true),
                        FieldValue::Flag(false),
                        FieldValue::Text("low".to_string()),
                        FieldValue::Text("1200".to_string()),
                        FieldValue::Text("24000".to_string()),
                        FieldValue::Text("1500".to_string()),
                        FieldValue::Text("60".to_string()),
                        FieldValue::Text("channel".to_string()),
                        FieldValue::Text(String::new()),
                    ],
                ),
        )
    }

    /// 工具输出压缩页的初始值：停用、思考关闭、思考深度 low、四个预算、未选渠道与模型。
    fn compression_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
            FormKind::ToolOutputCompression,
            vec![
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Text("low".to_string()),
                FieldValue::Text("1200".to_string()),
                FieldValue::Text("24000".to_string()),
                FieldValue::Text("1500".to_string()),
                FieldValue::Text("60".to_string()),
                FieldValue::Text(String::new()),
                FieldValue::Text(String::new()),
            ],
        ))
    }

    #[test]
    fn form_page_toggles_flag_and_picks_enum() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.focus(), Focus::Pane);
        assert_eq!(state.form_rows().len(), 4);

        // 「启用」就地翻转，不产生保存事件。
        assert_eq!(state.handle_key(KeyCode::Enter), None);
        assert_eq!(state.form_rows()[0].value, "启用");

        // effort 字段展开候选：游标停在当前值（高），上移一格选到「中」。
        state.handle_key(KeyCode::Down);
        assert_eq!(state.handle_key(KeyCode::Enter), None);
        assert_eq!(
            state.dropdown().map(|dropdown| dropdown.field),
            Some(DropdownField::Form(1))
        );
        state.handle_key(KeyCode::Up);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[1].value, "中");
        assert_eq!(state.dropdown(), None, "确认候选后收起浮层");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::Advisor);
        assert_eq!(values[0], FieldValue::Flag(true));
        assert_eq!(values[1], FieldValue::Text("medium".to_string()));
    }

    #[test]
    fn form_page_channel_and_model_dropdowns_write_the_draft() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Down);
        assert!(
            state.form_rows()[2].value.contains("主渠道"),
            "渠道列显示渠道名"
        );
        assert!(
            state.form_rows()[3].value.contains("渠道默认"),
            "模型列未选时标注「渠道默认」：{}",
            state.form_rows()[3].value
        );

        // 渠道列：候选来自渠道列表，选第二条后草稿写渠道 key。
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 2, "候选来自渠道列表");
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert!(
            state.form_rows()[2].value.contains("备用渠道"),
            "{}",
            state.form_rows()[2].value
        );

        // 模型列：候选 = 该渠道自带的模型 ID（这里没发现结果）。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        let options = state.dropdown_options();
        assert_eq!(options.len(), 1, "只有渠道自带的模型：{options:?}");
        assert_eq!(options[0].0, "gpt-4o");
        state.handle_key(KeyCode::Enter);
        assert!(state.form_rows()[3].value.contains("gpt-4o"));

        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(
            values[2],
            FieldValue::Text("gpt-backup".to_string()),
            "渠道列写渠道 key"
        );
        assert_eq!(
            values[3],
            FieldValue::Text("gpt-4o".to_string()),
            "模型列写模型 ID"
        );
    }

    #[test]
    fn compression_page_model_column_discovers_then_writes_the_draft() {
        let mut state = compression_state_with_channels();
        goto(&mut state, "tool_output_compression");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_field_count(), 9);
        assert_eq!(state.pane_hint(), super::COMPRESSION_HINT);

        // 走到模型列（第 9 项）按 Enter：先按需发现一次，再展开候选。
        for _ in 0..8 {
            state.handle_key(KeyCode::Down);
        }
        let event = state.handle_key(KeyCode::Enter);
        assert!(
            matches!(event, Some(SettingsEvent::DiscoverChannelModels { .. })),
            "进模型列应先发现一次：{event:?}"
        );
        let dropdown = state.dropdown().expect("应展开模型候选");
        assert_eq!(dropdown.field, DropdownField::Form(8));

        // 发现结果回填后候选里多出远端模型（草稿里的当前值也保留）。
        state.set_channel_models(
            vec!["deepseek-v4.1-flash".to_string(), "gpt-5.2".to_string()],
            String::new(),
        );
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            None,
            "模型候选只落进草稿，不即时保存"
        );
        assert_eq!(state.form_rows()[8].value, "gpt-5.2");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::ToolOutputCompression);
        assert_eq!(values[7], FieldValue::Text("channel".to_string()));
        assert_eq!(values[8], FieldValue::Text("gpt-5.2".to_string()));
    }

    #[test]
    fn switching_the_channel_clears_the_model_and_rediscovers() {
        let mut state = compression_state_with_channels();
        goto(&mut state, "tool_output_compression");
        state.handle_key(KeyCode::Enter);
        for _ in 0..7 {
            state.handle_key(KeyCode::Down);
        }
        // 渠道列：换到第二条渠道。
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert!(
            state.form_rows()[7].value.contains("硅基流动"),
            "{}",
            state.form_rows()[7].value
        );
        assert!(
            state.form_rows()[8].value.contains("渠道默认"),
            "换渠道后模型清空：{}",
            state.form_rows()[8].value
        );

        // 进模型列：新渠道还没发现过，应再发一次发现。
        state.handle_key(KeyCode::Down);
        let event = state.handle_key(KeyCode::Enter);
        assert!(
            matches!(event, Some(SettingsEvent::DiscoverChannelModels { .. })),
            "换渠道后模型列要重新发现：{event:?}"
        );
    }

    #[test]
    fn form_page_edits_positive_int_field() {
        let mut state = compression_state();
        goto(&mut state, "tool_output_compression");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 9);

        // 第 4 个字段是「最小压缩字符数」：进输入态、清空、重打。
        for _ in 0..3 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[3].value, "1200▌", "输入态带光标块");
        for _ in 0..4 {
            state.handle_key(KeyCode::Backspace);
        }
        for character in "2400".chars() {
            state.handle_key(KeyCode::Char(character));
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[3].value, "2400");

        // 编辑态里 `Esc` 放弃本次输入，草稿保持上一版。
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('9'));
        state.handle_key(KeyCode::Esc);
        assert_eq!(state.form_rows()[3].value, "2400");
    }

    #[test]
    fn form_page_keeps_draft_when_apply_fails() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 启用但没选模型

        state.apply_failed("设置未完成：启用顾问前请先选择顾问模型。".to_string());
        assert_eq!(state.form_rows()[0].value, "启用", "失败时不回滚草稿");
        assert!(state.status().contains("启用顾问前请先选择顾问模型"));
    }

    #[test]
    fn form_page_escape_returns_to_list() {
        let mut state = advisor_state();
        goto(&mut state, "advisor");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.handle_key(KeyCode::Esc), None);
        assert_eq!(state.focus(), Focus::List, "Esc 先回左侧，不关整屏");
    }

    /// 消息脱敏页的初始值（只给前八个，其余由 `FormState::new` 按字段表补齐）。
    fn desensitization_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
            FormKind::Desensitization,
            vec![
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Flag(true),
                FieldValue::Flag(false),
                FieldValue::Flag(false),
                FieldValue::Text("20".to_string()),
                FieldValue::Text("3.5".to_string()),
            ],
        ))
    }

    #[test]
    fn desensitization_page_lists_detectors_and_toggles() {
        let mut state = desensitization_state();
        goto(&mut state, "desensitization");
        assert_eq!(state.pane(), Pane::Form(FormKind::Desensitization));
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 21, "脱敏页共 21 个字段");
        assert_eq!(state.form_rows()[3].value, "开启", "熵检测兜底的两态文案");
        assert_eq!(state.form_rows()[7].value, "3.5", "小数按原样显示");

        // 走到第 11 行「PEM 私钥」并翻转。
        for _ in 0..10 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[10].value, "开启");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::Desensitization);
        assert_eq!(values[3], FieldValue::Flag(true));
        assert_eq!(values[10], FieldValue::Flag(true));
        assert_eq!(
            values[8],
            FieldValue::Text(String::new()),
            "逗号列表按原样带走"
        );
    }

    #[test]
    fn run_guard_page_edits_float_field() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::RunGuard,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Flag(true),
                    FieldValue::Text("4000".to_string()),
                    FieldValue::Text("400".to_string()),
                    FieldValue::Text("0.8".to_string()),
                    FieldValue::Text("8".to_string()),
                    FieldValue::Text("3".to_string()),
                    FieldValue::Text("20000".to_string()),
                    FieldValue::Text("2".to_string()),
                    FieldValue::Text("429, 500".to_string()),
                    FieldValue::Flag(false),
                    FieldValue::Text("5".to_string()),
                ],
            ));
        goto(&mut state, "run_guard");
        assert_eq!(state.pane(), Pane::Form(FormKind::RunGuard));
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 12);

        // 第 5 行是重复率阈值（小数）：进输入态改成 0.95。
        for _ in 0..4 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        for _ in 0..3 {
            state.handle_key(KeyCode::Backspace);
        }
        for character in "0.95".chars() {
            state.handle_key(KeyCode::Char(character));
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[4].value, "0.95");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::RunGuard);
        assert_eq!(values[4], FieldValue::Text("0.95".to_string()));
        assert_eq!(values[9], FieldValue::Text("429, 500".to_string()));
    }

    #[test]
    fn agent_workspace_page_picks_enum() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::AgentWorkspace,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Text("worktree".to_string()),
                    FieldValue::Text("HEAD".to_string()),
                    FieldValue::Flag(false),
                    FieldValue::Flag(true),
                    FieldValue::Flag(false),
                    FieldValue::Text("auto".to_string()),
                    FieldValue::Text(".env, node_modules".to_string()),
                    FieldValue::Text("npm install".to_string()),
                ],
            ));
        goto(&mut state, "agent_workspace");
        assert_eq!(state.pane(), Pane::Form(FormKind::AgentWorkspace));
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows().len(), 9);
        assert_eq!(state.form_rows()[1].value, "worktree");
        assert_eq!(state.form_rows()[7].value, ".env, node_modules");

        // 第 2 行是「隔离模式」：展开候选选到 local。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 2);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[1].value, "local");
    }

    #[test]
    fn image_gen_page_uses_size_candidates() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::ImageGen,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Text("openai".to_string()),
                    FieldValue::Text("https://api.example.com/v1".to_string()),
                    FieldValue::Text("EXAMPLE_API_KEY".to_string()),
                    FieldValue::Text("gpt-image-2".to_string()),
                    FieldValue::Text("1024x1024".to_string()),
                    FieldValue::Text("high".to_string()),
                    FieldValue::Text("webp".to_string()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text("2".to_string()),
                    FieldValue::Text("120".to_string()),
                ],
            ));
        goto(&mut state, "image_gen");
        assert_eq!(state.pane(), Pane::Form(FormKind::ImageGen));
        state.handle_key(KeyCode::Enter);
        let rows = state.form_rows();
        // OpenAI 通道：Gemini 的两行（画面比例/分辨率档位）不显示。
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[5].value, "1024x1024", "枚举字段显示候选文案");
        assert!(rows[5].has_menu, "枚举字段带下拉标记");
        assert!(!rows[2].has_menu, "文本字段没有下拉标记");

        // 第 6 行是「默认尺寸」：展开候选（8 个）后往后挪到 4K。
        for _ in 0..5 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 8);
        for _ in 0..5 {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_rows()[5].value, "3840x2160");

        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::ImageGen);
        assert_eq!(values[5], FieldValue::Text("3840x2160".to_string()));
        assert_eq!(
            values[3],
            FieldValue::Text("EXAMPLE_API_KEY".to_string()),
            "只带环境变量名，界面里没有凭据本身"
        );
    }

    /// 图像生成页两条通道的字段集互斥：换 provider 后行数与候选都跟着变。
    #[test]
    fn image_gen_page_switches_fields_with_provider() {
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_form(
                FormKind::ImageGen,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Text("openai".to_string()),
                    FieldValue::Text("https://api.example.com/v1".to_string()),
                    FieldValue::Text("EXAMPLE_API_KEY".to_string()),
                    FieldValue::Text("gpt-image-2".to_string()),
                    FieldValue::Text("1024x1024".to_string()),
                    FieldValue::Text("high".to_string()),
                    FieldValue::Text("webp".to_string()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text("2".to_string()),
                    FieldValue::Text("120".to_string()),
                ],
            ));
        goto(&mut state, "image_gen");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.form_field_count(), 10);

        // 走「服务商」行（第 2 行）展开候选并切到 gemini。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.dropdown_options().len(), 2);
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);

        let rows = state.form_rows();
        assert_eq!(
            rows.len(),
            9,
            "Gemini 通道少一行（三选一的目标格式与两项 OpenAI 参数互换）"
        );
        assert_eq!(rows[1].value, "gemini");
        // 行序：启用/provider/地址/凭据变量/模型/画面比例/分辨率档位/张数/超时。
        assert_eq!(rows[5].label, "画面比例");
        assert_eq!(rows[6].label, "分辨率档位");
        // 隐藏行的取值仍留在草稿里（切回 OpenAI 不会丢）。
        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(
            values[5],
            FieldValue::Text("1024x1024".to_string()),
            "隐藏的尺寸仍在草稿"
        );
        assert_eq!(
            values[7],
            FieldValue::Text("webp".to_string()),
            "隐藏的输出格式仍在草稿"
        );
    }

    /// 联网搜索页的初始值：默认引擎 tavily，Tavily 已配凭据，其余未配。
    fn web_search_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_web_search(WebSearchRow {
                engine: "tavily".to_string(),
                tavily_configured: true,
                exa_configured: false,
                firecrawl_configured: false,
                anysearch_configured: false,
                tinyfish_configured: false,
                github_configured: false,
            }),
        )
    }

    #[test]
    fn web_search_is_its_own_left_row() {
        let mut state = web_search_state();
        goto(&mut state, "web_search");
        assert_eq!(
            state.pane(),
            Pane::Form(FormKind::WebSearch),
            "左栏选中即挂载联网搜索页"
        );
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.focus(), Focus::Pane);

        let rows = state.form_rows();
        assert_eq!(rows[0].value, "tavily", "引擎列显示配置里的默认引擎");
        assert_eq!(rows[1].value, "••••••••", "已配凭据只显示掩码");
        assert_eq!(rows[2].value, "（未配置）", "未配凭据显示明确文案");

        // 未配任何凭据时只显示默认引擎。
        let mut plain = SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()));
        goto(&mut plain, "web_search");
        assert_eq!(plain.form_rows()[0].value, "duckduckgo");
    }

    #[test]
    fn web_search_page_masks_credentials_and_keeps_them_blank_on_save() {
        let mut state = web_search_state();
        goto(&mut state, "web_search");
        state.handle_key(KeyCode::Enter); // 进联网搜索页
        assert_eq!(state.pane(), Pane::Form(FormKind::WebSearch));

        let rows = state.form_rows();
        assert_eq!(rows.len(), 7, "默认引擎 + 五把 API Key + GitHub Token");
        assert_eq!(rows[0].value, "tavily", "引擎列显示当前值");
        assert!(rows[0].has_menu, "引擎是候选字段");
        assert_eq!(rows[1].value, "••••••••", "已配凭据只显示掩码");
        assert_eq!(rows[2].value, "（未配置）", "未配凭据显示明确文案");
        assert_eq!(rows[3].value, "（未配置）", "Firecrawl 也是未配");
        assert_eq!(rows[4].value, "（未配置）", "AnySearch 未配（可匿名调用）");
        assert_eq!(rows[5].value, "（未配置）", "TinyFish 未配（无匿名通道）");
        assert_eq!(rows[6].value, "（未配置）", "GitHub Token 未配");

        // 掩码原样保存 = 不改动：五把凭据都折成空串交给宿主。
        let Some(SettingsEvent::Apply(SettingsChange::Form { kind, values })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(kind, FormKind::WebSearch);
        assert_eq!(values[0], FieldValue::Text("tavily".to_string()));
        assert_eq!(values[1], FieldValue::Text(String::new()), "掩码不当作新值");
        assert_eq!(values[2], FieldValue::Text(String::new()));
        assert_eq!(values[3], FieldValue::Text(String::new()));
        assert_eq!(values[4], FieldValue::Text(String::new()));
        assert_eq!(values[5], FieldValue::Text(String::new()));
        assert_eq!(
            values[6],
            FieldValue::Text(String::new()),
            "GitHub Token 未配时也为空"
        );
    }

    #[test]
    fn web_search_page_offers_every_engine_but_not_retired_ones() {
        let mut state = web_search_state();
        goto(&mut state, "web_search");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.handle_key(KeyCode::Enter), None); // 展开「搜索引擎」候选
        let options: Vec<String> = state
            .dropdown_options()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(
            options,
            vec![
                "duckduckgo",
                "github",
                "arxiv",
                "huggingface",
                "tavily",
                "exa",
                "firecrawl",
                "anysearch",
                "tinyfish",
                "mixed"
            ],
            "候选来自 WEB_SEARCH_ENGINES：雅虎与 bing 已下线"
        );
    }

    #[test]
    fn web_search_page_stays_put_after_save() {
        let mut state = web_search_state();
        goto(&mut state, "web_search");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.pane(), Pane::Form(FormKind::WebSearch));

        state.apply_succeeded(
            &SettingsChange::Form {
                kind: FormKind::WebSearch,
                values: vec![
                    FieldValue::Text("exa".to_string()),
                    FieldValue::Text("exa-key".to_string()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                    FieldValue::Text(String::new()),
                ],
            },
            "联网搜索已设为 exa，已保存到 config.toml。".to_string(),
        );
        assert_eq!(
            state.pane(),
            Pane::Form(FormKind::WebSearch),
            "表单页保存后留在原地"
        );
        assert_eq!(state.status(), "联网搜索已设为 exa，已保存到 config.toml。");

        // 凭据状态由宿主按磁盘真实配置覆盖（这里模拟那一步）。
        state.set_web_search(WebSearchRow {
            engine: "exa".to_string(),
            tavily_configured: true,
            exa_configured: true,
            firecrawl_configured: true,
            anysearch_configured: true,
            tinyfish_configured: true,
            github_configured: true,
        });
        let rows = state.form_rows();
        assert_eq!(rows[0].value, "exa", "引擎对齐到磁盘上的生效值");
        for row in &rows[1..] {
            assert_eq!(
                row.value, "••••••••",
                "刚录入的明文在保存后立即换回掩码"
            );
        }
    }

    #[test]
    fn web_search_form_carries_the_github_token_field() {
        let mut state = web_search_state();
        goto(&mut state, "web_search");
        state.handle_key(KeyCode::Enter); // 进联网搜索页
        let rows = state.form_rows();
        assert_eq!(rows.len(), 7, "默认引擎 + 五把 API Key + GitHub Token");
        assert_eq!(rows[4].label, "AnySearch API Key");
        assert_eq!(rows[5].label, "TinyFish API Key");
        assert_eq!(rows[6].label, "GitHub Token");
        assert_eq!(rows[6].value, "（未配置）", "Token 与其余凭据同口径呈现");
    }

    #[test]
    fn web_search_page_typing_a_new_key_reaches_the_draft() {
        let mut state = web_search_state();
        goto(&mut state, "web_search");
        state.handle_key(KeyCode::Enter);
        // 焦点停在引擎列：往下切到 Tavily 密钥，再进输入态重填。
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        for character in "tvly-new".chars() {
            state.handle_key(KeyCode::Char(character));
        }
        state.handle_key(KeyCode::Enter);
        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(values[1], FieldValue::Text("tvly-new".to_string()));
    }

    /// 子任务设置页的初始行：总开关关闭 + 两个高级参数。
    fn subagents_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_subagents(vec![
                SubagentRow {
                    key: "enabled".to_string(),
                    label: "功能总开关".to_string(),
                    value: "已关闭".to_string(),
                    toggle: true,
                    active: false,
                    section: "子任务功能",
                },
                SubagentRow {
                    key: "model_request_concurrency".to_string(),
                    label: "请求并发数".to_string(),
                    value: "2".to_string(),
                    toggle: false,
                    active: false,
                    section: "高级参数",
                },
            ]),
        )
    }

    #[test]
    fn subagents_page_toggles_and_cycles_options() {
        let mut state = subagents_state();
        goto(&mut state, "subagents");
        assert_eq!(state.pane(), Pane::Subagents);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.subagent_rows().len(), 2);

        // 总开关：两个方向都是切换，产出的是目标状态而不是「翻转」。
        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Enabled(true)
            )))
        );

        // 高级参数：`→` 进一档（2 → 3）、`←` 退一档（2 → 1）。
        state.handle_key(KeyCode::Down);
        assert_eq!(
            state.handle_key(KeyCode::Right),
            Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Advanced {
                    key: "model_request_concurrency".to_string(),
                    value: 3,
                }
            )))
        );
        assert_eq!(
            state.handle_key(KeyCode::Left),
            Some(SettingsEvent::Apply(SettingsChange::Subagent(
                SubagentChange::Advanced {
                    key: "model_request_concurrency".to_string(),
                    value: 1,
                }
            )))
        );

        // 回填后行文本与状态跟着变。
        state.apply_succeeded(
            &SettingsChange::Subagent(SubagentChange::Enabled(true)),
            "子任务功能已开启。".to_string(),
        );
        assert_eq!(state.subagent_rows()[0].value, "已开启");
        assert!(state.subagent_rows()[0].active, "回填后开关状态跟着翻");
        assert_eq!(state.status(), "子任务功能已开启。");
    }

    /// 视觉设置页的初始值：代理关闭、一条模型、原生视觉未配置。
    fn vision_state() -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_channels(
                    vec![
                        channel_row("gpt-main", "主渠道", "openai", "gpt-5.2"),
                        channel_row("gpt-backup", "备用渠道", "openai", "gpt-4o"),
                    ],
                    "gpt-main",
                    ChannelRow::default(),
                )
                .with_model("gpt-main")
                .with_vision(false, vec![VisionModelRef::custom("gpt-main")], None),
        )
    }

    #[test]
    fn vision_page_cycles_native_then_adds_and_reorders() {
        let mut state = vision_state();
        goto(&mut state, "vision");
        assert_eq!(state.pane(), Pane::Vision);
        assert_eq!(
            state.vision_native_text(),
            "模型原生视觉：未配置（按模型能力）"
        );
        assert!(state.vision_enabled_text().contains("已停用"));
        assert!(state.vision_enabled_text().contains("1 个"));

        state.handle_key(KeyCode::Enter);
        // `N` 三态循环：未配置 → 开启 → 关闭 → 未配置。
        state.handle_key(KeyCode::Char('n'));
        assert_eq!(state.vision_native_text(), "模型原生视觉：已开启");
        state.handle_key(KeyCode::Char('n'));
        assert_eq!(state.vision_native_text(), "模型原生视觉：已关闭");
        state.handle_key(KeyCode::Char('n'));
        assert_eq!(
            state.vision_native_text(),
            "模型原生视觉：未配置（按模型能力）"
        );

        // `A` 展开渠道候选，选中还没加过的「备用渠道」。
        state.handle_key(KeyCode::Char('a'));
        assert_eq!(state.dropdown_options().len(), 2);
        state.handle_key(KeyCode::Down);
        assert_eq!(state.handle_key(KeyCode::Enter), None, "添加不是保存动作");
        assert_eq!(state.vision_rows().len(), 2);
        assert!(state.vision_rows()[1].contains("gpt-backup"));

        // 重复添加只提示，不重复入列。
        state.handle_key(KeyCode::Char('a'));
        state.handle_key(KeyCode::Down);
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.vision_rows().len(), 2, "同一条渠道不重复添加");

        // Ctrl+↑ 把选中项与上一条对调（故障转移顺序）。
        assert_eq!(state.handle_ctrl_key(KeyCode::Up), None);
        assert!(
            state.vision_rows()[0].contains("gpt-backup"),
            "选中项上移一位：{:?}",
            state.vision_rows()
        );

        // 没动过原生视觉时，保存事件不连带写回。
        state.handle_key(KeyCode::Char(' '));
        let Some(SettingsEvent::Apply(SettingsChange::Vision(change))) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert!(change.enabled);
        assert_eq!(change.models.len(), 2);
        assert_eq!(change.native, None, "原生视觉没动过就不带写回");

        // 动过之后才连带写回（值变化 vs 原值的比较）。
        state.handle_key(KeyCode::Char('n'));
        let Some(SettingsEvent::Apply(SettingsChange::Vision(change))) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(change.native, Some(Some(true)));
    }

    #[test]
    fn vision_page_deletes_and_reports_when_empty() {
        let mut state = vision_state();
        goto(&mut state, "vision");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Char('d'));
        assert!(state.vision_rows().is_empty());
        assert!(state.status().contains("已移除视觉模型"));

        state.handle_key(KeyCode::Char('d'));
        assert!(state.status().contains("没有可删除的视觉模型"));
    }

    fn mcp_state() -> SettingsState {
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_mcp(
            McpSettingsValues {
                enabled: true,
                allow_external_network_tools: false,
                require_confirmation_for_write: true,
                require_confirmation_for_command: true,
                audit_log_enabled: true,
                timeout_seconds: 30,
                servers: vec![McpServerRow {
                    name: "fs".to_string(),
                    enabled: true,
                    transport: "stdio".to_string(),
                    risk_level: "restricted".to_string(),
                }],
            },
        ))
    }

    #[test]
    fn mcp_pane_cycles_timeout_and_toggles_globals() {
        let mut state = mcp_state();
        goto(&mut state, "mcp");
        state.handle_key(KeyCode::Enter); // 进右侧面板
        state.mcp.selected = MCP_ROW_ENABLED;
        match state.handle_key(KeyCode::Char(' ')) {
            Some(SettingsEvent::Apply(SettingsChange::Mcp(McpChange::Globals {
                enabled, ..
            }))) => assert!(!enabled, "总开关被切换"),
            other => panic!("应产出全局变更：{other:?}"),
        }
        state.mcp.selected = MCP_ROW_TIMEOUT;
        match state.handle_key(KeyCode::Right) {
            Some(SettingsEvent::Apply(SettingsChange::Mcp(McpChange::Globals {
                timeout_seconds,
                ..
            }))) => assert_eq!(timeout_seconds, 60, "30 秒进到下一档"),
            other => panic!("应产出全局变更：{other:?}"),
        }
    }

    #[test]
    fn mcp_servers_view_opens_editor_and_saves() {
        let mut state = mcp_state();
        goto(&mut state, "mcp");
        state.handle_key(KeyCode::Enter);
        state.mcp.selected = MCP_ROW_SERVERS;
        assert!(
            state.handle_key(KeyCode::Enter).is_none(),
            "servers 行进入列表"
        );
        assert!(state.mcp_servers_view());

        // 编辑现有 Server：草稿带上原名，保存产出 SaveServer。
        state.handle_key(KeyCode::Enter);
        assert!(state.mcp_editor_title().is_some());
        match state.handle_mcp_editor_save() {
            Some(SettingsEvent::Apply(SettingsChange::Mcp(McpChange::SaveServer(draft)))) => {
                assert_eq!(draft.name, "fs");
                assert_eq!(draft.original_name.as_deref(), Some("fs"));
            }
            other => panic!("应产出保存事件：{other:?}"),
        }

        // 新建：A 打开空草稿，标题是添加。
        state.mcp.editor = None;
        state.handle_key(KeyCode::Char('a'));
        assert!(state.mcp_editor_title().unwrap().contains("添加"));
    }

    #[test]
    fn mcp_apply_succeeded_updates_server_rows() {
        let mut state = mcp_state();
        goto(&mut state, "mcp");
        state.apply_succeeded(
            &SettingsChange::Mcp(McpChange::SetServerEnabled {
                name: "fs".to_string(),
                enabled: false,
            }),
            "已保存".to_string(),
        );
        assert!(!state.mcp_server_rows()[0].enabled);

        state.apply_succeeded(
            &SettingsChange::Mcp(McpChange::SaveServer(Box::new(McpServerDraft {
                name: "net".to_string(),
                transport: "streamable_http".to_string(),
                risk_level: "external".to_string(),
                timeout_seconds: 60,
                ..McpServerDraft::default()
            }))),
            "已保存".to_string(),
        );
        assert_eq!(state.mcp_server_rows().len(), 2);
        assert!(state.mcp_editor_title().is_none(), "保存后关闭编辑器");

        state.apply_succeeded(
            &SettingsChange::Mcp(McpChange::DeleteServer {
                name: "fs".to_string(),
            }),
            "已保存".to_string(),
        );
        assert_eq!(state.mcp_server_rows().len(), 1);
        assert_eq!(state.mcp_server_rows()[0].name, "net");
    }

    /// 行号表与行数必须与标签表对齐；可执行行只有服务商与保存。
    #[test]
    fn tts_rows_line_up_with_their_labels() {
        assert_eq!(TTS_ROW_LABELS.len(), TTS_ROW_COUNT);
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let rows = state.tts_rows();
        let index = |row: usize| {
            state
                .tts_row_index(row)
                .expect("该行在当前服务商下应当可见")
        };
        assert_eq!(rows[index(TTS_ROW_ENABLED)].label, "启用 TTS");
        assert_eq!(rows[index(TTS_ROW_PROVIDER)].label, "服务商 provider");
        assert_eq!(rows[index(TTS_ROW_SAVE)].label, "保存");
        assert!(rows[index(TTS_ROW_SAVE)].action && rows[index(TTS_ROW_PROVIDER)].action);
        assert!(!rows[index(TTS_ROW_MODEL)].action);
        // 两家共用的字段区里不再有朗读语言行。
        let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
        assert!(!labels.iter().any(|label| label.contains("language_code")));
    }

    /// 切换服务商：可见行随之变化（Gemini 没有语速行）。
    #[test]
    fn switching_provider_swaps_the_channel_rows() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter); // 进入面板
        assert_eq!(
            state.tts_rows().len(),
            TTS_ROW_COUNT,
            "OpenAI 下所有行都可见"
        );
        let labels: Vec<&str> = state.tts_rows().iter().map(|row| row.label).collect();
        assert!(labels.contains(&"语速 speed"));

        // 选中服务商行后右切换档，字段区换成 Gemini 的（少一行语速）。
        for _ in 0..TTS_ROW_PROVIDER {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_focused(), TTS_ROW_PROVIDER);
        state.handle_key(KeyCode::Right);
        let labels: Vec<&str> = state.tts_rows().iter().map(|row| row.label).collect();
        assert!(!labels.contains(&"语速 speed"));
        assert_eq!(
            state.tts_rows().len(),
            TTS_ROW_COUNT - 1,
            "Gemini 没有语速行"
        );
        let voice = state.tts_row_index(TTS_ROW_VOICE).expect("音色行应当可见");
        assert_eq!(
            state.tts_rows()[voice].value,
            "Kore",
            "音色跟着换成 Gemini 的候选"
        );
    }

    /// 选中标记必须落在光标那一行：Gemini 下可见行少一行，与配置行号比会错位。
    #[test]
    fn tts_selection_marker_follows_the_visible_rows() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        // 切到 Gemini：语速行消失，其后的行在屏幕上前移一位。
        for _ in 0..TTS_ROW_PROVIDER {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Right);
        while state.tts_focused() != TTS_ROW_API_KEY {
            state.handle_key(KeyCode::Down);
        }
        let rows = state.tts_rows();
        // 语速行被跳过，因此屏幕行号比配置行号小 1。
        assert_eq!(state.tts_focused_index(), TTS_ROW_API_KEY - 1);
        assert_eq!(rows[state.tts_focused_index()].label, "API Key");
        // 每行自报屏幕下标，且只有选中行带焦点：渲染层直接照它画标记。
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.index, index);
            assert_eq!(row.focused, index == state.tts_focused_index());
        }
    }

    /// 通道文本行可编辑：回车进输入态，提交后落回该服务商的草稿。
    #[test]
    fn channel_text_rows_are_editable() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        // 从上往下按到「模型 model」行（它是文本行）。
        let steps = state.tts_row_index(TTS_ROW_MODEL).expect("模型行应当可见");
        for _ in 0..steps {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_focused(), TTS_ROW_MODEL);
        state.handle_key(KeyCode::Enter);
        assert!(state.tts.input.is_some(), "文本行进输入态");
        state.tts.input = None;
    }

    /// 保存行产出草稿事件，草稿带着当前服务商的字段。
    #[test]
    fn save_row_emits_a_draft_with_the_active_channel() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let steps = state.tts_row_index(TTS_ROW_SAVE).expect("保存行应当可见");
        for _ in 0..steps {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_focused(), TTS_ROW_SAVE);
        let Some(SettingsEvent::Apply(SettingsChange::Tts(TtsChange::Save(draft)))) =
            state.handle_key(KeyCode::Enter)
        else {
            panic!("保存行应当产出 Save(Save(draft))");
        };
        assert_eq!(draft.provider, "openai");
        assert!(matches!(draft.channel, TtsChannelDraft::OpenAi { .. }));
    }

    /// 音色候选按服务商切换，且都能循环（说明表不是空的）。
    #[test]
    fn voice_candidates_follow_the_provider() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let voice = state.tts_row_index(TTS_ROW_VOICE).expect("音色行应当可见");
        for _ in 0..voice {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(state.tts_rows()[voice].value, "alloy");
        state.handle_key(KeyCode::Right);
        assert_eq!(state.tts_rows()[voice].value, "ash");
    }

    #[test]
    fn channel_api_key_row_masks_and_blank_input_keeps_previous_value() {
        let mut row = channel_row("probe", "探测渠道", "openai", "gpt-5.2");
        // 掩码显示：明文永不进界面（只给末 4 位）。
        assert_eq!(channel_field_value(&row, ChannelField::ApiKey), "****…-key");
        row.api_key = String::new();
        assert_eq!(
            channel_field_value(&row, ChannelField::ApiKey),
            "（未配置）"
        );
        // 留空 = 不改动：误触回车不会把磁盘上的密钥抹掉。
        row.api_key = "sk-keep-me".to_string();
        set_channel_field(&mut row, ChannelField::ApiKey, "   ".to_string());
        assert_eq!(row.api_key, "sk-keep-me");
        // 非空 = 写入新值（对映 Python 渠道编辑器的 API Key 录入）。
        set_channel_field(&mut row, ChannelField::ApiKey, " sk-new-key ".to_string());
        assert_eq!(row.api_key, "sk-new-key");
        assert_eq!(channel_field_value(&row, ChannelField::ApiKey), "****…-key");
    }

    /// TTS 页的 API Key 行与渠道表单同款：只露掩码、输入态从空起步、留空不改动。
    #[test]
    fn tts_api_key_row_masks_and_blank_input_keeps_previous_value() {
        let mut values = TtsValues::default();
        values.openai.api_key = "sk-tts-secret".to_string();
        let mut state =
            SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_tts(values));
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let row = state
            .tts_row_index(TTS_ROW_API_KEY)
            .expect("API Key 行应当可见");
        assert_eq!(state.tts_rows()[row].value, MASKED_SECRET);

        // 进输入态：缓冲从空起步，避免明文被顺手回写。
        while state.tts_focused() != TTS_ROW_API_KEY {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        let buffer = state
            .tts
            .input
            .as_ref()
            .map(|(_, composer)| composer.text().to_string())
            .expect("应当进了输入态");
        assert!(buffer.is_empty(), "密钥输入态从空起步：{buffer:?}");
        // 直接提交空缓冲 = 不改动磁盘原值。
        state.tts.commit_input();
        assert_eq!(state.tts.openai.api_key, "sk-tts-secret");

        // 填了新值就替换，并立刻在草稿里以明文交给宿主写盘。
        state.tts.begin_input(TTS_ROW_API_KEY);
        if let Some((_, composer)) = state.tts.input.as_mut() {
            composer.insert("sk-brand-new");
        }
        state.tts.commit_input();
        match state.tts.draft().channel {
            TtsChannelDraft::OpenAi { api_key, .. } => assert_eq!(api_key, "sk-brand-new"),
            other => panic!("当前服务商应当是 OpenAI：{other:?}"),
        }
    }

    /// 未配置密钥时显示明确文案，而不是掩码或空白。
    #[test]
    fn tts_api_key_row_reports_missing_credential() {
        let mut state = SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows()).with_tts(TtsValues::default()),
        );
        goto(&mut state, "tts");
        state.handle_key(KeyCode::Enter);
        let row = state
            .tts_row_index(TTS_ROW_API_KEY)
            .expect("API Key 行应当可见");
        assert_eq!(state.tts_rows()[row].value, "（未配置）");
    }

    #[test]
    fn channel_api_key_field_sits_before_model_id() {
        let order = ChannelField::ORDER;
        assert_eq!(order.len(), 8, "渠道表单字段顺序：{order:?}");
        let key = order
            .iter()
            .position(|field| *field == ChannelField::ApiKey)
            .expect("表单里应有 API Key 行");
        assert_eq!(
            order[key + 1],
            ChannelField::ModelId,
            "API Key 之后应是模型 ID：{order:?}"
        );
        assert_eq!(ChannelField::ApiKey.label(), "API Key");
        assert!(ChannelField::ApiKey.is_text(), "API Key 应是可输入字段");
    }

    // ---------- 自部署分区的进度行 ----------

    /// 自部署分区初值：下载 0.8B 到一半。
    fn running_local(task: &str, done: u64, total: u64) -> DecisionLocalValues {
        DecisionLocalValues {
            size: "0.8B".to_string(),
            device: "auto".to_string(),
            sizes: vec![("0.8B".to_string(), "0.8B（约 2.2 GB）".to_string(), false)],
            task: Some(task.to_string()),
            progress: ProgressSnapshot {
                stage: "下载权重".to_string(),
                done,
                total,
                unit: omnicrawl_onejev::ProgressUnit::Bytes,
            },
            ..DecisionLocalValues::default()
        }
    }

    fn decision_local_state(values: DecisionLocalValues) -> SettingsState {
        SettingsState::new(
            SettingsValues::new(128_000, 80, tool_rows())
                .with_decision_models(Vec::new(), "", DecisionRow::default())
                .with_decision_local(values),
        )
    }

    #[test]
    fn task_progress_row_carries_a_bar_and_keeps_busy() {
        let state = decision_local_state(running_local("下载 OneJev 0.8B", 512, 1024));
        assert!(state.decision_local_busy(), "有 task 就是进行中");
        let rows = state.decision_local_rows();
        let progress = &rows[DECISION_LOCAL_ROW_PROGRESS];
        assert!(progress.bar, "进度行要画条");
        assert_eq!(progress.progress, Some(0.5));
        assert!(!progress.progress_detail.is_empty(), "要有量化文案");
        assert!(
            progress.value.contains("下载 OneJev 0.8B"),
            "进度行要点出任务名：{}",
            progress.value
        );
    }

    #[test]
    fn finished_task_releases_busy_and_hides_the_bar() {
        // 这是「任务早已结束、界面还按着进行中不放」的回归测试：
        // `busy` 完全由宿主给的 `task` 推导，任务清空即复位。
        let state = decision_local_state(DecisionLocalValues {
            size: "0.8B".to_string(),
            ..DecisionLocalValues::default()
        });
        assert!(!state.decision_local_busy(), "没有 task 就不该 busy");
        let rows = state.decision_local_rows();
        assert!(!rows[DECISION_LOCAL_ROW_PROGRESS].bar, "没任务不画条");
        assert!(rows[DECISION_LOCAL_ROW_PROGRESS].value.is_empty());
    }

    #[test]
    fn refresh_without_task_resets_busy() {
        // 宿主在任务结束后刷新：即便先前被 `apply_succeeded` 按住，也要放开。
        let mut state = decision_local_state(running_local("下载 OneJev 0.8B", 1, 2));
        goto(&mut state, "decision_models");
        assert!(state.decision_local_busy());
        state.refresh_decision_local(
            DecisionLocalValues {
                size: "0.8B".to_string(),
                ..DecisionLocalValues::default()
            },
            "下载完成。".to_string(),
        );
        assert!(!state.decision_local_busy(), "任务结束必须复位 busy");
        assert_eq!(state.status(), "下载完成。");
    }

    #[test]
    fn unquantified_task_falls_back_to_a_sliding_bar() {
        // 环境安装 / 服务冷启动没有字节数：比例给 `None`，渲染层改画滑动滑块。
        let state = decision_local_state(DecisionLocalValues {
            size: "0.8B".to_string(),
            task: Some("准备运行环境".to_string()),
            progress: ProgressSnapshot {
                stage: "2/3 安装 torch（CUDA 轮子，约 3 GB）…".to_string(),
                ..ProgressSnapshot::default()
            },
            ..DecisionLocalValues::default()
        });
        let rows = state.decision_local_rows();
        assert!(rows[DECISION_LOCAL_ROW_PROGRESS].bar);
        assert_eq!(rows[DECISION_LOCAL_ROW_PROGRESS].progress, None);
        assert!(rows[DECISION_LOCAL_ROW_PROGRESS]
            .value
            .contains("安装 torch"));
    }

    #[test]
    fn busy_activation_reports_the_stage_instead_of_just_waiting() {
        let mut state = decision_local_state(running_local("下载 OneJev 0.8B", 512, 1024));
        goto(&mut state, "decision_models");
        state.handle_key(KeyCode::Enter);
        // 进面板后排到进度行，再按 Enter：应被按住并给出阶段，而不是沉默。
        let rows = state.decision_local_rows();
        assert_eq!(rows.len(), DECISION_LOCAL_ROW_COUNT);
        assert!(state.decision_local_busy());
    }

    // ---------- 粘贴 ----------

    /// 渠道表单的 API Key 行：粘贴即落值，不必先按 Enter 进输入态。
    #[test]
    fn channel_api_key_accepts_pasted_text_without_entering_input_first() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter); // 进面板
        state.handle_key(KeyCode::Enter); // 编辑选中渠道
        for _ in 0..4 {
            state.handle_key(KeyCode::Down); // → API Key
        }
        assert_eq!(
            state.channel_form().map(|form| form.field),
            Some(ChannelField::ApiKey)
        );
        assert!(
            state.channel_form().and_then(|form| form.input).is_none(),
            "粘贴之前不在输入态"
        );

        assert!(state.paste("sk-pasted-key"), "API Key 行应接收粘贴");
        // 落进缓冲、还没提交：界面显示的是掩码，明文只在缓冲里。
        let form = state.channel_form().expect("仍在表单");
        assert_eq!(
            form.input.map(|composer| composer.text()),
            Some("sk-pasted-key")
        );

        state.handle_key(KeyCode::Enter); // 提交
        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.api_key, "sk-pasted-key");
        assert!(form.input.is_none(), "提交后退出输入态");
    }

    /// 粘贴多行凭据只取首行：设置字段都是单行框，换行不能变成提交。
    #[test]
    fn pasted_multiline_secret_keeps_only_the_first_line() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter);
        for _ in 0..4 {
            state.handle_key(KeyCode::Down); // → API Key
        }

        assert!(state.paste("sk-first\r\nsk-second\n"));
        state.handle_key(KeyCode::Enter);

        let form = state.channel_form().expect("仍在表单");
        assert_eq!(form.row.api_key, "sk-first", "只取首行，余下行不提交");
    }

    /// 开关与候选字段不接收粘贴（它们不是文本框），也就不会吞掉事件。
    #[test]
    fn non_text_fields_refuse_paste() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        state.handle_key(KeyCode::Enter);
        state.handle_key(KeyCode::Enter); // 字段停在渠道名

        state.handle_key(KeyCode::Down); // → 请求方式（候选字段）
        assert_eq!(
            state.channel_form().map(|form| form.field),
            Some(ChannelField::Provider)
        );
        assert!(!state.paste("openai"), "候选字段不接收粘贴");
        assert!(
            state.channel_form().and_then(|form| form.input).is_none(),
            "被拒的粘贴不该把字段顶进输入态"
        );
    }

    /// 表单页（如联网搜索的五把凭据）同样接收粘贴，未进输入态时自动进入。
    #[test]
    fn form_page_secret_accepts_pasted_text() {
        let mut state = web_search_state();
        goto(&mut state, "web_search");
        state.handle_key(KeyCode::Enter); // 进联网搜索页
        assert_eq!(state.pane(), Pane::Form(FormKind::WebSearch));

        state.handle_key(KeyCode::Down); // → Tavily API Key
        assert!(state.paste("tvly-pasted"));
        state.handle_key(KeyCode::Enter);

        let Some(SettingsEvent::Apply(SettingsChange::Form { values, .. })) =
            state.handle_ctrl_key(KeyCode::Char('s'))
        else {
            panic!("Ctrl+S 应产出保存事件");
        };
        assert_eq!(values[1], FieldValue::Text("tvly-pasted".to_string()));
    }

    /// 焦点还在左侧一级列表时拒收粘贴：那边没有输入框。
    #[test]
    fn paste_is_refused_while_the_list_column_has_focus() {
        let mut state = model_management_state();
        goto(&mut state, "model_management");
        assert_eq!(state.focus(), Focus::List);

        assert!(!state.paste("sk-leaked"), "左栏没有输入框，不该接收");
        assert!(state.channel_form().is_none(), "也不该顺手打开表单");
    }

    #[test]
    fn computer_use_model_field_opens_dropdown_with_candidates() {
        // 桌面操作页只有「桌面模型」一列、没有渠道列：候选按当前生效渠道给，
        // 不能因为缺渠道字段就退化成空列表——那会让回车看起来毫无反应。
        let mut state = model_management_state();
        goto(&mut state, "computer_use");
        state.handle_key(KeyCode::Enter);
        assert_eq!(state.pane(), Pane::Form(FormKind::ComputerUse));
        assert_eq!(state.form_field_count(), 5);

        // 字段顺序：启用 / 时间预算 / 截图上限 / 思考强度 / 桌面模型。
        for _ in 0..4 {
            state.handle_key(KeyCode::Down);
        }
        assert_eq!(
            state
                .form()
                .and_then(|form| form.field(form.focused()))
                .map(|spec| spec.kind),
            Some(FieldKind::ModelId)
        );

        state.handle_key(KeyCode::Enter);
        let dropdown = state.dropdown().expect("回车应展开模型候选");
        assert_eq!(dropdown.field, DropdownField::Form(4));
        let options = state.dropdown_options();
        assert!(
            !options.is_empty(),
            "候选不能为空，否则浮层不渲染、回车看着没反应"
        );
        assert!(
            options
                .iter()
                .any(|(_, value)| matches!(value, OptionValue::Text(model) if model == "gpt-5.2")),
            "当前渠道自带模型应在候选里：{options:?}"
        );
    }
