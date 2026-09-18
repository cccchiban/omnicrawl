//! 会话目录扫描的对照测试。
//!
//! 期望值由 `rust/tools/gen_session_scan_fixture.py` 驱动 Python 真实现生成；数据集同时给出
//! 目录树，两边在各自的临时目录里建同一棵树，再比对扫描结果。

use std::fs;
use std::path::PathBuf;

use omnicrawl_session::consistency::{discover_artifact_session_ids, discover_transcripts};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/session_scan_parity.json"))
        .expect("对照数据集必须是合法 JSON")
}

/// 在临时目录里按数据集建同一棵树；返回沙箱路径与清理句柄。
fn build_tree(tree: &[Value]) -> PathBuf {
    let sandbox = std::env::temp_dir().join(format!("omnicrawl-scan-{}", std::process::id()));
    let _ = fs::remove_dir_all(&sandbox);

    for item in tree {
        let item = item.as_str().expect("目录树项必须是字符串");
        let target = sandbox.join(item.trim_end_matches('/'));
        if item.ends_with('/') {
            fs::create_dir_all(&target).expect("建目录失败");
        } else {
            fs::create_dir_all(target.parent().expect("文件必须有父目录")).expect("建父目录失败");
            fs::write(&target, "").expect("写文件失败");
        }
    }
    sandbox
}

#[test]
fn 转录与_artifact_扫描与_python_一致() {
    let data = fixture();
    let tree = data["tree"].as_array().expect("数据集必须带目录树");
    let sandbox = build_tree(tree);

    let transcripts: Vec<Value> = discover_transcripts(&sandbox)
        .iter()
        .map(|item| {
            serde_json::json!({
                "session_id": item.session_id,
                "relative_path": item.relative_path,
            })
        })
        .collect();
    assert_eq!(
        serde_json::to_string(&Value::Array(transcripts)).expect("结果可序列化"),
        serde_json::to_string(&data["expected"]["transcripts"]).expect("期望值可序列化"),
        "转录扫描结果不一致"
    );

    let artifacts = discover_artifact_session_ids(&sandbox.join("artifacts"));
    assert_eq!(
        serde_json::to_string(&artifacts).expect("结果可序列化"),
        serde_json::to_string(&data["expected"]["artifact_session_ids"]).expect("期望值可序列化"),
        "artifact 扫描结果不一致"
    );

    let _ = fs::remove_dir_all(&sandbox);
}

#[test]
fn 目录缺失时返回空结果() {
    let sandbox = std::env::temp_dir().join(format!("omnicrawl-scan-empty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&sandbox);
    fs::create_dir_all(&sandbox).expect("建目录失败");

    assert!(discover_transcripts(&sandbox).is_empty());
    assert!(discover_artifact_session_ids(&sandbox.join("artifacts")).is_empty());

    let _ = fs::remove_dir_all(&sandbox);
}
