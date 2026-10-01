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
    state.begin_turn("t1".to_string(), "你好，帮我看看".to_string(), Vec::new());
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
fn status_line_follows_retry_and_streaming() {
    // 用户报的场景：模型重试一次之后，输入框上的状态显示不再自动刷新。
    // 两条都要成立——重试提示当场刷上去，模型重新出内容后再让位回运行态。
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    let now = Instant::now();
    state.begin_turn("t1".to_string(), "问问看".to_string(), Vec::new());

    let status_top = |lines: &[String]| {
        lines
            .iter()
            .position(|line| line.starts_with('╭'))
            .unwrap_or_else(|| panic!("应有一行方框上边框：{lines:?}"))
    };
    state.apply(
        &HostEvent::RetryStatus(omnicrawl_ipc::bridge::MessagePayload {
            message: "正在重试(第1次)".to_string(),
        }),
        now,
    );
    let lines = screen(&state, 80, 14);
    assert!(
        lines[status_top(&lines)].contains("正在重试(第1次)"),
        "重试提示要立刻画到状态行上：{lines:?}"
    );

    state.apply(&HostEvent::Delta(TextPayload { text: "答复".into() }), now);
    let lines = screen(&state, 80, 14);
    assert!(
        lines[status_top(&lines)].contains("正在调用"),
        "模型重新出内容后状态行要让位回运行态：{lines:?}"
    );
    assert!(
        !lines.join("\n").contains("正在重试"),
        "旧提示不该留在任何一块上：{lines:?}"
    );
}

#[test]
fn input_group_is_boxed_and_the_status_sits_on_its_top_border() {
    // 用户要求：任务清单 / 排队预览 / 提示 / 命令菜单 / 输入本体共用**一个方框**，
    // 运行状态（`⠋ 正在调用 [ ESC ]`）写在方框上边框上，而不是留在会话流里。
    let state = chatting_state();
    let lines = screen(&state, 80, 14);
    let top = lines
        .iter()
        .position(|line| line.contains("正在调用"))
        .unwrap_or_else(|| panic!("状态应当出现在方框上边框上：{lines:?}"));
    assert!(
        lines[top].starts_with('╭') && lines[top].ends_with('╮'),
        "状态行就是输入区方框的上边框：{:?}",
        lines[top]
    );
    assert!(lines[top].contains("[ ESC ]"), "运行中仍带 ESC 提示：{:?}", lines[top]);
    // 会话流里不再有状态行：状态只出现这一次（方框边框上那次）。
    assert_eq!(
        lines.iter().filter(|line| line.contains("正在调用")).count(),
        1,
        "状态只应出现在方框边框上：{lines:?}"
    );
    // 输入本体与下边框都在，内容紧贴边框。
    assert!(
        lines.iter().any(|line| line.contains("╰")),
        "应当有方框下边框：{lines:?}"
    );
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
fn block_cursor_sits_in_the_composer() {
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
    // 光标是自绘的白色粗块（不再设终端光标）：
    // 输入卡正文第一行 + 左缩进（只剩 1 格边框）+ 三个全角字的 6 列。
    let buffer = terminal.backend().buffer();
    let cell = &buffer[(1 + 6, 9)];
    assert_eq!(
        cell.style().bg,
        Some(ratatui::style::Color::White),
        "光标格应是白底粗块：{:?}",
        cell.style()
    );
    assert_eq!(
        terminal.backend().cursor_position().y,
        0,
        "不再让终端自己画光标"
    );
}

#[test]
fn long_composer_input_shows_a_scrollbar() {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    for index in 1..=8 {
        state.composer.insert(&format!("第{index}行"));
        state.composer.newline();
    }
    let lines = screen(&state, 80, 16);
    let bar = lines
        .iter()
        .filter(|line| line.contains('█'))
        .count();
    assert!(bar > 0, "输入超长时右侧要有细线滚动条：{lines:?}");
}

#[test]
fn history_scroll_hides_the_newest_lines() {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    for index in 1..=20 {
        state.begin_turn(format!("t{index}"), format!("第{index}条消息"), Vec::new());
    }
    state.begin_turn("t21".to_string(), "最后一条".to_string(), Vec::new());
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
    // 菜单行在方框之内，所以行首是边框 `│`，选中标记紧跟在它后面。
    assert!(
        lines[menu_row].contains("│› /settings"),
        "唯一候选即选中项：{:?}",
        lines[menu_row]
    );
    // 菜单**在输入区方框之内**（对映 Python `#composer-wrap`：菜单与输入共用一个框），
    // 所以它下面一行直接就是框内的输入文本行，再下面一行才是方框下边框。
    let composer_row = menu_row + 1;
    assert!(
        lines[composer_row + 1].contains('╰'),
        "输入行下面应当是方框下边框：{:?}",
        lines[composer_row + 1]
    );

    assert!(
        lines[composer_row].contains("/sett"),
        "输入卡内容行应带输入文本：{:?}",
        lines[composer_row]
    );
}

/// 1x1 红点 PNG（与 `image_preview` / `conversation` 单测用的是同一样本）。
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

/// 界面上一张 read_image 卡片：走真实的附件登记入口，等后台解码落定。
fn read_image_state() -> AppState {
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    state.telemetry.context_window = Some(1_000_000);
    let now = Instant::now();
    state.begin_turn("t1".to_string(), "看看这张图".to_string(), Vec::new());
    let call = tool_call(
        "read_image",
        serde_json::json!({"path": "shot.png", "prompt": "描述"}),
    );
    state.apply(
        &HostEvent::ToolStarted(ToolStartedPayload {
            step: 1,
            call: call.clone(),
        }),
        now,
    );
    state.apply(
        &HostEvent::ToolFinished(ToolEventPayload {
            call: call.clone(),
            result: ToolResult {
                ok: true,
                output:
                    "{\"path\":\"shot.png\",\"media_type\":\"image/png\",\"bytes\":12}"
                        .to_string(),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            },
        }),
        now + Duration::from_millis(300),
    );
    // 宿主从批次附件里登记；这里用同一条入口，走的也是同一条解码链路。
    state.register_tool_images(
        "c1",
        &[omnicrawl_controllers::types::ToolImageAttachment {
            media_type: "image/png".to_string(),
            data_base64: PNG_1X1.to_string(),
            filename: "shot.png".to_string(),
            detail: "auto".to_string(),
        }],
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if state.tick_image_previews() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    state
}

#[test]
fn a_read_image_card_paints_the_thumbnail_and_drops_the_json_payload() {
    let state = read_image_state();
    // 占位块占了几行：图片至少要把这些格子涂上颜色。
    let rows = omnicrawl_tui::ui::conversation::display_lines(&state, 78)
        .iter()
        .filter(|line| line.image.is_some())
        .count();
    assert!(rows > 0, "read_image 的卡片应当铺出图片占位行");

    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).expect("测试终端");
    terminal
        .draw(|frame| ui::render(frame, &state, None, None, None))
        .expect("渲染不应失败");
    let buffer = terminal.backend().buffer();
    // 半块字形把图片写成字符格的前/背景色（纯色图的字符本身是空格），因此判据是
    // 「这些格子拿到了非默认的 Rgb 色」——没有图片时占位行是一片默认色。
    let painted = buffer
        .content
        .iter()
        .filter(|cell| {
            matches!(cell.fg, ratatui::style::Color::Rgb(..))
                || matches!(cell.bg, ratatui::style::Color::Rgb(..))
        })
        .count();
    assert!(
        painted >= rows,
        "缩略图应当画进 {rows} 行占位格，实得 {painted} 个着色格"
    );

    let text = screen(&state, 80, 30).join("
");
    assert!(
        !text.contains("media_type"),
        "read_image 的 JSON 载荷应当让位给图片：{text}"
    );
    assert!(
        !text.contains("正在准备图片"),
        "解码完成后不该还停在提示行：{text}"
    );
}

#[test]
fn an_image_tool_without_attachments_keeps_its_payload() {
    // 附件没到（模型没看图、工具没执行）时按普通卡片渲染，不会先空出一块。
    let mut state = AppState::new(
        "omnicrawl".to_string(),
        "stub-model".to_string(),
        ApprovalMode::Manual,
    );
    state.telemetry.context_window = Some(1_000_000);
    let now = Instant::now();
    state.begin_turn("t1".to_string(), "看看这张图".to_string(), Vec::new());
    let call = tool_call(
        "read_image",
        serde_json::json!({"path": "shot.png", "prompt": "描述"}),
    );
    state.apply(
        &HostEvent::ToolStarted(ToolStartedPayload {
            step: 1,
            call,
        }),
        now,
    );
    let text = screen(&state, 80, 30).join("
");
    assert!(!text.contains("正在准备图片"), "{text}");
    assert!(text.contains("read_image"), "{text}");
}
