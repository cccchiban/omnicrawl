//! 飞书文件上传的接口形状测试（进程内传输桩件）。
//!
//! Python 侧用 `lark-oapi` SDK 的 `CreateImageRequest` / `CreateFileRequest`，SDK 内部
//! 不暴露请求字节，因此这一层没有可对照的 JSON 快照；这里钉住的是我们自己拼的
//! multipart 形状（字段顺序、文件名、内容类型、认证头）与失败回落。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use omnicrawl_connectors::feishu::{file_type_for, multipart_body, FeishuApi, FileTransport};
use omnicrawl_connectors::http::{HttpReply, HttpTransport};
use serde_json::{json, Value};

#[derive(Default)]
struct MockHttp {
    requests: Mutex<Vec<Value>>,
    upload_code: AtomicUsize,
    body: Mutex<Vec<u8>>,
}

impl MockHttp {
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("请求锁中毒").clone()
    }

    fn last_body(&self) -> Vec<u8> {
        self.body.lock().expect("正文锁中毒").clone()
    }

    fn set_upload_code(&self, code: usize) {
        self.upload_code.store(code, Ordering::SeqCst);
    }
}

impl HttpTransport for MockHttp {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        _timeout: Duration,
    ) -> Result<HttpReply, String> {
        let raw = body.unwrap_or_default().to_vec();
        *self.body.lock().expect("正文锁中毒") = raw.clone();
        self.requests.lock().expect("请求锁中毒").push(json!({
            "method": method,
            "url": url,
            "content_type": headers
                .iter()
                .find(|(key, _value)| key.eq_ignore_ascii_case("content-type"))
                .map(|(_key, value)| value.clone()),
            "authorization": headers
                .iter()
                .find(|(key, _value)| key.eq_ignore_ascii_case("authorization"))
                .map(|(_key, value)| value.clone()),
            "body_len": raw.len(),
        }));
        let payload = if url.contains("tenant_access_token") {
            json!({"code": 0, "tenant_access_token": "t-mock", "expire": 3600})
        } else if url.contains("/im/v1/images") {
            if self.upload_code.load(Ordering::SeqCst) == 0 {
                json!({"code": 0, "data": {"image_key": "img_mock"}})
            } else {
                json!({"code": self.upload_code.load(Ordering::SeqCst), "msg": "upload failed"})
            }
        } else if url.contains("/im/v1/files") {
            if self.upload_code.load(Ordering::SeqCst) == 0 {
                json!({"code": 0, "data": {"file_key": "file_mock"}})
            } else {
                json!({"code": self.upload_code.load(Ordering::SeqCst), "msg": "upload failed"})
            }
        } else if url.contains("/im/v1/messages") {
            json!({"code": 0, "data": {"message_id": "om_mock"}})
        } else {
            json!({"code": 0})
        };
        Ok(HttpReply {
            status: 200,
            body: payload.to_string().into_bytes(),
        })
    }
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(name: &str, contents: &[u8]) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("omnicrawl-upload-{stamp}"));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("写临时文件");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

fn api_with(mock: Arc<MockHttp>) -> FeishuApi {
    FeishuApi::new("app", "secret", mock)
}

#[test]
fn upload_image_posts_multipart_with_expected_shape() {
    let mock = Arc::new(MockHttp::default());
    let api = api_with(mock.clone());
    let file = TempFile::new("a.png", b"PNG-BYTES");

    let key = api.upload_image(file.path());

    assert_eq!(key.as_deref(), Some("img_mock"));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "先取令牌再上传");
    let upload = &requests[1];
    assert_eq!(upload["method"], "POST");
    assert_eq!(
        upload["url"],
        "https://open.feishu.cn/open-apis/im/v1/images"
    );
    assert_eq!(upload["authorization"], "Bearer t-mock");
    let content_type = upload["content_type"].as_str().expect("缺少内容类型");
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "内容类型应带边界：{content_type}"
    );
    let boundary = content_type
        .trim_start_matches("multipart/form-data; boundary=")
        .to_string();

    let body = String::from_utf8_lossy(&mock.last_body()).to_string();
    assert!(body.contains(&format!("--{boundary}\r\n")));
    let image_type_at = body
        .find("name=\"image_type\"")
        .expect("缺少 image_type 字段");
    let file_at = body
        .find("name=\"image\"; filename=\"a.png\"")
        .expect("缺少文件字段");
    assert!(image_type_at < file_at, "文本字段应在文件字段之前");
    assert!(
        body.contains("\r\n\r\nmessage\r\n"),
        "image_type 应为 message"
    );
    assert!(body.contains("Content-Type: application/octet-stream"));
    assert!(body.contains("PNG-BYTES"), "文件内容应原样进入正文");
    assert!(body.ends_with(&format!("\r\n--{boundary}--\r\n")));
}

#[test]
fn upload_file_sends_file_type_and_name_fields() {
    let mock = Arc::new(MockHttp::default());
    let api = api_with(mock.clone());
    let file = TempFile::new("报告.PPTX", b"PPT-BYTES");

    let key = api.upload_file(file.path());

    assert_eq!(key.as_deref(), Some("file_mock"));
    let requests = mock.requests();
    assert_eq!(
        requests[1]["url"],
        "https://open.feishu.cn/open-apis/im/v1/files"
    );
    let body = String::from_utf8_lossy(&mock.last_body()).to_string();
    assert!(body.contains("name=\"file_type\"\r\n\r\nppt\r\n"), "{body}");
    assert!(
        body.contains("name=\"file_name\"\r\n\r\n报告.PPTX\r\n"),
        "{body}"
    );
    assert!(body.contains("name=\"file\"; filename=\"报告.PPTX\""));
    assert!(body.contains("PPT-BYTES"));
}

#[test]
fn upload_failures_fall_back_to_none() {
    let mock = Arc::new(MockHttp::default());
    let api = api_with(mock.clone());
    let file = TempFile::new("a.png", b"PNG-BYTES");

    mock.set_upload_code(99991663);
    assert_eq!(api.upload_image(file.path()), None, "接口报错应回落 None");

    mock.set_upload_code(0);
    assert_eq!(
        api.upload_image(Path::new("不存在的目录/不存在.png")),
        None,
        "读文件失败应回落 None"
    );
}

#[test]
fn file_type_mapping_follows_python_table() {
    assert_eq!(file_type_for("a.pdf"), "pdf");
    assert_eq!(file_type_for("A.PPTX"), "ppt");
    assert_eq!(file_type_for("voice.opus"), "opus");
    assert_eq!(file_type_for("clip.mp4"), "mp4");
    assert_eq!(file_type_for("unknown.bin"), "stream");
    assert_eq!(file_type_for("no_suffix"), "stream");
}

#[test]
fn transport_sends_raw_and_text_through_messages_endpoint() {
    let mock = Arc::new(MockHttp::default());
    let api = api_with(mock.clone());

    api.send_raw("ou_1", r#"{"text":"hi"}"#, "text", "open_id");
    api.send_text("ou_1", "纯文本", "open_id");

    let requests = mock.requests();
    let messages: Vec<&Value> = requests
        .iter()
        .filter(|request| {
            request["url"]
                .as_str()
                .unwrap_or_default()
                .contains("/im/v1/messages")
        })
        .collect();
    assert_eq!(messages.len(), 2, "两次发送都要落到消息接口");
    assert!(messages[0]["url"]
        .as_str()
        .unwrap_or_default()
        .contains("receive_id_type=open_id"));
}

#[test]
fn multipart_body_orders_fields_before_file() {
    let body = multipart_body(
        "BOUND",
        &[("file_type", "stream"), ("file_name", "x.bin")],
        "file",
        "x.bin",
        b"DATA",
    );
    let text = String::from_utf8(body).expect("正文是 UTF-8");
    assert_eq!(
        text,
        "--BOUND\r\nContent-Disposition: form-data; name=\"file_type\"\r\n\r\nstream\r\n\
--BOUND\r\nContent-Disposition: form-data; name=\"file_name\"\r\n\r\nx.bin\r\n\
--BOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.bin\"\r\n\
Content-Type: application/octet-stream\r\n\r\nDATA\r\n--BOUND--\r\n"
    );
}
