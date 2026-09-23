//! 配置对话弹层（`/settings --chat`）：一句话 → 本地配置路由 → 写回并同步运行态。
//!
//! 对映 Python `ui/fullscreen/screens/config_chat.py` 的 `ConfigChatPane`。语义上它是「无上下文
//! 的本地写回器」：历史只在本控件里，不送进模型；一句话经 `omnicrawl-config-chat` 的本地路由器
//! 折成若干条命令，全部校验通过后逐条写盘（任一条不合法则一个字节都不改）。
//!
//! **路由器装载必须离开界面线程**：内嵌权重 17 MB，装载时要为别名表逐条跑一遍前向建索引向量，
//! 实测约 50 秒（release）。因此本模块自带一个常驻工作线程：弹层打开时拉起，装载完成后回一条
//! `Ready`；之后每句话进工作线程、结论经通道回填历史。界面线程只做按键与渲染。
//!
//! 与 Python 的差异：Python 侧挂在设置屏里（`settings.py` 的 `config_chat` 项）；Rust 侧做成
//! **独立弹层**——设置面板已经是一整屏模态页，把对话塞进它的右侧会挤掉其余一级项的可读宽度，
//! 而 `/settings --chat` 的入口本来就是直接跳进这一页。

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

use crossterm::event::KeyCode;
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config_chat::service::ConfigChatService;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

/// 历史里保留的最大行数（超出丢最旧的）。
const MAX_HISTORY: usize = 200;
/// 输入缓冲的字符上限（远大于可视宽度，只为拦住粘贴整篇文章）。
const MAX_INPUT_CHARS: usize = 4000;

/// 一条历史：用户说的话，或系统给的结论 / 报错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEntry {
    pub from_user: bool,
    pub text: String,
}

/// 工作线程 → 界面的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatWorkerMessage {
    /// 路由器装载完成（或装载失败，原因在 `error` 里）。
    Ready { error: Option<String> },
    /// 一句话的写回结论（每行一句中文说明）。
    Applied { lines: Vec<String> },
}

/// 常驻工作线程句柄：一个发送端（提交文本）+ 一个接收端（结论）。
struct ChatWorker {
    requests: Sender<String>,
    results: Receiver<ChatWorkerMessage>,
}

impl ChatWorker {
    /// 拉起工作线程：先建服务（装载路由器），再逐句处理请求。
    fn start() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<String>();
        let (result_tx, result_rx) = mpsc::channel::<ChatWorkerMessage>();
        thread::spawn(move || {
            let env = ConfigEnvironment::from_process();
            let mut service = ConfigChatService::embedded(&env, None);
            let error = if service.available() {
                None
            } else {
                Some(service.unavailable_reason())
            };
            // 真正触发装载：别名表要逐条前向建索引向量，这一步是几十秒的来源。
            if error.is_none() {
                let probe = service.labels().len();
                if probe == 0 {
                    let _ = result_tx.send(ChatWorkerMessage::Ready {
                        error: Some("配置对话资源不可用：标签表为空。".to_string()),
                    });
                    return;
                }
            }
            if result_tx.send(ChatWorkerMessage::Ready { error }).is_err() {
                return;
            }
            while let Ok(text) = request_rx.recv() {
                let lines = apply_text(&mut service, &text);
                if result_tx
                    .send(ChatWorkerMessage::Applied { lines })
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            requests: request_tx,
            results: result_rx,
        }
    }

    fn submit(&self, text: &str) -> Result<(), String> {
        self.requests
            .send(text.to_string())
            .map_err(|_| "配置对话工作线程已退出。".to_string())
    }

    fn try_recv(&self) -> Option<ChatWorkerMessage> {
        match self.results.try_recv() {
            Ok(message) => Some(message),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

/// 一句话 → 写回结果（对映 Python `_on_input` 的结论行）。
fn apply_text(service: &mut ConfigChatService<'_>, text: &str) -> Vec<String> {
    if !service.available() {
        return vec![format!("配置对话不可用：{}", service.unavailable_reason())];
    }
    match service.apply_text(text) {
        Ok(changes) if changes.is_empty() => vec!["没有识别到可修改的配置。".to_string()],
        Ok(changes) => changes
            .iter()
            .map(|change| format!("已更新 {}：{}", change.path, display_value(&change.value)))
            .collect(),
        Err(error) => vec![format!("配置对话失败：{}", error.message())],
    }
}

/// 配置对话的状态机：输入缓冲 + 历史 + 工作线程。
pub struct ConfigChatState {
    input: String,
    history: Vec<ChatEntry>,
    worker: Option<ChatWorker>,
    /// 工作线程是否已报告就绪（路由器装载完成）。
    ready: bool,
    /// 是否有一句话正在处理（等结论回来）。
    pending: bool,
    /// 不可用原因（渲染在提示行；工作线程报告失败后填入）。
    unavailable: Option<String>,
}

/// 弹层按键的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigChatEvent {
    /// 用户按 Esc / Ctrl+C：关闭弹层。
    Close,
    /// 用户提交了一句话（宿主交给工作线程）。
    Submit(String),
}

impl ConfigChatState {
    /// 建弹层（不拉工作线程；单元测试用这个）。资源不可用时仍可打开并显示原因。
    pub fn new() -> Self {
        let env = ConfigEnvironment::from_process();
        let service = ConfigChatService::embedded(&env, None);
        let unavailable = if service.available() {
            None
        } else {
            Some(service.unavailable_reason())
        };
        Self {
            input: String::new(),
            history: Vec::new(),
            worker: None,
            ready: false,
            pending: false,
            unavailable,
        }
    }

    /// 建弹层并拉起工作线程（装载路由器在后台跑）。
    pub fn start() -> Self {
        let mut state = Self::new();
        if state.unavailable.is_none() {
            state.worker = Some(ChatWorker::start());
        }
        state
    }

    /// 服务是否可用（标签表装载成功，且工作线程没报失败）。
    pub fn available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// 不可用原因。
    pub fn unavailable_reason(&self) -> String {
        self.unavailable
            .clone()
            .unwrap_or_else(|| "配置对话模型尚未加载。".to_string())
    }

    pub fn input(&self) -> &str {
        self.input.as_str()
    }

    pub fn history(&self) -> &[ChatEntry] {
        self.history.as_slice()
    }

    /// 路由器是否已装载完成（提示行据此给出「请稍候」）。
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// 是否有请求在跑（提示行显示忙碌）。
    pub fn busy(&self) -> bool {
        self.pending
    }

    /// 记一条历史（工作线程回填结论、或宿主提示时用）。
    pub fn push(&mut self, from_user: bool, text: impl Into<String>) {
        self.history.push(ChatEntry {
            from_user,
            text: text.into(),
        });
        if self.history.len() > MAX_HISTORY {
            let overflow = self.history.len() - MAX_HISTORY;
            self.history.drain(0..overflow);
        }
    }

    /// 一次按键；提交时把输入交回宿主（本层不做配置读写）。
    pub fn handle_key(&mut self, key: KeyCode) -> Option<ConfigChatEvent> {
        match key {
            KeyCode::Esc => Some(ConfigChatEvent::Close),
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.input.clear();
                Some(ConfigChatEvent::Submit(text))
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Char(character) => {
                if !character.is_control() && self.input.chars().count() < MAX_INPUT_CHARS {
                    self.input.push(character);
                }
                None
            }
            _ => None,
        }
    }

    /// 把一句话交给工作线程；返回给用户看的即时说明（成功提交时为空）。
    pub fn submit(&mut self, text: &str) -> Option<String> {
        if let Some(reason) = self.unavailable.clone() {
            return Some(format!("配置对话不可用：{reason}"));
        }
        let Some(worker) = self.worker.as_ref() else {
            return Some("配置对话工作线程未启动。".to_string());
        };
        if !self.ready {
            return Some("本地路由器仍在装载（首次约需一分钟），请稍候再试。".to_string());
        }
        match worker.submit(text) {
            Ok(()) => {
                self.pending = true;
                None
            }
            Err(message) => Some(message),
        }
    }

    /// 每帧轮询工作线程：就绪状态与结论行都从这里回填。
    pub fn tick(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let mut messages: Vec<ChatWorkerMessage> = Vec::new();
        while let Some(message) = worker.try_recv() {
            messages.push(message);
        }
        for message in messages {
            match message {
                ChatWorkerMessage::Ready { error } => match error {
                    None => self.ready = true,
                    Some(reason) => self.unavailable = Some(reason),
                },
                ChatWorkerMessage::Applied { lines } => {
                    self.pending = false;
                    for line in lines {
                        self.push(false, line);
                    }
                }
            }
        }
    }

    /// 路由器装载耗时提示（弹层还没就绪时渲染用）。
    fn loading_hint(&self) -> &'static str {
        "正在装载本地路由器（内嵌权重约 17 MB，首次约需一分钟）…"
    }
}

impl Default for ConfigChatState {
    fn default() -> Self {
        Self::new()
    }
}

/// 写回值的展示文本（与设置面板的取值风格一致：布尔用中文，其余直接写值）。
fn display_value(value: &omnicrawl_config::toml::Value) -> String {
    match value {
        omnicrawl_config::toml::Value::Boolean(flag) => {
            (if *flag { "开启" } else { "关闭" }).to_string()
        }
        omnicrawl_config::toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 目录里可改的配置路径数量（弹层打开时用于诊断「资源是否真的装载了」）。
pub fn configured_paths() -> usize {
    let env = ConfigEnvironment::from_process();
    let service = ConfigChatService::embedded(&env, None);
    service.labels().len()
}

/// 弹层渲染：历史区 + 输入行 + 提示行。
pub fn render(frame: &mut Frame, area: Rect, state: &ConfigChatState) {
    let dialog = centered(area, 76, 22);
    frame.render_widget(Clear, dialog);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" 通过对话修改设置 ");
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let [history_area, input_area, hint_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let mut lines: Vec<Line> = Vec::new();
    for entry in state.history() {
        let (prefix, style) = if entry.from_user {
            ("你：", Style::default().fg(Color::Cyan))
        } else {
            ("· ", Style::default().fg(Color::Gray))
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style.add_modifier(Modifier::BOLD)),
            Span::styled(entry.text.clone(), style),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "用一句话描述要改的设置，例如：把上下文窗口改成 256K；关闭思考显示。",
            Style::default().fg(Color::DarkGray),
        ));
    }
    // 历史超出可视高度时只显示最新几行。
    let visible = history_area.height as usize;
    if lines.len() > visible {
        lines.drain(0..lines.len() - visible);
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        history_area,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::raw(state.input().to_string()),
        ])),
        input_area,
    );

    let hint = if !state.available() {
        state.unavailable_reason()
    } else if state.busy() {
        "正在写回配置…".to_string()
    } else if !state.ready() {
        state.loading_hint().to_string()
    } else {
        "Enter 应用 · Esc 返回".to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::styled(hint, Style::default().fg(Color::DarkGray))),
        hint_area,
    );
}

/// 居中矩形：先按高度居中一行，再在行内居中（窄屏自动夹到可用范围）。
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let [row] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(row);
    cell
}

/// 参考音频那类路径在历史里只显示文件名（宿主回填提示时复用）。
pub fn short_path(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_enter_and_close_are_reported() {
        let mut state = ConfigChatState::new();
        for character in "改端口".chars() {
            assert!(state.handle_key(KeyCode::Char(character)).is_none());
        }
        assert_eq!(state.input(), "改端口");
        let event = state.handle_key(KeyCode::Enter);
        assert_eq!(event, Some(ConfigChatEvent::Submit("改端口".to_string())));
        assert_eq!(state.input(), "", "提交后输入框清空");
        assert_eq!(state.handle_key(KeyCode::Esc), Some(ConfigChatEvent::Close));
    }

    #[test]
    fn empty_submit_is_ignored_and_backspace_pops() {
        let mut state = ConfigChatState::new();
        assert!(state.handle_key(KeyCode::Enter).is_none());
        state.handle_key(KeyCode::Char('a'));
        assert!(state.handle_key(KeyCode::Backspace).is_none());
        assert_eq!(state.input(), "");
    }

    #[test]
    fn history_is_capped() {
        let mut state = ConfigChatState::new();
        for index in 0..(MAX_HISTORY + 10) {
            state.push(false, format!("行 {index}"));
        }
        assert_eq!(state.history().len(), MAX_HISTORY);
        assert_eq!(state.history()[0].text, "行 10");
    }

    #[test]
    fn embedded_resources_cover_the_config_labels() {
        let state = ConfigChatState::new();
        assert!(
            state.available(),
            "内嵌配置对话资源应可用：{}",
            state.unavailable_reason()
        );
        assert!(configured_paths() > 0, "标签表不应为空");
    }

    #[test]
    fn submit_without_worker_reports_a_reason() {
        let mut state = ConfigChatState::new();
        let message = state.submit("把上下文窗口改成 256K");
        assert!(message.is_some(), "没有工作线程时必须如实说明");
        assert!(!state.busy());
    }

    #[test]
    fn display_value_is_readable() {
        assert_eq!(
            display_value(&omnicrawl_config::toml::Value::Boolean(true)),
            "开启"
        );
        assert_eq!(
            display_value(&omnicrawl_config::toml::Value::Integer(9000)),
            "9000"
        );
    }
}
