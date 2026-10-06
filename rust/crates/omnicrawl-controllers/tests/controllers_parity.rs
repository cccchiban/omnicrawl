//! `omnicrawl/agent/controllers/` 的跨语言对照（parity）。
//!
//! 数据集是冻结的对照契约：改动任一侧后先跑
//! `python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json`，再跑本套件用同一份输入重放并逐字段比对。
//!
//! 模板装载一组需要读仓库内的 `rust/assets/templates/`：那一组对照的是「读到的内容」，
//! 因此测试直接按照仓库布局定位模板目录。

use omnicrawl_controllers::{building, compression, memory, output, shared, undo, workspace};
use omnicrawl_controllers::{AgentError, ToolImageAttachment, ToolResult};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn norm(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 数据集里的临时目录前缀是在 Windows 上生成的：重放前整体换成本机等价路径，
/// 否则同一份用例在别的平台上会落进「相对路径」分支，与期望值不符。
const FIXTURE_TMP_PREFIX: &str = "C:/Users/Administrator/AppData/Local/Temp/tmp2v6l_cwx";

fn localize(value: &str) -> String {
    value.replace(
        FIXTURE_TMP_PREFIX,
        &norm(&std::env::temp_dir().join("omnicrawl-parity")),
    )
}

/// 把一条用例里的所有字符串都本地化后再重放：输入与期望值同步替换，语义不变。
fn localized_case(case: &Value) -> Value {
    match case {
        Value::String(text) => Value::String(localize(text)),
        Value::Array(items) => Value::Array(items.iter().map(localized_case).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), localized_case(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().expect("对象参数")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

fn indices(value: &Value) -> Vec<usize> {
    value
        .as_array()
        .expect("下标数组")
        .iter()
        .map(|item| item.as_u64().expect("下标") as usize)
        .collect()
}

fn assert_result(actual: &ToolResult, expected: &Value, label: &str) {
    assert_eq!(
        actual.ok,
        expected["ok"].as_bool().expect("ok"),
        "成功标记（{label}）"
    );
    assert_eq!(
        actual.output.chars().count() as u64,
        expected["output_len"].as_u64().expect("output_len"),
        "输出长度（{label}）"
    );
    assert_eq!(
        sha256_hex(&actual.output),
        expected["output_sha256"].as_str().expect("output_sha256"),
        "输出摘要（{label}）"
    );
    assert_eq!(
        actual.full_output.chars().count() as u64,
        expected["full_output_len"]
            .as_u64()
            .expect("full_output_len"),
        "展示文本长度（{label}）"
    );
    assert_eq!(
        sha256_hex(&actual.full_output),
        expected["full_output_sha256"]
            .as_str()
            .expect("full_output_sha256"),
        "展示文本摘要（{label}）"
    );
    if let Some(text) = expected["output"].as_str() {
        if text.chars().count() <= 120 {
            assert_eq!(actual.output, text, "输出原文（{label}）");
        }
    }
    if let Some(text) = expected["full_output"].as_str() {
        if text.chars().count() <= 120 {
            assert_eq!(actual.full_output, text, "展示原文（{label}）");
        }
    }
    assert_eq!(
        actual.model_images.len() as u64,
        expected["model_images"].as_u64().expect("model_images"),
        "图片数（{label}）"
    );
    assert_eq!(
        actual.error_code,
        expected["error_code"].as_str().map(str::to_string),
        "错误码（{label}）"
    );
    assert_eq!(
        actual.completed_at.is_none(),
        expected["completed_at_is_none"]
            .as_bool()
            .expect("completed_at_is_none"),
        "完成时刻（{label}）"
    );
}

/// 断言「按数据集该失败就失败」，失败时核对文案（可按前缀核对平台相关尾巴）。
fn expect_outcome<T>(result: Result<T, AgentError>, case: &Value, label: &str) -> Option<T> {
    let expected_ok = case["ok"].as_bool().expect("ok");
    match result {
        Ok(value) => {
            assert!(expected_ok, "本该失败却成功（{label}）");
            Some(value)
        }
        Err(error) => {
            assert!(
                !expected_ok,
                "本该成功却失败：{}（{label}）",
                error.message()
            );
            match case["compare"].as_str() {
                Some("prefix") => assert!(
                    error
                        .message()
                        .starts_with(case["error_prefix"].as_str().expect("error_prefix")),
                    "错误前缀（{label}）：{}",
                    error.message()
                ),
                _ => assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                ),
            }
            None
        }
    }
}

fn template_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../rust/assets/templates")
}

// --------------------------------------------------------------------------- shared

#[test]
fn shared_constants_match_python() {
    let data = fixture();
    let expected = &data["shared"]["constants"];
    assert_eq!(
        shared::DEFAULT_TOOL_TIMEOUT_SECONDS,
        expected["default_tool_timeout_seconds"]
            .as_i64()
            .expect("int")
    );
    assert_eq!(
        shared::MAX_TOOL_TIMEOUT_SECONDS,
        expected["max_tool_timeout_seconds"].as_i64().expect("int")
    );
    assert_eq!(
        shared::TOOL_OUTPUT_INLINE_LIMIT_CHARS,
        expected["tool_output_inline_limit_chars"]
            .as_u64()
            .expect("int") as usize
    );
    assert_eq!(
        shared::TOOL_OUTPUT_BATCH_BUDGET_CHARS,
        expected["tool_output_batch_budget_chars"]
            .as_u64()
            .expect("int") as usize
    );
    assert_eq!(
        shared::TOOL_OUTPUT_ARCHIVED_PREVIEW_CHARS,
        expected["tool_output_archived_preview_chars"]
            .as_u64()
            .expect("int") as usize
    );
    assert_eq!(
        shared::SUBAGENT_LIFECYCLE_WAIT_SECONDS,
        expected["subagent_lifecycle_wait_seconds"]
            .as_f64()
            .expect("float")
    );
    assert_eq!(
        shared::SYSTEM_PROMPT_FILE,
        expected["system_prompt_file"].as_str().expect("text")
    );
    assert_eq!(
        shared::AGENTS_INSTRUCTIONS_FILE,
        expected["agents_instructions_file"].as_str().expect("text")
    );
    assert_eq!(
        shared::CONTEXT_OVERFLOW_RECOVERY_PROMPT,
        expected["context_overflow_recovery_prompt"]
            .as_str()
            .expect("text")
    );
    assert_eq!(
        shared::ASK_USER_ADVISOR_HINT,
        expected["ask_user_advisor_hint"].as_str().expect("text")
    );
    assert_eq!(
        shared::CONTEXT_OVERFLOW_ERROR_MARKERS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        strings(&expected["context_overflow_error_markers"])
    );
    assert_eq!(
        shared::RATE_LIMIT_ERROR_MARKERS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        strings(&expected["rate_limit_error_markers"])
    );
    let mut continue_texts = shared::CONTINUE_LAST_TASK_TEXTS.to_vec();
    continue_texts.sort_unstable();
    assert_eq!(
        continue_texts,
        strings(&expected["continue_last_task_texts"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    let mut read_only = shared::READ_ONLY_UNDO_TOOLS.to_vec();
    read_only.sort_unstable();
    assert_eq!(
        read_only,
        strings(&expected["read_only_undo_tools"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    let mut reversible = shared::REVERSIBLE_UNDO_TOOLS.to_vec();
    reversible.sort_unstable();
    assert_eq!(
        reversible,
        strings(&expected["reversible_undo_tools"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    let mut exempt = shared::MEMORY_UNDO_EXEMPT_TOOLS.to_vec();
    exempt.sort_unstable();
    assert_eq!(
        exempt,
        strings(&expected["memory_undo_exempt_tools"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
}

#[test]
fn shared_int_env_matches_python() {
    let data = fixture();
    for case in data["shared"]["int_env"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let result = shared::read_int_env(
            "OC_PROBE_INT",
            case["raw"].as_str(),
            case["default"].as_i64().expect("default"),
            case["min"].as_i64().expect("min"),
            case["max"].as_i64().expect("max"),
        );
        if let Some(value) = expect_outcome(result, case, label) {
            assert_eq!(
                value,
                case["value"].as_i64().expect("value"),
                "取值（{label}）"
            );
        }
    }
}

#[test]
fn shared_int_range_matches_python() {
    let data = fixture();
    for case in data["shared"]["int_range"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let result = shared::validate_int_range(
            "tool_timeout",
            case["value_in"].as_i64().expect("value_in"),
            case["min"].as_i64().expect("min"),
            case["max"].as_i64().expect("max"),
        );
        if let Some(value) = expect_outcome(result, case, label) {
            assert_eq!(
                value,
                case["value_in"].as_i64().expect("value_in"),
                "取值（{label}）"
            );
        }
    }
}

#[test]
fn shared_unknown_tool_result_matches_python() {
    let data = fixture();
    for case in data["shared"]["unknown_tool"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let active = strings(&case["active_tools"]);
        let active_refs: Vec<&str> = active.iter().map(String::as_str).collect();
        let result = shared::unknown_tool_result(
            case["requested_name"].as_str().expect("requested_name"),
            &active_refs,
        );
        assert_result(&result, &case["result"], label);
    }
}

#[test]
fn shared_tool_timeout_result_matches_python() {
    let data = fixture();
    for case in data["shared"]["timeout_result"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let result = shared::tool_timeout_result(
            case["seconds"].as_i64().expect("seconds"),
            case["hint"].as_str().expect("hint"),
        );
        assert_result(&result, &case["result"], label);
    }
}

#[test]
fn shared_execute_with_timeout_matches_python() {
    let data = fixture();
    for case in data["shared"]["execute_with_timeout"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let index = case["index"].as_u64().expect("index") as usize;
        let timeout = case["timeout_seconds"].as_u64().expect("timeout");
        let expected = &case["result"];
        let result = if expected["ok"].as_bool().expect("ok") {
            shared::execute_call_with_timeout(
                move |slot| ToolResult {
                    ok: true,
                    output: format!("完成 {slot}"),
                    ..ToolResult::default()
                },
                index,
                timeout,
            )
        } else {
            shared::execute_call_with_timeout(
                move |_slot| {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    ToolResult {
                        ok: true,
                        output: "太晚".to_string(),
                        ..ToolResult::default()
                    }
                },
                index,
                timeout,
            )
        };
        assert_result(&result, expected, label);
    }
}

// ----------------------------------------------------------------------------- undo

#[derive(Default)]
struct CountingStore {
    captures: std::cell::Cell<usize>,
}

impl undo::SnapshotStore for CountingStore {
    fn has_head(&self, _workspace: &Path) -> bool {
        true
    }

    fn capture(&self, _workspace: &Path) -> Result<undo::WorktreeSnapshot, undo::SnapshotError> {
        self.captures.set(self.captures.get() + 1);
        Ok(undo::WorktreeSnapshot::default())
    }

    fn transition(
        &self,
        _workspace: &Path,
        _expected: &undo::WorktreeSnapshot,
        _target: &undo::WorktreeSnapshot,
    ) -> Result<Vec<String>, undo::SnapshotError> {
        Ok(Vec::new())
    }
}

#[test]
fn undo_tool_safety_matches_python() {
    let data = fixture();
    for case in data["undo"]["tool_is_undo_safe"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let actual = undo::tool_is_undo_safe(
            case["name"].as_str().expect("name"),
            &object(&case["arguments"]),
        );
        assert_eq!(
            actual,
            case["expected"]["value"].as_bool().expect("value"),
            "undo 安全性（{label}）"
        );
    }
}

#[test]
fn undo_ledger_matches_python() {
    let data = fixture();
    for case in data["undo"]["ledger"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let mut snapshot = undo::ActiveTurnSnapshot {
            snapshot_id: "snap-1".to_string(),
            workspace: Some(PathBuf::from("/ws")),
            ..undo::ActiveTurnSnapshot::default()
        };
        let store = CountingStore::default();
        let mut captures_requested = 0;
        for step in case["sequence"].as_array().expect("sequence") {
            let need = undo::record_tool_execution(
                &mut snapshot,
                step["name"].as_str().expect("name"),
                &object(&step["arguments"]),
            );
            if need == undo::TurnCaptureNeed::Needed {
                captures_requested += 1;
                undo::ensure_turn_captured(&mut snapshot, &store);
            }
        }
        assert_eq!(
            snapshot.executed_tools,
            strings(&case["executed_tools"]),
            "执行账本（{label}）"
        );
        assert_eq!(
            snapshot.irreversible_tools,
            strings(&case["irreversible_tools"]),
            "不可逆账本（{label}）"
        );
        assert_eq!(
            captures_requested,
            case["captures"].as_u64().expect("captures") as usize,
            "补捕获次数（{label}）"
        );
        assert_eq!(
            store.captures.get(),
            if case["capture_attempted"]
                .as_bool()
                .expect("capture_attempted")
            {
                1
            } else {
                0
            },
            "实际捕获次数（{label}）"
        );
    }
}

#[test]
fn undo_restore_precheck_matches_python() {
    let data = fixture();
    for case in data["undo"]["restore_precheck"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let events: Vec<undo::UndoEvent> = case["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|event| undo::UndoEvent {
                event_type: event["type"].as_str().expect("type").to_string(),
                payload: event["payload"].clone(),
            })
            .collect();
        let workspace = PathBuf::from(case["workspace"].as_str().expect("workspace"));
        let result = undo::precheck_restore(&events, &workspace);
        if let Some(decision) = expect_outcome(result, case, label) {
            assert_eq!(
                case["decision"].as_str().expect("decision"),
                "none",
                "预检结论（{label}）"
            );
            assert!(matches!(decision, undo::RestorePrecheck::NoSnapshot));
        }
    }
}

#[test]
#[cfg(windows)]
// 数据集里的期望值一律是 Windows 形态（路径分隔符、盘符与平台常量），
// 被测实现也按 win32 分支做字符串化：POSIX 主机上必然形态不符。
// 这条对照只在 Windows 主机上有意义。
fn undo_artifact_path_matches_python() {
    let data = fixture();
    for case in data["undo"]["artifact_path"].as_array().expect("cases") {
        let case = localized_case(case);
        let label = case["label"].as_str().expect("label");
        let root = PathBuf::from(case["root"].as_str().expect("root"));
        let result =
            undo::resolve_artifact_path(&root, case["relative"].as_str().expect("relative"))
                .map_err(AgentError::from);
        if let Some(path) = expect_outcome(result, &case, label) {
            assert_eq!(
                norm(&path),
                case["value"].as_str().expect("value"),
                "解析路径（{label}）"
            );
        }
    }
}

#[test]
fn undo_is_relative_to_matches_python() {
    let data = fixture();
    for case in data["undo"]["is_relative_to"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let actual = undo::is_relative_to(
            Path::new(case["path"].as_str().expect("path")),
            Path::new(case["parent"].as_str().expect("parent")),
        );
        assert_eq!(
            actual,
            case["expected"].as_bool().expect("expected"),
            "{label}"
        );
    }
}

// ------------------------------------------------------------------------ workspace

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let index = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "oc-controllers-{tag}-{}-{index}",
        std::process::id()
    ))
}

#[test]
fn workspace_switch_matches_python() {
    let data = fixture();
    for case in data["workspace"]["switch_workspace"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let worktrees = case["worktrees"].as_array().expect("worktrees");
        if case["draining"].as_bool().expect("draining") {
            let error = workspace::subagent_drain_error();
            assert!(!case["ok"].as_bool().expect("ok"), "应当失败（{label}）");
            assert_eq!(
                error.message(),
                case["error"].as_str().expect("error"),
                "{label}"
            );
            continue;
        }
        if !worktrees.is_empty() {
            let items: Vec<workspace::WorktreeRef> = worktrees
                .iter()
                .map(|item| workspace::WorktreeRef {
                    branch: item["branch"].as_str().unwrap_or_default().to_string(),
                    task_id: item["task_id"].as_str().unwrap_or_default().to_string(),
                })
                .collect();
            let error = workspace::pending_worktrees_error(&items);
            assert_eq!(
                error.message(),
                case["error"].as_str().expect("error"),
                "{label}"
            );
            continue;
        }
        // 数据集里的临时路径在生成后即被删除，与文件系统状态有关的分支由测试自造等价目标。
        let expected_error = case["error"].as_str().unwrap_or_default();
        if expected_error.contains("不是目录") {
            let file = temp_path("switch-file");
            std::fs::write(&file, "x").expect("建文件");
            let error = workspace::resolve_switch_target(&file.to_string_lossy())
                .expect_err("目标不是目录应当失败");
            assert!(
                error.message().starts_with("工作区切换失败："),
                "错误前缀（{label}）：{}",
                error.message()
            );
            assert!(
                error
                    .message()
                    .ends_with(case["error_suffix"].as_str().expect("error_suffix")),
                "错误后缀（{label}）：{}",
                error.message()
            );
            let _ = std::fs::remove_file(&file);
            continue;
        }
        if case["ok"].as_bool().expect("ok") {
            let dir = temp_path("switch-dir");
            std::fs::create_dir_all(&dir).expect("建目录");
            let actual =
                workspace::resolve_switch_target(&dir.to_string_lossy()).expect("应当成功");
            assert_eq!(norm(&actual), norm(&dir), "解析结果（{label}）");
            let _ = std::fs::remove_dir_all(&dir);
            continue;
        }
        let missing = temp_path("switch-missing");
        let error = workspace::resolve_switch_target(&missing.to_string_lossy())
            .expect_err("路径不存在应当失败");
        assert!(
            error
                .message()
                .starts_with(case["error_prefix"].as_str().expect("error_prefix")),
            "错误前缀（{label}）：{}",
            error.message()
        );
        assert!(
            error
                .message()
                .contains(case["error_suffix"].as_str().expect("error_suffix")),
            "错误后缀（{label}）：{}",
            error.message()
        );
    }
}

#[test]
fn workspace_protection_message_matches_python() {
    let data = fixture();
    for case in data["workspace"]["protect_message"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = workspace::workspace_extra_protection_message(
            case["is_memory"].as_bool().expect("is_memory"),
            case["is_session"].as_bool().expect("is_session"),
            case["relative"].as_str().expect("relative"),
        );
        assert_eq!(
            actual,
            case["expected"].as_str().map(str::to_string),
            "保护提示（{label}）"
        );
    }
}

#[test]
fn workspace_path_membership_matches_python() {
    let data = fixture();
    for case in data["workspace"]["path_membership"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = undo::is_relative_to(
            &undo::resolve_path(Path::new(case["path"].as_str().expect("path"))),
            &undo::resolve_path(Path::new(case["root"].as_str().expect("root"))),
        );
        assert_eq!(
            actual,
            case["expected"].as_bool().expect("expected"),
            "{label}"
        );
    }
}

// --------------------------------------------------------------------------- memory

struct FakeExpiredStore {
    expired: Vec<PathBuf>,
}

impl memory::ExpiredMemoryStore for FakeExpiredStore {
    fn clean_expired_memories(&self) -> Result<Vec<PathBuf>, memory::MemoryStoreError> {
        Ok(self.expired.clone())
    }
}

#[test]
#[cfg(windows)]
// 数据集里的期望值一律是 Windows 形态（路径分隔符、盘符与平台常量），
// 被测实现也按 win32 分支做字符串化：POSIX 主机上必然形态不符。
// 这条对照只在 Windows 主机上有意义。
fn memory_project_root_matches_python() {
    let data = fixture();
    for case in data["memory"]["project_root"].as_array().expect("cases") {
        let case = localized_case(case);
        let label = case["label"].as_str().expect("label");
        let workspace = PathBuf::from(case["workspace"].as_str().expect("workspace"));
        let result =
            memory::project_memory_root(&workspace, case["directory"].as_str().expect("directory"));
        if let Some(root) = expect_outcome(result, &case, label) {
            assert_eq!(
                norm(&root),
                case["root"].as_str().expect("root"),
                "项目级记忆根（{label}）"
            );
        }
    }
}

#[test]
fn memory_session_store_matches_python() {
    let data = fixture();
    for case in data["memory"]["session_store"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        if !case["session_state"].as_bool().expect("session_state") {
            assert!(
                case["root"].is_null(),
                "会话未启用时不该给出记忆目录（{label}）"
            );
            continue;
        }
        let result = memory::session_memory_root(
            Path::new(case["user_root"].as_str().expect("user_root")),
            case["session_id"].as_str().expect("session_id"),
        );
        if let Some(root) = expect_outcome(result, case, label) {
            assert_eq!(
                norm(&root),
                case["root"].as_str().expect("root"),
                "会话级记忆根（{label}）"
            );
        }
    }
}

#[test]
fn memory_delete_session_matches_python() {
    let data = fixture();
    for case in data["memory"]["delete_session"]
        .as_array()
        .expect("cases")
        .iter()
    {
        let label = case["label"].as_str().expect("label");
        let session_id = case["session_id"].as_str().expect("session_id");
        let root = temp_path("delete");
        let _ = std::fs::remove_dir_all(&root);
        let target = root.join(memory::SESSION_MEMORY_DIRECTORY).join(session_id);
        if case["existed_before"].as_bool().expect("existed_before") {
            std::fs::create_dir_all(&target).expect("建目录");
        }
        let result = memory::delete_session_memory(
            &root,
            case["memory_enabled"].as_bool().expect("memory_enabled"),
            session_id,
        );
        expect_outcome(result, case, label);
        assert_eq!(
            target.is_dir(),
            case["exists_after"].as_bool().expect("exists_after"),
            "目录是否仍在（{label}）"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn memory_clean_memory_matches_python() {
    let data = fixture();
    for case in data["memory"]["clean_memory"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        if !case["stores"].as_bool().expect("stores") {
            let result =
                memory::clean_memory(&[("project", None), ("session", None), ("user", None)]);
            let error = result.expect_err("三类都没绑定应当失败");
            assert_eq!(
                error.message(),
                case["error"].as_str().expect("error"),
                "{label}"
            );
            continue;
        }
        let expected = strings(&case["expected"]);
        let mut by_scope: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for entry in &expected {
            let (scope, path) = entry.split_once(':').expect("scope:path");
            by_scope
                .entry(scope.to_string())
                .or_default()
                .push(PathBuf::from(path));
        }
        let stores: Vec<FakeExpiredStore> = ["project", "session", "user"]
            .iter()
            .map(|scope| FakeExpiredStore {
                expired: by_scope.remove(*scope).unwrap_or_default(),
            })
            .collect();
        let refs: Vec<(&str, Option<&dyn memory::ExpiredMemoryStore>)> = vec![
            ("project", Some(&stores[0])),
            ("session", Some(&stores[1])),
            ("user", Some(&stores[2])),
        ];
        let actual = memory::clean_memory(&refs).expect("清理应当成功");
        assert_eq!(actual, expected, "清理条目（{label}）");
    }
}

#[test]
fn memory_user_memory_root_matches_python() {
    let data = fixture();
    let expected = data["memory"]["user_memory_root"].as_str().expect("root");
    let user_root = Path::new(expected)
        .parent()
        .expect("user root")
        .to_path_buf();
    assert_eq!(norm(&memory::user_memory_root(&user_root)), expected);
}

// --------------------------------------------------------------------------- output

#[test]
fn output_batch_budget_matches_python() {
    let data = fixture();
    for case in data["output"]["batch_budget"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let sizes: Vec<Option<usize>> = case["sizes"]
            .as_array()
            .expect("sizes")
            .iter()
            .map(|size| size.as_u64().map(|value| value as usize))
            .collect();
        assert_eq!(
            output::plan_batch_archive(&sizes),
            indices(&case["archived_indices"]),
            "落盘下标（{label}）"
        );
        let mut results: Vec<ToolResult> = sizes
            .iter()
            .map(|size| ToolResult {
                ok: true,
                output: "x".repeat(size.unwrap_or(0)),
                ..ToolResult::default()
            })
            .collect();
        let mut paths: HashMap<usize, String> = HashMap::new();
        for (key, value) in case["archive_paths"].as_object().expect("archive_paths") {
            paths.insert(
                key.parse::<usize>().expect("index"),
                value.as_str().unwrap().to_string(),
            );
        }
        output::rewrite_archived_results(&mut results, &indices(&case["archived_indices"]), &paths);
        for (index, expected) in case["value"].as_array().expect("value").iter().enumerate() {
            if expected.is_null() {
                continue;
            }
            assert_result(&results[index], expected, label);
        }
    }
}

#[test]
fn output_archived_preview_matches_python() {
    let data = fixture();
    for case in data["output"]["archived_preview"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let chars = case["chars"].as_u64().expect("chars") as usize;
        let text = match case["output"].as_str() {
            Some(text) => text.to_string(),
            None => case["fill_char"].as_str().expect("fill_char").repeat(chars),
        };
        assert_eq!(text.chars().count(), chars, "语料长度（{label}）");
        let actual =
            output::format_archived_output_preview(&text, case["path"].as_str().expect("path"));
        assert_eq!(
            actual,
            case["expected"].as_str().expect("expected"),
            "{label}"
        );
    }
}

#[test]
fn output_preview_text_matches_python() {
    let data = fixture();
    for case in data["output"]["preview_text"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let actual = output::preview_text(
            case["text"].as_str().expect("text"),
            case["limit"].as_u64().expect("limit") as usize,
        );
        assert_eq!(
            actual,
            case["expected"].as_str().expect("expected"),
            "{label}"
        );
    }
}

#[test]
fn output_tool_result_message_matches_python() {
    let data = fixture();
    for case in data["output"]["tool_result_message"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = output::tool_result_message(
            case["tool"].as_str().expect("tool"),
            case["ok"].as_bool().expect("ok"),
            case["output"].as_str().expect("output"),
            case["tool_call_id"].as_str().expect("tool_call_id"),
        );
        assert_eq!(
            serde_json::to_string(&actual).expect("序列化"),
            serde_json::to_string(&case["expected"]).expect("序列化"),
            "工具结果消息（{label}）"
        );
    }
}

#[test]
fn output_vision_matches_python() {
    let data = fixture();
    let images = vec![ToolImageAttachment {
        media_type: "image/png".to_string(),
        data_base64: "QUJD".to_string(),
        filename: String::new(),
        detail: "high".to_string(),
    }];
    for case in data["output"]["vision"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let expected = &case["result"];
        let expected_output = expected["output"].as_str().expect("output");
        let messages = case["messages"].clone();
        let arguments = object(&case["arguments"]);
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        if case["tool"] == "read_image" && prompt.is_empty() {
            assert_eq!(
                expected_output,
                output::READ_IMAGE_MISSING_PROMPT_ERROR,
                "缺少 prompt 的文案（{label}）"
            );
            assert!(messages.as_array().expect("messages").is_empty());
            continue;
        }
        if expected_output == output::VISION_MISSING_LLM_ERROR {
            assert!(!case["has_llm"].as_bool().expect("has_llm"));
            continue;
        }
        if let Some(cause) = expected_output.strip_prefix("视觉模型分析失败：") {
            assert_eq!(
                output::vision_failure_error(cause),
                expected_output,
                "{label}"
            );
            continue;
        }
        if case["native_vision"].as_bool().expect("native_vision") {
            let actual = output::vision_observation_messages(true, true, &prompt, &images);
            assert_eq!(
                serde_json::to_string(&Value::Array(actual)).expect("序列化"),
                serde_json::to_string(&messages).expect("序列化"),
                "视觉观察消息（{label}）"
            );
            assert_eq!(expected_output, "原始输出", "{label}");
            continue;
        }
        if case["vision_enabled"].as_bool().expect("vision_enabled") {
            let display = output::vision_display_text(expected_output, "vision-x", "图片里有猫。");
            assert_eq!(
                display,
                expected["full_output"].as_str().expect("full_output"),
                "视觉展示文本（{label}）"
            );
            let followup = output::vision_followup_message("vision-x", "图片里有猫。");
            assert_eq!(
                serde_json::to_string(&followup).expect("序列化"),
                serde_json::to_string(&messages.as_array().expect("messages")[0]).expect("序列化"),
                "视觉跟进消息（{label}）"
            );
            continue;
        }
        assert!(
            messages.as_array().expect("messages").is_empty(),
            "未启用视觉时代理不该出现（{label}）"
        );
        assert_eq!(expected_output, "原始输出", "{label}");
    }
}

// ---------------------------------------------------------------------- compression

#[test]
fn compression_should_compact_matches_python() {
    let data = fixture();
    for case in data["compression"]["should_compact"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = compression::should_compact(
            case["name"].as_str().expect("name"),
            case["output"].as_str().expect("output"),
            case["min_chars"].as_u64().expect("min_chars") as usize,
        );
        assert_eq!(
            actual,
            case["expected"].as_bool().expect("expected"),
            "是否压缩（{label}）"
        );
        assert_eq!(
            case["output"].as_str().expect("output").chars().count() as u64,
            case["chars"].as_u64().expect("chars"),
            "语料长度（{label}）"
        );
    }
}

#[test]
fn compression_arguments_summary_matches_python() {
    let data = fixture();
    for case in data["compression"]["arguments_summary"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = compression::arguments_summary(&case["arguments"]);
        if let Some(expected) = case["expected"].as_str() {
            if expected.chars().count() <= 200 {
                assert_eq!(actual, expected, "参数摘要（{label}）");
            }
            assert_eq!(
                actual.chars().count(),
                expected.chars().count(),
                "摘要长度（{label}）"
            );
            assert_eq!(
                sha256_hex(&actual),
                sha256_hex(expected),
                "摘要内容（{label}）"
            );
        }
    }
}

#[test]
fn compression_compacted_display_matches_python() {
    let data = fixture();
    for case in data["compression"]["compacted_display"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = compression::compacted_display(
            case["compressed"].as_str().expect("compressed"),
            case["compressed_chars"].as_u64().expect("compressed_chars") as usize,
            case["raw_chars"].as_u64().expect("raw_chars") as usize,
            case["model"].as_str().expect("model"),
        );
        assert_eq!(
            actual,
            case["expected"].as_str().expect("expected"),
            "{label}"
        );
    }
}

#[test]
fn compression_constants_match_python() {
    let data = fixture();
    let expected = &data["compression"]["constants"];
    assert_eq!(
        compression::ARGUMENTS_PREVIEW_CHARS,
        expected["arguments_preview_chars"].as_u64().expect("int") as usize
    );
    assert_eq!(
        compression::MAX_PARALLEL_COMPRESSIONS,
        expected["max_parallel_compressions"].as_u64().expect("int") as usize
    );
    assert_eq!(
        compression::COMPACTION_GRACE_SECONDS,
        expected["compaction_grace_seconds"]
            .as_f64()
            .expect("float")
    );
    let mut tools = compression::COMPACTABLE_TOOLS.to_vec();
    tools.sort_unstable();
    assert_eq!(
        tools,
        strings(&expected["compactable_tools"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
}

#[test]
fn compression_prompt_and_cleanup_match_python() {
    let data = fixture();
    let expected = &data["compression"];

    assert_eq!(
        compression::system_prompt_text(),
        expected["system_prompt"].as_str().expect("提示词"),
        "内置压缩系统提示"
    );
    assert_eq!(
        compression::OUTPUT_OPEN,
        expected["markers"]["output_open"].as_str().expect("标记")
    );
    assert_eq!(
        compression::OUTPUT_CLOSE,
        expected["markers"]["output_close"].as_str().expect("标记")
    );
    assert_eq!(
        compression::OMITTED_NOTE,
        expected["markers"]["omitted_note"].as_str().expect("标记")
    );

    for case in expected["build_messages"]
        .as_array()
        .expect("build_messages")
    {
        let label = case["name"].as_str().expect("用例名");
        let actual = compression::build_messages(
            case["tool_name"].as_str().expect("工具名"),
            case["arguments_summary"].as_str().expect("参数摘要"),
            case["task_hint"].as_str().expect("任务提示"),
            case["output"].as_str().expect("原始输出"),
        );
        assert_eq!(
            serde_json::json!(actual),
            case["expected"],
            "压缩请求消息（{label}）"
        );
    }

    for case in expected["sample_output"].as_array().expect("sample_output") {
        let label = case["name"].as_str().expect("用例名");
        let actual = compression::sample_output(
            case["output"].as_str().expect("输出"),
            case["max_chars"].as_u64().expect("上限") as usize,
        );
        assert_eq!(actual, case["expected"], "输出采样（{label}）");
    }

    for case in expected["clean_reply_text"]
        .as_array()
        .expect("clean_reply_text")
    {
        let label = case["name"].as_str().expect("用例名");
        let actual = compression::clean_reply_text(case["text"].as_str().expect("回包"));
        assert_eq!(actual, case["expected"], "回包清洗（{label}）");
    }

    for case in expected["label_line"].as_array().expect("label_line") {
        let label = case["name"].as_str().expect("用例名");
        let actual = compression::is_label_line(case["line"].as_str().expect("行"));
        assert_eq!(
            actual,
            case["expected"].as_bool().expect("布尔"),
            "标签行判定（{label}）"
        );
    }

    for case in expected["bound_text"].as_array().expect("bound_text") {
        let label = case["name"].as_str().expect("用例名");
        let actual = compression::bound_text(
            case["text"].as_str().expect("文本"),
            case["max_chars"].as_u64().expect("上限") as usize,
        );
        assert_eq!(actual, case["expected"], "结果截断（{label}）");
    }

    for case in expected["looks_like_cancellation"]
        .as_array()
        .expect("looks_like_cancellation")
    {
        let label = case["name"].as_str().expect("用例名");
        let actual = compression::looks_like_cancellation(
            case["type_name"].as_str().expect("类型名"),
            case["message"].as_str().expect("消息"),
        );
        assert_eq!(
            actual,
            case["expected"].as_bool().expect("布尔"),
            "取消判定（{label}）"
        );
    }

    for case in expected["reasoning_effort"]
        .as_array()
        .expect("reasoning_effort")
    {
        let label = case["name"].as_str().expect("用例名");
        let actual = compression::effective_reasoning_effort(
            case["thinking_enabled"].as_bool().expect("开关"),
            case["effort"].as_str().expect("深度"),
        );
        assert_eq!(actual, case["expected"], "思考深度（{label}）");
    }
}

// ------------------------------------------------------------------------- building

#[test]
fn building_advisor_guidelines_matches_python() {
    let data = fixture();
    for case in data["building"]["advisor_guidelines"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = building::advisor_guidelines_block(
            case["active"].as_bool().expect("active"),
            case["blacklisted"].as_bool().expect("blacklisted"),
        );
        assert_eq!(
            actual,
            case["expected"].as_str().expect("expected"),
            "顾问准则（{label}）"
        );
    }
    assert_eq!(
        building::ADVISOR_GUIDELINES_BLOCK,
        data["building"]["advisor_guidelines"][0]["expected"]
            .as_str()
            .expect("expected")
    );
}

#[test]
fn building_activate_mode_matches_python() {
    let data = fixture();
    let templates = template_dir();
    for case in data["building"]["activate_mode"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let mode = case["mode"].as_str().expect("mode");
        let normalized = match building::normalize_mode_name(mode) {
            Ok(value) => value,
            Err(error) => {
                assert!(
                    !case["ok"].as_bool().expect("ok"),
                    "本该成功：{}（{label}）",
                    error
                );
                assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "{label}"
                );
                continue;
            }
        };
        let result = building::read_mode_prompt(&templates, &normalized);
        if let Some(prompt) = expect_outcome(result, case, label) {
            assert_eq!(
                normalized,
                case["normalized"].as_str().expect("normalized"),
                "归一化模式名（{label}）"
            );
            assert_eq!(
                prompt.chars().count() as u64,
                case["prompt_len"].as_u64().expect("prompt_len"),
                "模板长度（{label}）"
            );
            assert_eq!(
                sha256_hex(&prompt),
                case["prompt_sha256"].as_str().expect("prompt_sha256"),
                "模板内容（{label}）"
            );
        }
    }
}

#[test]
fn building_system_prompt_matches_python() {
    let data = fixture();
    for case in data["building"]["system_prompt"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let advisor_block = building::advisor_guidelines_block(
            case["advisor_active"].as_bool().expect("advisor_active"),
            case["blacklisted"].as_bool().expect("blacklisted"),
        );
        let actual = building::system_prompt_with_mode(
            "基础提示词。",
            advisor_block,
            case["mode_name"].as_str().expect("mode_name"),
            case["mode_prompt"].as_str().expect("mode_prompt"),
        );
        assert_eq!(
            actual,
            case["expected"].as_str().expect("expected"),
            "system prompt（{label}）"
        );
    }
}

#[test]
fn building_temp_dir_display_matches_python() {
    let data = fixture();
    for case in data["building"]["temp_dir_display"]
        .as_array()
        .expect("cases")
    {
        let label = case["label"].as_str().expect("label");
        let actual = building::agent_temp_dir_display(case["display"].as_str());
        assert_eq!(
            actual,
            case["expected"].as_str().expect("expected"),
            "{label}"
        );
    }
}
