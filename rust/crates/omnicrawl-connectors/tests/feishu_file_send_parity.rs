//! 飞书 `[FILE:...]` 发文件的跨语言 parity：期望值来自 Python 真实现。
//!
//! Python 侧用探针把「上传返回什么 key」固定下来，逐条记录上传与发送调用；
//! Rust 侧用同样记录调用的 `Recorder` 重放同一批用例，比对调用序列与结果。
//! 临时根在数据集里是 `<ROOT>` 占位。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use omnicrawl_connectors::feishu::{send_generated_files, send_local_file, FileTransport};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/feishu_file_send_parity.json");
const ROOT_PLACEHOLDER: &str = "<ROOT>";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn work_root() -> &'static Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("omnicrawl-file-send-parity-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).expect("建目录");
        for (name, content) in [
            ("pic.png", "png"),
            ("clip.mp3", "mp3"),
            ("clip.mp4", "mp4"),
            ("notes.txt", "text"),
            ("README", "readme"),
            ("a.png", "a"),
            ("b.mp3", "b"),
            ("c.pdf", "c"),
        ] {
            fs::write(root.join(name), content).expect("写文件");
        }
        root
    })
    .as_path()
}

fn mask(text: &str, root: &Path) -> String {
    let base = root.to_string_lossy().to_string();
    text.replace(&base.replace('\\', "\\\\"), ROOT_PLACEHOLDER)
        .replace(&base, ROOT_PLACEHOLDER)
}

fn unmask(text: &str, root: &Path) -> String {
    text.replace(ROOT_PLACEHOLDER, &root.to_string_lossy())
}

/// 记录调用的宿主：上传返回固定 key，发送只记录。
struct Recorder {
    image_key: Option<String>,
    file_key: Option<String>,
    root: PathBuf,
    calls: Mutex<Vec<Value>>,
}

impl Recorder {
    fn new(image_key: Option<&str>, file_key: Option<&str>, root: &Path) -> Self {
        Self {
            image_key: image_key.map(str::to_string),
            file_key: file_key.map(str::to_string),
            root: root.to_path_buf(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn push(&self, value: Value) {
        self.calls.lock().expect("记录锁").push(value);
    }

    fn take(&self) -> Vec<Value> {
        self.calls.lock().expect("记录锁").clone()
    }

    fn name_of(&self, path: &Path) -> String {
        mask(
            &path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default(),
            &self.root,
        )
    }
}

impl FileTransport for Recorder {
    fn upload_image(&self, path: &Path) -> Option<String> {
        self.push(json!({"call": "upload_image", "name": self.name_of(path)}));
        self.image_key.clone()
    }

    fn upload_file(&self, path: &Path) -> Option<String> {
        let suffix = path
            .extension()
            .map(|value| format!(".{}", value.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        self.push(json!({
            "call": "upload_file",
            "name": self.name_of(path),
            "suffix": suffix,
        }));
        self.file_key.clone()
    }

    fn send_raw(&self, receive_id: &str, body: &str, message_type: &str, receive_id_type: &str) {
        self.push(json!({
            "call": "send_raw",
            "receive_id": receive_id,
            "body": body,
            "msg_type": message_type,
            "receive_id_type": receive_id_type,
        }));
    }

    fn send_text(&self, receive_id: &str, text: &str, receive_id_type: &str) {
        self.push(json!({
            "call": "send_text",
            "receive_id": receive_id,
            "text": mask(text, &self.root),
            "receive_id_type": receive_id_type,
        }));
    }
}

#[test]
fn local_file_sending_matches_python() {
    let fixture = fixture();
    let root = work_root();
    let receive_id = fixture["receive_id"].as_str().expect("receive_id");
    let receive_id_type = fixture["receive_id_type"]
        .as_str()
        .expect("receive_id_type");

    for case in fixture["local_files"].as_array().expect("local_files") {
        let label = case["label"].as_str().unwrap_or("");
        let image_key = case["image_key"].as_str();
        let file_key = case["file_key"].as_str();
        let recorder = Recorder::new(image_key, file_key, root);
        let path = unmask(case["path"].as_str().expect("path"), root);
        let result = send_local_file(&recorder, receive_id, &path, receive_id_type);

        assert_eq!(Value::Bool(result), case["result"], "返回结果（{label}）");
        assert_eq!(
            Value::Array(recorder.take()),
            case["calls"],
            "调用序列（{label}）"
        );
    }
}

#[test]
fn generated_files_matches_python() {
    let fixture = fixture();
    let root = work_root();
    let receive_id = fixture["receive_id"].as_str().expect("receive_id");
    let receive_id_type = fixture["receive_id_type"]
        .as_str()
        .expect("receive_id_type");

    for case in fixture["generated_files"]
        .as_array()
        .expect("generated_files")
    {
        let label = case["label"].as_str().unwrap_or("");
        let recorder = Recorder::new(Some("ik1"), Some("fk1"), root);
        let text = unmask(case["text"].as_str().expect("text"), root);
        send_generated_files(&recorder, receive_id, &text, receive_id_type);
        assert_eq!(
            Value::Array(recorder.take()),
            case["calls"],
            "标记扫描（{label}）"
        );
    }
}
