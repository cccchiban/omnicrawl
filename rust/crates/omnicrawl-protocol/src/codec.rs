//! OpenAI 风格历史与会话消息、工具声明之间的编解码（兼容迁移期）。

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::{Map, Value};

use crate::message::{
    ConversationMessage, ImageBlock, ImageDetail, MessageBlock, Role, TextBlock, ToolCallBlock,
    ToolResultBlock, ToolSpec,
};

/// 把现有 OpenAI Chat 风格历史转换为统一消息块。
pub fn conversation_from_openai_messages(messages: &[Value]) -> Vec<ConversationMessage> {
    let mut converted: Vec<ConversationMessage> = Vec::new();

    for message in messages {
        let Some(message) = message.as_object() else {
            continue;
        };
        let role = message_role(message);

        if role == "tool" {
            let call_id = first_non_empty_str(message, &["tool_call_id", "id"]);
            let text = message.get("content").and_then(Value::as_str).unwrap_or("");
            converted.push(ConversationMessage {
                role: Role::Tool,
                blocks: vec![MessageBlock::ToolResult(ToolResultBlock::new(
                    call_id, true, text,
                ))],
                reasoning: String::new(),
                tools: Vec::new(),
            });
            continue;
        }

        let mut tools: Vec<ToolSpec> = Vec::new();
        if role == "system" {
            if let Some(Value::Array(raw_tools)) = message.get("tools") {
                tools = raw_tools
                    .iter()
                    .filter_map(tool_spec_from_openai_item)
                    .collect();
            }
        }

        let mut blocks: Vec<MessageBlock> = Vec::new();
        match message.get("content") {
            Some(Value::String(content)) if !content.is_empty() => {
                blocks.push(MessageBlock::Text(TextBlock::new(content.clone())));
            }
            Some(Value::Array(parts)) => blocks.extend(blocks_from_openai_content_parts(parts)),
            _ => {}
        }

        if let Some(Value::Array(tool_calls)) = message.get("tool_calls") {
            for item in tool_calls {
                if let Some(block) = tool_call_block_from_openai_item(item) {
                    blocks.push(MessageBlock::ToolCall(block));
                }
            }
        }

        if !blocks.is_empty() || matches!(role.as_str(), "user" | "assistant" | "system") {
            // 思考模式网关要求历史 assistant 消息回传 reasoning_content，丢弃会被拒（HTTP 400）。
            let reasoning = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            converted.push(ConversationMessage {
                role: Role::parse(&role),
                blocks,
                reasoning,
                tools,
            });
        }
    }

    converted
}

/// 收集 system 消息中携带的动态工具声明，供不支持消息内 tools 的 Provider 合并。
///
/// 按工具名去重：同一工具在多次搜索加载中重复出现时只合并一次，避免请求级 tools
/// 出现重复函数声明。
pub fn tools_from_conversation_messages(messages: &[ConversationMessage]) -> Vec<ToolSpec> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut result: Vec<ToolSpec> = Vec::new();

    for message in messages {
        if message.role != Role::System || message.tools.is_empty() {
            continue;
        }
        for tool in &message.tools {
            if !seen.insert(tool.name.as_str()) {
                continue;
            }
            result.push(tool.clone());
        }
    }

    result
}

/// 把 Chat Completions 风格 tools 数组项转成统一 `ToolSpec`。
pub fn tool_spec_from_openai_item(item: &Value) -> Option<ToolSpec> {
    let item = item.as_object()?;
    let function = match item.get("function") {
        Some(Value::Object(function)) => function,
        _ => item,
    };

    let name = function
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return None;
    }

    let parameters = match function.get("parameters") {
        Some(Value::Object(parameters)) => parameters.clone(),
        _ => default_parameters(),
    };
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");

    Some(ToolSpec::new(name, description, parameters))
}

/// 解析 Host 内部使用的 OpenAI 风格文本/图片内容块。
pub fn blocks_from_openai_content_parts(parts: &[Value]) -> Vec<MessageBlock> {
    let mut blocks: Vec<MessageBlock> = Vec::new();

    for part in parts {
        let Some(part) = part.as_object() else {
            continue;
        };
        let part_type = part
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();

        if part_type == "text" || part_type == "input_text" {
            if let Some(Value::String(text)) = part.get("text") {
                if !text.is_empty() {
                    blocks.push(MessageBlock::Text(TextBlock::new(text.clone())));
                }
            }
            continue;
        }
        if part_type != "image_url" && part_type != "input_image" {
            continue;
        }

        let detail = match part.get("detail") {
            Some(Value::String(detail)) if !detail.is_empty() => detail.clone(),
            _ => "auto".to_string(),
        };
        let (image_url, detail) = match part.get("image_url") {
            Some(Value::Object(image_url)) => {
                let detail = match image_url.get("detail") {
                    Some(Value::String(detail)) if !detail.is_empty() => detail.clone(),
                    _ => detail,
                };
                match image_url.get("url") {
                    Some(Value::String(url)) => (url.clone(), detail),
                    _ => continue,
                }
            }
            Some(Value::String(url)) => (url.clone(), detail),
            _ => continue,
        };

        let Some((media_type, data_base64)) = parse_data_image_url(image_url.trim()) else {
            continue;
        };
        blocks.push(MessageBlock::Image(ImageBlock::new(
            media_type,
            data_base64,
            ImageDetail::parse(&detail),
        )));
    }

    blocks
}

/// Python 侧只接受能解析成对象的 JSON 参数，其余（数组、标量、语法错误）都退化为空对象。
pub fn parse_arguments_object(raw: &str) -> Map<String, Value> {
    if raw.trim().is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(arguments)) => arguments,
        _ => Map::new(),
    }
}

/// Python 侧 `str(message.get("role") or "user")`：缺失或空字符串回落 `user`。
fn message_role(message: &Map<String, Value>) -> String {
    match message.get("role") {
        Some(Value::String(role)) if !role.is_empty() => role.clone(),
        _ => "user".to_string(),
    }
}

fn first_non_empty_str(message: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match message.get(*key) {
            Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn tool_call_block_from_openai_item(item: &Value) -> Option<ToolCallBlock> {
    let item = item.as_object()?;
    let function = item.get("function").and_then(Value::as_object);
    let name = function
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if name.is_empty() {
        return None;
    }

    let arguments = match function.and_then(|function| function.get("arguments")) {
        Some(Value::String(raw)) if !raw.trim().is_empty() => parse_arguments_object(raw),
        Some(Value::Object(arguments)) => arguments.clone(),
        _ => Map::new(),
    };
    let call_id = item.get("id").and_then(Value::as_str).unwrap_or("");

    Some(ToolCallBlock::new(call_id, name, arguments))
}

fn default_parameters() -> Map<String, Value> {
    let mut parameters = Map::new();
    parameters.insert("type".to_string(), Value::String("object".to_string()));
    parameters.insert("properties".to_string(), Value::Object(Map::new()));
    parameters
}

/// 解析 `data:image/<png|jpeg|webp|gif>;base64,<payload>`（大小写不敏感、整体锚定）。
fn parse_data_image_url(url: &str) -> Option<(String, String)> {
    let rest = strip_prefix_ignore_ascii_case(url, "data:")?;
    let (media_type, payload) = split_once_ignore_ascii_case(rest, ";base64,")?;
    let media_type = media_type.to_lowercase();
    if !matches!(
        media_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return None;
    }
    if payload.is_empty()
        || !payload
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return None;
    }
    Some((media_type, payload.to_string()))
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.as_bytes().get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix.as_bytes()) {
        return None;
    }
    Some(&value[prefix.len()..])
}

/// 一次请求里工具调用 id 的体检结果。
///
/// 既用于判断「要不要在发出去之前改写」，也用于把细节写进日志与宿主提示。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CallIdReport {
    /// assistant `tool_calls[].id` 中重复出现的取值（含空串），按首次出现顺序。
    pub duplicate_calls: Vec<String>,
    /// `tool` 消息 `tool_call_id` 中重复出现的取值，按首次出现顺序。
    pub duplicate_results: Vec<String>,
    /// 空 id 的出现处数（assistant 侧 + `tool` 侧合计）。
    pub empty_ids: usize,
    /// 实际改写掉的 id 处数。
    pub renamed: usize,
    /// 有 assistant 调用却没有任何 `tool` 结果的 id。
    pub dangling_calls: Vec<String>,
    /// 有 `tool` 结果却找不到 assistant 调用的 id。
    pub orphan_results: Vec<String>,
}

impl CallIdReport {
    /// 是否存在协议层面的 id 问题（重复、空、配对缺失）。
    pub fn has_violation(&self) -> bool {
        !self.duplicate_calls.is_empty()
            || !self.duplicate_results.is_empty()
            || self.empty_ids > 0
            || !self.dangling_calls.is_empty()
            || !self.orphan_results.is_empty()
    }

    /// 是否真的改写过 id。
    pub fn repaired(&self) -> bool {
        self.renamed > 0
    }

    /// 单行中文摘要：直接进日志或宿主提示，不再二次加工。
    pub fn describe(&self) -> String {
        if !self.has_violation() {
            return "工具调用 id 无异常".to_string();
        }
        format!(
            "改写 {} 处；assistant 侧重复 {:?}；tool 侧重复 {:?}；空 id {} 处；缺结果的调用 {:?}；缺调用的结果 {:?}",
            self.renamed,
            self.duplicate_calls,
            self.duplicate_results,
            self.empty_ids,
            self.dangling_calls,
            self.orphan_results,
        )
    }
}

/// assistant 侧一处工具调用 id 在消息列表里的位置。
#[derive(Debug, Clone)]
struct CallSpot {
    message: usize,
    item: usize,
    id: String,
}

/// `tool` 侧一处结果 id 在消息列表里的位置。
#[derive(Debug, Clone)]
struct ResultSpot {
    message: usize,
    id: String,
}

/// 收集所有 assistant `tool_calls[].id` 的位置与取值（缺失 id 记成空串）。
fn collect_call_spots(messages: &[Value]) -> Vec<CallSpot> {
    let mut spots = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let Some(object) = message.as_object() else {
            continue;
        };
        let Some(Value::Array(calls)) = object.get("tool_calls") else {
            continue;
        };
        for (item_index, item) in calls.iter().enumerate() {
            spots.push(CallSpot {
                message: message_index,
                item: item_index,
                id: item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    spots
}

/// 收集所有 `tool` 消息的 `tool_call_id`（缺失记成空串）。
fn collect_result_spots(messages: &[Value]) -> Vec<ResultSpot> {
    let mut spots = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let Some(object) = message.as_object() else {
            continue;
        };
        if object.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        spots.push(ResultSpot {
            message: message_index,
            id: first_non_empty_str(object, &["tool_call_id", "id"]),
        });
    }
    spots
}

/// 列表里出现过的全部 id 取值（两侧合计）：新 id 不能与它们撞车。
fn existing_ids(calls: &[CallSpot], results: &[ResultSpot]) -> BTreeSet<String> {
    calls
        .iter()
        .map(|spot| spot.id.clone())
        .chain(results.iter().map(|spot| spot.id.clone()))
        .filter(|id| !id.is_empty())
        .collect()
}

/// 按出现顺序把同一个 id 的多次出现标号：返回「第几次出现」下标。
fn occurrence_index(seen: &mut HashMap<String, usize>, id: &str) -> usize {
    let slot = seen.entry(id.to_string()).or_insert(0);
    let index = *slot;
    *slot += 1;
    index
}

/// 线上请求的工具调用 id 体检与就地修复。
///
/// 规则（对得上网关那句「同一请求不能重复提交相同的 call_id」，`type: invalid_tool_state`）：
/// 一次请求里 assistant `tool_calls[].id` 必须**互不重复**，`tool` 消息的 `tool_call_id`
/// 必须与之**逐一双向配对**，且两侧都不允许空 id。出现重复或空 id 时按「出现顺序」重新配对：
/// 同一个原 id 的第 k 次出现（k≥1，即 0 基的第 k 个）在两侧拿到同一个新 id，因此
/// assistant 调用与它的结果始终指向同一个新 id，协议仍然合法。
///
/// **只改传进来的这份列表**：调用方给的是「即将发出去」的副本，运行期消息与转录事件保持原样。
/// 这样淘汰标注（`tool_call_evicted` 按原始 id 匹配）与重启后的历史投影都不会与线上 id 打架。
/// 保留首次出现的原 id，也是为了让历史前缀里没问题的部分逐字不变（提示缓存保护）。
///
/// 返回值同时给出「改了多少处」与「哪一侧还有配对缺口」：后者只上报不改写——凭空补一条
/// `tool` 结果会伪造事实，凭空删一条已知调用会丢上下文，二者都比让网关拒绝更糟。
pub fn normalize_call_ids(messages: &mut [Value]) -> CallIdReport {
    let calls = collect_call_spots(messages);
    let results = collect_result_spots(messages);
    let mut report = CallIdReport::default();
    if calls.is_empty() && results.is_empty() {
        return report;
    }

    // 先数清每个 id 两侧各出现几次：只有「重复」「空」才需要改写。
    let mut call_counts: HashMap<String, usize> = HashMap::new();
    for spot in &calls {
        *call_counts.entry(spot.id.clone()).or_insert(0) += 1;
    }
    let mut result_counts: HashMap<String, usize> = HashMap::new();
    for spot in &results {
        *result_counts.entry(spot.id.clone()).or_insert(0) += 1;
    }
    for (id, count) in &call_counts {
        if id.is_empty() {
            report.empty_ids += count;
        } else if *count > 1 {
            report.duplicate_calls.push(id.clone());
        }
    }
    for (id, count) in &result_counts {
        if id.is_empty() {
            report.empty_ids += count;
        } else if *count > 1 {
            report.duplicate_results.push(id.clone());
        }
    }
    for id in call_counts.keys() {
        if !result_counts.contains_key(id) {
            report.dangling_calls.push(id.clone());
        }
    }
    for id in result_counts.keys() {
        if !call_counts.contains_key(id) {
            report.orphan_results.push(id.clone());
        }
    }

    // 需要改写的 id 按「原 id + 第几次出现」定名，两侧共用同一张表，配对因此不会走散。
    let mut ledger: HashMap<(String, usize), String> = HashMap::new();
    let mut existing = existing_ids(&calls, &results);
    let mut counter = 0usize;
    let mut rename_for = |id: &str, occurrence: usize| -> Option<String> {
        if !id.is_empty() && occurrence == 0 {
            return None;
        }
        if let Some(existing) = ledger.get(&(id.to_string(), occurrence)) {
            return Some(existing.clone());
        }
        loop {
            counter += 1;
            let candidate = format!("call_dedup{counter}");
            if existing.insert(candidate.clone()) {
                ledger.insert((id.to_string(), occurrence), candidate.clone());
                return Some(candidate);
            }
        }
    };

    let mut seen_calls: HashMap<String, usize> = HashMap::new();
    let mut call_renames: Vec<(usize, usize, String)> = Vec::new();
    for spot in &calls {
        let occurrence = occurrence_index(&mut seen_calls, &spot.id);
        if let Some(new_id) = rename_for(&spot.id, occurrence) {
            call_renames.push((spot.message, spot.item, new_id));
        }
    }
    let mut seen_results: HashMap<String, usize> = HashMap::new();
    let mut result_renames: Vec<(usize, String)> = Vec::new();
    for spot in &results {
        let occurrence = occurrence_index(&mut seen_results, &spot.id);
        if let Some(new_id) = rename_for(&spot.id, occurrence) {
            result_renames.push((spot.message, new_id));
        }
    }

    for (message_index, item_index, new_id) in &call_renames {
        if let Some(call) = messages[*message_index]
            .get_mut("tool_calls")
            .and_then(Value::as_array_mut)
            .and_then(|calls| calls.get_mut(*item_index))
            .and_then(Value::as_object_mut)
        {
            call.insert("id".to_string(), Value::String(new_id.clone()));
        }
    }
    for (message_index, new_id) in &result_renames {
        if let Some(object) = messages[*message_index].as_object_mut() {
            object.insert("tool_call_id".to_string(), Value::String(new_id.clone()));
        }
    }
    report.renamed = call_renames.len() + result_renames.len();
    report
}

/// 轮换**尾部一批**工具调用的 id：最近一条带 `tool_calls` 的 assistant 消息及其后所有
/// 指向这批 id 的 `tool` 结果，逐一声明成 `call_{tag}{n}`。
///
/// 用途只有一个：网关把「同一请求重复提交相同的 call_id」判成 400 之后，原样重发等于
/// 再犯一次。只动尾巴不动前缀，历史部分的提示缓存逐字保留，也避免把整段历史的 id 洗掉。
/// 返回被改写的处数；没有可用批次时返回 0。
/// 调用方负责保证 `tag` 只含 `[A-Za-z0-9]`，以免拼出网关不认的 id。
pub fn rotate_tail_call_ids(messages: &mut [Value], tag: &str) -> usize {
    let calls = collect_call_spots(messages);
    let Some(last) = calls
        .iter()
        .filter(|spot| !spot.id.is_empty())
        .map(|spot| spot.message)
        .max()
    else {
        return 0;
    };
    let batch: Vec<CallSpot> = calls
        .into_iter()
        .filter(|spot| spot.message == last && !spot.id.is_empty())
        .collect();
    let mut existing = existing_ids(&batch, &[]);
    let mut ledger: HashMap<String, String> = HashMap::new();
    let mut counter = 0usize;
    let mut renamed = 0usize;
    for spot in &batch {
        let new_id = match ledger.get(&spot.id) {
            Some(existing) => existing.clone(),
            None => {
                let fresh = loop {
                    counter += 1;
                    let candidate = format!("call_{tag}{counter}");
                    if existing.insert(candidate.clone()) {
                        break candidate;
                    }
                };
                ledger.insert(spot.id.clone(), fresh.clone());
                fresh
            }
        };
        if let Some(call) = messages[spot.message]
            .get_mut("tool_calls")
            .and_then(Value::as_array_mut)
            .and_then(|calls| calls.get_mut(spot.item))
            .and_then(Value::as_object_mut)
        {
            call.insert("id".to_string(), Value::String(new_id));
            renamed += 1;
        }
    }
    // 尾巴上指向这批原 id 的 tool 结果同步改名，配对不能走散。
    for (message_index, message) in messages.iter_mut().enumerate() {
        if message_index <= last {
            continue;
        }
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        if object.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let current = first_non_empty_str(object, &["tool_call_id", "id"]);
        let Some(new_id) = ledger.get(&current) else {
            continue;
        };
        object.insert("tool_call_id".to_string(), Value::String(new_id.clone()));
        renamed += 1;
    }
    renamed
}

fn split_once_ignore_ascii_case<'a>(haystack: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || haystack_bytes.len() < needle_bytes.len() {
        return None;
    }
    let index = (0..=haystack_bytes.len() - needle_bytes.len()).find(|&index| {
        haystack_bytes[index..index + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes)
    })?;
    Some((&haystack[..index], &haystack[index + needle_bytes.len()..]))
}

#[cfg(test)]
mod call_id_tests {
    use super::*;
    use serde_json::json;

    /// 造一条 assistant 工具调用消息。
    fn assistant(id: &str, name: &str) -> Value {
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": "{}"},
            }],
        })
    }

    /// 造一条 tool 结果消息。
    fn result(id: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": "ok"})
    }

    /// 取出列表里两侧的 id 出现序列。
    fn ids(messages: &[Value]) -> (Vec<String>, Vec<String>) {
        let calls = collect_call_spots(messages)
            .into_iter()
            .map(|spot| spot.id)
            .collect();
        let results = collect_result_spots(messages)
            .into_iter()
            .map(|spot| spot.id)
            .collect();
        (calls, results)
    }

    #[test]
    fn clean_history_is_left_untouched() {
        let mut messages = vec![
            assistant("call_a", "bash"),
            result("call_a"),
            json!({"role": "assistant", "content": "收尾"}),
        ];
        let before = messages.clone();
        let report = normalize_call_ids(&mut messages);
        assert_eq!(messages, before, "无问题时必须逐字不动（提示缓存保护）");
        assert!(!report.has_violation());
        assert!(!report.repaired());
    }

    #[test]
    fn repeated_id_across_batches_is_renamed_pairwise() {
        // 网关按「每次请求的序号」发 id 时，一个回合里几个批次都会拿到 call_0：
        // 同一次请求里就会出现两条 assistant 调用共用同一个 call_id。
        let mut messages = vec![
            assistant("call_0", "bash"),
            result("call_0"),
            assistant("call_0", "read"),
            result("call_0"),
        ];
        let report = normalize_call_ids(&mut messages);
        let (calls, results) = ids(&messages);
        assert_eq!(calls, vec!["call_0", "call_dedup1"], "后出现的重复 id 要改名");
        assert_eq!(results, vec!["call_0", "call_dedup1"], "结果按同一张表改名");
        assert!(
            calls
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                == calls.len()
                && results.iter().collect::<BTreeSet<_>>().len() == results.len(),
            "改名后两侧都必须唯一：{calls:?} / {results:?}"
        );
        assert_eq!(report.renamed, 2);
        assert_eq!(report.duplicate_calls, vec!["call_0".to_string()]);
        assert_eq!(report.duplicate_results, vec!["call_0".to_string()]);
    }

    #[test]
    fn same_id_twice_inside_one_message_is_renamed_in_order() {
        let mut messages = vec![json!({
            "role": "assistant",
            "tool_calls": [
                {"id": "call_x", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                {"id": "call_x", "type": "function", "function": {"name": "read", "arguments": "{}"}},
            ],
        })];
        let report = normalize_call_ids(&mut messages);
        let (calls, _) = ids(&messages);
        assert_eq!(calls, vec!["call_x", "call_dedup1"]);
        assert_eq!(report.renamed, 1);
    }

    #[test]
    fn empty_ids_get_unique_names_on_both_sides() {
        let mut messages = vec![
            assistant("", "bash"),
            result(""),
            assistant("", "read"),
            result(""),
        ];
        let report = normalize_call_ids(&mut messages);
        let (calls, results) = ids(&messages);
        assert_eq!(calls, vec!["call_dedup1", "call_dedup2"]);
        assert_eq!(results, vec!["call_dedup1", "call_dedup2"]);
        assert_eq!(report.empty_ids, 4, "空 id 两侧各两处");
        assert_eq!(report.renamed, 4);
    }

    #[test]
    fn pairing_gaps_are_reported_without_rewriting_content() {
        let mut messages = vec![
            assistant("call_only_call", "bash"),
            result("call_only_result"),
        ];
        let report = normalize_call_ids(&mut messages);
        assert_eq!(report.dangling_calls, vec!["call_only_call".to_string()]);
        assert_eq!(report.orphan_results, vec!["call_only_result".to_string()]);
        assert_eq!(report.renamed, 0, "配对缺口只上报，不凭空增删消息");
        assert!(report.has_violation());
    }

    #[test]
    fn rotation_only_touches_the_tail_batch() {
        let mut messages = vec![
            assistant("call_old", "bash"),
            result("call_old"),
            assistant("call_tail", "read"),
            result("call_tail"),
        ];
        let renamed = rotate_tail_call_ids(&mut messages, "rt");
        assert_eq!(renamed, 2, "尾部一批只改 assistant 与它自己的结果");
        let (calls, results) = ids(&messages);
        assert_eq!(calls, vec!["call_old", "call_rt1"]);
        assert_eq!(results, vec!["call_old", "call_rt1"]);
    }

    #[test]
    fn rotation_skips_when_tail_has_no_named_call() {
        let mut messages = vec![json!({"role": "user", "content": "你好"})];
        assert_eq!(rotate_tail_call_ids(&mut messages, "rt"), 0);
    }
}
