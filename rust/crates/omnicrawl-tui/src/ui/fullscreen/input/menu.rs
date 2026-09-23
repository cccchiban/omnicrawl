//! 斜杠命令菜单的实时筛选与渲染（对映 Python `input/menu.py`）。
//!
//! 输入框里键入 `/` 时弹出命令候选：按当前前缀过滤统一命令源、上下键选择、
//! Enter/Tab 补全，命令名之后出现空格则改为按参数提示筛选（`/settings --` → `--chat`）。
//!
//! 与 Python 的分工一致：候选来源是 [`omnicrawl_commands::slash::build_slash_command_options`]
//! （由调用方传入，本模块不持有宿主），输入框本身的文本写入与光标移动由调用方按
//! [`MenuAction`] 执行——因此这里是纯状态机，可单独验证。

use omnicrawl_commands::CommandOption;

use crate::ui::fullscreen::terminal::theme::{ACCENT_AMBER, TEXT_MUTED, TEXT_SECONDARY};
use crate::ui::fullscreen::text::StyledText;

/// 菜单可见候选行数（对映 `OmniCrawlApp.COMMAND_MENU_VISIBLE_OPTIONS`）。
pub const COMMAND_MENU_VISIBLE_OPTIONS: usize = 8;

/// 参数候选的类别名（对映 `_parameter_options` 的 `category`）。
const PARAMETER_CATEGORY: &str = "参数";

/// 菜单打开时消费的按键；其余按键不经过本模块。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    Up,
    Down,
    Enter,
    Tab,
}

/// 一次按键处理后的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// 未消费该按键，交给上层（没有候选，或 Enter 需要提交执行）。
    Passthrough,
    /// 已消费：选中项移动，需要重绘。
    Redraw,
    /// 已消费：把输入框补全为 `insert`；调用方负责写入文本并把光标移到末尾。
    Complete { insert: String },
    /// 已消费：菜单已收起，输入框保持不变。
    Hidden,
}

/// 一行可供渲染的候选（对映 `_render_command_menu` 的逐行输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuRow {
    /// 是否为当前选中项（渲染用 `›` 与琥珀色）。
    pub selected: bool,
    pub command: String,
    /// 已折叠空白的描述；为空表示不追加说明段。
    pub description: String,
}

/// 斜杠命令菜单状态机。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandMenu {
    matches: Vec<CommandOption>,
    selection: usize,
}

impl CommandMenu {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前候选（完整列表，供上下键选择）。
    pub fn matches(&self) -> &[CommandOption] {
        &self.matches
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    /// 菜单是否打开（有候选）。
    pub fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }

    /// 可见候选行数（供输入区高度计算，对映编辑层的 `min(len(matches), LIMIT)`）。
    pub fn visible_rows(&self) -> usize {
        self.matches.len().min(COMMAND_MENU_VISIBLE_OPTIONS)
    }

    /// 对映 `_refresh_command_menu`：按当前输入实时筛选。
    ///
    /// 前导空格被忽略（允许在 `/` 前误敲空格）；命令名之后出现空白则改走参数筛选。
    pub fn refresh(&mut self, value: &str, options: &[CommandOption]) {
        let stripped = value.trim_start();
        if !stripped.starts_with('/') {
            self.hide();
            return;
        }
        if stripped.chars().any(char::is_whitespace) {
            self.refresh_parameters(stripped, options);
            return;
        }
        let query = value.trim().to_lowercase();
        let mut matches: Vec<CommandOption> = options
            .iter()
            .filter(|option| option.search.to_lowercase().contains(&query))
            .cloned()
            .collect();
        // Python 排序稳定：仅把前缀命中提到前面，同级保留统一命令源的产品顺序。
        matches.sort_by_cached_key(|option| !option.command.to_lowercase().starts_with(&query));
        let hints = parameter_hints(&matches, &query);
        matches.extend(hints);
        self.set_matches(matches);
    }

    /// 对映 `_refresh_parameter_menu`：命令名后已出现空格，按参数前缀筛选。
    pub fn refresh_parameters(&mut self, stripped: &str, options: &[CommandOption]) {
        let head_end = stripped.find(char::is_whitespace).unwrap_or(stripped.len());
        let head = stripped[..head_end].to_lowercase();
        let tail = stripped[head_end..].trim().to_lowercase();
        let source = options
            .iter()
            .find(|option| option.command.to_lowercase() == head);
        let mut matches: Vec<CommandOption> = Vec::new();
        if let Some(source) = source {
            // 运行时 Skill 等候选没有 parameters，缺省视为无参数。
            let declared: &[(String, String)] = source.parameters.as_deref().unwrap_or(&[]);
            matches = parameter_options(&source.command, declared)
                .into_iter()
                // 参数已输入完整时不再提示，避免反复补全同一段文本。
                .filter(|entry| {
                    let key = entry.command.to_lowercase();
                    key.starts_with(&tail) && key != tail
                })
                .collect();
        }
        self.set_matches(matches);
    }

    /// 对映 `_handle_composer_command_key`：菜单打开时消费选择键。
    ///
    /// Enter/Tab 只补全，不提交命令；但输入已是完整命令时 Enter 必须放行给上层提交，
    /// 否则 `/settings`、`/new` 这类无参数命令永远打不开。
    pub fn handle_key(&mut self, key: MenuKey, composer_text: &str) -> MenuAction {
        if self.matches.is_empty() {
            return MenuAction::Passthrough;
        }
        match key {
            MenuKey::Up | MenuKey::Down => {
                let offset: isize = if key == MenuKey::Up { -1 } else { 1 };
                let len = self.matches.len() as isize;
                self.selection = (self.selection as isize + offset).rem_euclid(len) as usize;
                MenuAction::Redraw
            }
            MenuKey::Enter | MenuKey::Tab => {
                let selected = &self.matches[self.selection];
                let target = selected.insert.clone();
                if composer_text == target || composer_text.trim() == selected.command {
                    self.hide();
                    return if key == MenuKey::Enter {
                        MenuAction::Passthrough
                    } else {
                        MenuAction::Hidden
                    };
                }
                self.hide();
                MenuAction::Complete { insert: target }
            }
        }
    }

    /// 收起菜单并清空候选（对映 `_hide_command_menu` 的状态部分）。
    pub fn hide(&mut self) {
        self.matches.clear();
        self.selection = 0;
    }

    fn set_matches(&mut self, matches: Vec<CommandOption>) {
        self.matches = matches;
        self.selection = 0;
    }

    /// 可见窗口 `[start, end)`（对映 `_render_command_menu` 的起点计算）。
    pub fn visible_window(&self) -> (usize, usize) {
        let limit = COMMAND_MENU_VISIBLE_OPTIONS as isize;
        let len = self.matches.len() as isize;
        let start = (self.selection as isize - limit + 1)
            .min(len - limit)
            .max(0) as usize;
        (start, start + COMMAND_MENU_VISIBLE_OPTIONS)
    }

    /// 对映 `_render_command_menu` 的行内容：选中标记、命令名与折叠空白的描述。
    pub fn render_rows(&self) -> Vec<MenuRow> {
        let (start, end) = self.visible_window();
        let end = end.min(self.matches.len());
        self.matches[start..end]
            .iter()
            .enumerate()
            .map(|(offset, option)| MenuRow {
                selected: start + offset == self.selection,
                command: option.command.clone(),
                description: option
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            })
            .collect()
    }

    /// 对映 `_render_command_menu` 的样式文本：选中项 `› ` + 粗体琥珀，未选中 `  ` + 次级色，
    /// 描述段以弱化色追加 `  · `。
    pub fn render(&self) -> StyledText {
        let rows = self.render_rows();
        let mut lines = StyledText::new();
        for (offset, row) in rows.iter().enumerate() {
            let style = if row.selected {
                format!("bold {ACCENT_AMBER}")
            } else {
                TEXT_SECONDARY.to_string()
            };
            lines.push(if row.selected { "› " } else { "  " }, &style);
            lines.push(&row.command, &style);
            if !row.description.is_empty() {
                lines.push(&format!("  · {}", row.description), TEXT_MUTED);
            }
            if offset + 1 < rows.len() {
                lines.push("\n", "");
            }
        }
        lines
    }
}

/// 对映 `_parameter_options`：把命令声明的参数转成菜单候选。
fn parameter_options(command: &str, parameters: &[(String, String)]) -> Vec<CommandOption> {
    parameters
        .iter()
        .map(|(parameter, summary)| CommandOption {
            command: parameter.clone(),
            insert: format!("{command} {parameter}"),
            title: parameter.clone(),
            description: summary.clone(),
            category: PARAMETER_CATEGORY.to_string(),
            search: parameter.clone(),
            parameters: None,
        })
        .collect()
}

/// 对映 `_parameter_hints`：输入已是完整命令名时，追加它的参数候选作为提示。
fn parameter_hints(options: &[CommandOption], query: &str) -> Vec<CommandOption> {
    for option in options {
        if option.command.to_lowercase() == query {
            if let Some(parameters) = &option.parameters {
                if !parameters.is_empty() {
                    return parameter_options(&option.command, parameters);
                }
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(command: &str, description: &str, parameters: &[(&str, &str)]) -> CommandOption {
        let declared: Vec<(String, String)> = parameters
            .iter()
            .map(|(name, summary)| (name.to_string(), summary.to_string()))
            .collect();
        CommandOption {
            command: command.to_string(),
            insert: command.to_string(),
            title: command.to_string(),
            description: description.to_string(),
            category: "内置".to_string(),
            search: command.to_lowercase(),
            parameters: if declared.is_empty() {
                None
            } else {
                Some(declared)
            },
        }
    }

    fn options() -> Vec<CommandOption> {
        vec![
            option("/new", "开启新对话", &[]),
            option("/model", "切换模型", &[]),
            option(
                "/settings",
                "打开设置",
                &[("--chat", "通过对话改设置"), ("--tools", "工具开关")],
            ),
            option("/sessions", "会话列表", &[]),
        ]
    }

    #[test]
    fn refresh_filters_by_substring_and_keeps_product_order() {
        let mut menu = CommandMenu::new();
        menu.refresh("/se", &options());
        // `/settings` 与 `/sessions` 都命中；两者都是前缀命中，保留原顺序。
        let names: Vec<&str> = menu.matches().iter().map(|o| o.command.as_str()).collect();
        assert_eq!(names, vec!["/settings", "/sessions"]);
        assert_eq!(menu.selection(), 0);
        assert!(menu.is_open());
    }

    #[test]
    fn prefix_hits_sort_before_substring_hits() {
        let mut menu = CommandMenu::new();
        let table = vec![
            // Skill 候选的 `search` 里带别名，`/my-skill` 落在子串位置而非前缀位置；
            // 这正是排序规则唯一能生效的场景（内置命令的 search 就是命令名本身）。
            CommandOption {
                search: "/skill:my-skill /my-skill my skill 说明".to_string(),
                ..option("/skill:my-skill", "说明", &[])
            },
            option("/my-note", "笔记", &[]),
        ];
        menu.refresh("/my", &table);
        // 前缀命中排到子串命中之前，同级才保留统一命令源的产品顺序。
        let names: Vec<&str> = menu.matches().iter().map(|o| o.command.as_str()).collect();
        assert_eq!(names, vec!["/my-note", "/skill:my-skill"]);

        menu.refresh("/my-s", &table);
        // 只有一个子串命中时同样保留它。
        let names: Vec<&str> = menu.matches().iter().map(|o| o.command.as_str()).collect();
        assert_eq!(names, vec!["/skill:my-skill"]);
    }

    #[test]
    fn without_slash_prefix_menu_hides() {
        let mut menu = CommandMenu::new();
        menu.refresh("/se", &options());
        assert!(menu.is_open());
        menu.refresh("hello", &options());
        assert!(!menu.is_open());
        assert_eq!(menu.selection(), 0);
    }

    #[test]
    fn leading_whitespace_is_ignored() {
        let mut menu = CommandMenu::new();
        menu.refresh("   /new", &options());
        let names: Vec<&str> = menu.matches().iter().map(|o| o.command.as_str()).collect();
        assert_eq!(names, vec!["/new"]);
    }

    #[test]
    fn complete_command_name_appends_parameter_hints() {
        let mut menu = CommandMenu::new();
        menu.refresh("/settings", &options());
        let names: Vec<&str> = menu.matches().iter().map(|o| o.command.as_str()).collect();
        assert_eq!(names, vec!["/settings", "--chat", "--tools"]);
        assert_eq!(menu.matches()[1].insert, "/settings --chat");
        assert_eq!(menu.matches()[1].category, PARAMETER_CATEGORY);
        assert_eq!(menu.matches()[1].search, "--chat");
    }

    #[test]
    fn parameter_menu_filters_by_prefix_and_skips_exact_match() {
        let mut menu = CommandMenu::new();
        menu.refresh("/settings --", &options());
        let names: Vec<&str> = menu.matches().iter().map(|o| o.command.as_str()).collect();
        assert_eq!(names, vec!["--chat", "--tools"]);

        menu.refresh("/settings --c", &options());
        let names: Vec<&str> = menu.matches().iter().map(|o| o.command.as_str()).collect();
        assert_eq!(names, vec!["--chat"]);

        // 参数已输入完整 → 不再提示。
        menu.refresh("/settings --chat", &options());
        assert!(!menu.is_open());
    }

    #[test]
    fn command_without_parameters_has_no_parameter_menu() {
        let mut menu = CommandMenu::new();
        menu.refresh("/new ", &options());
        assert!(!menu.is_open());
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut menu = CommandMenu::new();
        menu.refresh("/se", &options());
        assert_eq!(menu.handle_key(MenuKey::Up, ""), MenuAction::Redraw);
        assert_eq!(menu.selection(), 1);
        assert_eq!(menu.handle_key(MenuKey::Down, ""), MenuAction::Redraw);
        assert_eq!(menu.selection(), 0);
    }

    #[test]
    fn key_is_not_consumed_when_menu_is_closed() {
        let mut menu = CommandMenu::new();
        assert_eq!(
            menu.handle_key(MenuKey::Enter, "/new"),
            MenuAction::Passthrough
        );
    }

    #[test]
    fn enter_completes_unless_text_is_already_the_command() {
        let mut menu = CommandMenu::new();
        menu.refresh("/se", &options());
        assert_eq!(
            menu.handle_key(MenuKey::Enter, "/se"),
            MenuAction::Complete {
                insert: "/settings".to_string()
            }
        );
        // 补全后菜单收起。
        assert!(!menu.is_open());
    }

    #[test]
    fn enter_passes_through_when_input_is_complete_command() {
        let mut menu = CommandMenu::new();
        menu.refresh("/settings", &options());
        // 文本等于 command（`insert` 也是它）→ 收起菜单并放行 Enter 提交。
        assert_eq!(
            menu.handle_key(MenuKey::Enter, "/settings"),
            MenuAction::Passthrough
        );
        assert!(!menu.is_open());
    }

    #[test]
    fn enter_passes_through_when_text_matches_command_with_padding() {
        let mut menu = CommandMenu::new();
        menu.refresh("/settings", &options());
        assert_eq!(
            menu.handle_key(MenuKey::Enter, "  /settings  "),
            MenuAction::Passthrough
        );
        assert!(!menu.is_open());
    }

    #[test]
    fn tab_hides_instead_of_passing_through_when_complete() {
        let mut menu = CommandMenu::new();
        menu.refresh("/settings", &options());
        assert_eq!(
            menu.handle_key(MenuKey::Tab, "/settings"),
            MenuAction::Hidden
        );
        assert!(!menu.is_open());
    }

    #[test]
    fn visible_window_follows_selection() {
        let mut menu = CommandMenu::new();
        let table: Vec<CommandOption> = (0..20)
            .map(|index| option(&format!("/cmd{index:02}"), "x", &[]))
            .collect();
        menu.refresh("/cmd", &table);
        assert_eq!(menu.matches().len(), 20);
        assert_eq!(menu.visible_window(), (0, 8));
        assert_eq!(menu.visible_rows(), 8);
        for _ in 0..12 {
            menu.handle_key(MenuKey::Down, "");
        }
        assert_eq!(menu.selection(), 12);
        // min(12 - 8 + 1, 20 - 8) = min(5, 12) = 5
        assert_eq!(menu.visible_window(), (5, 13));
    }

    #[test]
    fn visible_window_is_zero_when_candidates_fit() {
        let mut menu = CommandMenu::new();
        menu.refresh("/se", &options());
        assert_eq!(menu.visible_window(), (0, 8));
    }

    #[test]
    fn render_marks_selection_and_collapses_description() {
        let mut menu = CommandMenu::new();
        let table = vec![
            option("/new", "开启   新对话", &[]),
            option("/next", "下一个", &[]),
        ];
        menu.refresh("/ne", &table);
        menu.handle_key(MenuKey::Down, "");
        let rows = menu.render_rows();
        assert!(!rows[0].selected);
        assert_eq!(rows[0].command, "/new");
        assert_eq!(rows[0].description, "开启 新对话");
        assert!(rows[1].selected);

        let rendered = menu.render();
        let plain = rendered.plain();
        assert!(plain.contains("  /new  · 开启 新对话"));
        assert!(plain.contains("› /next  · 下一个"));
        let styles: Vec<&str> = rendered.spans().iter().map(|s| s.style.as_str()).collect();
        assert!(styles.contains(&format!("bold {ACCENT_AMBER}").as_str()));
        assert!(styles.contains(&TEXT_MUTED));
    }

    #[test]
    fn render_skips_blank_description_segment() {
        let mut menu = CommandMenu::new();
        let table = vec![option("/new", "   ", &[])];
        menu.refresh("/new", &table);
        // 刷新后选择位恒为 0，唯一候选即选中项；纯空白描述被折叠为空，不追加 `  · ` 段。
        let rows = menu.render_rows();
        assert!(rows[0].selected);
        assert_eq!(rows[0].description, "");
        assert_eq!(menu.render().plain(), "› /new");
    }

    #[test]
    fn hide_clears_matches_and_selection() {
        let mut menu = CommandMenu::new();
        menu.refresh("/se", &options());
        menu.handle_key(MenuKey::Down, "");
        menu.hide();
        assert!(!menu.is_open());
        assert_eq!(menu.selection(), 0);
        assert!(menu.matches().is_empty());
    }
}
