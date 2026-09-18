//! 大转录读取：内存映射分支必须与整份读取给出同一批事件，坏行照样只跳过。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use omnicrawl_session::{utc_now, SessionEvent, SessionStore};
use serde_json::{json, Map, Value};

fn temp_root(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("时钟可用")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("omnicrawl-transcript-{tag}-{unique}"));
    std::fs::create_dir_all(&path).expect("建临时目录");
    path
}

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("载荷是 JSON 对象")
}

#[test]
fn mapped_transcript_matches_buffered_read() {
    let root = temp_root("mmap");
    let store = SessionStore::open(&root);
    store.ensure().expect("建目录");
    let created = store
        .start_session("/workspace", "大转录", utc_now())
        .expect("建会话");
    let path = root
        .join("sessions")
        .join(format!("{}.jsonl", created.session_id));

    let rows = 800;
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("打开转录");
    for index in 0..rows {
        let event = SessionEvent::create(
            &created.session_id,
            "tool_result",
            payload(json!({"output": "内核转录填充".repeat(120), "index": index})),
            None,
            utc_now(),
        )
        .expect("创建事件");
        writeln!(file, "{}", event.to_json_line()).expect("写事件行");
    }
    writeln!(file, "{{not-json").expect("写坏行");
    file.flush().expect("落盘");
    drop(file);

    let size = std::fs::metadata(&path).expect("转录元数据").len();
    assert!(
        size >= 1 << 20,
        "用例必须超过内存映射阈值，实际 {size} 字节"
    );

    let events = store.read_events(&created.session_id).expect("读取转录");
    assert_eq!(events.len(), rows + 1, "坏行只跳过，其余事件都要读回");
    assert_eq!(events[0].event_type, "session_started");
    assert_eq!(events[1].event_type, "tool_result");

    std::fs::remove_dir_all(&root).ok();
}
