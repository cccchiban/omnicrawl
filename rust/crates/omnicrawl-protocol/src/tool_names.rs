//! 工具名的「线上形态」：发给上游的 `function.name` 必须落在合法字符集内。
//!
//! 背景：OpenAI 兼容网关（以及 OpenAI 本体）要求函数名匹配 `^[a-zA-Z0-9_-]+$`，
//! Gemini 还额外要求首字符是字母或下划线。而 MCP 暴露给模型的名字是天生的
//! 「带分隔符」形态——工具是 `server.tool`（`registry.py` 的
//! `namespace_capability_name`），资源是 `mcp_read_resource__<uri>`（URI 里有
//! `:` 与 `/`）。原样把它们塞进请求体，上游会直接 400：
//!
//! ```text
//! Invalid 'tools[0].function.name': string does not match pattern.
//! Expected a string that matches the pattern '^[a-zA-Z0-9_-]+$'.
//! ```
//!
//! 于是这一层做两件事：
//!
//! 1. 把**发给上游的声明名**收敛到合法字符集（非法字符换成 `_`，数字开头补 `_`，
//!    超长截断，撞名追加 `_2`/`_3`…）；
//! 2. 保留「线上名 → 内部名」的反查表，模型回传的调用名据此**还原成内部原名**。
//!
//! 因此除了线上那一层，其它地方（内核派发、宿主工具表与审批、审计、转录事件、
//! 界面上的工具卡）看到的仍是 `server.tool`。Python 侧目前是把原名直接发出去，
//! 带 MCP 的会话在严格网关上必然失败——这是 Rust 单侧的修正，不是新增功能。

use std::collections::HashMap;

use crate::event::ModelReply;
use crate::message::{MessageBlock, ToolSpec};

/// 多数 Provider 的名义长度上限（超出部分上游可能直接拒绝或截断解析）。
pub const MAX_TOOL_NAME_CHARS: usize = 64;

/// 线上名与内部名的双向对照表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolNameMap {
    wire_to_internal: HashMap<String, String>,
    internal_to_wire: HashMap<String, String>,
}

impl ToolNameMap {
    /// 内部名 → 线上名；没有登记（本来合法）时原样返回。
    pub fn wire<'a>(&'a self, internal: &'a str) -> &'a str {
        self.internal_to_wire
            .get(internal)
            .map(String::as_str)
            .unwrap_or(internal)
    }

    /// 线上名 → 内部名；没有登记（模型自造的名字）时原样返回，交给下游按未知工具处理。
    pub fn internal<'a>(&'a self, wire: &'a str) -> &'a str {
        self.wire_to_internal
            .get(wire)
            .map(String::as_str)
            .unwrap_or(wire)
    }

    pub fn is_empty(&self) -> bool {
        self.wire_to_internal.is_empty()
    }

    /// 把模型回复里的工具调用名还原成内部名：既改 `tool_calls`，也改 assistant 原文里的
    /// `tool_calls[].function.name`——后者会被落进转录（`tool_call_requested.function_name`）
    /// 并回灌给模型，必须与内部名一致，否则同一段历史会有两种写法。
    pub fn restore_reply(&self, reply: &mut ModelReply) {
        if self.is_empty() {
            return;
        }
        for call in reply.tool_calls.iter_mut() {
            let restored = self.internal(&call.name).to_string();
            call.name = restored;
        }
        for block in reply.assistant_message.blocks.iter_mut() {
            if let MessageBlock::ToolCall(call) = block {
                let restored = self.internal(&call.name).to_string();
                call.name = restored;
            }
        }
    }
}

/// 按上游约束收敛一组工具声明，返回（可发送的声明, 对照表）。
///
/// 声明顺序即撞名时的仲裁顺序（先到先得），所以同名冲突的解析是稳定的：同一份工具表
/// 每次都会得到同一组线上名，prompt cache 的前缀因此不会被无谓地改写。
pub fn conform_tool_names(tools: &[ToolSpec]) -> (Vec<ToolSpec>, ToolNameMap) {
    let mut map = ToolNameMap::default();
    let mut used: HashMap<String, String> = HashMap::new();
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let wire = unique_wire_name(&tool.name, &mut used);
        if wire != tool.name {
            map.wire_to_internal
                .insert(wire.clone(), tool.name.clone());
            map.internal_to_wire.insert(tool.name.clone(), wire.clone());
        }
        out.push(ToolSpec::new(
            wire,
            tool.description.clone(),
            tool.parameters.clone(),
        ));
    }
    (out, map)
}

/// 占位并返回一个未被占用过的线上名。
fn unique_wire_name(internal: &str, used: &mut HashMap<String, String>) -> String {
    let base = sanitize_tool_name(internal);
    if used.insert(base.clone(), internal.to_string()).is_none() {
        return base;
    }
    let mut index = 2usize;
    loop {
        let suffix = format!("_{index}");
        let room = MAX_TOOL_NAME_CHARS.saturating_sub(suffix.len());
        let candidate = format!("{}{suffix}", truncate_chars(&base, room));
        if used
            .insert(candidate.clone(), internal.to_string())
            .is_none()
        {
            return candidate;
        }
        index += 1;
    }
}

/// 单个名字的字符集收敛：非 `[A-Za-z0-9_-]` 一律换成 `_`，数字开头补 `_`，超长截断。
pub fn sanitize_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(MAX_TOOL_NAME_CHARS));
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    match out.chars().next() {
        // 空名字与数字开头的名字（Gemini 不允许）都补一个下划线。
        None => out.push('_'),
        Some(first) if first.is_ascii_digit() => out.insert(0, '_'),
        _ => {}
    }
    truncate_chars(&out, MAX_TOOL_NAME_CHARS)
}

/// 按字符数截断（这里只会遇到 ASCII，取字符边界即可）。
fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::message::ToolCallBlock;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec::new(name, "说明", json!({"type": "object"}).as_object().cloned().unwrap())
    }

    #[test]
    fn sanitize_replaces_illegal_characters_and_leading_digits() {
        assert_eq!(sanitize_tool_name("fathom.search"), "fathom_search");
        assert_eq!(
            sanitize_tool_name("mcp_read_resource__file://x"),
            "mcp_read_resource__file___x"
        );
        assert_eq!(sanitize_tool_name("9lives"), "_9lives");
        assert_eq!(sanitize_tool_name(""), "_");
        assert_eq!(sanitize_tool_name("read_image-2"), "read_image-2");
    }

    #[test]
    fn long_names_are_truncated_to_the_provider_limit() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_tool_name(&long).chars().count(), MAX_TOOL_NAME_CHARS);
        // 超长名字撞名后也要保持在上限内。
        let (tools, map) = conform_tool_names(&[spec(&long), spec(&format!("{long}b"))]);
        for tool in &tools {
            assert!(tool.name.chars().count() <= MAX_TOOL_NAME_CHARS, "{}", tool.name);
        }
        assert_ne!(tools[0].name, tools[1].name);
        assert_eq!(map.internal(&tools[1].name), format!("{long}b"));
    }

    #[test]
    fn conform_keeps_legal_names_and_maps_dotted_ones() {
        let (tools, map) = conform_tool_names(&[
            spec("read"),
            spec("fathom.search"),
            spec("mcp_read_resource__fathom.docs"),
        ]);
        assert_eq!(tools[0].name, "read");
        assert_eq!(tools[1].name, "fathom_search");
        assert_eq!(tools[2].name, "mcp_read_resource__fathom_docs");
        assert_eq!(map.wire("read"), "read");
        assert_eq!(map.internal("fathom_search"), "fathom.search");
        assert_eq!(
            map.internal("mcp_read_resource__fathom_docs"),
            "mcp_read_resource__fathom.docs"
        );
        // 模型自造的名字原样透传。
        assert_eq!(map.internal("不存在"), "不存在");
    }

    #[test]
    fn colliding_wire_names_get_stable_suffixes() {
        let (tools, map) = conform_tool_names(&[spec("a.b"), spec("a_b"), spec("a-b")]);
        assert_eq!(tools[0].name, "a_b");
        assert_eq!(tools[1].name, "a_b_2");
        assert_eq!(tools[2].name, "a-b");
        assert_eq!(map.internal("a_b"), "a.b");
        assert_eq!(map.internal("a_b_2"), "a_b");
    }

    #[test]
    fn restore_reply_rewrites_calls_and_the_assistant_message() {
        let (_, map) = conform_tool_names(&[spec("fathom.search")]);
        let mut arguments = serde_json::Map::new();
        arguments.insert("query".to_string(), json!("x"));
        let call = ToolCallBlock::new("call-1", "fathom_search", arguments.clone());
        let message = crate::message::ConversationMessage {
            role: crate::message::Role::Assistant,
            blocks: vec![MessageBlock::ToolCall(call.clone())],
            reasoning: String::new(),
            tools: Vec::new(),
        };
        let mut reply = ModelReply {
            assistant_message: message,
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![call],
            usage: None,
            finish_reason: "tool_calls".to_string(),
            content_streamed: false,
            warnings: Vec::new(),
        };
        map.restore_reply(&mut reply);
        assert_eq!(reply.tool_calls[0].name, "fathom.search");
        assert_eq!(reply.tool_calls[0].call_id, "call-1");
        assert_eq!(reply.tool_calls[0].arguments.get("query"), Some(&json!("x")));
        match &reply.assistant_message.blocks[0] {
            MessageBlock::ToolCall(call) => assert_eq!(call.name, "fathom.search"),
            other => panic!("应当仍是工具调用块：{other:?}"),
        }
        // 空对照表是零开销路径：什么都不改。
        let mut untouched = reply.clone();
        ToolNameMap::default().restore_reply(&mut untouched);
        assert_eq!(untouched, reply);
        let _ = BTreeMap::<String, String>::new();
    }
}
