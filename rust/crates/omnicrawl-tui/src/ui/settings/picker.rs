//! 「渠道 + 模型」两段式模型选择：工具输出压缩页与顾问设置页共用。
//!
//! 取代原先的双列选择器（Python `ModelPickerPane` 的 Rust 对映）：模型选择现在是
//! 两个普通下拉——先选渠道（候选来自渠道列表），再在该渠道的候选模型里选一项。
//! 表单草稿里存的是两段原始值（渠道 key 与模型 ID），本模块只负责「发现」这一件事：
//! 记住已发现结果属于哪条渠道、是否在发现中、以及候选的合并口径。
//!
//! 发现由宿主在后台执行（网络 I/O 不能占用界面线程），结果经
//! [`ModelDiscovery::set_models`] 回填，写盘仍走表单页的 `Ctrl+S`。

use super::ChannelRow;

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

/// 从 `profile/model_id` 形式的引用里取出模型段；纯渠道 key 没有模型段。
pub fn model_part(token: &str) -> Option<&str> {
    match token.trim().split_once('/') {
        Some((_, model)) if !model.is_empty() => Some(model),
        _ => None,
    }
}

/// 某个 token 是否指向这条渠道。
///
/// 三种写法都算命中：渠道 key、渠道配的模型 ID，以及 `profile_id/...` 前缀。
pub fn channel_owns_token(channel: &ChannelRow, token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return false;
    }
    token == channel.key
        || (!channel.model_id.is_empty() && token == channel.model_id)
        || (!channel.profile_id.is_empty() && token.starts_with(&format!("{}/", channel.profile_id)))
}

/// 把配置里的模型引用拆成（渠道 key, 模型 ID）。
///
/// 引用可能是 `profile/model_id`、裸模型名，或渠道 key；认不出渠道时渠道回落到列表首条、
/// 模型沿用原值，保存时再拼回 `profile/model_id`（与 `/advisor` 的解析口径同源）。
pub fn split_token(token: &str, channels: &[ChannelRow]) -> (String, String) {
    let token = token.trim();
    if token.is_empty() {
        return (
            channels
                .first()
                .map(|channel| channel.key.clone())
                .unwrap_or_default(),
            String::new(),
        );
    }
    if let Some(channel) = channels
        .iter()
        .find(|channel| channel_owns_token(channel, token))
    {
        // 渠道 key 本身即「沿用渠道自带的模型」。
        let model = if token == channel.key {
            String::new()
        } else {
            model_part(token).unwrap_or(token).to_string()
        };
        return (channel.key.clone(), model);
    }
    (
        channels
            .first()
            .map(|channel| channel.key.clone())
            .unwrap_or_default(),
        model_part(token).unwrap_or(token).to_string(),
    )
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

/// 一个表单页的模型发现状态。
#[derive(Debug, Clone, Default)]
pub struct ModelDiscovery {
    /// 已发现结果所属的渠道 key（换渠道即作废）。
    channel: String,
    /// 该渠道已发现的模型。
    models: Vec<String>,
    /// 是否正在等宿主的发现结果。
    discovering: bool,
    /// 是否已经为该渠道发起过一次发现（避免每次按键都重发）。
    requested: bool,
    status: String,
}

impl ModelDiscovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否为这条渠道发起发现：没发过、也不在发现中。
    pub fn wants_discovery(&self, channel: &str) -> bool {
        !channel.trim().is_empty() && self.channel != channel.trim()
    }

    /// 开始等发现结果（状态机在发出 `DiscoverChannelModels` 时调用）。
    pub fn mark_discovering(&mut self, channel: &str) {
        self.channel = channel.trim().to_string();
        self.models.clear();
        self.discovering = true;
        self.requested = true;
        self.status = "正在发现可用模型…".to_string();
    }

    /// 是否正在等发现结果。
    pub fn handles_discovery(&self) -> bool {
        self.discovering
    }

    /// 已经为哪条渠道拉过候选。
    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn discovered(&self) -> &[String] {
        &self.models
    }

    /// 发现结果回填：空列表时保留原候选，只把原因写进状态行。
    pub fn set_models(&mut self, models: Vec<String>, message: &str) {
        self.discovering = false;
        if models.is_empty() {
            let reason = if message.trim().is_empty() {
                "未返回可用模型"
            } else {
                message.trim()
            };
            self.status = format!("未发现可用模型（{reason}）；可直接手填模型 ID。");
            return;
        }
        let mut merged: Vec<String> = Vec::new();
        for model in models {
            let trimmed = model.trim();
            if !trimmed.is_empty() {
                push_unique(&mut merged, trimmed.to_string());
            }
        }
        self.models = merged;
        self.status = format!("已发现 {} 个可用模型。", self.models.len());
    }

    /// 模型下拉的候选：渠道自带的模型 ID → 该渠道的发现结果 → 当前值（去重保序）。
    ///
    /// 发现结果只在仍属于这条渠道时才算候选：换渠道后要重新发现，
    /// 免得把上一条渠道的模型列表当成新渠道的候选。
    pub fn candidates(&self, channel: Option<&ChannelRow>, current: &str) -> Vec<String> {
        let mut models: Vec<String> = Vec::new();
        if let Some(channel) = channel {
            let model = channel.model_id.trim();
            if !model.is_empty() {
                push_unique(&mut models, model.to_string());
            }
            if self.channel == channel.key {
                for model in &self.models {
                    push_unique(&mut models, model.clone());
                }
            }
        }
        let current = current.trim();
        if !current.is_empty() {
            push_unique(&mut models, current.to_string());
        }
        models
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = text.into();
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
        assert_eq!(window_bounds(0, 0, 4), (0, 0), "空列表没有窗口");
        assert_eq!(window_bounds(3, 2, 4), (0, 3), "不足一屏全放");
        assert_eq!(window_bounds(10, 0, 4), (0, 4));
        assert_eq!(window_bounds(10, 5, 4), (3, 7));
        assert_eq!(window_bounds(10, 9, 4), (6, 10), "尾部贴底");
    }

    #[test]
    fn token_and_matching_helpers_follow_python() {
        assert_eq!(
            token_for("channel-2", "Qwen/Qwen3.5"),
            "channel-2/Qwen/Qwen3.5"
        );
        assert_eq!(token_for("", "bare-model"), "bare-model");
        assert_eq!(model_part("channel-2/Qwen/Qwen3.5"), Some("Qwen/Qwen3.5"));
        assert_eq!(model_part("channel-2"), None, "纯渠道 key 没有模型段");

        let row = channel("channel-2", "硅基流动", "channel-2", "");
        assert!(channel_matches(&row, ""), "空搜索串命中所有渠道");
        assert!(channel_matches(&row, "硅基"), "中文名参与匹配");
        assert!(channel_matches(&row, "channel-2"), "key 参与匹配");
        assert!(!channel_matches(&row, "anthropic"), "没命中的关键字要过滤掉");
        assert!(model_matches("Qwen/Qwen3.5-35B-A3B", "qwen"), "大小写无关");
        assert!(!model_matches("Qwen/Qwen3.5-35B-A3B", "llama"));
    }

    #[test]
    fn split_token_resolves_channel_and_model() {
        let rows = channels();
        assert_eq!(
            split_token("channel-2/Qwen/Qwen3.5-35B-A3B", &rows),
            ("channel-2".to_string(), "Qwen/Qwen3.5-35B-A3B".to_string())
        );
        assert_eq!(
            split_token("channel", &rows),
            ("channel".to_string(), String::new()),
            "纯渠道 key：沿用渠道自带的模型"
        );
        assert_eq!(
            split_token("bare-model", &rows),
            ("channel".to_string(), "bare-model".to_string()),
            "裸模型名：渠道回落首条"
        );
        assert_eq!(
            split_token("", &rows),
            ("channel".to_string(), String::new()),
            "空引用：默认落在第一条渠道"
        );
    }

    #[test]
    fn discovery_is_per_channel_and_merged_with_the_channel_model() {
        let rows = channels();
        let mut discovery = ModelDiscovery::new();
        assert!(
            discovery.wants_discovery("channel"),
            "换到一条没拉过的渠道要发起发现"
        );
        discovery.mark_discovering("channel");
        assert!(!discovery.wants_discovery("channel"), "发现中不再重发");
        assert!(discovery.handles_discovery());
        discovery.set_models(vec!["Qwen/Qwen3.5-35B-A3B".to_string()], "");
        assert!(!discovery.handles_discovery());

        assert_eq!(
            discovery.candidates(Some(&rows[0]), "current-model"),
            [
                "deepseek-v4.1-flash",
                "Qwen/Qwen3.5-35B-A3B",
                "current-model"
            ],
            "渠道自带模型 + 发现结果 + 当前值"
        );
        assert!(
            discovery.wants_discovery("channel-2"),
            "换渠道后要重新发现"
        );
        assert_eq!(
            discovery.candidates(Some(&rows[1]), ""),
            Vec::<String>::new(),
            "没拉过的渠道只有它自己的模型（这里是空的）"
        );
    }

    #[test]
    fn discovery_failure_keeps_the_reason_in_status() {
        let mut discovery = ModelDiscovery::new();
        discovery.mark_discovering("channel");
        discovery.set_models(Vec::new(), "缺少 API Key");
        assert!(
            discovery.status().contains("缺少 API Key"),
            "{}",
            discovery.status()
        );
        assert!(!discovery.handles_discovery());
        assert!(discovery.discovered().is_empty());
    }
}
