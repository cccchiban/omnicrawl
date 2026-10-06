//! 压缩驱动的端到端自洽性：真会话目录上跑一次「测量 → 压缩 → 归档 → 落盘 → 重建历史」。

use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_compaction::{CompactionConfig, CompactionDriver, TurnBoundary};
use omnicrawl_controllers::context_compaction::{
    CompactionBatch, ContextCompactionService, ModelSummaryResult, SummaryCompactorPort,
    SummaryGenerationError, TokenUsageSample,
};
use omnicrawl_session::{utc_now, SessionStore};
use serde_json::{json, Value};

fn temp_root(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("时钟可用")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("omnicrawl-compaction-{tag}-{unique}"));
    std::fs::create_dir_all(&path).expect("建临时目录");
    path
}

/// 与 Python 侧校验用例同构的合法结构化摘要：来源事件引用真实事件 ID。
fn structured_summary(ids: &[String]) -> Value {
    let user = ids[0].clone();
    let assistant = ids[1].clone();
    let write_call = ids[2].clone();
    let bash_result = ids[5].clone();
    json!({
        "objective": ["目标一"],
        "current_state": ["状态一"],
        "constraints": [{"text": "约束一", "source_event_ids": [user]}],
        "decisions": [{"text": "决策一", "source_event_ids": [assistant]}],
        "completed": [{"text": "完成一", "source_event_ids": [user]}],
        "open_issues": [{"text": "问题一", "source_event_ids": [assistant]}],
        "artifacts": [{"text": "产物一", "source_event_ids": [write_call]}],
        "exact_evidence": [{"text": "用户原文", "source_event_ids": [user]}],
        "failed_attempts": [{"text": "失败一", "source_event_ids": [bash_result]}],
        "excluded_approaches": [{"text": "排除一", "source_event_ids": [write_call]}],
        "key_concepts": [{"text": "概念一", "source_event_ids": [user]}],
        "problem_solving_process": [{"text": "过程一", "source_event_ids": [assistant]}],
        "user_messages": [{"text": "用户原文", "source_event_ids": [user]}],
        "next_steps": [{"text": "下一步一", "source_event_ids": [write_call]}],
        "read_files": [{"path": "src/a.py", "description": "已读", "source_event_ids": [user]}],
        "modified_files": [{"path": "src/a.py", "description": "已改", "source_event_ids": [write_call]}],
    })
}

struct FakeCompactor {
    structured: Value,
}

impl SummaryCompactorPort for FakeCompactor {
    fn compact(
        &self,
        _batch: &CompactionBatch,
        _target_summary_tokens: i64,
        _validation_feedback: &[String],
    ) -> Result<ModelSummaryResult, SummaryGenerationError> {
        Ok(ModelSummaryResult {
            structured: self.structured.clone(),
            usage: TokenUsageSample::new(100, 20, 0).expect("用量非负"),
            profile: "fake".to_string(),
            provider: "kernel".to_string(),
            attempts: 1,
        })
    }
}

fn seed_session(store: &SessionStore) -> (String, Vec<String>) {
    let created = store
        .start_session("/workspace", "标题", utc_now())
        .expect("建会话");
    let events: Vec<(&str, Value)> = vec![
        ("user_message", json!({"content": "用户原文"})),
        ("assistant_message", json!({"content": "回答"})),
        (
            "tool_call_requested",
            json!({"tool": "write_file", "tool_call_id": "c1", "arguments": {"path": "src/a.py"}}),
        ),
        (
            "tool_result",
            json!({"tool": "write_file", "tool_call_id": "c1", "ok": true, "output": "done"}),
        ),
        (
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c2", "arguments": {"command": "ls"}}),
        ),
        (
            "tool_result",
            json!({"tool": "bash", "tool_call_id": "c2", "ok": false, "output": "boom"}),
        ),
    ];
    let mut ids: Vec<String> = Vec::new();
    for (event_type, payload) in events {
        let payload = payload.as_object().cloned().expect("载荷是对象");
        let event = store
            .append_event(&created.session_id, event_type, payload, None, utc_now())
            .expect("追加事件");
        ids.push(event.event_id);
    }
    (created.session_id, ids)
}

#[test]
fn after_turn_compacts_archives_and_rebuilds_history() {
    let root = temp_root("after-turn");
    let store = Arc::new(SessionStore::open(&root));
    store.ensure().expect("建目录");
    let (session_id, ids) = seed_session(&store);

    let service = ContextCompactionService::new().with_compactor(Box::new(FakeCompactor {
        structured: structured_summary(&ids),
    }));
    let config = CompactionConfig {
        trigger_context_tokens: 1,
        ..CompactionConfig::default()
    };
    let driver = CompactionDriver::new(Arc::clone(&store), service, config);

    let history_messages = vec![json!({"role": "user", "content": "用户原文"})];
    let boundary = TurnBoundary {
        session_id: &session_id,
        system_prompt: "系统提示词",
        context_messages: &[json!({"role": "system", "content": "上下文块"})],
        history_messages: &history_messages,
        tool_schemas: &[json!({"type": "function", "function": {"name": "bash"}})],
        usage: TokenUsageSample::new(100, 20, 50).expect("用量非负"),
        last_request_input_tokens: 0,
        workspace_root: "/workspace",
        task_hint: "整理仓库",
    };
    let report = driver.after_turn(&boundary).expect("压缩不抛错");

    assert!(report.compacted, "达到阈值应当压缩：{}", report.diagnostic);
    assert_eq!(
        report.measurement_payload["auto_decision"].as_str(),
        Some("trigger_reached")
    );
    let history = report.history.expect("压缩后给出历史");
    assert!(history.len() >= 2, "历史含摘要与最终回复：{history:?}");
    assert_eq!(history[0]["role"].as_str(), Some("assistant"));
    assert!(report.notice.is_some(), "给出压缩边界提示");

    let events = store.read_events(&session_id).expect("读事件");
    let types: Vec<&str> = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect();
    assert!(
        types.contains(&"context_compaction_measurement"),
        "测量事件落盘：{types:?}"
    );
    assert_eq!(
        types
            .iter()
            .filter(|item| **item == "compact_summary")
            .count(),
        1,
        "压缩摘要只落一条"
    );

    // 批量选择只取完整回合（user + assistant），工具链事件留在原文里。
    let archived = store.read_compacted_events(&session_id).expect("读归档");
    assert_eq!(archived.len(), 2, "被压缩窗口的原始事件全部归档");
    assert!(archived.iter().all(|event| event["event_id"]
        .as_str()
        .map(|id| ids.contains(&id.to_string()))
        .unwrap_or(false)));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn below_threshold_only_records_measurement() {
    let root = temp_root("below");
    let store = Arc::new(SessionStore::open(&root));
    store.ensure().expect("建目录");
    let (session_id, _ids) = seed_session(&store);

    let driver = CompactionDriver::new(
        Arc::clone(&store),
        ContextCompactionService::new(),
        CompactionConfig::default(),
    );
    let history_messages = vec![json!({"role": "user", "content": "短"})];
    let boundary = TurnBoundary {
        session_id: &session_id,
        system_prompt: "系统提示词",
        context_messages: &[],
        history_messages: &history_messages,
        tool_schemas: &[],
        usage: TokenUsageSample::new(10, 2, 0).expect("用量非负"),
        last_request_input_tokens: 0,
        workspace_root: "/workspace",
        task_hint: "短任务",
    };
    let report = driver.after_turn(&boundary).expect("测量不抛错");

    assert!(!report.compacted, "未达阈值不压缩");
    assert!(report.history.is_none());
    assert_eq!(
        report.measurement_payload["auto_decision"].as_str(),
        Some("below_trigger_threshold")
    );
    let events = store.read_events(&session_id).expect("读事件");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "context_compaction_measurement")
            .count(),
        1,
        "未触发也要写测量事件"
    );

    std::fs::remove_dir_all(&root).ok();
}
