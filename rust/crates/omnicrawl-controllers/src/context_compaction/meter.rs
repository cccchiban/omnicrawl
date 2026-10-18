//! 上下文计量：把「进模型的形状」折成 Token，并产出逐事件的价格快照。
//!
//! 这一层只做两件事，与判定、压缩、摘要都无关：
//!
//! 1. **投影后计价**：多模态 `content` 里的图片先被换成句柄文本再计价。图片真正的传输表示是
//!    `data:image/...;base64,...`，Base64 长度与原图体积成正比、与模型实际消耗的视觉 Token
//!    无关；拿它按字符折算会把一张 1 MB 的图算成二十五万 Token，让压缩在真实占用很低时就触发。
//!    句柄只留 MIME 与解码后的字节数，计价口径因此与图片体积脱钩。
//! 2. **逐事件快照**：一次测量同时给出总量与每个事件的价格，范围选择据此从尾部累加，
//!    不必为了知道「最后几个回合值多少」而重算整段历史。
//!
//! 估算规则本身是固定的：CJK 每字 1 Token，其余每 4 字符 1 Token，再加结构开销。没有任何
//! 配置项——容量属于模型路由（`context_window_tokens`），不属于估算器。

use serde_json::Value;

/// 一个多模态内容部件里图片的固定结构开销（类型标签 + 句柄字段框架）。
const IMAGE_BLOCK_OVERHEAD: i64 = 4;

/// 消息级固定开销（角色字段框架）。
pub const MESSAGE_OVERHEAD: i64 = 4;

/// 多模态部件数组的 `type`：OpenAI Chat 用 `image_url`，Responses 用 `input_image`。
const IMAGE_PART_TYPES: [&str; 2] = ["image_url", "input_image"];

/// 文本计价：CJK 每字 1 Token、其余每 4 字符 1 Token。
///
/// 由调用方注入而不是写死在这里：控制器的估算与 TUI 的进度条各有一套等价实现，
/// 共用本模块的投影规则即可，不必让两个 crate 互相依赖。
pub type TextPricer<'a> = &'a dyn Fn(&str) -> i64;

/// 图片在模型请求里的可见表示：只有 MIME 与体积，不含 Base64。
pub fn image_handle_text(media_type: &str, payload_chars: usize) -> String {
    let media_type = if media_type.is_empty() {
        "image/unknown"
    } else {
        media_type
    };
    let bytes = payload_chars / 4 * 3;
    format!("<image {media_type}, {bytes}B>")
}

/// 从 data URL 里取 MIME 与 Base64 载荷长度；不是 base64 data URL 时返回 `None`。
fn parse_data_url(url: &str) -> Option<(&str, usize)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.contains(";base64") {
        return None;
    }
    Some((meta.split(';').next().unwrap_or_default(), payload.len()))
}

/// 一个图片部件的计价：句柄文本 + 固定结构开销。
fn image_part_tokens(part: &Value, price: TextPricer<'_>) -> i64 {
    let url = part
        .get("image_url")
        .and_then(|value| match value {
            Value::String(url) => Some(url.as_str()),
            other => other.get("url").and_then(Value::as_str),
        })
        .unwrap_or_default();
    let handle = match parse_data_url(url) {
        Some((media_type, payload_chars)) => image_handle_text(media_type, payload_chars),
        // 非 data URL（远程地址、已卸载标记）：按地址长度计价，本来就短。
        None => format!("<image {url}>"),
    };
    IMAGE_BLOCK_OVERHEAD + price(&handle)
}

fn value_tokens(value: &Value, price: TextPricer<'_>) -> i64 {
    match value {
        Value::String(text) => price(text),
        other => price(&crate::json::python_dumps_compact_sorted(other)),
    }
}

/// 一条消息 `content` 的计价：多模态数组按部件投影后求和，其余按原口径。
pub fn content_tokens(content: &Value, price: TextPricer<'_>) -> i64 {
    match content {
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                if part
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| IMAGE_PART_TYPES.contains(&kind))
                {
                    image_part_tokens(part, price)
                } else {
                    value_tokens(part, price)
                }
            })
            .sum(),
        other => value_tokens(other, price),
    }
}

/// 一条模型可见消息的计价：角色、投影后的内容与工具字段。
pub fn message_tokens(message: &Value, price: TextPricer<'_>) -> i64 {
    let mut total = MESSAGE_OVERHEAD;
    total += price(
        message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    total += content_tokens(
        &message.get("content").cloned().unwrap_or(Value::from("")),
        price,
    );
    for key in ["name", "tool_call_id", "tool_calls"] {
        if let Some(value) = message.get(key) {
            total += value_tokens(value, price);
        }
    }
    total
}

/// 一批模型可见消息的计价。
pub fn messages_tokens(messages: &[Value], price: TextPricer<'_>) -> i64 {
    messages
        .iter()
        .map(|message| message_tokens(message, price))
        .sum()
}

/// 单个事件的价格：按它的模型可见投影计价。
pub fn event_tokens(
    event: &crate::context_compaction::policy::SourceEvent,
    price: TextPricer<'_>,
) -> i64 {
    match crate::context_compaction::projection::event_to_model_message(event) {
        Some(message) => message_tokens(&message, price),
        None => 0,
    }
}

/// 一条上下文请求的完整计价：系统提示词 + 上下文消息 + 工具声明 + 会话历史。
///
/// 这是「这次请求到底多大」的唯一入口。宿主回报的「压缩后大小」也走它，两边因此不可能
/// 各算各的——历史上这段拼装在 `session.rs` 与预算测量里各写了一遍，改一处就会漂。
pub fn conversation_tokens(
    system_prompt: &str,
    context_messages: &[Value],
    tool_schemas: &[Value],
    history_messages: &[Value],
    price: TextPricer<'_>,
) -> i64 {
    let mut total = price(system_prompt);
    total += messages_tokens(context_messages, price);
    total += schema_tokens(tool_schemas, price);
    total += messages_tokens(history_messages, price);
    total
}

/// 工具声明的计价：整体序列化后按字符密度算，等同于每步重新发送的那份 schema。
///
/// 空数组也算一次序列化价格（`[]` 折 1 Token）：与 Python 侧同一处实现保持逐字一致，
/// 免得空工具面时两边差一个常量。
pub fn schema_tokens(tool_schemas: &[Value], price: TextPricer<'_>) -> i64 {
    price(&crate::json::python_dumps_compact_sorted(&Value::Array(
        tool_schemas.to_vec(),
    )))
}

/// 一次请求的组成部件，用于把总量拆回各段。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConversationBreakdown {
    pub system_tokens: i64,
    pub context_tokens: i64,
    pub tool_tokens: i64,
    pub history_tokens: i64,
}

impl ConversationBreakdown {
    pub fn total(&self) -> i64 {
        self.system_tokens + self.context_tokens + self.tool_tokens + self.history_tokens
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "system_tokens": self.system_tokens,
            "context_tokens": self.context_tokens,
            "tool_tokens": self.tool_tokens,
            "history_tokens": self.history_tokens,
            "total_tokens": self.total(),
        })
    }
}

/// 逐段计价，供诊断与回放核对「大小都花在哪」。
pub fn conversation_breakdown(
    system_prompt: &str,
    context_messages: &[Value],
    tool_schemas: &[Value],
    history_messages: &[Value],
    price: TextPricer<'_>,
) -> ConversationBreakdown {
    ConversationBreakdown {
        system_tokens: price(system_prompt),
        context_tokens: messages_tokens(context_messages, price),
        tool_tokens: schema_tokens(tool_schemas, price),
        history_tokens: messages_tokens(history_messages, price),
    }
}

/// 测量结果的价格来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementBaseline {
    /// 没有可用锚点时完全由启发式得出。
    Estimated(i64),
    /// 供应商上报的输入 Token（真实用量），后续只叠加有符号增量。
    Usage(i64),
}

impl MeasurementBaseline {
    pub fn tokens(&self) -> i64 {
        match self {
            Self::Estimated(tokens) | Self::Usage(tokens) => *tokens,
        }
    }
}

/// 一次成功调用留下的锚点：它当时的表面价格，以及那次调用的基准。
///
/// 增量必须相对**当时的表面**计算，只记基准数值不够：压缩之后表面会小于锚点表面，
/// 那部分差值正是要减掉的量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementAnchor {
    pub surface_tokens: i64,
    pub baseline: MeasurementBaseline,
}

/// 一次测量的逐事件价格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenNode {
    pub event_id: String,
    pub event_type: String,
    pub tokens: i64,
}

/// 某次测量的完整快照：来源、增量、总量与逐事件账。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextMeasurement {
    pub baseline: Option<MeasurementBaseline>,
    /// 相对锚点表面的有符号增量：压缩、淘汰让它变负。
    pub surface_delta_tokens: i64,
    pub total_tokens: i64,
    pub nodes: Vec<TokenNode>,
}

/// 组装一次测量。
///
/// `provider_used` 只在「不低于锚点基准」时被采信——供应商数字滞后（摘要调用尚未回来）
/// 会让压力虚低，宁可取更保守的那个。
pub fn measure_surface(
    anchor: Option<MeasurementAnchor>,
    nodes: Vec<TokenNode>,
    provider_used: Option<i64>,
) -> ContextMeasurement {
    let surface_tokens: i64 = nodes.iter().map(|node| node.tokens).sum();
    let (baseline, surface_delta_tokens) = match anchor {
        // 没有锚点就没有可比的历史表面：当前表面自己就是答案。
        None => (
            provider_used
                .map(MeasurementBaseline::Usage)
                .unwrap_or(MeasurementBaseline::Estimated(surface_tokens)),
            0,
        ),
        Some(anchor) => {
            let baseline = match (anchor.baseline, provider_used) {
                (MeasurementBaseline::Estimated(estimated), Some(used)) if used >= estimated => {
                    MeasurementBaseline::Usage(used)
                }
                (baseline, _) => baseline,
            };
            (baseline, surface_tokens - anchor.surface_tokens)
        }
    };
    ContextMeasurement {
        baseline: Some(baseline),
        surface_delta_tokens,
        total_tokens: std::cmp::max(0, baseline.tokens() + surface_delta_tokens),
        nodes,
    }
}

/// 纯字符数的 Token 下界折算（4 字符 1 token，不考虑 CJK）。
///
/// 工具调用压缩按字符数报「省了多少」，量的是待压缩正文而不是进模型的消息，
/// 因此只取一个不会高估的下界。
pub fn chars_token_floor(chars: usize) -> i64 {
    ((chars + 3) / 4) as i64
}

/// CJK 判定：文本计价规则里「一字一 Token」的那部分。
pub fn is_cjk_char(item: char) -> bool {
    matches!(item as u32,
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x3040..=0x30ff | 0xac00..=0xd7af)
}

/// 控制器侧的文本计价规则（CJK 每字 1 Token、其余每 4 字符 1 Token）。
pub fn text_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    let total = text.chars().count() as i64;
    let cjk = text.chars().filter(|item| is_cjk_char(*item)).count() as i64;
    let rest = total - cjk;
    cjk + (rest + 3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn price(text: &str) -> i64 {
        text_tokens(text)
    }

    #[test]
    fn image_parts_are_priced_by_handle_not_payload() {
        let small = json!([{
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{}", "A".repeat(4_000))},
        }]);
        let large = json!([{
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{}", "A".repeat(4_000_000))},
        }]);
        let small_tokens = content_tokens(&small, &price);
        let large_tokens = content_tokens(&large, &price);
        // 体积放大 1000 倍，价格只随句柄里的字节数字变化（最多几 Token）。
        assert!(
            large_tokens - small_tokens <= 4,
            "{small_tokens} vs {large_tokens}"
        );
        assert!(
            large_tokens < 40,
            "图片按句柄计价，不应随 Base64 增长：{large_tokens}"
        );
    }

    #[test]
    fn responses_image_parts_are_projected_too() {
        let part = json!([{
            "type": "input_image",
            "image_url": format!("data:image/webp;base64,{}", "B".repeat(40_000)),
        }]);
        let tokens = content_tokens(&part, &price);
        assert!(tokens < 40, "Responses 形状也要走投影：{tokens}");
    }

    #[test]
    fn image_handle_carries_media_type_and_decoded_bytes() {
        let text = image_handle_text("image/jpeg", 8);
        assert!(text.contains("image/jpeg"));
        assert!(text.contains("6B"));
    }

    #[test]
    fn text_only_content_keeps_the_old_price() {
        let text = json!("中文结果");
        assert_eq!(content_tokens(&text, &price), 4);
        let message = json!({"role": "user", "content": "hi"});
        assert_eq!(message_tokens(&message, &price), 6);
    }

    fn anchor(surface_tokens: i64) -> MeasurementAnchor {
        MeasurementAnchor {
            surface_tokens,
            baseline: MeasurementBaseline::Estimated(surface_tokens),
        }
    }

    #[test]
    fn usage_baseline_is_taken_only_when_not_lower_than_the_estimate() {
        let nodes = vec![TokenNode {
            event_id: "e1".to_string(),
            event_type: "user_message".to_string(),
            tokens: 100,
        }];
        let conservative = measure_surface(Some(anchor(100)), nodes.clone(), Some(80));
        assert_eq!(
            conservative.baseline,
            Some(MeasurementBaseline::Estimated(100))
        );
        let trusted = measure_surface(Some(anchor(100)), nodes, Some(120));
        assert_eq!(trusted.baseline, Some(MeasurementBaseline::Usage(120)));
        // 表面没变时增量为 0；总量取更保守的供应商采样。
        assert_eq!(trusted.surface_delta_tokens, 0);
        assert_eq!(trusted.total_tokens, 120);
    }

    #[test]
    fn shrinking_surface_produces_a_negative_delta() {
        let nodes = vec![TokenNode {
            event_id: "s1".to_string(),
            event_type: "compact_summary".to_string(),
            tokens: 300,
        }];
        let measured = measure_surface(Some(anchor(5_000)), nodes, None);
        assert_eq!(measured.surface_delta_tokens, -4_700);
        assert_eq!(measured.total_tokens, 300, "锚点 + 负增量即压缩后的压力");
        // 表面缩到比负增量还小：压力夹在 0，不出现负数。
        let emptied = measure_surface(
            Some(anchor(5_000)),
            vec![TokenNode {
                event_id: "s1".to_string(),
                event_type: "compact_summary".to_string(),
                tokens: 0,
            }],
            None,
        );
        assert_eq!(emptied.total_tokens, 0);
    }
}
