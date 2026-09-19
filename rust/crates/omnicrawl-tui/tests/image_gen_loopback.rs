//! 真实传输回环：本地 `TcpListener` 起最小 HTTP 服务，验证 `image_gen` 经 `UreqWebTransport`
//! 发出的 JSON 生成请求、multipart 编辑请求与 Base64 落盘。只打 127.0.0.1，不访问公网。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use omnicrawl_tui::tools::image_gen::{self, ImageGenOptions};
use omnicrawl_tui::tools::web_transport::UreqWebTransport;

const IMAGE_JSON: &str = r#"{"data":[{"b64_json":"aGVsbG8=","output_format":"png"}]}"#;

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> String {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }
}

fn serve(requests: usize) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let seen: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    std::thread::spawn(move || {
        for _ in 0..requests {
            let Ok((stream, _)) = listener.accept() else {
                break;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();

            let mut headers = Vec::new();
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header.trim().is_empty() {
                    break;
                }
                let line = header.trim_end().to_string();
                if let Some((key, value)) = line.split_once(':') {
                    let key = key.trim().to_string();
                    let value = value.trim().to_string();
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.parse::<usize>().unwrap_or(0);
                    }
                    headers.push((key, value));
                }
            }
            let mut body = vec![0u8; length];
            if length > 0 {
                let _ = reader.read_exact(&mut body);
            }
            captured.lock().expect("请求锁").push(CapturedRequest {
                method,
                path,
                headers,
                body,
            });

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{IMAGE_JSON}",
                IMAGE_JSON.len()
            );
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn arguments(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("omnicrawl-tui-image-gen-loopback-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    root
}

fn options(root: &Path, base_url: &str) -> ImageGenOptions {
    ImageGenOptions {
        transport: Arc::new(UreqWebTransport::new()),
        workspace_root: root.to_path_buf(),
        enabled: true,
        base_url: base_url.to_string(),
        api_key: "test-key".to_string(),
        // 显式禁用代理：回环只打本机。
        proxy: Some(String::new()),
        ..ImageGenOptions::default()
    }
}

#[test]
fn image_gen_posts_json_and_multipart_to_a_local_server() {
    let (base, seen) = serve(4);
    let root = workspace("requests");
    std::fs::write(root.join("ref.png"), b"\x89PNG\r\n\x1a\nreference").expect("写参考图");
    let options = options(&root, &base);

    let generated = image_gen::image_gen(&options, &arguments(json!({"prompt": "一只猫"})))
        .expect("生成请求应当成功");
    assert!(
        generated.starts_with("已生成 1 张图片（模型 gpt-image-2）："),
        "{generated}"
    );

    let edited = image_gen::image_gen(
        &options,
        &arguments(json!({"prompt": "改背景", "image": "ref.png"})),
    )
    .expect("编辑请求应当成功");
    assert!(
        edited.starts_with("已生成 1 张图片（模型 gpt-image-2）："),
        "{edited}"
    );

    let captured = seen.lock().expect("请求锁").clone();
    let generate = captured
        .iter()
        .find(|item| item.path.contains("/images/generations"))
        .expect("应当收到生成请求");
    assert_eq!(generate.method, "POST");
    assert_eq!(generate.header("authorization"), "Bearer test-key");
    assert!(generate
        .header("content-type")
        .starts_with("application/json"));
    let body = String::from_utf8_lossy(&generate.body);
    assert!(body.contains("\"model\":\"gpt-image-2\""), "{body}");
    assert!(body.contains("\"prompt\":\"一只猫\""), "{body}");
    assert!(body.contains("\"response_format\":\"b64_json\""), "{body}");
    assert!(body.contains("\"n\":1"), "{body}");

    let edit = captured
        .iter()
        .find(|item| item.path.contains("/images/edits"))
        .expect("应当收到编辑请求");
    assert_eq!(edit.method, "POST");
    assert!(
        edit.header("content-type")
            .starts_with("multipart/form-data; boundary="),
        "{}",
        edit.header("content-type")
    );
    let body = String::from_utf8_lossy(&edit.body);
    assert!(
        body.contains("name=\"image\"; filename=\"ref.png\""),
        "{body}"
    );
    assert!(body.contains("name=\"prompt\""), "{body}");
    assert!(body.contains("改背景"), "{body}");
    assert!(body.contains("name=\"model\""), "{body}");
    assert!(
        edit.body.windows(4).any(|window| window == b"PNG\r"),
        "参考图字节应当随 multipart 上传"
    );

    // Base64 落盘：内容与响应体里的 aGVsbG8= 一致。
    let images_dir = root.join(".omnicrawl").join(".agent_tmp").join("images");
    let entries: Vec<_> = std::fs::read_dir(&images_dir)
        .expect("默认图片目录应当存在")
        .flatten()
        .collect();
    assert_eq!(entries.len(), 2, "生成与编辑各落盘一张");
    for entry in entries {
        assert_eq!(std::fs::read(entry.path()).expect("读图片"), b"hello");
    }
}
