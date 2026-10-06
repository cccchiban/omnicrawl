//! 文件选择弹层（对映 `omnicrawl/ui/fullscreen/screens/file_picker.py`）。
//!
//! 只挑音频文件：目录可进出，文件按扩展名白名单过滤（`.wav` / `.mp3` / `.flac` /
//! `.ogg` / `.aac` / `.m4a`），键盘全程可用。顶部路径输入框可直接填路径跳转，
//! 对映 Python 里挂载即聚焦 `Input` 的行为。
//!
//! 与 Python 的实现差异（都记在 `README.md` 的已知差异里）：
//! - Python 用 Textual 的 `DirectoryTree` 惰性展开树，本层是**单层目录列表**加
//!   进入/返回上级，视觉效果不同、可达路径集合一致；
//! - 隐藏文件（`.` 开头）与 Python 一样不过滤，随扩展名规则走；
//! - 双击选择不适用（本层的鼠标交互尚未接线），选中一律用 `Enter`。

use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;

use super::fit;
use super::fullscreen::terminal::theme;
use crate::state::Composer;

/// 过滤用的音频扩展名（对映 Python 的 `AudioFileTree.AUDIO_SUFFIXES`）。
pub const AUDIO_SUFFIXES: [&str; 6] = [".wav", ".mp3", ".flac", ".ogg", ".aac", ".m4a"];

/// 弹层对话框的期望尺寸（对映 CSS 的 `width: 88; height: 34`）。
const DIALOG_WIDTH: u16 = 88;
const DIALOG_HEIGHT: u16 = 34;

/// 目录里的一条候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// 弹层产出的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePickerEvent {
    /// 取消（`Esc`）：宿主关闭弹层，不动原值。
    Cancel,
    /// 选中一个音频文件。
    Chosen(PathBuf),
}

/// 文件选择弹层的状态。
pub struct FilePickerState {
    title: String,
    dir: PathBuf,
    entries: Vec<PickerEntry>,
    selected: usize,
    input: Composer,
    /// 路径输入框是否持有按键（Python 挂载时聚焦输入框，同义）。
    input_active: bool,
    status: String,
    /// 当前可视行数：由宿主每帧把弹层高度交给它，滚动窗口据此计算。
    visible_rows: usize,
}

impl FilePickerState {
    /// 从 `start` 起步：给文件时从其父目录起步（对映 Python 的 `start_path` 处理）。
    pub fn new(start: &Path, title: &str) -> Self {
        let start = expand_user(start);
        let dir = if start.is_dir() {
            start
        } else {
            start
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        let mut input = Composer::default();
        input.insert(&dir.to_string_lossy());
        let mut state = Self {
            title: title.to_string(),
            dir: PathBuf::from("."),
            entries: Vec::new(),
            selected: 0,
            input,
            input_active: true,
            status: String::new(),
            visible_rows: 20,
        };
        state.navigate_to(&dir);
        state
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entries(&self) -> &[PickerEntry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn input_active(&self) -> bool {
        self.input_active
    }

    pub fn input_text(&self) -> String {
        self.input.text().to_string()
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_visible_rows(&mut self, rows: usize) {
        self.visible_rows = rows.max(1);
    }

    /// 进入一个目录并重列条目；目录不可读时保留原目录并给出状态。
    pub fn navigate_to(&mut self, path: &Path) {
        match fs::read_dir(path) {
            Ok(reader) => {
                let mut entries: Vec<PickerEntry> = Vec::new();
                for item in reader.flatten() {
                    let item_path = item.path();
                    let is_dir = item_path.is_dir();
                    let name = item.file_name().to_string_lossy().to_string();
                    if !is_dir && !is_audio_file(&item_path) {
                        continue;
                    }
                    entries.push(PickerEntry {
                        name,
                        path: item_path,
                        is_dir,
                    });
                }
                // 目录在前、同类按名字排序（对映 Python `DirectoryTree` 的展示顺序）。
                // 小写键相同时（如 `ref.WAV` 与 `ref.wav`）再按原名比较：否则顺序会落到
                // `read_dir` 的枚举顺序上，而它在不同文件系统/平台并不一致。
                entries.sort_by(|left, right| {
                    right
                        .is_dir
                        .cmp(&left.is_dir)
                        .then_with(|| sort_key(&left.name).cmp(&sort_key(&right.name)))
                        .then_with(|| left.name.cmp(&right.name))
                });
                self.dir = path.to_path_buf();
                self.entries = entries;
                self.selected = 0;
                self.status.clear();
                let mut input = Composer::default();
                input.insert(&self.dir.to_string_lossy());
                self.input = input;
            }
            Err(error) => {
                self.status = format!("目录不可读：{error}");
            }
        }
    }

    /// 回到上一级目录；已经在根目录时保持不动。
    pub fn go_parent(&mut self) {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return;
        };
        self.navigate_to(&parent);
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let count = self.entries.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// `Enter`：目录进入、音频文件选中、其它文件给出状态。
    pub fn activate(&mut self) -> Option<FilePickerEvent> {
        let entry = self.entries.get(self.selected)?;
        if entry.is_dir {
            let path = entry.path.clone();
            self.navigate_to(&path);
            return None;
        }
        if is_audio_file(&entry.path) {
            return Some(FilePickerEvent::Chosen(entry.path.clone()));
        }
        self.status = "只支持音频文件（.wav / .mp3 / .flac / .ogg / .aac / .m4a）。".to_string();
        None
    }

    /// 路径输入框提交：目录跳转、音频文件选中，其余给出状态（对映 `on_input_submitted`）。
    pub fn submit_input(&mut self) -> Option<FilePickerEvent> {
        let candidate = expand_user(Path::new(self.input.text().trim()));
        if candidate.is_dir() {
            self.navigate_to(&candidate);
            return None;
        }
        if is_audio_file(&candidate) {
            return Some(FilePickerEvent::Chosen(candidate));
        }
        self.status = "路径不存在或不是音频文件。".to_string();
        None
    }

    /// 键盘：输入态与列表态各一组键位。
    pub fn handle_key(&mut self, key: KeyCode) -> Option<FilePickerEvent> {
        if self.input_active {
            return match key {
                KeyCode::Esc => Some(FilePickerEvent::Cancel),
                KeyCode::Enter => self.submit_input(),
                KeyCode::Tab | KeyCode::Down => {
                    self.input_active = false;
                    None
                }
                KeyCode::Char(character) => {
                    self.input.insert(&character.to_string());
                    None
                }
                KeyCode::Backspace => {
                    self.input.backspace();
                    None
                }
                KeyCode::Delete => {
                    self.input.delete();
                    None
                }
                KeyCode::Left => {
                    self.input.move_left();
                    None
                }
                KeyCode::Right => {
                    self.input.move_right();
                    None
                }
                KeyCode::Home => {
                    self.input.move_home();
                    None
                }
                KeyCode::End => {
                    self.input.move_end();
                    None
                }
                _ => None,
            };
        }
        match key {
            KeyCode::Esc => Some(FilePickerEvent::Cancel),
            KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Down => {
                self.move_selection(1);
                None
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Backspace | KeyCode::Left => {
                self.go_parent();
                None
            }
            KeyCode::Tab => {
                self.input_active = true;
                None
            }
            _ => None,
        }
    }

    /// 滚动窗口的起始下标：选中项始终留在可视区内。
    fn window_offset(&self) -> usize {
        let visible = self.visible_rows.max(1);
        if self.selected < visible {
            return 0;
        }
        self.selected + 1 - visible
    }
}

/// 扩展名白名单判定（大小写不敏感，对映 `path.suffix.lower() in AUDIO_SUFFIXES`）。
pub fn is_audio_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .map(|extension| {
                let suffix = format!(".{}", extension.to_string_lossy().to_lowercase());
                AUDIO_SUFFIXES.contains(&suffix.as_str())
            })
            .unwrap_or(false)
}

/// 排序键：小写化，让 `A.wav` 与 `a.wav` 相邻（与 Python 侧的树展示一致）。
fn sort_key(name: &str) -> String {
    name.to_lowercase()
}

/// `~` 展开（只处理开头的 `~` / `~/`，与 `expanduser` 的常用形态一致）。
fn expand_user(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        if let Some(home) = home_dir() {
            let rest = text[1..].trim_start_matches(['/', '\\']);
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// 渲染弹层：居中对话框（标题、路径输入框、条目列表、状态与提示）。
pub fn render(frame: &mut Frame, area: Rect, state: &FilePickerState) {
    if area.width < 8 || area.height < 6 {
        return;
    }
    let width = DIALOG_WIDTH.min(area.width.saturating_sub(2)).max(8);
    let height = DIALOG_HEIGHT.min(area.height.saturating_sub(2)).max(6);
    let dialog = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, dialog);
    let focused = theme::rich_style(theme::BORDER_STRONG);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(focused);
    frame.render_widget(block, dialog);

    let inner = Rect {
        x: dialog.x + 2,
        y: dialog.y + 1,
        width: dialog.width.saturating_sub(4),
        height: dialog.height.saturating_sub(2),
    };
    if inner.width < 4 || inner.height < 4 {
        return;
    }
    let rows: [Rect; 6] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let title = Line::styled(
        fit(state.title(), rows[0].width as usize),
        theme::rich_style(theme::ACCENT_WHITE).add_modifier(Modifier::BOLD),
    );
    frame.render_widget(Paragraph::new(title), rows[0]);

    // 路径输入框：输入态下前面给出光标位，列表态整行淡色。
    let path_text = if state.input_active() {
        format!("> {}", state.input_text())
    } else {
        format!("  {}", state.dir().to_string_lossy())
    };
    let path_style = if state.input_active() {
        theme::rich_style(theme::ACCENT_WHITE)
    } else {
        theme::rich_style(theme::TEXT_MUTED)
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(&path_text, rows[1].width as usize),
            path_style,
        )),
        rows[1],
    );

    let list_area = rows[3];
    let mut lines: Vec<Line<'static>> = Vec::new();
    if state.entries().is_empty() {
        lines.push(Line::styled(
            "（没有可选的目录或音频文件）",
            theme::rich_style(theme::TEXT_MUTED),
        ));
    } else {
        let offset = state.window_offset();
        let visible = list_area.height as usize;
        for (index, entry) in state
            .entries()
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
        {
            let selected = index == state.selected();
            let marker = if selected { "› " } else { "  " };
            let suffix = if entry.is_dir { "/" } else { "" };
            let text = format!("{marker}{}{suffix}", entry.name);
            let style = if selected {
                theme::rich_style(theme::ACCENT_AMBER).add_modifier(Modifier::BOLD)
            } else if entry.is_dir {
                theme::rich_style(theme::ACCENT_WHITE)
            } else {
                theme::rich_style(theme::TEXT_SECONDARY)
            };
            lines.push(Line::styled(fit(&text, list_area.width as usize), style));
        }
    }
    frame.render_widget(Paragraph::new(lines), list_area);

    let status_style = if state.status().is_empty() {
        theme::rich_style(theme::TEXT_MUTED)
    } else {
        theme::rich_style(theme::ACCENT_RED)
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(state.status(), rows[4].width as usize),
            status_style,
        )),
        rows[4],
    );

    let hint = if state.input_active() {
        "输入路径后回车跳转 · Tab 切到列表 · Esc 取消"
    } else {
        "↑/↓ 浏览 · 回车选择或进入 · Backspace 上级目录 · Tab 回到输入框 · Esc 取消"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(hint, rows[5].width as usize),
            theme::rich_style(theme::TEXT_MUTED),
        )),
        rows[5],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("omnicrawl-picker-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 文件系统是否区分大小写：写一个小写探针，再按大写查它。
    ///
    /// Windows 与默认的 macOS 不区分，`ref.wav` 与 `ref.WAV` 因此是**同一个文件**，
    /// 那个用例不能照搬「两个名字都在」的断言。
    fn case_sensitive(dir: &Path) -> bool {
        fs::write(dir.join("case-probe.tmp"), b"x").expect("写探针");
        let sensitive = !dir.join("CASE-PROBE.TMP").exists();
        let _ = fs::remove_file(dir.join("case-probe.tmp"));
        sensitive
    }

    #[test]
    fn lists_directories_and_audio_files_only() {
        let dir = temp_dir("list");
        fs::create_dir_all(dir.join("voices")).expect("子目录");
        fs::write(dir.join("ref.wav"), b"x").expect("音频");
        fs::write(dir.join("ref.WAV"), b"x").expect("大写扩展名");
        fs::write(dir.join("notes.txt"), b"x").expect("非音频");
        let sensitive = case_sensitive(&dir);

        let state = FilePickerState::new(&dir, "选择音频文件");
        let names: Vec<&str> = state
            .entries()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        // 不区分大小写的文件系统上两个名字只留一个，此时按目录里真实的名字断言。
        let expected: Vec<&str> = if sensitive {
            vec!["voices", "ref.WAV", "ref.wav"]
        } else {
            vec!["voices", "ref.wav"]
        };
        assert_eq!(names, expected, "目录在前，其余按名字排序");
        assert!(state.entries()[0].is_dir);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_enters_directory_then_selects_file() {
        let dir = temp_dir("enter");
        fs::create_dir_all(dir.join("audio")).expect("子目录");
        fs::write(dir.join("audio/ref.wav"), b"x").expect("音频");

        let mut state = FilePickerState::new(&dir, "选择音频文件");
        assert_eq!(state.activate(), None, "目录先进入");
        assert_eq!(state.dir(), dir.join("audio").as_path());
        assert_eq!(
            state.activate(),
            Some(FilePickerEvent::Chosen(dir.join("audio/ref.wav")))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_input_navigates_and_reports_unknown_paths() {
        let dir = temp_dir("input");
        fs::create_dir_all(dir.join("audio")).expect("子目录");
        fs::write(dir.join("audio/ref.wav"), b"x").expect("音频");

        let mut state = FilePickerState::new(&dir, "选择音频文件");
        state.input = Composer::default();
        state.input.insert(&dir.join("audio").to_string_lossy());
        assert_eq!(state.submit_input(), None);
        assert_eq!(state.dir(), dir.join("audio").as_path());

        state.input = Composer::default();
        state.input.insert(&dir.join("missing").to_string_lossy());
        assert_eq!(state.submit_input(), None);
        assert_eq!(state.status(), "路径不存在或不是音频文件。");

        state.input = Composer::default();
        state
            .input
            .insert(&dir.join("audio/ref.wav").to_string_lossy());
        assert_eq!(
            state.submit_input(),
            Some(FilePickerEvent::Chosen(dir.join("audio/ref.wav")))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_navigation_stops_at_root() {
        let dir = temp_dir("parent");
        fs::create_dir_all(dir.join("a/b")).expect("子目录");
        let mut state = FilePickerState::new(&dir.join("a/b"), "选择音频文件");
        state.go_parent();
        assert_eq!(state.dir(), dir.join("a").as_path());
        state.go_parent();
        assert_eq!(state.dir(), dir.as_path());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_switch_between_input_and_list() {
        let dir = temp_dir("keys");
        fs::write(dir.join("ref.wav"), b"x").expect("音频");

        let mut state = FilePickerState::new(&dir, "选择音频文件");
        assert!(state.input_active(), "挂载即聚焦输入框");
        assert_eq!(state.handle_key(KeyCode::Tab), None);
        assert!(!state.input_active());
        assert_eq!(
            state.handle_key(KeyCode::Esc),
            Some(FilePickerEvent::Cancel)
        );
        assert_eq!(state.handle_key(KeyCode::Tab), None);
        assert!(state.input_active());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_wraps_and_window_follows() {
        let dir = temp_dir("window");
        for index in 0..5 {
            fs::write(dir.join(format!("ref{index}.wav")), b"x").expect("音频");
        }
        let mut state = FilePickerState::new(&dir, "选择音频文件");
        state.set_visible_rows(2);
        state.move_selection(-1);
        assert_eq!(state.selected(), 4, "向上越界回最后一项");
        assert_eq!(state.window_offset(), 3, "窗口跟着选中项滚动");
        state.move_selection(1);
        assert_eq!(state.selected(), 0);
        assert_eq!(state.window_offset(), 0);

        let _ = fs::remove_dir_all(&dir);
    }
}
