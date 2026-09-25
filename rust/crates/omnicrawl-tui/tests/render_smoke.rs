//! 渲染冒烟：把界面画进 `TestBackend`，检查 HUD、消息流、面板与输入框真的出现在屏幕上。
//!
//! 这里不动终端也不起内核，只驱动 `ui::render` 与状态机。

use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

use omnicrawl_core::{ToolCall, ToolResult};
use omnicrawl_ipc::bridge::{HostEvent, TextPayload, ToolEventPayload, ToolStartedPayload};
use omnicrawl_ipc::Id;
use omnicrawl_tui::args::ApprovalMode;
use omnicrawl_tui::host::{ASK_USER_TOOL, TODO_TOOL};
use omnicrawl_tui::state::{AppState, Record};
use omnicrawl_tui::ui;

/// 渲染一帧并返回可见文本行。
///
/// 宽字符（CJK）只在自己的格里写 symbol，后面那格是占位空格；拼行时要跳过占位格，
/// 否则中文会被空格拆开。
fn screen(state: &AppState, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("测试终端应当可用");
    terminal
        .draw(|frame| ui::render(frame, state, None, None, None))
        .expect("渲染不应失败");
    let buffer = terminal.backend().buffer();
    let row_width = buffer.area.width as usize;
    buffer
        .content
        .chunks(row_width)
        .map(|row| {
            let mut line = String::new();
            let mut skip = 0usize;
            for cell in row {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let symbol = cell.symbol();
                line.push_str(symbol);
                skip = UnicodeWidthStr::width(symbol).saturating_sub(1);
            }
            line
        })
        .collect()
}

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.to_string(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        id: "c1".to_string(),
        function_name: name.to_string(),
    }
}

fn chatting_state() -> AppState {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    state.telemetry.context_window = Some(1_000_000);
    let now = Instant::now();
    state.begin_turn("t1".to_string(), "你好，帮我看看".to_string());
    state.apply(
        &HostEvent::ReasoningDelta(TextPayload {
            text: "先读文件".to_string(),
        }),
        now,
    );
    state.apply(
        &HostEvent::ToolStarted(ToolStartedPayload {
            step: 1,
            call: tool_call("bash", serde_json::json!({"command": "pytest -q"})),
        }),
        now,
    );
    state.apply(
        &HostEvent::ToolFinished(ToolEventPayload {
            call: tool_call("bash", serde_json::json!({"command": "pytest -q"})),
            result: ToolResult {
                ok: true,
                output: "12 passed".to_string(),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            },
        }),
        now + Duration::from_millis(800),
    );
    state.apply(
        &HostEvent::Delta(TextPayload {
            text: "测试通过。".to_string(),
        }),
        now,
    );
    state.start_batch(
        Id::Number(1),
        vec![tool_call(
            TODO_TOOL,
            serde_json::json!({"todos": [
                {"id": "1", "step": "写骨架", "completed": true},
                {"id": "2", "step": "接审批", "completed": false}
            ]}),
        )],
    );
    state.status = Some("正在调用".to_string());
    // 底部轮播由宿主每帧推进；测试直接调一次，拿到当前遥测页文本。
    state.refresh_carousel(Instant::now(), "high");
    state
}

#[test]
fn bottom_hud_conversation_todos_and_composer_are_on_screen() {
    let state = chatting_state();
    let lines = screen(&state, 110, 24);
    let text = lines.join("\n");

    // 底部单行轮播：贴齐屏幕底缘，内容是遥测 + 模型状态（对映 `#bottom-carousel`）。
    let hud = lines.last().expect("应有一行底部 HUD");
    assert!(hud.contains("0/1M 0%"), "底栏应带上下文占用：{hud:?}");
    assert!(hud.contains("t/s"), "底栏应带速率段：{hud:?}");
    assert!(hud.contains("stub-model"), "底栏应带模型名：{hud:?}");
    assert!(hud.contains("THK HIGH"), "底栏应带推理强度：{hud:?}");
    assert!(hud.contains("MAN"), "底栏应带审批模式：{hud:?}");
    assert!(hud.contains("MCP 0"), "底栏应带 MCP 段：{hud:?}");
    assert!(!hud.contains('│'), "底栏改用 ⁕ 分隔：{hud:?}");
    assert!(
        !lines[0].contains("stub-model"),
        "顶部不再有 HUD：{:?}",
        lines[0]
    );

    assert!(text.contains("user："), "缺少用户标签行：{text}");
    assert!(text.contains("你好，帮我看看"), "缺少用户正文：{text}");
    assert!(text.contains("◇ 测试通过。"), "缺少正文：{text}");
    assert!(text.contains("思考"), "缺少思考段：{text}");
    assert!(text.contains("bash"), "缺少工具卡：{text}");
    assert!(text.contains("✓ 成功"), "工具卡应显示成功状态：{text}");
    assert!(text.contains("▣ 写骨架"), "缺少已完成项：{text}");
    assert!(text.contains("▢ 接审批"), "缺少未完成项：{text}");
    assert!(
        text.contains("› 输入消息或 / 命令"),
        "缺少输入框占位：{text}"
    );
    assert!(text.contains("正在调用"), "缺少状态行：{text}");
}

#[test]
fn approval_panel_and_question_panel_take_over_bottom_area() {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    state.start_batch(
        Id::Number(2),
        vec![tool_call(
            "bash",
            serde_json::json!({"command": "pytest -q"}),
        )],
    );
    let text = screen(&state, 100, 20).join("\n");
    assert!(text.contains("工具审批"), "缺少审批面板：{text}");
    assert!(text.contains("bash"), "审批面板应显示工具名：{text}");
    assert!(text.contains("[Y] 批准"), "缺少批准提示：{text}");

    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    state.start_batch(
        Id::Number(3),
        vec![tool_call(
            ASK_USER_TOOL,
            serde_json::json!({
                "question": "选哪个方案？",
                "kind": "select",
                "options": ["先补工具层", "先补设置面板"]
            }),
        )],
    );
    let terminal_text = screen(&state, 100, 20).join("\n");
    assert!(
        terminal_text.contains("当前问题"),
        "缺少提问面板：{terminal_text}"
    );
    assert!(terminal_text.contains("选哪个方案？"), "{terminal_text}");
    assert!(
        terminal_text.contains("▸ 先补工具层"),
        "应标出当前选择：{terminal_text}"
    );
    assert!(terminal_text.contains("先补设置面板"), "{terminal_text}");
}

#[test]
fn cursor_sits_in_the_composer() {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    state.composer.insert("输入中");
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).expect("测试终端应当可用");
    terminal
        .draw(|frame| ui::render(frame, &state, None, None, None))
        .expect("渲染不应失败");
    let position = terminal.backend().cursor_position();
    // 输入卡在第 11 行、框内第一行：左缩进 = 边框 1 + 卡片内边距 2 + 编辑器内边距 1，
    // 再加上三个全角字的 6 列。
    assert_eq!(position.y, 9);
    assert_eq!(position.x, 4 + 6);
}

#[test]
fn history_scroll_hides_the_newest_lines() {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    for index in 1..=20 {
        state.begin_turn(format!("t{index}"), format!("第{index}条消息"));
    }
    state.begin_turn("t21".to_string(), "最后一条".to_string());
    let bottom = screen(&state, 80, 14).join("\n");
    assert!(bottom.contains("最后一条"), "默认应跟随最新：{bottom}");
    assert!(
        !bottom.contains("第1条消息"),
        "窗口应只显示最新部分：{bottom}"
    );

    state.scroll_by(-999);
    let scrolled = screen(&state, 80, 14).join("\n");
    assert!(
        scrolled.contains("第1条消息"),
        "上翻到顶应看到最早的消息：{scrolled}"
    );
    assert!(
        !scrolled.contains("最后一条"),
        "上翻后不该再跟最新：{scrolled}"
    );
    state.scroll_to_bottom();
    assert!(
        screen(&state, 80, 14).join("\n").contains("最后一条"),
        "回到底部应重新跟随最新"
    );
    // 记录本身没有被滚动丢掉。
    assert_eq!(
        state
            .records
            .iter()
            .filter(|record| matches!(record, Record::User(_)))
            .count(),
        21
    );
}

#[test]
fn command_menu_renders_directly_above_the_composer() {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    state
        .composer
        .set_commands(omnicrawl_tui::commands::command_options());
    // 屏幕上一行菜单：候选来自统一命令源，选中项带 `› ` 与描述段。
    // 用 `/sett` 而不是 `/se`：后者同时是 `/sessions` 的前缀，会有两行候选。
    state.composer.insert("/sett");
    let lines = screen(&state, 80, 12);
    let menu_row = lines
        .iter()
        .position(|line| line.contains("· 打开中文设置面板"))
        .unwrap_or_else(|| panic!("菜单项应当出现在屏幕上：{lines:?}"));
    assert!(
        lines[menu_row].starts_with("› /settings"),
        "唯一候选即选中项：{:?}",
        lines[menu_row]
    );
    // 菜单行下面是输入卡的「上方空行」（CSS margin: 1 0 0 0）与卡片上边框，
    // 再下面才是卡内文本行。
    let composer_row = menu_row + 3;
    assert!(
        lines[menu_row + 1].trim().is_empty(),
        "菜单与输入卡之间是卡片上方的空行：{:?}",
        lines[menu_row + 1]
    );
    assert!(
        lines[menu_row + 2].contains('╭'),
        "输入卡上边框应在文本行之前：{:?}",
        lines[menu_row + 2]
    );
    assert!(
        lines[composer_row].contains("/sett"),
        "输入卡内容行应带输入文本：{:?}",
        lines[composer_row]
    );
}
