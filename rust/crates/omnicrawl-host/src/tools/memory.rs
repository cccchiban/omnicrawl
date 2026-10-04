//! 记忆工具执行体：按 `scope` 解析三处记忆存储，再交给 `omnicrawl-session` 的存储层。
//!
//! 语义基准是 `omnicrawl/agent/toolkit/memory_tools.py`（四个工具的参数与结果适配）与
//! `omnicrawl/agent/controllers/tools/implementations.py` 的
//! `_require_memory_store_for_arguments`（作用域路由与「未启用」文案）。

use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_controllers::json::{python_dumps, python_number_text};
use omnicrawl_controllers::memory::{
    project_memory_root, session_memory_root, user_data_root, user_memory_root,
    DEFAULT_MEMORY_DIRECTORY,
};
use omnicrawl_controllers::tool_args::{
    read_limited_int, read_optional_string_list, read_required_string_list,
};
use omnicrawl_controllers::tool_impl::{
    memory_store_missing_error, parse_memory_scope, MemoryScope,
};
use omnicrawl_session::memory_store::{
    MemoryRecord, MemorySearchResult, MemoryStore, MemoryWriteRequest,
};
use serde_json::{json, Map, Value};

use super::decision_search::{
    apply_order, reranked_order, RerankOptions, RERANK_POOL_LIMIT,
};
use super::error::{ToolError, ToolOutcome};

/// 三个作用域各自的开关与目录；会话级还需要当前会话 ID。
#[derive(Debug, Clone, Default)]
pub struct MemoryOptions {
    pub workspace_root: PathBuf,
    pub home: PathBuf,
    /// 项目级记忆目录（相对路径以工作区为基准），空串时用默认目录。
    pub project_directory: String,
    pub project_enabled: bool,
    pub user_enabled: bool,
    pub session_id: Option<String>,
    pub session_enabled: bool,
    /// 搜索重排（`decision_models.toml` 的 `memory_search_rerank` 开关）；未开启时走本地排序。
    pub rerank: Arc<RerankOptions>,
}

impl MemoryOptions {
    /// 解析某个作用域的存储；该作用域未启用时返回「未启用」错误。
    pub fn store(&self, scope: MemoryScope) -> Result<MemoryStore, ToolError> {
        let root = match scope {
            MemoryScope::Project => {
                if !self.project_enabled {
                    return Err(disabled_error(scope));
                }
                let directory = if self.project_directory.trim().is_empty() {
                    DEFAULT_MEMORY_DIRECTORY.to_string()
                } else {
                    self.project_directory.clone()
                };
                project_memory_root(&self.workspace_root, &directory)
                    .map_err(|error| ToolError::new(error.message().to_string()))?
            }
            MemoryScope::User => {
                if !self.user_enabled {
                    return Err(disabled_error(scope));
                }
                user_memory_root(&user_data_root(&self.home_path()))
            }
            MemoryScope::Session => {
                let session_id = match self.session_id.as_deref() {
                    Some(session_id) if self.session_enabled => session_id,
                    _ => return Err(disabled_error(scope)),
                };
                session_memory_root(&user_data_root(&self.home_path()), session_id)
                    .map_err(|error| ToolError::new(error.message().to_string()))?
            }
        };
        Ok(MemoryStore::open(root))
    }

    fn home_path(&self) -> PathBuf {
        if self.home.as_os_str().is_empty() {
            default_home()
        } else {
            self.home.clone()
        }
    }
}

/// 用户主目录（与内核 `omnicrawl-controllers::workspace` 的解析同序）。
pub fn default_home() -> PathBuf {
    for name in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from(".")
}

fn disabled_error(scope: MemoryScope) -> ToolError {
    ToolError::new(memory_store_missing_error(scope).message().to_string())
}

fn store_error(error: omnicrawl_session::SessionStoreError) -> ToolError {
    ToolError::new(error.message().to_string())
}

fn scope_for(arguments: &Map<String, Value>) -> Result<MemoryScope, ToolError> {
    parse_memory_scope(arguments.get("scope"))
        .map_err(|error| ToolError::new(error.message().to_string()))
}

fn text_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Number(_) => python_number_text(value),
        _ => String::new(),
    }
}

fn argument_text(arguments: &Map<String, Value>, key: &str) -> String {
    match arguments.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(value) => text_value(value).trim().to_string(),
    }
}

fn memory_ids(arguments: &Map<String, Value>) -> Result<Vec<Value>, ToolError> {
    let ids = read_required_string_list(arguments, "memory_ids");
    if ids.is_empty() {
        return Err(ToolError::new("memory_ids 不能为空。"));
    }
    Ok(ids.into_iter().map(Value::String).collect())
}

fn search_result_value(result: &MemorySearchResult) -> Value {
    json!({
        "id": result.id,
        "summary": result.summary,
        "storage_directory": result.storage_directory,
        "related_directories": result.related_directories,
        "timestamp": MemoryStore::render_timestamp(result.timestamp),
    })
}

/// 送进重排的候选描述：只带判断相关度所需的三项，不含正文。
fn rerank_candidate_text(result: &MemorySearchResult) -> String {
    let mut parts = vec![result.summary.clone()];
    if !result.storage_directory.trim().is_empty() {
        parts.push(format!("存储目录：{}", result.storage_directory));
    }
    if !result.related_directories.is_empty() {
        parts.push(format!("关联目录：{}", result.related_directories.join("、")));
    }
    parts.join("\n")
}

fn record_value(record: &MemoryRecord) -> Value {
    json!({
        "id": record.id,
        "timestamp": MemoryStore::render_timestamp(record.timestamp),
        "related_directories": record.related_directories,
        "content": record.content,
    })
}

/// 重排指令：只要求按相关度排序，不做其他判断。
const RERANK_INSTRUCTIONS: &str = "给定 state 里的检索查询与调用意图，判断每个候选项与查询的相关程度；\
state 是事实来源，不要根据候选项自身措辞推测查询。";

pub fn memory_search(options: &MemoryOptions, arguments: &Map<String, Value>) -> ToolOutcome {
    let scope = scope_for(arguments)?;
    let store = options.store(scope)?;
    let query = argument_text(arguments, "query");
    if query.is_empty() {
        return Err(ToolError::new("query 不能为空。"));
    }
    let reason = argument_text(arguments, "reason");
    if reason.is_empty() {
        return Err(ToolError::new("reason 不能为空。"));
    }
    let candidates: Vec<Value> = read_optional_string_list(arguments, "candidate_directories")
        .unwrap_or_default()
        .into_iter()
        .map(Value::String)
        .collect();
    let max_results = read_limited_int(arguments, "max_results", 5, 20) as u32;
    // 重排开启时先取更宽的一池候选：本地排序只用于挑池子，最终顺序与条数由决策模型决定。
    let pool = if options.rerank.active() {
        max_results.max(RERANK_POOL_LIMIT as u32)
    } else {
        max_results
    };
    let mut results = store
        .search(&query, &candidates, pool)
        .map_err(store_error)?;
    if options.rerank.active() {
        let state = json!({
            "scope": scope.as_str(),
            "query": query,
            "reason": reason,
            "candidate_directories": candidates,
        });
        let texts: Vec<String> = results.iter().map(rerank_candidate_text).collect();
        if let Some(order) = reranked_order(
            options.rerank.as_ref(),
            state,
            RERANK_INSTRUCTIONS,
            &query,
            &texts,
        ) {
            results = apply_order(results, &order);
        }
    }
    results.truncate(max_results as usize);
    Ok(python_dumps(
        &Value::Array(results.iter().map(search_result_value).collect()),
        2,
    ))
}

pub fn memory_read(options: &MemoryOptions, arguments: &Map<String, Value>) -> ToolOutcome {
    let scope = scope_for(arguments)?;
    let store = options.store(scope)?;
    let ids = memory_ids(arguments)?;
    let records = store.read(&ids).map_err(store_error)?;
    Ok(python_dumps(
        &Value::Array(records.iter().map(record_value).collect()),
        2,
    ))
}

pub fn memory_expand_related(
    options: &MemoryOptions,
    arguments: &Map<String, Value>,
) -> ToolOutcome {
    let scope = scope_for(arguments)?;
    let store = options.store(scope)?;
    let ids = memory_ids(arguments)?;
    let max_depth = read_limited_int(arguments, "max_depth", 1, 3) as u32;
    let max_results = read_limited_int(arguments, "max_results", 5, 20) as u32;
    let results = store
        .expand_related(&ids, max_depth, max_results)
        .map_err(store_error)?;
    Ok(python_dumps(
        &Value::Array(results.iter().map(search_result_value).collect()),
        2,
    ))
}

pub fn memory_write(options: &MemoryOptions, arguments: &Map<String, Value>) -> ToolOutcome {
    let scope = scope_for(arguments)?;
    // Python 侧先解析作用域存储、再进工具函数校验参数，未启用作用域要优先报错。
    let store = options.store(scope)?;
    let Some(Value::Array(items)) = arguments.get("memories") else {
        return Err(ToolError::new("memories 必须是非空列表。"));
    };
    if items.is_empty() {
        return Err(ToolError::new("memories 必须是非空列表。"));
    }

    let mut requests: Vec<MemoryWriteRequest> = Vec::new();
    for (index, raw_memory) in items.iter().enumerate() {
        let number = index + 1;
        let Some(memory) = raw_memory.as_object() else {
            return Err(ToolError::new(format!(
                "第 {number} 条记忆必须是 JSON 对象。"
            )));
        };
        let content = match memory.get("content") {
            None | Some(Value::Null) => String::new(),
            Some(value) => text_value(value).trim().to_string(),
        };
        if content.is_empty() {
            return Err(ToolError::new(format!(
                "第 {number} 条记忆 content 不能为空。"
            )));
        }
        let related_directories = match memory.get("related_directories") {
            None => Vec::new(),
            Some(Value::Array(entries)) => {
                if !entries.iter().all(Value::is_string) {
                    return Err(ToolError::new(format!(
                        "第 {number} 条记忆 related_directories 必须是字符串列表。"
                    )));
                }
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            }
            Some(_) => {
                return Err(ToolError::new(format!(
                    "第 {number} 条记忆 related_directories 必须是字符串列表。"
                )))
            }
        };
        requests.push(MemoryWriteRequest {
            content,
            related_directories,
            storage_directory: optional_string(memory, "storage_directory", number)?,
            source_event: optional_string(memory, "source_event", number)?,
        });
    }

    let records = store.write(&requests).map_err(store_error)?;
    Ok(python_dumps(
        &Value::Array(records.iter().map(record_value).collect()),
        2,
    ))
}

fn optional_string(
    memory: &Map<String, Value>,
    key: &str,
    number: usize,
) -> Result<Option<String>, ToolError> {
    match memory.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ToolError::new(format!(
            "第 {number} 条记忆 {key} 必须是字符串或 null。"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn options(name: &str) -> (MemoryOptions, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-memory-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let options = MemoryOptions {
            workspace_root: root.clone(),
            home: root.join("home"),
            project_directory: ".omnicrawl/.oclmemory".to_string(),
            project_enabled: true,
            user_enabled: true,
            session_id: Some("session-1".to_string()),
            session_enabled: true,
            ..MemoryOptions::default()
        };
        (options, root)
    }

    #[test]
    fn disabled_scopes_report_their_own_message() {
        let options = MemoryOptions::default();
        let error = memory_search(&options, &arguments(json!({"query": "x", "reason": "y"})))
            .expect_err("默认配置下项目级记忆未启用");
        assert_eq!(error.message, "project 级记忆系统未启用。");
    }

    #[test]
    fn scope_is_resolved_before_arguments_are_validated() {
        let options = MemoryOptions::default();
        for (tool, payload) in [
            ("write", json!({"memories": []})),
            ("read", json!({"memory_ids": []})),
            ("expand", json!({"memory_ids": []})),
            ("search", json!({"query": "", "reason": ""})),
        ] {
            let outcome = match tool {
                "write" => memory_write(&options, &arguments(payload)),
                "read" => memory_read(&options, &arguments(payload)),
                "expand" => memory_expand_related(&options, &arguments(payload)),
                _ => memory_search(&options, &arguments(payload)),
            };
            let error = outcome.expect_err("未启用作用域应当优先报错");
            assert_eq!(error.message, "project 级记忆系统未启用。", "{tool}");
        }
    }

    #[test]
    fn invalid_scope_is_rejected_with_its_value() {
        let (options, _root) = options("scope");
        let error = memory_read(
            &options,
            &arguments(json!({"scope": "team", "memory_ids": ["m1"]})),
        )
        .expect_err("非法作用域应当被拒绝");
        assert_eq!(
            error.message,
            "scope 仅支持 project、session、user；收到：team。"
        );
    }

    #[test]
    fn write_then_search_round_trips_through_the_store() {
        let (options, _root) = options("round-trip");
        let written = memory_write(
            &options,
            &arguments(json!({
                "memories": [{
                    "content": "Rust 客户端需要独立执行工具。",
                    "related_directories": ["rust/crates"],
                }],
            })),
        )
        .expect("写入应当成功");
        let records: Value = serde_json::from_str(&written).expect("结果是 JSON");
        assert_eq!(records.as_array().map(Vec::len), Some(1));
        assert_eq!(records[0]["related_directories"][0], "rust/crates");

        let found = memory_search(
            &options,
            &arguments(json!({
                "query": "Rust 客户端",
                "reason": "继续实现工具",
                "candidate_directories": ["rust/crates"],
            })),
        )
        .expect("搜索应当成功");
        let results: Value = serde_json::from_str(&found).expect("结果是 JSON");
        assert!(
            results
                .as_array()
                .map(|items| !items.is_empty())
                .unwrap_or(false),
            "{found}"
        );
    }

    #[test]
    fn write_validates_each_memory_shape() {
        let (options, _root) = options("validate");
        let error = memory_write(&options, &arguments(json!({"memories": []})))
            .expect_err("空列表应当被拒绝");
        assert_eq!(error.message, "memories 必须是非空列表。");

        let error = memory_write(
            &options,
            &arguments(json!({"memories": [{"related_directories": ["a"]}]})),
        )
        .expect_err("缺 content 应当被拒绝");
        assert_eq!(error.message, "第 1 条记忆 content 不能为空。");

        let error = memory_write(
            &options,
            &arguments(json!({"memories": [{"content": "x", "related_directories": null}]})),
        )
        .expect_err("null 关联目录应当被拒绝");
        assert_eq!(
            error.message,
            "第 1 条记忆 related_directories 必须是字符串列表。"
        );

        let error = memory_write(
            &options,
            &arguments(json!({"memories": [{"content": "x", "source_event": 7}]})),
        )
        .expect_err("非字符串来源事件应当被拒绝");
        assert_eq!(
            error.message,
            "第 1 条记忆 source_event 必须是字符串或 null。"
        );
    }

    /// 重排桩：按给定顺序返回下标，并记录收到的候选文本（不起网络）。
    struct StubRerank {
        order: Vec<usize>,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl super::super::decision_search::RerankClient for StubRerank {
        fn rank(
            &self,
            request: &super::super::decision_search::RerankRequest<'_>,
        ) -> Result<Vec<usize>, String> {
            *self.seen.lock().expect("记录未被毒化") = request.candidates.to_vec();
            Ok(self.order.clone())
        }
    }

    fn rerank_options(order: Vec<usize>, seen: Arc<std::sync::Mutex<Vec<String>>>) -> RerankOptions {
        use super::super::decision_search::RerankChannel;
        RerankOptions {
            enabled: true,
            channel: Some(RerankChannel {
                mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
                model: "jev-latest".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                api_key: "jv_test".to_string(),
                api_key_env: "JEV_API_KEY".to_string(),
            }),
            client: Arc::new(StubRerank { order, seen }),
            ..RerankOptions::default()
        }
    }

    #[test]
    fn rerank_reorders_results_and_keeps_max_results() {
        let (mut options, _root) = options("rerank");
        let mut written: Vec<String> = Vec::new();
        for content in [
            "内核重写：工具表装配完成",
            "内核重写：记忆中检索排序",
            "内核重写：知识库索引计划",
        ] {
            let record = memory_write(
                &options,
                &arguments(json!({"memories": [{"content": content}]})),
            )
            .expect("写入应当成功");
            let records: Value = serde_json::from_str(&record).expect("写入结果是 JSON");
            written.push(
                records[0]["id"].as_str().unwrap_or_default().to_string(),
            );
        }

        // 先取一次本地顺序作为基准。
        let baseline = memory_search(
            &options,
            &arguments(json!({"query": "内核重写", "reason": "取基准顺序", "max_results": 3})),
        )
        .expect("搜索应当成功");
        let baseline: Value = serde_json::from_str(&baseline).expect("结果是 JSON");
        let local_ids: Vec<String> = baseline
            .as_array()
            .expect("结果是数组")
            .iter()
            .map(|item| item["id"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(local_ids.len(), 3, "{baseline}");
        assert!(
            local_ids.iter().all(|id| written.contains(id)),
            "基准顺序里应当包含全部三条"
        );

        // 倒序重排：结果顺序应当是本地顺序的反向，条数仍按 max_results。
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        options.rerank = Arc::new(rerank_options(vec![2, 1, 0], Arc::clone(&seen)));
        let found = memory_search(
            &options,
            &arguments(json!({"query": "内核重写", "reason": "确认重排", "max_results": 2})),
        )
        .expect("搜索应当成功");
        let results: Value = serde_json::from_str(&found).expect("结果是 JSON");
        let items = results.as_array().expect("结果是数组");
        assert_eq!(items.len(), 2, "返回条数仍按 max_results：{found}");

        let mut expected: Vec<String> = local_ids.clone();
        expected.reverse();
        let actual: Vec<String> = items
            .iter()
            .map(|item| item["id"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(actual, expected[..2].to_vec(), "按重排顺序返回：{found}");

        let recorded = seen.lock().expect("记录未被毒化").clone();
        assert_eq!(recorded.len(), 3, "候选池比 max_results 宽：{recorded:?}");
        assert!(
            recorded.iter().any(|text| text.contains("存储目录")),
            "候选描述应带判断相关度所需的字段：{recorded:?}"
        );
    }

    #[test]
    fn rerank_failure_falls_back_to_local_order() {
        let (mut options, _root) = options("rerank-fail");
        // 渠道缺失：重排不可用，检索照旧返回本地排序结果。
        options.rerank = Arc::new(RerankOptions {
            enabled: true,
            channel: None,
            ..RerankOptions::default()
        });
        memory_write(
            &options,
            &arguments(json!({"memories": [{"content": "内核检索回退"}]})),
        )
        .expect("写入应当成功");
        let found = memory_search(
            &options,
            &arguments(json!({"query": "内核检索回退", "reason": "回归 fail-open"})),
        )
        .expect("检索不该因为重排不可用而失败");
        let results: Value = serde_json::from_str(&found).expect("结果是 JSON");
        assert_eq!(results.as_array().map(Vec::len), Some(1), "{found}");
    }
}
