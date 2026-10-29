//! 压缩进度回调：真正压缩的路径必须在摘要请求之前报一次「即将开始」。
//!
//! 用户报的场景：阈值触发的压缩期间状态行什么也不显示，界面上看不出正在压缩。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use omnicrawl_compaction::{CompactionConfig, CompactionDriver, TurnBoundary};
use omnicrawl_controllers::compression::{
    DualCompactionInput, DualCompactionOutput, DualCompactionPort, ToolCompactionInput,
    ToolCompactionOutput, ToolCompactionPort,
};
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
    let path = std::env::temp_dir().join(format!("omnicrawl-compaction-progress-{tag}-{unique}"));
    std::fs::create_dir_all(&path).expect("建临时目录");
    path
}

fn seed_session(store: &SessionStore) -> (String, Vec<String>) {
    let created = store
        .start_session("/workspace", "标题", utc_now())
        .expect("建会话");
    let mut ids: Vec<String> = Vec::new();
    let events = [
        ("user_message", json!({"content": "把仓库整理一下"})),
        ("assistant_message", json!({"content": "我先看看"})),
        (
            "tool_call_requested",
            json!({"tool": "bash", "tool_call_id": "c1", "arguments": {"command": "ls"}}),
        ),
    ];
    for (event_type, payload) in events {
        let event = store
            .append_event(
                &created.session_id,
                event_type,
                payload.as_object().unwrap().clone(),
                None,
                utc_now(),
            )
            .expect("追加事件");
        ids.push(event.event_id);
    }
    (created.session_id, ids)
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
            usage: TokenUsageSample::default(),
            profile: "fake".to_string(),
            provider: String::new(),
            attempts: 1,
        })
    }
}

struct FakeDual;

impl DualCompactionPort for FakeDual {
    fn compact(&self, _input: &DualCompactionInput) -> Result<DualCompactionOutput, String> {
        Ok(DualCompactionOutput {
            text: "压缩后的正文".to_string(),
            archive_path: String::new(),
            raw_chars: 0,
        })
    }
}

struct FakeTools {
    text: String,
    raw_chars: usize,
    summary_chars: usize,
}

impl ToolCompactionPort for FakeTools {
    fn compact(&self, _input: &ToolCompactionInput) -> Result<ToolCompactionOutput, String> {
        Ok(ToolCompactionOutput {
            text: self.text.clone(),
            raw_chars: self.raw_chars,
            summary_chars: self.summary_chars,
        })
    }
}

fn structured_summary(ids: &[String]) -> Value {
    let user = ids[0].clone();
    let assistant = ids[1].clone();
    let call = ids[2].clone();
    json!({
        "objective": ["目标一"],
        "current_state": ["状态一"],
        "constraints": [{"text": "约束一", "source_event_ids": [user]}],
        "decisions": [{"text": "决策一", "source_event_ids": [assistant]}],
        "completed": [{"text": "完成一", "source_event_ids": [user]}],
        "open_issues": [{"text": "问题一", "source_event_ids": [assistant]}],
        "artifacts": [{"text": "产物一", "source_event_ids": [call]}],
        "exact_evidence": [{"text": "把仓库整理一下", "source_event_ids": [user]}],
        "failed_attempts": [{"text": "失败一", "source_event_ids": [call]}],
        "excluded_approaches": [{"text": "排除一", "source_event_ids": [assistant]}],
        "key_concepts": [{"text": "概念一", "source_event_ids": [assistant]}],
        "problem_solving_process": [{"text": "过程一", "source_event_ids": [assistant]}],
        "user_messages": [{"text": "把仓库整理一下", "source_event_ids": [user]}],
        "next_steps": [{"text": "下一步一", "source_event_ids": [call]}],
    })
}

fn boundary<'a>(session_id: &'a str, history: &'a [Value]) -> TurnBoundary<'a> {
    TurnBoundary {
        session_id,
        system_prompt: "系统提示词",
        context_messages: &[],
        history_messages: history,
        tool_schemas: &[],
        usage: TokenUsageSample::new(100, 20, 50).expect("用量非负"),
        last_request_input_tokens: 0,
        workspace_root: "/workspace",
        task_hint: "整理仓库",
    }
}

/// 结构化摘要路径：服务在请求摘要模型之前触发一次。
#[test]
fn structured_compaction_reports_progress_before_the_request() {
    let root = temp_root("structured");
    let store = Arc::new(SessionStore::open(&root));
    store.ensure().expect("建目录");
    let (session_id, ids) = seed_session(&store);

    let calls = Rc::new(Cell::new(0usize));
    let counter = Rc::clone(&calls);
    let service = ContextCompactionService::new()
        .with_compactor(Box::new(FakeCompactor {
            structured: structured_summary(&ids),
        }))
        .with_progress(Rc::new(move || counter.set(counter.get() + 1)));

    let driver = CompactionDriver::new(
        Arc::clone(&store),
        service,
        CompactionConfig {
            trigger_context_tokens: 1,
            ..CompactionConfig::default()
        },
    );
    let history = vec![json!({"role": "user", "content": "用户原文"})];
    let report = driver
        .after_turn(&boundary(&session_id, &history))
        .expect("压缩不抛错");

    assert!(report.compacted, "达到阈值应当压缩：{}", report.diagnostic);
    assert_eq!(calls.get(), 1, "压缩时应当报一次进度");

    std::fs::remove_dir_all(&root).ok();
}

/// 未触发压缩的回合不能报进度：状态行不该在什么都没干的时候写「压缩中」。
#[test]
fn below_threshold_reports_no_progress() {
    let root = temp_root("below");
    let store = Arc::new(SessionStore::open(&root));
    store.ensure().expect("建目录");
    let (session_id, ids) = seed_session(&store);

    let calls = Rc::new(Cell::new(0usize));
    let counter = Rc::clone(&calls);
    let service = ContextCompactionService::new()
        .with_compactor(Box::new(FakeCompactor {
            structured: structured_summary(&ids),
        }))
        .with_progress(Rc::new(move || counter.set(counter.get() + 1)));

    let driver = CompactionDriver::new(
        Arc::clone(&store),
        service,
        CompactionConfig {
            trigger_context_tokens: 1_000_000,
            ..CompactionConfig::default()
        },
    );
    let history = vec![json!({"role": "user", "content": "短"})];
    let report = driver
        .after_turn(&boundary(&session_id, &history))
        .expect("测量不抛错");

    assert!(!report.compacted, "未达阈值不压缩");
    assert_eq!(calls.get(), 0, "没压缩就不该报进度");

    std::fs::remove_dir_all(&root).ok();
}

/// 双段压缩路径绕过服务层，必须由驱动自己在两段请求发出前报进度。
#[test]
fn dual_compaction_reports_progress_before_the_requests() {
    let root = temp_root("dual");
    let store = Arc::new(SessionStore::open(&root));
    store.ensure().expect("建目录");
    let (session_id, _ids) = seed_session(&store);

    let calls = Rc::new(Cell::new(0usize));
    let counter = Rc::clone(&calls);
    let driver = CompactionDriver::new(
        Arc::clone(&store),
        ContextCompactionService::new(),
        CompactionConfig {
            trigger_context_tokens: 1,
            ..CompactionConfig::default()
        },
    )
    .with_dual_compaction(Box::new(FakeDual))
    .with_progress(Rc::new(move || counter.set(counter.get() + 1)));

    let history = vec![json!({"role": "user", "content": "用户原文"})];
    let report = driver
        .after_turn(&boundary(&session_id, &history))
        .expect("双段压缩不抛错");

    assert!(report.compacted, "达到阈值应当压缩：{}", report.diagnostic);
    assert_eq!(calls.get(), 1, "双段路径也应当报一次进度");

    std::fs::remove_dir_all(&root).ok();
}

/// 工具段压缩路径同样要在请求发出前报进度，且两段各自出计量行。
#[test]
fn tool_segment_reports_progress_and_its_own_notice() {
    let root = temp_root("tools");
    let store = Arc::new(SessionStore::open(&root));
    store.ensure().expect("建目录");
    let (session_id, ids) = seed_session(&store);

    let calls = Rc::new(Cell::new(0usize));
    let counter = Rc::clone(&calls);
    let service = ContextCompactionService::new().with_compactor(Box::new(FakeCompactor {
        structured: structured_summary(&ids),
    }));
    let driver = CompactionDriver::new(
        Arc::clone(&store),
        service,
        CompactionConfig {
            trigger_context_tokens: 1,
            ..CompactionConfig::default()
        },
    )
    .with_tool_compaction(Box::new(FakeTools {
        text: "工具段概括".to_string(),
        raw_chars: 40_000,
        summary_chars: 400,
    }))
    .with_progress(Rc::new(move || counter.set(counter.get() + 1)));

    let history = vec![json!({"role": "user", "content": "用户原文"})];
    let report = driver
        .after_turn(&boundary(&session_id, &history))
        .expect("压缩不抛错");

    assert!(report.compacted, "达到阈值应当压缩：{}", report.diagnostic);
    // 本用例只把进度回调接在驱动上：工具段那条由驱动报，摘要那条由服务报（未接）。
    assert_eq!(calls.get(), 1, "工具段请求发出前报一次进度");
    assert_eq!(
        report.notices.first().map(String::as_str),
        Some("已压缩：9,900 Token"),
        "工具段在前：{:?}",
        report.notices
    );
    assert!(
        report
            .notices
            .iter()
            .all(|notice| !notice.contains("：0 Token")),
        "没有净省的段不出计量行：{:?}",
        report.notices
    );
    // 两段拼成一条 assistant 消息：工具段在前、摘要段在后。
    let history = report.history.expect("压缩后给出历史");
    let first = history[0]["content"].as_str().unwrap_or_default();
    assert!(first.contains("## 工具调用压缩"), "工具段在前：{first}");
    assert!(first.contains("## 结构化工作摘要"), "摘要段在后：{first}");

    std::fs::remove_dir_all(&root).ok();
}

/// 没压小（净省为 0）时工具段既不出计量行、也不拼进历史：压缩不该让上下文变大。
#[test]
fn tool_segment_without_savings_is_dropped_entirely() {
    let root = temp_root("tools-zero");
    let store = Arc::new(SessionStore::open(&root));
    store.ensure().expect("建目录");
    let (session_id, ids) = seed_session(&store);

    let service = ContextCompactionService::new().with_compactor(Box::new(FakeCompactor {
        structured: structured_summary(&ids),
    }));
    let driver = CompactionDriver::new(
        Arc::clone(&store),
        service,
        CompactionConfig {
            trigger_context_tokens: 1,
            ..CompactionConfig::default()
        },
    )
    .with_tool_compaction(Box::new(FakeTools {
        text: "没压小的工具段".to_string(),
        raw_chars: 400,
        summary_chars: 4_000,
    }));

    let history = vec![json!({"role": "user", "content": "用户原文"})];
    let report = driver
        .after_turn(&boundary(&session_id, &history))
        .expect("压缩不抛错");

    assert!(report.compacted, "摘要段成功时仍压缩");
    assert!(
        report
            .notices
            .iter()
            .all(|notice| !notice.contains("：0 Token")),
        "不该出现「0 Token」：{:?}",
        report.notices
    );
    let history = report.history.expect("压缩后给出历史");
    let first = history[0]["content"].as_str().unwrap_or_default();
    assert!(
        !first.contains("## 工具调用压缩"),
        "没压小的工具段不拼进历史：{first}"
    );

    std::fs::remove_dir_all(&root).ok();
}
