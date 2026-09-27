//! 设置面板的渲染与键位回归：用 `TestBackend` 驱动，不起内核也不碰配置。
//!
//! 覆盖：两栏与准星边框、焦点在两栏之间的转移、上下文候选下拉（含高亮底色）、
//! 工具开关行的状态文本、状态行回填、窄终端下的左栏收缩。

use crossterm::event::KeyCode;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

use omnicrawl_tui::ui::settings::render::helpers::left_column_width;
use omnicrawl_tui::ui::settings::{
    ChannelRow, ChoiceKind, ContextField, FieldValue, Focus, FormKind, HitAction, Pane,
    SettingsChange, SettingsEvent, SettingsState, SettingsValues, SubagentRow, ToolSwitchRow,
    VisionModelRef,
};

const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;
const LEFT_COLUMN_WIDTH: u16 = 30;
const MIN_RIGHT_WIDTH: u16 = 20;
const COLUMN_PADDING: u16 = 1;

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
            label: "TTS 语音合成（MOSS-TTS-Nano）".to_string(),
            enabled: true,
            registered: false,
        },
    ]
}

fn state() -> SettingsState {
    SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()))
}

/// 构造一条渠道行（协议留空，渲染不看它）。
fn channel(key: &str, name: &str, provider: &str, model_id: &str, enabled: bool) -> ChannelRow {
    ChannelRow {
        key: key.to_string(),
        profile_id: format!("{key}-profile"),
        name: name.to_string(),
        provider: provider.to_string(),
        protocol: String::new(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: "sk-test-key".to_string(),
        api_key_env: "EXAMPLE_API_KEY".to_string(),
        model_id: model_id.to_string(),
        user_agent: String::new(),
        enabled,
    }
}

/// 渲染一帧并返回终端缓冲。
fn draw(state: &SettingsState, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("测试终端");
    terminal
        .draw(|frame| omnicrawl_tui::ui::settings::render::render(frame, frame.area(), state))
        .expect("渲染设置面板");
    terminal.backend().buffer().clone()
}

/// 整屏文本（每行右去空白），用于内容断言。
///
/// `Paragraph` 把宽字符写进一格后按宽度前进，续格留空（未标记 `skip`），
/// 所以这里按显示宽度跳过续格，才能还原出可比的文本。
fn text(buffer: &Buffer) -> String {
    let mut lines: Vec<String> = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        let mut skip = 0usize;
        for x in 0..buffer.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let symbol = buffer[(x, y)].symbol();
            line.push_str(symbol);
            skip = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

/// 找出某个字形首次出现的坐标。
fn find(buffer: &Buffer, glyph: &str) -> (u16, u16) {
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            if buffer[(x, y)].symbol() == glyph {
                return (x, y);
            }
        }
    }
    panic!("缓冲里没有字形 {glyph}");
}

#[test]
fn renders_both_columns_with_row_labels() {
    let screen = text(&draw(&state(), WIDTH, HEIGHT));

    assert!(screen.contains("通过对话修改设置"), "左栏第一项：{screen}");
    assert!(screen.contains("上下文"));
    assert!(screen.contains("工具设置"));
    assert!(screen.contains("思考显示"), "左栏最后一项");
    assert!(
        screen.contains("该设置页尚未迁移到 Rust 宿主"),
        "未迁移的一级项要给出提示：{screen}"
    );
    assert!(
        screen.contains("↑↓ 选择设置项（右侧实时预览）"),
        "底部帮助行"
    );
    assert!(screen.contains("通过对话修改设置"), "右侧标题显示当前项");
}

#[test]
fn crosshair_border_follows_focus() {
    let mut state = state();
    let list_focus = draw(&state, WIDTH, HEIGHT);
    // 焦点在左栏：准星四角落在左栏框上（左右各内缩 1 格）。
    let left_box_right = COLUMN_PADDING + (LEFT_COLUMN_WIDTH - COLUMN_PADDING * 2) - 1;
    assert_eq!(find(&list_focus, "⇘"), (COLUMN_PADDING, 0));
    assert_eq!(find(&list_focus, "⇙").0, left_box_right);
    assert_eq!(find(&list_focus, "⇗").0, COLUMN_PADDING);
    assert_eq!(find(&list_focus, "⇖").0, left_box_right);

    state.handle_key(KeyCode::Enter);
    assert_eq!(state.focus(), Focus::Pane);
    let pane_focus = draw(&state, WIDTH, HEIGHT);
    // 焦点进右栏：左栏恢复圆角，准星移到右栏。
    assert_eq!(find(&pane_focus, "⇘").0, LEFT_COLUMN_WIDTH + COLUMN_PADDING);
    assert_eq!(
        find(&pane_focus, "⇖").0,
        WIDTH - COLUMN_PADDING - 1,
        "右栏框贴到终端右边"
    );
}

#[test]
fn context_panel_shows_current_values() {
    let mut state = state();
    for _ in 0..5 {
        state.handle_key(KeyCode::Down);
    }
    assert_eq!(state.pane(), Pane::Context);
    let screen = text(&draw(&state, WIDTH, HEIGHT));

    assert!(screen.contains("上下文长度"));
    assert!(screen.contains("128K"), "当前窗口：{screen}");
    assert!(screen.contains("上下文阈值"));
    assert!(screen.contains("80%"));
    assert!(screen.contains("Tab 切换字段；选中即保存。"));
}

#[test]
fn context_dropdown_overlay_lists_candidates_and_highlights_current() {
    let mut state = state();
    for _ in 0..5 {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter); // 进入右栏
    state.handle_key(KeyCode::Enter); // 展开候选
    assert_eq!(state.context_field(), ContextField::Window);
    assert!(state.dropdown().is_some());

    let buffer = draw(&state, WIDTH, HEIGHT);
    let screen = text(&buffer);
    assert!(screen.contains("32K"), "候选项要列出来：{screen}");
    assert!(screen.contains("2048K"));

    // 高亮项（当前值 128K）用琥珀底色。
    let (x, y) = find(&buffer, "⇘");
    assert!(x > 0 && y == 0);
    let highlighted = buffer
        .content
        .iter()
        .any(|cell| cell.style().bg == Some(Color::Yellow));
    assert!(highlighted, "展开的候选列表要有高亮项");
}

#[test]
fn esc_collapses_dropdown_then_returns_then_closes() {
    let mut state = state();
    for _ in 0..5 {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter);
    state.handle_key(KeyCode::Enter); // 展开
    assert_eq!(
        state.handle_key(KeyCode::Esc),
        None,
        "展开态 Esc 只收起下拉"
    );
    assert!(state.dropdown().is_none());
    assert_eq!(state.focus(), Focus::Pane);
    assert_eq!(state.handle_key(KeyCode::Esc), None, "再按一次回左栏");
    assert_eq!(state.focus(), Focus::List);
    assert_eq!(
        state.handle_key(KeyCode::Esc),
        Some(SettingsEvent::Close),
        "左栏 Esc 退出设置"
    );
}

#[test]
fn tools_panel_lists_switch_states() {
    let mut state = state();
    while state.selected_key() != "tools" {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter);
    let screen = text(&draw(&state, WIDTH, HEIGHT));

    assert!(screen.contains("读取文件内容：已启用"), "{screen}");
    assert!(screen.contains("执行 PowerShell 命令：已关闭"));
    assert!(
        screen.contains("TTS 语音合成（MOSS-TTS-Nano）：已启用（未注册）"),
        "未注册的工具要标注：{screen}"
    );
    assert!(screen.contains("↑↓ 选择  ←→/Enter/空格 切换  Esc 返回"));
}

#[test]
fn status_line_reports_apply_result() {
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
        "读取文件内容已关闭，已保存到 C:\\Users\\demo\\.OmniCrawl\\config.toml。".to_string(),
    );
    let screen = text(&draw(&state, WIDTH, HEIGHT));

    assert!(screen.contains("读取文件内容：已关闭"));
    assert!(
        screen.contains("已保存到"),
        "状态行要报出保存结果：{screen}"
    );
}

#[test]
fn kernel_rejection_is_appended_without_rolling_back() {
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
        "读取文件内容已关闭，已保存到 config.toml。".to_string(),
    );
    state.note_kernel_rejection(
        &SettingsChange::ToolSwitch {
            name: "read".to_string(),
            enabled: false,
        },
        "内核未接受即时更新（内核未持有模型配置。），将在下次会话生效。",
    );
    let screen = text(&draw(&state, WIDTH, HEIGHT));

    assert!(screen.contains("将在下次会话生效"));
    assert!(screen.contains("读取文件内容：已关闭"), "值不回滚");
}

#[test]
fn narrow_terminal_shrinks_left_column_and_keeps_both_boxes() {
    let state = state();
    let width = 40u16;
    let buffer = draw(&state, width, HEIGHT);
    let expected_left = left_column_width(width, LEFT_COLUMN_WIDTH, 24, MIN_RIGHT_WIDTH);
    assert_eq!(
        expected_left, 24,
        "40 列时左栏收缩到最小宽度（右栏再窄也不把左栏压到 24 以下）"
    );

    // 两栏都还在：左栏准星在 1，右栏准星在收缩后的左栏右边。
    assert_eq!(find(&buffer, "⇘"), (COLUMN_PADDING, 0));
    assert_eq!(
        find(&buffer, "⇙").0,
        COLUMN_PADDING + (expected_left - COLUMN_PADDING * 2) - 1
    );
    let screen = text(&buffer);
    assert!(screen.contains("通过对话修改设置"));
    // 页面底部那行全局帮助已删除（用户要求）；提示改画在右侧面板的下边框上。
    assert!(!screen.contains("↑↓ 选择设置项"), "不应再有底部帮助行");
}

#[test]
fn model_page_renders_current_channel_and_candidates() {
    let mut state = SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_model(
        vec![
            ("主渠道".to_string(), "gpt-main".to_string()),
            ("备用渠道".to_string(), "gpt-backup".to_string()),
        ],
        "gpt-main",
    ));
    while state.selected_key() != "model" {
        state.handle_key(KeyCode::Down);
    }
    let collapsed = text(&draw(&state, WIDTH, HEIGHT));
    assert!(
        collapsed.contains("主渠道"),
        "折叠框显示当前渠道：{collapsed}"
    );

    state.handle_key(KeyCode::Enter); // 进右侧面板
    state.handle_key(KeyCode::Enter); // 展开候选
    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(
        screen.contains("备用渠道"),
        "候选里要有第二个渠道：{screen}"
    );
    assert!(screen.contains("主渠道"));
}

#[test]
fn channels_page_renders_list_then_form() {
    let mut state =
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_channels(
            vec![
                channel("gpt-main", "主渠道", "openai", "gpt-5.2", true),
                channel("claude-backup", "备用渠道", "anthropic", "claude-4", false),
            ],
            "gpt-main",
            channel("新渠道", "新渠道", "openai", "gpt-5.2", true),
        ));
    while state.selected_key() != "channels" {
        state.handle_key(KeyCode::Down);
    }
    let list = text(&draw(&state, WIDTH, HEIGHT));
    assert!(list.contains("主渠道"), "渠道列表：{list}");
    assert!(list.contains("（当前）"), "默认渠道要有标记：{list}");
    assert!(list.contains("已关闭"), "第二条显示关闭：{list}");
    assert!(list.contains("N 新建"), "列表提示：{list}");

    state.handle_key(KeyCode::Enter); // 进右侧面板
    state.handle_key(KeyCode::Enter); // 编辑选中渠道
    let form = text(&draw(&state, WIDTH, HEIGHT));
    assert!(form.contains("渠道名称：主渠道"), "表单字段：{form}");
    assert!(form.contains("Provider：openai"), "{form}");
    // 新增的 API Key 行：只给掩码，明文不进界面。
    assert!(form.contains("API Key：****…-key"), "密钥行应只显示掩码：{form}");
    assert!(form.contains("模型 ID：gpt-5.2"), "{form}");
    assert!(form.contains("Ctrl+S 保存"), "表单提示：{form}");
}

/// 「模型 ID」自动检测出的候选列表：超出一屏时窗口跟着游标走，前后各留一行省略提示。
#[test]
fn channel_model_candidates_scroll_with_the_selection() {
    let mut state =
        SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_channels(
            vec![channel("gpt-main", "主渠道", "openai", "gpt-5.2", true)],
            "gpt-main",
            channel("新渠道", "新渠道", "openai", "gpt-5.2", true),
        ));
    while state.selected_key() != "channels" {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter); // 进右侧面板
    state.handle_key(KeyCode::Enter); // 编辑渠道
    for _ in 0..6 {
        state.handle_key(KeyCode::Down); // 走到「模型 ID」字段
    }
    state.handle_key(KeyCode::Enter); // 请宿主检测；这里直接回填结果
    let models: Vec<String> = (1..=30).map(|index| format!("model-{index:02}")).collect();
    state.set_channel_models(models, String::new());

    let head = text(&draw(&state, WIDTH, HEIGHT));
    assert!(head.contains("model-01"), "窗口起点是最前面的候选：{head}");
    assert!(
        head.contains("... 后面 22 个"),
        "超出一屏时给出尾部条目数：{head}"
    );
    assert!(
        !head.contains("... 前面"),
        "游标还在第一项时不该有前省略行：{head}"
    );

    for _ in 0..10 {
        state.handle_key(KeyCode::Down);
    }
    let scrolled = text(&draw(&state, WIDTH, HEIGHT));
    assert!(
        scrolled.contains("model-11"),
        "游标走到第 11 项时该候选必须可见：{scrolled}"
    );
    assert!(
        scrolled.contains("... 前面 6 个"),
        "窗口跟着游标下移：{scrolled}"
    );
}

#[test]
fn very_short_terminal_still_renders() {
    let state = state();
    let buffer = draw(&state, 30, 8);
    let screen = text(&buffer);
    assert!(!screen.is_empty(), "极窄极矮也要出内容而不是 panic");
}

#[test]
fn reasoning_page_renders_value_and_candidate_overlay() {
    let mut state = state();
    while state.selected_key() != "reasoning" {
        state.handle_key(KeyCode::Down);
    }
    assert_eq!(state.pane(), Pane::Choice(ChoiceKind::Reasoning));
    let collapsed = text(&draw(&state, WIDTH, HEIGHT));
    assert!(
        collapsed.contains("关闭"),
        "缺省档位 none 的文案：{collapsed}"
    );

    state.handle_key(KeyCode::Enter); // 进右侧面板：边框变白粗
    state.handle_key(KeyCode::Enter); // 展开候选
    let buffer = draw(&state, WIDTH, HEIGHT);
    let screen = text(&buffer);
    assert!(screen.contains("超高"), "候选项要列出来：{screen}");
    assert!(screen.contains("最大"));
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.style().bg == Some(Color::Yellow)),
        "展开的候选列表要有高亮项"
    );
}

#[test]
fn choice_page_status_line_reports_saved_path() {
    let mut state = state();
    while state.selected_key() != "memory" {
        state.handle_key(KeyCode::Down);
    }
    assert_eq!(state.pane(), Pane::Choice(ChoiceKind::Memory));
    assert_eq!(state.choice_value(), "关闭", "缺省关闭");

    state.handle_key(KeyCode::Enter); // 进面板
    state.handle_key(KeyCode::Enter); // 展开：游标落在「关闭」
    state.handle_key(KeyCode::Up); // 移到「开启」
    assert_eq!(
        state.handle_key(KeyCode::Enter),
        Some(SettingsEvent::Apply(SettingsChange::Feature {
            key: "memory".to_string(),
            enabled: true,
        }))
    );
    state.apply_succeeded(
        &SettingsChange::Feature {
            key: "memory".to_string(),
            enabled: true,
        },
        "记忆功能已开启，已保存到 config.toml。".to_string(),
    );

    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(screen.contains("记忆功能已开启"), "状态行：{screen}");
    assert!(screen.contains("开启"), "折叠框已翻到开启");
}

/// 带表单初值的设置状态：顾问页（停用 / effort=high / 选备用渠道）与压缩页（启用）。
fn form_state() -> SettingsState {
    SettingsState::new(
        SettingsValues::new(128_000, 80, tool_rows())
            .with_model(
                vec![
                    ("主渠道".to_string(), "gpt-main".to_string()),
                    ("备用渠道".to_string(), "gpt-backup".to_string()),
                ],
                "gpt-main",
            )
            .with_form(
                FormKind::Advisor,
                vec![
                    FieldValue::Flag(false),
                    FieldValue::Text("high".to_string()),
                    FieldValue::Text("gpt-backup".to_string()),
                ],
            )
            .with_form(
                FormKind::ToolOutputCompression,
                vec![
                    FieldValue::Flag(true),
                    FieldValue::Flag(true),
                    FieldValue::Text("low".to_string()),
                    FieldValue::Text("1200".to_string()),
                    FieldValue::Text("24000".to_string()),
                    FieldValue::Text("1500".to_string()),
                    FieldValue::Text("60".to_string()),
                    FieldValue::Text("gpt-main".to_string()),
                ],
            ),
    )
}

#[test]
fn advisor_page_renders_fields_and_candidate_overlay() {
    let mut state = form_state();
    while state.selected_key() != "advisor" {
        state.handle_key(KeyCode::Down);
    }
    assert_eq!(state.pane(), Pane::Form(FormKind::Advisor));
    let collapsed = text(&draw(&state, WIDTH, HEIGHT));
    assert!(
        collapsed.contains("effort：高"),
        "字段与当前值：{collapsed}"
    );
    assert!(
        collapsed.contains("顾问模型：备用渠道"),
        "模型项显示渠道名：{collapsed}"
    );
    assert!(
        !collapsed.contains("该设置页尚未迁移到 Rust 宿主"),
        "顾问页已经迁移：{collapsed}"
    );

    state.handle_key(KeyCode::Enter); // 进右侧面板
    state.handle_key(KeyCode::Down); // 移到 effort
    state.handle_key(KeyCode::Enter); // 展开候选
    let buffer = draw(&state, WIDTH, HEIGHT);
    let screen = text(&buffer);
    assert!(screen.contains("超高"), "候选项要列出来：{screen}");
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.style().bg == Some(Color::Yellow)),
        "展开的候选列表要有高亮项"
    );
}

#[test]
fn compression_page_renders_budgets_and_status() {
    let mut state = form_state();
    while state.selected_key() != "tool_output_compression" {
        state.handle_key(KeyCode::Down);
    }
    assert_eq!(state.pane(), Pane::Form(FormKind::ToolOutputCompression));

    state.handle_key(KeyCode::Enter);
    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(screen.contains("最小压缩字符数：1200"), "{screen}");
    assert!(screen.contains("单条压缩超时（秒）：60"), "{screen}");
    assert!(
        screen.contains("压缩模型：主渠道"),
        "模型项显示渠道名：{screen}"
    );
    assert!(screen.contains("Ctrl+S 保存"), "表单提示：{screen}");

    // 保存失败只在状态行留话，草稿原样保留（不静默改用户的输入）。
    state.apply_failed("设置未完成：压缩结果上限必须是正整数。".to_string());
    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(screen.contains("压缩结果上限必须是正整数"), "{screen}");
    assert!(
        screen.contains("最小压缩字符数：1200"),
        "失败不丢草稿：{screen}"
    );
}

#[test]
fn compression_page_renders_the_inline_model_picker() {
    let mut state = form_state();
    while state.selected_key() != "tool_output_compression" {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter);
    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(screen.contains("M 模型选择器"), "提示行里要有入口：{screen}");
    assert!(screen.contains("渠道选择"), "左列标题：{screen}");
    assert!(screen.contains("模型"), "右列标题：{screen}");
    assert!(screen.contains("按 / 搜索"), "搜索提示行：{screen}");

    // `M` 进选择器：左列列出渠道（当前渠道带 ● 标记），提示行换成选择器键位。
    state.handle_key(KeyCode::Char('m'));
    assert!(state.model_picker().is_some_and(|picker| picker.focused()));
    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(screen.contains("主渠道"), "左列列出渠道：{screen}");
    assert!(screen.contains("切换列"), "选择器提示行：{screen}");
}

#[test]
fn newly_migrated_form_pages_render_their_fields() {
    let mut state = form_state();
    for (key, label) in [
        ("desensitization", "屏蔽异常时中止"),
        ("run_guard", "重复率阈值（0～1）"),
        ("agent_workspace", "隔离模式"),
        ("image_gen", "默认尺寸"),
    ] {
        while state.selected_key() != key {
            state.handle_key(KeyCode::Down);
        }
        state.handle_key(KeyCode::Enter);
        let screen = text(&draw(&state, WIDTH, HEIGHT));
        assert!(screen.contains(label), "{key} 页要渲染出字段：{screen}");
        assert!(
            !screen.contains("该设置页尚未迁移到 Rust 宿主"),
            "{key} 页已经迁移：{screen}"
        );
        state.handle_key(KeyCode::Esc);
    }
}

#[test]
fn subagents_page_renders_sections_and_values() {
    let mut state = SettingsState::new(
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
                key: "max_concurrency".to_string(),
                label: "最大并发数".to_string(),
                value: "2".to_string(),
                toggle: false,
                active: false,
                section: "高级参数",
            },
        ]),
    );
    while state.selected_key() != "subagents" {
        state.handle_key(KeyCode::Down);
    }
    assert_eq!(state.pane(), Pane::Subagents);
    state.handle_key(KeyCode::Enter);
    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(screen.contains("子任务功能"), "分区标题：{screen}");
    assert!(screen.contains("高级参数"), "{screen}");
    assert!(screen.contains("功能总开关：已关闭"), "{screen}");
    assert!(screen.contains("最大并发数：2"), "{screen}");
    assert!(screen.contains("切换或换档"), "提示行：{screen}");
    assert!(
        !screen.contains("该设置页尚未迁移到 Rust 宿主"),
        "子任务页已经迁移：{screen}"
    );
}

#[test]
fn vision_page_renders_native_state_and_model_rows() {
    let mut state = SettingsState::new(SettingsValues::new(128_000, 80, tool_rows()).with_vision(
        true,
        vec![
            VisionModelRef::custom("gpt-main"),
            VisionModelRef::custom("gpt-backup"),
        ],
        Some(false),
    ));
    while state.selected_key() != "vision" {
        state.handle_key(KeyCode::Down);
    }
    assert_eq!(state.pane(), Pane::Vision);
    state.handle_key(KeyCode::Enter);
    let screen = text(&draw(&state, WIDTH, HEIGHT));
    assert!(screen.contains("模型原生视觉：已关闭"), "{screen}");
    assert!(screen.contains("视觉代理：已启用"), "{screen}");
    assert!(screen.contains("[1] gpt-main"), "列表首项：{screen}");
    assert!(screen.contains("[2] gpt-backup"), "列表次项：{screen}");
    assert!(screen.contains("Ctrl+S 保存"), "提示行：{screen}");
    assert!(
        !screen.contains("该设置页尚未迁移到 Rust 宿主"),
        "视觉页已经迁移：{screen}"
    );
}

// ---------- 鼠标：点选与悬停 ----------

/// 右栏第 `index` 个可点行在屏幕上的行号。
///
/// 反查渲染时记下的命中区，而不是在测试里再算一遍面板内部的布局（标题行、缩进、
/// 窗口滚动都会有影响）。
fn pane_row_y(state: &SettingsState, index: usize) -> u16 {
    (0..HEIGHT)
        .find(|row| state.hit_at(40, *row) == Some(HitAction::PaneRow(index)))
        .unwrap_or_else(|| panic!("右栏第 {index} 行应当可点"))
}

/// 左栏第 `index` 个一级项所在的行号（同理反查）。
fn list_row_y(state: &SettingsState, index: usize) -> u16 {
    (0..HEIGHT)
        .find(|row| state.hit_at(5, *row) == Some(HitAction::Row(index)))
        .unwrap_or_else(|| panic!("左栏第 {index} 行应当可点"))
}

/// 该行是不是被悬停加亮。
///
/// 扫整行而不是看单格：中文标签占两列，其后一个「续格」的 `skip` 为真，刷新时会被
/// 跳过（它本来就被宽字形覆盖），逐格断言会误报。
/// 给定横向范围内的该行是否有黄色字体（悬停高亮：不再用下划线）。
///
/// 只看悬停区域那几列：右栏的选中项本来就是琥珀色，全行扫会把别人的黄也算进来。
fn row_is_hovered_yellow(state: &SettingsState, area: ratatui::layout::Rect) -> bool {
    let buffer = draw(state, WIDTH, HEIGHT);
    (area.x..area.x.saturating_add(area.width))
        .any(|x| buffer[(x, area.y)].style().fg == Some(Color::Yellow))
}

#[test]
fn left_column_click_switches_page_and_enters_the_pane() {
    let mut state = state();
    draw(&state, WIDTH, HEIGHT);
    let y = list_row_y(&state, 7);
    let action = state.hit_at(5, y).expect("左栏行应当可点");
    assert!(state.click(action).is_none(), "切页不产出配置变更事件");
    assert_eq!(state.selected(), 7);
    assert_eq!(state.selected_key(), "tools");
    assert_eq!(state.pane(), Pane::Tools);
    assert_eq!(state.focus(), Focus::Pane, "点左栏等于 Enter，直接进右栏");
}

#[test]
fn pane_row_click_selects_first_and_activates_on_the_same_row() {
    let mut state = state();
    while state.selected_key() != "tools" {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter);
    draw(&state, WIDTH, HEIGHT);
    let y = pane_row_y(&state, 1);

    let action = state.hit_at(40, y).expect("工具行应当可点");
    assert!(
        state.click(action).is_none(),
        "首次点击只把选中移到该行，不能顺手把开关翻掉"
    );
    assert_eq!(state.tool_selected(), 1);

    let event = state.click(action).expect("再点当前行等同 Enter");
    assert!(
        matches!(event, SettingsEvent::Apply(_)),
        "工具开关行确认后应当产出配置变更事件"
    );
}

#[test]
fn dropdown_option_click_confirms_the_choice() {
    let mut state = state();
    while state.selected_key() != "reasoning" {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter);
    state.handle_key(KeyCode::Enter); // 展开候选
    let options = state.dropdown_options();
    assert!(!options.is_empty(), "推理强度页应当有候选");
    draw(&state, WIDTH, HEIGHT);

    // 浮层画在面板之上：同一个落点应当先命中候选行。
    let (column, row) = (
        40u16,
        (0..HEIGHT as usize)
            .map(|row| row as u16)
            .find(|row| state.hit_at(40, *row) == Some(HitAction::Option(2)))
            .expect("浮层第 3 项应当可点"),
    );
    let action = state.hit_at(column, row).expect("浮层候选可点");
    let event = state.click(action).expect("选候选并确认");
    match event {
        SettingsEvent::Apply(SettingsChange::Reasoning { effort }) => {
            assert_eq!(effort, "medium", "第三项应当是中档")
        }
        other => panic!("应当产出推理强度变更：{other:?}"),
    }
    // 状态机只产出事件：界面上的取值由 `App` 落盘后回填，这里不看 `choice_value()`。
}

#[test]
fn hover_paints_the_row_under_the_cursor_yellow() {
    let mut state = state();
    draw(&state, WIDTH, HEIGHT);
    assert!(state.hover_area().is_none(), "未悬停时没有加亮行");

    let y = list_row_y(&state, 2);
    let action = state.hit_at(5, y).expect("左栏行应当可点");
    state.set_hover(Some(action));
    let area = state.hover_area().expect("悬停行应当有区域");
    assert_eq!(area.y, y);
    assert!(row_is_hovered_yellow(&state, area), "悬停行应当是黄色字体");
    let above = ratatui::layout::Rect { y: y - 1, ..area };
    let below = ratatui::layout::Rect { y: y + 1, ..area };
    assert!(!row_is_hovered_yellow(&state, above), "上一行不该加亮");
    assert!(!row_is_hovered_yellow(&state, below), "下一行不该加亮");

    state.set_hover(None);
    assert!(
        !row_is_hovered_yellow(&state, area),
        "光标移开后加亮应当消失"
    );
}

/// 键位提示必须画在右侧面板的**下边框**上（用户要求）：同一行既有提示文字又有 '─'，
/// 而且不再占框内单独一行。
#[test]
fn pane_hint_sits_on_the_bottom_border() {
    let mut state = state();
    // 走到「工具」页（ROW_ORDER 第 8 项）：单选类面板本来就没有提示行，这里要挑有的。
    for _ in 0..7 {
        state.handle_key(KeyCode::Down);
    }
    state.handle_key(KeyCode::Enter);
    assert_eq!(state.focus(), omnicrawl_tui::ui::settings::Focus::Pane);
    let buffer = draw(&state, WIDTH, HEIGHT);
    let hint = state.pane_hint();
    assert!(!hint.is_empty(), "进入面板后应当有键位提示");

    let line = state.pane_hint();
    let marker = line.split_whitespace().next().unwrap_or_default().to_string();
    assert!(
        marker.starts_with('↑') || marker.contains('↑'),
        "提示以方向键开头：{line}"
    );
    let border_row = text(&buffer)
        .lines()
        .find(|row| row.contains(&marker) && row.contains('─'))
        .map(|row| row.to_string());
    assert!(
        border_row.is_some(),
        "提示应当在下边框那一行：{hint:?}\n{}",
        text(&buffer)
    );
}
