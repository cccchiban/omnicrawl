//! 内嵌双列模型选择器：对映 Python `ModelPickerPane` 在
//! `ToolOutputCompressionSettingsPane` 里的 `selection_only` 用法
//! （只「选择」压缩模型，不切换主模型）。
//!
//! 本模块只持有界面状态与纯逻辑（过滤、窗口、键位、把选中项折算成
//! `profile/model_id`）；网络发现由宿主执行，结果经 [`ModelPicker::set_models`]
//! 回填，写盘仍走表单页的 `Ctrl+S`——选择器自己不写配置、不碰内核。
//!
//! 与 Python 的差异（已记进 README 的「已知差异」）：
//! - Python 右列来自 `build_catalog` 的 custom/detected 两类条目；Rust 右列 =
//!   该渠道已发现的模型 + 渠道配置里的 `model_id`（少一层 models.toml 归一化）。
//! - Python 的搜索框是一个常驻 `Input`；Rust 用 `/`（或 `Tab`）开一个输入缓冲，
//!   `Esc` 只取消本次搜索、再按一次才离开选择器。
//! - 每列只渲染 [`PICKER_WINDOW`] 条，前后用省略行提示还有多少条（与 Python 相同），
//!   列高由渲染层按可用空间收敛。
//! - 鼠标点击尚未接线（面板其余部分是键盘优先，选择器也照此处理）。

use crossterm::event::KeyCode;

use super::ChannelRow;
use crate::state::Composer;

/// 每列同时可见的条目数（对映 Python `render_channel_text`/`render_column_text`
/// 的 `window_size = 4`）。
pub const PICKER_WINDOW: usize = 4;

/// 选择器获得焦点时的提示行（对映 Python `selection_only` 的那条帮助文本）。
pub const PICKER_HINT: &str = "↑↓ 选择  ←→ 切换列  / 搜索  R 刷新  Enter 选择  Esc 返回表单";

/// 选择器对一次按键的处理结果；由设置状态机翻译成 `SettingsEvent`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerKey {
    /// 已就地处理（移动、切列、过滤）。
    Handled,
    /// 请求宿主发现当前渠道的可用模型。
    Discover,
    /// 确认选择：值是写进表单的模型引用（形如 `profile/model_id`）。
    Confirm(String),
    /// 离开选择器，焦点回到表单字段。
    Blur,
    /// 当前列的选中项不可用（原因已写进状态行）。
    Nothing,
}

/// 双列模型选择器的界面状态。
#[derive(Debug, Clone)]
pub struct ModelPicker {
    /// 是否持有焦点；无焦点时键位仍由表单处理（`M` 才进来）。
    focused: bool,
    /// 0 = 渠道列，1 = 模型列（对映 Python 的 `_active_column`）。
    column: usize,
    /// 渠道列的选中位置（下标落在**过滤后**的可见序列里）。
    channel: usize,
    /// 模型列的选中位置（下标落在**过滤后**的可见序列里）。
    model: usize,
    /// 两列共用的搜索串（小写）。
    query: String,
    /// 搜索输入态；`None` 表示不在输入。
    input: Option<Composer>,
    /// 当前渠道的候选模型。
    models: Vec<String>,
    /// 是否正在等宿主的发现结果（决定回填路由）。
    discovering: bool,
    /// 进入选择器时是否已自动请求过一次发现。
    discovered_once: bool,
    /// 「当前值」：表单里的模型引用，用来画 `●` 标记。
    current: String,
    status: String,
}

impl ModelPicker {
    /// 按当前模型引用与渠道列表建选择器：左列自动落到当前值所在的渠道。
    pub fn new(current: &str, channels: &[ChannelRow]) -> Self {
        let mut picker = Self {
            focused: false,
            column: 0,
            channel: 0,
            model: 0,
            query: String::new(),
            input: None,
            models: Vec::new(),
            discovering: false,
            discovered_once: false,
            current: current.trim().to_string(),
            status: String::new(),
        };
        picker.align_to_current(channels);
        picker.reload_models(channels);
        picker.status = picker.default_status(channels);
        picker
    }

    // ---------- 只读访问（渲染层与状态机用） ----------

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn column(&self) -> usize {
        self.column
    }

    /// 某一列的选中位置（下标落在过滤后的可见序列里）；渲染层画高亮用。
    pub fn column_position(&self, column: usize) -> usize {
        if column == 0 {
            self.channel
        } else {
            self.model
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// 是否处于搜索输入态（渲染层据此画输入行）。
    pub fn searching(&self) -> bool {
        self.input.is_some()
    }

    /// 搜索输入态的当前文本。
    pub fn search_text(&self) -> &str {
        self.input.as_ref().map(|composer| composer.text()).unwrap_or("")
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
    }

    /// 是否正在等发现结果：`set_channel_models` 据此决定回填给谁。
    pub fn handles_discovery(&self) -> bool {
        self.discovering
    }

    /// 开始等发现结果（状态机在发出 `DiscoverChannelModels` 时调用）。
    pub fn mark_discovering(&mut self) {
        self.discovering = true;
        self.discovered_once = true;
        self.status = "正在发现可用模型…".to_string();
    }

    /// 当前值（表单里的模型引用）。
    pub fn current(&self) -> &str {
        &self.current
    }

    /// 当前值是否落在某个可见渠道上（左列画 `●` 用）；返回的是过滤后序列里的位置。
    pub fn current_channel(&self, channels: &[ChannelRow]) -> Option<usize> {
        let current = self.current.trim();
        if current.is_empty() {
            return None;
        }
        self.channel_indices(channels).into_iter().position(|index| {
            let channel = &channels[index];
            current == channel.key
                || (!channel.model_id.is_empty() && current == channel.model_id)
                || (!channel.profile_id.is_empty()
                    && current.starts_with(&format!("{}/", channel.profile_id)))
        })
    }

    /// 当前值对应的模型 id（右列画 `●` 用）。
    pub fn current_model(&self) -> Option<&str> {
        let current = self.current.trim();
        if current.is_empty() {
            return None;
        }
        match current.split_once('/') {
            Some((_, model)) if !model.is_empty() => Some(model),
            _ => None,
        }
    }

    /// 过滤后的渠道下标（绝对值，对映 Python `_filtered_channels`：只保留启用的渠道）。
    pub fn channel_indices(&self, channels: &[ChannelRow]) -> Vec<usize> {
        channels
            .iter()
            .enumerate()
            .filter(|(_, channel)| channel.enabled && channel_matches(channel, &self.query))
            .map(|(index, _)| index)
            .collect()
    }

    /// 候选模型的展示序列（供渲染层按窗口取）。
    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// 过滤后的模型下标（绝对值，对映 Python `_filtered`）。
    pub fn model_indices(&self) -> Vec<usize> {
        self.models
            .iter()
            .enumerate()
            .filter(|(_, model)| model_matches(model, &self.query))
            .map(|(index, _)| index)
            .collect()
    }

    /// 左列选中的渠道。
    pub fn selected_channel<'a>(&self, channels: &'a [ChannelRow]) -> Option<&'a ChannelRow> {
        let visible = self.channel_indices(channels);
        let position = clamp_index(self.channel, visible.len())?;
        channels.get(visible[position])
    }

    /// 右列选中的模型。
    pub fn selected_model(&self) -> Option<&str> {
        let visible = self.model_indices();
        let position = clamp_index(self.model, visible.len())?;
        self.models.get(visible[position]).map(String::as_str)
    }

    // ---------- 宿主回填 ----------

    /// 发现结果回填：空列表时保留原因文本，便于用户知道为什么没有候选。
    ///
    /// 已有候选时（例如刷新失败）保留它们，只把原因写进状态行。
    pub fn set_models(&mut self, models: Vec<String>, message: &str, channels: &[ChannelRow]) {
        self.discovering = false;
        if models.is_empty() {
            let reason = if message.trim().is_empty() {
                "未返回可用模型"
            } else {
                message.trim()
            };
            if self.models.is_empty() {
                self.reload_models(channels);
                self.status = format!("未发现可用模型（{reason}）；可直接在表单里输入模型 ID。");
            } else {
                self.status = format!("模型发现失败（{reason}）；已保留原有候选。");
            }
            return;
        }
        let mut merged: Vec<String> = Vec::new();
        if let Some(model) = self.selected_channel(channels).map(|channel| channel.model_id.trim().to_string()) {
            if !model.is_empty() {
                push_unique(&mut merged, model);
            }
        }
        for model in models {
            let trimmed = model.trim();
            if !trimmed.is_empty() {
                push_unique(&mut merged, trimmed.to_string());
            }
        }
        // 当前值可能来自 models.toml 的条目、不在发现结果里，也补进候选好让它可见。
        if let Some(current) = self.current_model().map(str::to_string) {
            push_unique(&mut merged, current);
        }
        self.models = merged;
        self.model = self
            .current_model()
            .and_then(|current| self.models.iter().position(|model| model == current))
            .unwrap_or(0);
        self.status = self.default_status(channels);
    }

    // ---------- 焦点与键位 ----------

    /// 获得焦点：把选中项对齐到当前值。
    pub fn focus(&mut self, channels: &[ChannelRow]) {
        self.focused = true;
        self.input = None;
        self.align_to_current(channels);
        self.reload_models(channels);
        self.status = self.default_status(channels);
    }

    /// 失去焦点：收起搜索输入态。
    pub fn blur(&mut self) {
        self.focused = false;
        self.input = None;
    }

    /// 首次进入选择器、且还没发现过时是否要自动拉一次模型列表。
    pub fn wants_discovery(&self) -> bool {
        self.focused && !self.discovering && !self.discovered_once
    }

    /// 处理一次按键；`channels` 用于左列内容与选中项折算。
    pub fn handle_key(&mut self, key: KeyCode, channels: &[ChannelRow]) -> PickerKey {
        if self.handle_search_key(key, channels) {
            return PickerKey::Handled;
        }
        match key {
            KeyCode::Esc => {
                self.blur();
                PickerKey::Blur
            }
            KeyCode::Up => {
                self.move_in_column(channels, -1);
                PickerKey::Handled
            }
            KeyCode::Down => {
                self.move_in_column(channels, 1);
                PickerKey::Handled
            }
            KeyCode::Left => {
                self.column = 0;
                self.status = self.default_status(channels);
                PickerKey::Handled
            }
            KeyCode::Right => {
                self.column = 1;
                self.status = self.default_status(channels);
                PickerKey::Handled
            }
            KeyCode::Tab | KeyCode::Char('/') => {
                self.begin_search();
                PickerKey::Handled
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if self.discovering {
                    PickerKey::Handled
                } else {
                    PickerKey::Discover
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.confirm(channels),
            _ => PickerKey::Handled,
        }
    }

    /// 搜索输入态：所有可打印键进缓冲，`Enter` 收起、`Esc` 取消本次搜索。
    ///
    /// 返回 `true` 表示按键已在搜索态消费掉。
    fn handle_search_key(&mut self, key: KeyCode, channels: &[ChannelRow]) -> bool {
        let Some(composer) = self.input.as_mut() else {
            return false;
        };
        match key {
            KeyCode::Enter => {
                self.input = None;
            }
            KeyCode::Esc => {
                self.input = None;
                self.query.clear();
                self.reset_indices();
                self.status = self.default_status(channels);
            }
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.move_left(),
            KeyCode::Right => composer.move_right(),
            KeyCode::Home => composer.move_home(),
            KeyCode::End => composer.move_end(),
            KeyCode::Up => self.move_in_column(channels, -1),
            KeyCode::Down => self.move_in_column(channels, 1),
            KeyCode::Char(character) => {
                composer.insert(&character.to_string());
            }
            _ => {}
        }
        self.sync_query();
        true
    }

    fn begin_search(&mut self) {
        let mut composer = Composer::default();
        if !self.query.is_empty() {
            composer.insert(&self.query);
        }
        self.input = Some(composer);
        self.status = "输入关键字过滤两列；Enter 应用，Esc 取消本次搜索。".to_string();
    }

    /// 把输入缓冲同步进过滤串（实时过滤，对映 Python `on_input_changed`）。
    fn sync_query(&mut self) {
        let text = self
            .input
            .as_ref()
            .map(|composer| composer.text().trim().to_lowercase())
            .unwrap_or_else(|| self.query.clone());
        if text == self.query {
            return;
        }
        self.query = text;
        self.reset_indices();
    }

    fn reset_indices(&mut self) {
        self.channel = 0;
        self.model = 0;
    }

    /// 在当前列里移动；渠道列循环（对映 Python `action_move_up/down` 的取模），
    /// 模型列也循环（Python 是夹取，这里循环更顺手，见 README 已知差异）。
    fn move_in_column(&mut self, channels: &[ChannelRow], delta: isize) {
        if self.column == 0 {
            let count = self.channel_indices(channels).len() as isize;
            if count > 0 {
                let position = clamp_index(self.channel, count as usize).unwrap_or(0) as isize;
                self.channel = ((position + delta).rem_euclid(count)) as usize;
                self.reload_models(channels);
            }
        } else {
            let count = self.model_indices().len() as isize;
            if count > 0 {
                let position = clamp_index(self.model, count as usize).unwrap_or(0) as isize;
                self.model = ((position + delta).rem_euclid(count)) as usize;
            }
        }
        self.status = self.default_status(channels);
    }

    /// 确认：左列用渠道自己配的 `model_id`，右列用 `profile/model_id`。
    fn confirm(&mut self, channels: &[ChannelRow]) -> PickerKey {
        if self.column == 0 {
            let Some(channel) = self.selected_channel(channels) else {
                self.status = "渠道列表为空：请先在「模型渠道」页新建渠道。".to_string();
                return PickerKey::Nothing;
            };
            let model = channel.model_id.trim();
            if model.is_empty() {
                self.status =
                    "该渠道没有配置模型：请按 → 到模型列选择，或按 R 重新发现。".to_string();
                return PickerKey::Nothing;
            }
            return PickerKey::Confirm(token_for(&channel.profile_id, model));
        }
        let Some(model) = self.selected_model() else {
            self.status = "当前渠道没有可用模型：先按 R 发现，或在表单里直接输入模型 ID。"
                .to_string();
            return PickerKey::Nothing;
        };
        let profile_id = self
            .selected_channel(channels)
            .map(|channel| channel.profile_id.clone())
            .unwrap_or_default();
        PickerKey::Confirm(token_for(&profile_id, model))
    }

    /// 左列换渠道后候选跟着换（发现结果不跨渠道复用），并把选中位置拉回当前值所在渠道。
    fn align_to_current(&mut self, channels: &[ChannelRow]) {
        let Some(position) = self.current_channel(channels) else {
            self.channel = 0;
            return;
        };
        self.channel = position;
    }

    fn reload_models(&mut self, channels: &[ChannelRow]) {
        self.model = 0;
        let mut models = Vec::new();
        if let Some(channel) = self.selected_channel(channels) {
            let model = channel.model_id.trim();
            if !model.is_empty() {
                push_unique(&mut models, model.to_string());
            }
        }
        if let Some(current) = self.current_model().map(str::to_string) {
            push_unique(&mut models, current);
        }
        self.models = models;
    }

    /// 状态行：对映 Python `_render_lists` 里那句「渠道 N · 当前：X · 可用模型 M」。
    fn default_status(&self, channels: &[ChannelRow]) -> String {
        let visible_channels = self.channel_indices(channels).len();
        if visible_channels == 0 {
            return "没有可用渠道：请先在「模型渠道」页新建渠道。".to_string();
        }
        let name = self
            .selected_channel(channels)
            .map(|channel| {
                if channel.name.trim().is_empty() {
                    channel.key.clone()
                } else {
                    channel.name.clone()
                }
            })
            .unwrap_or_else(|| "未选择".to_string());
        let models = self.model_indices().len();
        let column = if self.column == 0 { "渠道列" } else { "模型列" };
        format!("{column} · 渠道 {visible_channels} · 当前：{name} · 可用模型 {models}")
    }
}

/// 过滤：搜索串命中渠道的任一可见字段（对映 Python 的 haystack 拼法）。
pub fn channel_matches(channel: &ChannelRow, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {} {} {}",
        channel.key,
        channel.name,
        channel.profile_id,
        channel.provider,
        channel.protocol,
        channel.model_id
    )
    .to_lowercase();
    haystack.contains(query.trim())
}

/// 过滤：搜索串命中模型 id。
pub fn model_matches(model: &str, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    model.to_lowercase().contains(query.trim())
}

/// 选择器写回表单的模型引用（对映 Python `_selection_token`）。
pub fn token_for(profile_id: &str, model_id: &str) -> String {
    let model_id = model_id.trim();
    let profile_id = profile_id.trim();
    if profile_id.is_empty() {
        model_id.to_string()
    } else {
        format!("{profile_id}/{model_id}")
    }
}

/// 渲染窗口：返回 `(起点, 终点)`，选中项尽量居中（对映 Python 的 window 计算）。
pub fn window_bounds(total: usize, selected: usize, size: usize) -> (usize, usize) {
    if total == 0 || size == 0 {
        return (0, 0);
    }
    let size = size.min(total);
    let selected = selected.min(total - 1);
    let start = selected
        .saturating_sub(size / 2)
        .min(total.saturating_sub(size));
    (start, start + size)
}

/// 下标夹取；空序列返回 `None`。
pub fn clamp_index(index: usize, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(index.min(len - 1))
    }
}

fn push_unique(items: &mut Vec<String>, value: String) {
    if !items.iter().any(|item| item == &value) {
        items.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(key: &str, name: &str, profile: &str, model: &str) -> ChannelRow {
        ChannelRow {
            key: key.to_string(),
            name: name.to_string(),
            profile_id: profile.to_string(),
            provider: "openai".to_string(),
            protocol: "openai_chat_completions".to_string(),
            base_url: "https://example.test/v1".to_string(),
            model_id: model.to_string(),
            enabled: true,
            ..ChannelRow::default()
        }
    }

    fn channels() -> Vec<ChannelRow> {
        vec![
            channel("channel", "主渠道", "channel", "deepseek-v4.1-flash"),
            channel("channel-2", "硅基流动", "channel-2", ""),
        ]
    }

    #[test]
    fn window_bounds_keeps_the_selection_visible() {
        assert_eq!(window_bounds(0, 0, PICKER_WINDOW), (0, 0), "空列表没有窗口");
        assert_eq!(window_bounds(3, 2, PICKER_WINDOW), (0, 3), "不足一屏全放");
        assert_eq!(window_bounds(10, 0, PICKER_WINDOW), (0, 4));
        assert_eq!(window_bounds(10, 5, PICKER_WINDOW), (3, 7));
        assert_eq!(window_bounds(10, 9, PICKER_WINDOW), (6, 10), "尾部贴底");
    }

    #[test]
    fn token_and_matching_helpers_follow_python() {
        assert_eq!(
            token_for("channel-2", "Qwen/Qwen3.5"),
            "channel-2/Qwen/Qwen3.5"
        );
        assert_eq!(token_for("", "bare-model"), "bare-model");
        let row = channel("channel-2", "硅基流动", "channel-2", "");
        assert!(channel_matches(&row, ""), "空搜索串命中所有渠道");
        assert!(channel_matches(&row, "硅基"), "中文名参与匹配");
        assert!(channel_matches(&row, "channel-2"), "key 参与匹配");
        assert!(
            !channel_matches(&row, "anthropic"),
            "没命中的关键字要过滤掉"
        );
        assert!(model_matches("Qwen/Qwen3.5-35B-A3B", "qwen"), "大小写无关");
        assert!(!model_matches("Qwen/Qwen3.5-35B-A3B", "llama"));
    }

    #[test]
    fn new_aligns_to_the_channel_of_the_current_value() {
        let rows = channels();
        let picker = ModelPicker::new("channel-2/Qwen/Qwen3.5-35B-A3B", &rows);
        assert_eq!(picker.selected_channel(&rows).unwrap().key, "channel-2");
        assert_eq!(picker.current_model(), Some("Qwen/Qwen3.5-35B-A3B"));

        let picker = ModelPicker::new("channel", &rows);
        assert_eq!(picker.selected_channel(&rows).unwrap().key, "channel");
        assert_eq!(picker.current_model(), None, "纯渠道 key 没有模型段");
        assert_eq!(picker.current_channel(&rows), Some(0), "左列标在 current 上");
    }

    #[test]
    fn confirm_builds_the_profile_model_token() {
        let rows = channels();
        let mut picker = ModelPicker::new("", &rows);
        picker.focus(&rows);
        picker.set_models(vec!["Qwen/Qwen3.5-35B-A3B".to_string()], "", &rows);
        assert_eq!(
            picker.models(),
            ["deepseek-v4.1-flash", "Qwen/Qwen3.5-35B-A3B"],
            "候选 = 渠道自己的 model_id + 发现结果"
        );
        picker.handle_key(KeyCode::Right, &rows);
        picker.handle_key(KeyCode::Down, &rows);
        assert_eq!(picker.selected_model(), Some("Qwen/Qwen3.5-35B-A3B"));
        assert_eq!(
            picker.handle_key(KeyCode::Enter, &rows),
            PickerKey::Confirm("channel/Qwen/Qwen3.5-35B-A3B".to_string())
        );
    }

    #[test]
    fn confirm_on_the_channel_column_uses_its_configured_model() {
        let rows = channels();
        let mut picker = ModelPicker::new("", &rows);
        picker.focus(&rows);
        assert_eq!(
            picker.handle_key(KeyCode::Enter, &rows),
            PickerKey::Confirm("channel/deepseek-v4.1-flash".to_string())
        );
    }

    #[test]
    fn confirm_without_candidates_reports_nothing() {
        let rows = channels();
        let mut picker = ModelPicker::new("", &rows);
        picker.focus(&rows);
        picker.handle_key(KeyCode::Down, &rows);
        assert_eq!(
            picker.selected_channel(&rows).unwrap().key,
            "channel-2",
            "↓ 落到第二个渠道"
        );
        assert_eq!(picker.handle_key(KeyCode::Enter, &rows), PickerKey::Nothing);
        assert!(
            picker.status().contains("没有配置模型"),
            "{}",
            picker.status()
        );
    }

    #[test]
    fn search_filters_both_columns_and_esc_cancels_it() {
        let rows = channels();
        let mut picker = ModelPicker::new("", &rows);
        picker.focus(&rows);
        picker.set_models(
            vec!["deepseek-v4.1-flash".to_string(), "gpt-5.6".to_string()],
            "",
            &rows,
        );
        picker.handle_key(KeyCode::Right, &rows);
        picker.handle_key(KeyCode::Char('/'), &rows);
        assert!(picker.searching());
        for character in "gpt".chars() {
            picker.handle_key(KeyCode::Char(character), &rows);
        }
        assert_eq!(picker.query(), "gpt");
        assert_eq!(picker.model_indices().len(), 1, "右列被过滤");
        picker.handle_key(KeyCode::Enter, &rows);
        assert!(!picker.searching(), "Enter 应用过滤");
        assert_eq!(picker.query(), "gpt");
        picker.handle_key(KeyCode::Char('/'), &rows);
        picker.handle_key(KeyCode::Esc, &rows);
        assert_eq!(picker.query(), "", "Esc 取消本次搜索");
        assert!(picker.focused(), "取消搜索不离开选择器");
    }

    #[test]
    fn escape_leaves_the_picker_and_r_asks_for_discovery() {
        let rows = channels();
        let mut picker = ModelPicker::new("", &rows);
        picker.focus(&rows);
        assert!(picker.wants_discovery(), "刚进来且没发现过 → 自动拉一次");
        assert_eq!(
            picker.handle_key(KeyCode::Char('r'), &rows),
            PickerKey::Discover
        );
        picker.mark_discovering();
        assert!(!picker.wants_discovery(), "发现中不再重复请求");
        assert!(picker.handles_discovery(), "回填要路由到选择器");
        assert_eq!(
            picker.handle_key(KeyCode::Char('r'), &rows),
            PickerKey::Handled,
            "发现中按 r 不重复发请求"
        );
        picker.set_models(vec!["a".to_string()], "", &rows);
        assert_eq!(picker.handle_key(KeyCode::Esc, &rows), PickerKey::Blur);
        assert!(!picker.focused());
    }

    #[test]
    fn discovery_failure_keeps_candidates_and_reports_reason() {
        let rows = channels();
        let mut picker = ModelPicker::new("", &rows);
        picker.focus(&rows);
        picker.set_models(vec!["chosen".to_string()], "", &rows);
        picker.mark_discovering();
        picker.set_models(Vec::new(), "缺少 API Key", &rows);
        assert!(
            picker.status().contains("缺少 API Key"),
            "{}",
            picker.status()
        );
        assert!(
            picker.models().iter().any(|model| model == "chosen"),
            "失败不清空已有候选：{:?}",
            picker.models()
        );
        assert!(!picker.handles_discovery());
    }

    #[test]
    fn switching_channel_reloads_its_own_candidates() {
        let rows = channels();
        let mut picker = ModelPicker::new("", &rows);
        picker.focus(&rows);
        picker.set_models(vec!["only-of-channel".to_string()], "", &rows);
        picker.handle_key(KeyCode::Down, &rows);
        assert_eq!(
            picker.selected_channel(&rows).unwrap().key,
            "channel-2",
            "换到第二个渠道"
        );
        assert!(
            !picker.models().iter().any(|model| model == "only-of-channel"),
            "发现结果不跨渠道复用：{:?}",
            picker.models()
        );
        assert!(
            picker.status().contains("渠道列") && picker.status().contains("硅基流动"),
            "状态行应换到新渠道：{}",
            picker.status()
        );
    }

    #[test]
    fn disabled_channels_are_hidden_like_python() {
        let mut rows = channels();
        rows[1].enabled = false;
        let mut picker = ModelPicker::new("", &rows);
        assert_eq!(picker.channel_indices(&rows), vec![0]);
        assert_eq!(
            picker.handle_key(KeyCode::Up, &rows),
            PickerKey::Handled,
            "只有一个可见渠道时移动不越界"
        );
        assert_eq!(picker.selected_channel(&rows).unwrap().key, "channel");
    }
}
