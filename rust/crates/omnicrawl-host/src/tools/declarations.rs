//! 工具声明：把工具表的参数契约转成 OpenAI functions 形状。
//!
//! 语义基准是 `omnicrawl/agent/runtime/llm_protocol.py` 的 `build_tool_declaration` /
//! `tool_parameters_schema` / `infer_tool_property_schema`（该模块尚未搬入内核，因此在宿主侧
//! 实现）：工具表里 `read` / `Edit_file` / `write_file` 的参数契约写的是**示例值**而不是
//! JSON Schema，声明前必须按示例推断类型，并用 `minProperties` 兜底禁止空对象调用。
//!
//! MCP 暴露的 Schema 来自各家实现（Pydantic、Rust、TypeScript），常见 `type` 数组、
//! `$ref`/`$defs`、null 联合与缺 `items` 的数组。这些写法在严格网关（CodeBuddy、
//! Gemini、OpenAI 兼容校验）眼里是「工具定义不合规」（HTTP 400
//! `invalid_function_parameters`），因此声明前统一收敛一次。

use omnicrawl_controllers::tool_args::{compact_tool_description, compact_tool_schema};
use omnicrawl_controllers::tool_catalog::ToolSpec;
use serde_json::{json, Map, Value};

/// 一条工具的 provider 声明。
pub fn declaration(spec: &ToolSpec) -> Value {
    let mut inferred = tool_parameters_schema(&spec.argument_schema);
    inline_local_refs(&mut inferred);
    normalize_type_unions(&mut inferred);
    drop_nullable_branches(&mut inferred);
    ensure_array_items(&mut inferred);
    let compacted = serde_json::to_string(&inferred)
        .map(|text| compact_tool_schema(&text))
        .unwrap_or_else(|_| json!({"type": "object", "properties": {}}));
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": compact_tool_description(&spec.description),
            "parameters": compacted,
        }
    })
}

/// 示例式契约 → 真 JSON Schema。
pub fn tool_parameters_schema(argument_schema: &str) -> Value {
    let raw = serde_json::from_str::<Value>(argument_schema)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let is_real_schema = raw.get("type").and_then(Value::as_str) == Some("object")
        && raw.get("properties").map(Value::is_object).unwrap_or(false);
    if is_real_schema {
        let mut schema = raw;
        apply_defaults(&mut schema);
        return schema;
    }

    let mut properties = Map::new();
    if let Some(example) = raw.as_object() {
        for (key, value) in example {
            properties.insert(key.clone(), infer_tool_property_schema(value));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::from("object"));
    let empty = properties.is_empty();
    schema.insert("properties".to_string(), Value::Object(properties));
    // 示例值风格没有显式 required：只要声明了参数，就禁止空对象调用。
    if !empty {
        schema.insert("minProperties".to_string(), Value::from(1));
    }
    Value::Object(schema)
}

fn apply_defaults(schema: &mut Value) {
    if let Some(object) = schema.as_object_mut() {
        object
            .entry("type".to_string())
            .or_insert_with(|| Value::from("object"));
        object
            .entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

/// 把数组型 `type`（`["integer", "null"]`，Pydantic 的可空字段就长这样）折成上游认的形状。
///
/// OpenAI 兼容网关与 DeepSeek 只接受字符串型 `type`；数组型在 MCP 暴露的 Schema 里极常见，
/// 原样发出去会被判成非法函数定义（HTTP 400 `invalid_function_parameters`）。折法：
///
/// - 单元素数组 → 直接退化成字符串（`["string"]` → `"string"`）；
/// - 多元素数组 → 去掉 `type`，改写成 `anyOf: [{type: A}, {type: B}]`，同层的其它关键字
///   （`minimum`、`description` 之类）原地保留；
/// - 同层已经有 `anyOf`/`oneOf` 时只摘掉 `type`，让已有分支继续表达类型。
///
/// 只递归 Schema 承载位置（`properties`/`items`/`additionalProperties`/`anyOf` 等），
/// 不碰 `enum`/`const` 里的值。
fn normalize_type_unions(node: &mut Value) {
    let Some(object) = node.as_object_mut() else {
        return;
    };

    if let Some(Value::Array(types)) = object.get("type").cloned() {
        let names: Vec<String> = types
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect();
        if names.len() == types.len() && !names.is_empty() {
            object.remove("type");
            match names.as_slice() {
                [single] => {
                    object.insert("type".to_string(), Value::String(single.clone()));
                }
                _ => {
                    if !object.contains_key("anyOf") && !object.contains_key("oneOf") {
                        let branches = names
                            .iter()
                            .map(|name| json!({"type": name}))
                            .collect::<Vec<Value>>();
                        object.insert("anyOf".to_string(), Value::Array(branches));
                    }
                }
            }
        }
    }

    for key in ["items", "additionalProperties", "not"] {
        if let Some(child) = object.get_mut(key) {
            normalize_type_unions(child);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(branches)) = object.get_mut(key) {
            for branch in branches.iter_mut() {
                normalize_type_unions(branch);
            }
        }
    }
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for schema in properties.values_mut() {
            normalize_type_unions(schema);
        }
    }
}

/// 展开 Schema 里的本地 `$ref`（`#/$defs/X`、`#/definitions/X`）。
///
/// Rust / Pydantic 一侧的 MCP 服务习惯把可复用结构放进 `$defs`，属性只剩一个
/// `{"$ref": "#/$defs/Command"}`。压缩阶段会把 `$defs` 与 `$ref` 一起丢掉，留在
/// 请求里的就是空 Schema `{}`——上游看到「参数没有类型」会判成工具定义不合规
/// （HTTP 400 `invalid_function_parameters`）。展开后再压缩，属性才带着真实类型。
///
/// 只认文档内部的指针；外部 URL、同级 `$ref` 环与过深嵌套都收敛成 `{}` 或
/// `{"type": "object"}`，绝不把解析不了的引用原样发给上游。
fn inline_local_refs(schema: &mut Value) {
    let root = schema.clone();
    inline_refs_at(schema, &root, 0);
}

/// `$ref` 展开的最大深度：自引用结构（树节点、递归表达式）到此为止。
const MAX_REF_DEPTH: usize = 16;

fn inline_refs_at(node: &mut Value, root: &Value, depth: usize) {
    let Value::Object(object) = node else {
        if let Value::Array(items) = node {
            for item in items {
                inline_refs_at(item, root, depth + 1);
            }
        }
        return;
    };

    if let Some(reference) = object
        .get("$ref")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        object.remove("$ref");
        match resolve_local_ref(root, &reference).cloned() {
            Some(mut expanded) if depth < MAX_REF_DEPTH => {
                inline_refs_at(&mut expanded, root, depth + 1);
                if let Value::Object(expanded_map) = expanded {
                    // `$ref` 旁边的兄弟键（description 之类）优先于目标里的同名键。
                    let siblings = std::mem::take(object);
                    *object = expanded_map;
                    for (key, value) in siblings {
                        object.insert(key, value);
                    }
                } else {
                    object
                        .entry("type".to_string())
                        .or_insert_with(|| json!("object"));
                }
            }
            // 解析不了（外部 URL、`#/x` 不存在、超深递归）：退成对象，绝不原样上送。
            _ => {
                object
                    .entry("type".to_string())
                    .or_insert_with(|| json!("object"));
            }
        }
    }

    for (key, child) in object.iter_mut() {
        // 定义表本身不再递归：它的内容只在使用处按需展开。
        if key == "$defs" || key == "definitions" {
            continue;
        }
        inline_refs_at(child, root, depth + 1);
    }
}

/// 解析 `#/a/b` 形式的 JSON Pointer；解析不了返回 `None`。
fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix("#/")?;
    let mut current = root;
    for segment in pointer.split('/') {
        let decoded = segment.replace("~1", "/").replace("~0", "~");
        current = current.get(decoded.as_str())?;
    }
    Some(current)
}

/// 去掉 `anyOf` / `oneOf` 里的纯 null 分支。
///
/// 可空字段在上游等价于「可以不填」，而 `{"type": "null"}` 这类分支在 CodeBuddy、
/// Gemini 一类网关里会被判成非法类型。折法是：
///
/// - 只有一个非 null 分支 → 把分支提升到本层（`["integer", "null"]` → `"type": "integer"`）；
/// - 有多个非 null 分支 → 保留 `anyOf`，只去掉 null；
/// - 全是 null → 整个分支表摘掉，属性退成任意值（空 Schema）。
fn drop_nullable_branches(node: &mut Value) {
    if let Value::Object(object) = node {
        for key in ["anyOf", "oneOf"] {
            // 不是数组（畸形 Schema）就原样留着，交给压缩阶段处理。
            let Some(Value::Array(branches)) = object.get(key).cloned() else {
                continue;
            };
            object.remove(key);
            let mut kept: Vec<Value> = branches
                .into_iter()
                .filter(|branch| !is_null_schema(branch))
                .collect();
            for branch in kept.iter_mut() {
                drop_nullable_branches(branch);
            }
            match kept.len() {
                0 => {}
                1 => {
                    if let Some(Value::Object(branch)) = kept.pop() {
                        for (branch_key, branch_value) in branch {
                            object.entry(branch_key).or_insert(branch_value);
                        }
                    }
                }
                _ => {
                    object.insert(key.to_string(), Value::Array(kept));
                }
            }
        }
        for (key, child) in object.iter_mut() {
            if key == "anyOf" || key == "oneOf" {
                continue;
            }
            drop_nullable_branches(child);
        }
    } else if let Value::Array(items) = node {
        for item in items {
            drop_nullable_branches(item);
        }
    }
}

/// 数组必须带 `items`：OpenAI 的函数 Schema 校验把「`type: array` 却没有 `items`」
/// 判成非法定义，而多类型联合折出来的 `{"type": "array"}` 分支正好缺这一项。
/// 未知元素类型时用空 Schema `{}`（任意元素），不臆造成字符串。
fn ensure_array_items(node: &mut Value) {
    if let Value::Object(object) = node {
        if object.get("type").and_then(Value::as_str) == Some("array")
            && !object.contains_key("items")
        {
            object.insert("items".to_string(), json!({}));
        }
        for child in object.values_mut() {
            ensure_array_items(child);
        }
    } else if let Value::Array(items) = node {
        for item in items {
            ensure_array_items(item);
        }
    }
}

fn is_null_schema(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.len() == 1 && map.get("type").and_then(Value::as_str) == Some("null")
        }
        _ => false,
    }
}

/// 按示例值推断属性类型。
pub fn infer_tool_property_schema(example: &Value) -> Value {
    match example {
        Value::Bool(_) => json!({"type": "boolean"}),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                json!({"type": "integer"})
            } else {
                json!({"type": "number"})
            }
        }
        Value::Array(_) => json!({"type": "array", "items": {"type": "string"}}),
        Value::Object(_) => json!({"type": "object"}),
        _ => json!({"type": "string"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_contract_is_inferred_into_a_schema() {
        let schema = tool_parameters_schema(
            r#"{"path":"main.py","start_line":1,"max_lines":200,"function_name":"Class.method","text":"目标片段","context_lines":20}"#,
        );
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["start_line"]["type"], "integer");
        assert_eq!(schema["properties"]["context_lines"]["type"], "integer");
        assert_eq!(schema["minProperties"], 1);
        // 键序跟着示例走（保序 map），与 Python 的 dict 顺序一致。
        let keys: Vec<&String> = schema["properties"]
            .as_object()
            .expect("properties 是对象")
            .keys()
            .collect();
        assert_eq!(keys.first().map(|key| key.as_str()), Some("path"));
    }

    #[test]
    fn real_schema_is_kept_as_is() {
        let schema = tool_parameters_schema(
            r#"{"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}"#,
        );
        assert_eq!(schema["required"][0], "command");
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema.get("minProperties").is_none());
    }

    fn spec(name: &str, argument_schema: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "说明".to_string(),
            argument_schema: argument_schema.to_string(),
            requires_confirmation: false,
            model_output_is_bounded: false,
            run_in_subprocess: true,
        }
    }

    #[test]
    fn declaration_flattens_type_arrays() {
        // MCP（Pydantic）暴露的可空字段是 `["integer", "null"]`，多类型字段更长；
        // 上游只认字符串型 type，声明前必须折成任何网关都接受的形状。
        let declared = declaration(&spec(
            "cad_snapshot",
            r#"{"type":"object","properties":{"accountId":{"type":["integer","null"],"minimum":1},"packet":{"type":["object","array","string"]},"only":{"type":["string"]}}}"#,
        ));
        let parameters = &declared["function"]["parameters"];
        assert_eq!(parameters["properties"]["only"]["type"], "string");
        // 只剩一个非 null 分支时直接提升成单类型，不再留 anyOf。
        assert_eq!(parameters["properties"]["accountId"]["type"], "integer");
        assert!(parameters["properties"]["accountId"].get("anyOf").is_none());
        assert_eq!(parameters["properties"]["accountId"]["minimum"], 1);
        assert_eq!(
            parameters["properties"]["packet"]["anyOf"]
                .as_array()
                .expect("anyOf 是数组")
                .len(),
            3
        );
        assert!(
            !parameters["properties"]["packet"]["anyOf"]
                .to_string()
                .contains("null"),
            "null 分支必须摘掉：{parameters}"
        );
        // 联合里的 array 分支必须补上 items，否则上游判「数组没元素类型」。
        assert_eq!(
            parameters["properties"]["packet"]["anyOf"][1]["type"],
            "array"
        );
        assert!(parameters["properties"]["packet"]["anyOf"][1]
            .get("items")
            .is_some());
    }

    #[test]
    fn declaration_inlines_local_refs() {
        // concat 一类 MCP 服务的参数是 `{"$ref": "#/$defs/Command"}`；压缩会丢掉
        // `$defs`/`$ref`，不先展开就会把属性声明成空 Schema `{}`，上游判成工具定义不合规。
        let declared = declaration(&spec(
            "concat_edit_apply",
            r##"{"type":"object","properties":{"command":{"$ref":"#/$defs/Command","description":"要应用的编辑"}},"$defs":{"Command":{"type":"object","properties":{"op":{"type":"string"}},"required":["op"]}}}"##,
        ));
        let command = &declared["function"]["parameters"]["properties"]["command"];
        assert_eq!(command["type"], "object");
        assert_eq!(command["properties"]["op"]["type"], "string");
        assert_eq!(command["required"][0], "op");
        assert!(declared["function"]["parameters"].get("$defs").is_none());
    }

    #[test]
    fn unresolvable_ref_still_declares_an_object() {
        // 解析不了的引用绝不能原样上送：退成 object 让模型至少能传一个对象。
        let declared = declaration(&spec(
            "external_ref",
            r#"{"type":"object","properties":{"payload":{"$ref":"https://example.com/schema.json#/Payload"}}}"#,
        ));
        let payload = &declared["function"]["parameters"]["properties"]["payload"];
        assert_eq!(payload["type"], "object");
        assert!(payload.get("$ref").is_none());
    }

    #[test]
    fn array_and_bool_examples_infer_types() {
        assert_eq!(
            infer_tool_property_schema(&json!(["a", "b"])),
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(
            infer_tool_property_schema(&json!(true)),
            json!({"type": "boolean"})
        );
        assert_eq!(
            infer_tool_property_schema(&json!(1.5)),
            json!({"type": "number"})
        );
        assert_eq!(
            infer_tool_property_schema(&json!(null)),
            json!({"type": "string"})
        );
    }
}
