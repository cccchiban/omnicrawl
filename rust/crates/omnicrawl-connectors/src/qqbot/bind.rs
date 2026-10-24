//! 扫码绑定：三方 Agent 用二维码直接拿到 AppID 与 AppSecret。
//!
//! 对齐官方 `@tencent-connect/qqbot-connector` 的流程（`lite/create_bind_task` →
//! 展示二维码 → 轮询 `lite/poll_bind_result`）：创建任务时本地生成 32 字节随机
//! `key`（base64），绑定完成后平台返回用该 key 加密的 `bot_encrypt_secret`，
//! 本地用 AES-256-GCM 解出 AppSecret——因此 Secret 只在本地解密，不走第三方。
//!
//! 加密载荷的形状：`base64(nonce[12] || ciphertext || tag[16])`。

use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::http::{HttpTransport, UreqTransport};
use crate::json;

/// 绑定页面所在域名。
pub const BIND_HOST: &str = "q.qq.com";

/// 扫码页面地址；`source` 是会显示在扫码页上的接入方名称。
pub const CONNECT_PAGE: &str = "https://q.qq.com/qqbot/openclaw/connect.html";

/// 轮询间隔：官方 SDK 用 2 秒。
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 单次绑定任务的默认等待上限。
pub const DEFAULT_BIND_TIMEOUT: Duration = Duration::from_secs(600);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 绑定状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindStatus {
    None,
    Pending,
    Completed,
    Expired,
}

impl BindStatus {
    pub fn from_code(code: i64) -> BindStatus {
        match code {
            1 => BindStatus::Pending,
            2 => BindStatus::Completed,
            3 => BindStatus::Expired,
            _ => BindStatus::None,
        }
    }
}

/// 一次绑定任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindTask {
    pub task_id: String,
    /// 本地生成的 AES-256 密钥（base64）。
    pub key: String,
}

/// 轮询结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindResult {
    pub status: BindStatus,
    pub app_id: String,
    /// 平台返回的密文（base64），完成时才有。
    pub encrypted_secret: String,
    pub user_openid: String,
}

/// 绑定成功后的凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindCredentials {
    pub app_id: String,
    pub app_secret: String,
    pub user_openid: String,
}

/// 绑定失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindError(pub String);

impl std::fmt::Display for BindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BindError {}

/// 随机 32 字节密钥（base64）。失败时回落到时间与 PID 派生的伪随机值。
pub fn generate_bind_key() -> String {
    let mut bytes = [0_u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        let mut state = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos() as u64)
            .unwrap_or(0)
            ^ u64::from(std::process::id());
        for slot in bytes.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = (state & 0xff) as u8;
        }
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 扫码页地址。
pub fn build_connect_url(task_id: &str, source: &str) -> String {
    format!(
        "{CONNECT_PAGE}?task_id={}&source={}&_wv=2",
        crate::http::encode_component(task_id),
        crate::http::encode_component(source)
    )
}

/// 创建绑定任务。
pub fn create_bind_task(transport: &dyn HttpTransport) -> Result<BindTask, BindError> {
    let key = generate_bind_key();
    let url = format!("https://{BIND_HOST}/lite/create_bind_task");
    let payload = post_json(transport, &url, &json!({"key": key}))?;
    if payload.get("retcode").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(BindError(format!(
            "创建绑定任务失败：{}",
            payload
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("平台未返回原因")
        )));
    }
    let task_id = payload
        .get("data")
        .and_then(|data| data.get("task_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BindError("创建绑定任务失败：缺少 task_id".to_string()))?;
    Ok(BindTask {
        task_id: task_id.to_string(),
        key,
    })
}

/// 轮询一次绑定结果。
pub fn poll_bind_result(
    transport: &dyn HttpTransport,
    task_id: &str,
) -> Result<BindResult, BindError> {
    let url = format!("https://{BIND_HOST}/lite/poll_bind_result");
    let payload = post_json(transport, &url, &json!({"task_id": task_id}))?;
    if payload.get("retcode").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(BindError(format!(
            "查询绑定结果失败：{}",
            payload
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("平台未返回原因")
        )));
    }
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    Ok(BindResult {
        status: BindStatus::from_code(data.get("status").and_then(Value::as_i64).unwrap_or(0)),
        // `bot_appid` 可能是数字，统一按字符串取。
        app_id: match data.get("bot_appid") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.to_string(),
            _ => String::new(),
        },
        encrypted_secret: data
            .get("bot_encrypt_secret")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        user_openid: data
            .get("user_openid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// AES-256-GCM 解密：`base64(nonce[12] || ciphertext || tag[16])` + base64 key。
pub fn decrypt_secret(encrypted: &str, key_base64: &str) -> Result<String, BindError> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

    let engine = base64::engine::general_purpose::STANDARD;
    let key = engine
        .decode(key_base64.trim())
        .map_err(|error| BindError(format!("本地密钥不是合法 base64：{error}")))?;
    if key.len() != 32 {
        return Err(BindError(format!(
            "本地密钥长度不是 32 字节（实际 {}）",
            key.len()
        )));
    }
    let payload = engine
        .decode(encrypted.trim())
        .map_err(|error| BindError(format!("密文不是合法 base64：{error}")))?;
    if payload.len() < 12 + 16 {
        return Err(BindError(
            "密文长度不足（缺少 nonce 或校验标签）".to_string(),
        ));
    }
    let (nonce, rest) = payload.split_at(12);
    let (ciphertext, tag) = rest.split_at(rest.len() - 16);
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|error| BindError(format!("初始化 AES-256-GCM 失败：{error}")))?;
    let mut sealed = ciphertext.to_vec();
    sealed.extend_from_slice(tag);
    let nonce = Nonce::try_from(nonce).map_err(|_| BindError("密文 nonce 长度不对".to_string()))?;
    let plain = cipher
        .decrypt(&nonce, sealed.as_ref())
        .map_err(|_| BindError("解密失败：密钥不匹配或密文已损坏".to_string()))?;
    String::from_utf8(plain).map_err(|error| BindError(format!("解密结果不是 UTF-8：{error}")))
}

/// 把绑定成功的结果解成凭据。
pub fn credentials_from(
    result: &BindResult,
    key_base64: &str,
) -> Result<BindCredentials, BindError> {
    let secret = decrypt_secret(&result.encrypted_secret, key_base64)?;
    Ok(BindCredentials {
        app_id: result.app_id.clone(),
        app_secret: secret,
        user_openid: result.user_openid.clone(),
    })
}

/// 二维码的终端渲染（`█` 块，按两行合一压缩高度）。
pub fn qr_terminal(url: &str) -> Result<String, BindError> {
    let code = qrcode::QrCode::new(url.as_bytes())
        .map_err(|error| BindError(format!("生成二维码失败：{error}")))?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .dark_color(qrcode::render::unicode::Dense1x2::Dark)
        .light_color(qrcode::render::unicode::Dense1x2::Light)
        .build())
}

fn post_json(transport: &dyn HttpTransport, url: &str, body: &Value) -> Result<Value, BindError> {
    let reply = transport
        .post_json(url, &json::dumps(body), REQUEST_TIMEOUT)
        .map_err(|error| BindError(format!("绑定接口请求失败：{error}")))?;
    if !(200..300).contains(&reply.status) {
        return Err(BindError(format!("绑定接口返回 HTTP {}", reply.status)));
    }
    serde_json::from_slice::<Value>(&reply.body)
        .map_err(|error| BindError(format!("绑定接口返回非 JSON：{error}")))
}

/// 阻塞式扫码绑定：打印二维码并按 [`POLL_INTERVAL`] 轮询，直到成功、过期或超时。
pub fn bind_with_console(timeout: Duration, source: &str) -> Result<BindCredentials, BindError> {
    let transport = UreqTransport::new();
    bind_with(
        &transport,
        timeout,
        source,
        &mut |url| match qr_terminal(url) {
            Ok(art) => {
                println!("{art}");
                println!("请用手机 QQ 扫描上方二维码，完成机器人绑定。");
                println!("也可以直接在浏览器打开：{url}");
            }
            Err(error) => {
                println!("二维码渲染失败：{error}");
                println!("请在浏览器打开：{url}");
            }
        },
    )
}

/// 可注入的扫码绑定：`display` 负责把二维码 URL 展示给用户。
pub fn bind_with(
    transport: &dyn HttpTransport,
    timeout: Duration,
    source: &str,
    display: &mut dyn FnMut(&str),
) -> Result<BindCredentials, BindError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(BindError("扫码绑定超时，请重试。".to_string()));
        }
        let task = create_bind_task(transport)?;
        display(&build_connect_url(&task.task_id, source));
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(BindError("扫码绑定超时，请重试。".to_string()));
            }
            match poll_bind_result(transport, &task.task_id) {
                Ok(result) => match result.status {
                    BindStatus::Completed => return credentials_from(&result, &task.key),
                    BindStatus::Expired => {
                        println!("二维码已过期，正在刷新…");
                        break;
                    }
                    _ => std::thread::sleep(POLL_INTERVAL),
                },
                // 网络抖动不算失败：继续轮询到超时为止。
                Err(error) => {
                    eprintln!("查询绑定结果失败，稍后重试：{error}");
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpReply;
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit};
    use std::sync::Mutex;

    /// 记录请求并按 URL 回放的传输桩件。
    #[derive(Default)]
    struct MockHttp {
        requests: Mutex<Vec<(String, String)>>,
        polls: Mutex<usize>,
        secret: Mutex<String>,
    }

    impl HttpTransport for MockHttp {
        fn request(
            &self,
            _method: &str,
            url: &str,
            _headers: &[(String, String)],
            body: Option<&[u8]>,
            _timeout: Duration,
        ) -> Result<HttpReply, String> {
            let text = body
                .map(|raw| String::from_utf8_lossy(raw).to_string())
                .unwrap_or_default();
            self.requests
                .lock()
                .expect("锁中毒")
                .push((url.to_string(), text));
            let payload = if url.ends_with("/lite/create_bind_task") {
                json!({"retcode": 0, "data": {"task_id": "task-1"}})
            } else if url.ends_with("/lite/poll_bind_result") {
                let mut polls = self.polls.lock().expect("锁中毒");
                *polls += 1;
                let status = if *polls < 2 { 1 } else { 2 };
                json!({
                    "retcode": 0,
                    "data": {
                        "status": status,
                        "bot_appid": 102012345,
                        "bot_encrypt_secret": self.secret.lock().unwrap().clone(),
                        "user_openid": "F6A8B9C0",
                    },
                })
            } else {
                json!({})
            };
            Ok(HttpReply {
                status: 200,
                body: payload.to_string().into_bytes(),
                headers: Vec::new(),
            })
        }
    }

    /// 按官方形状加密：`nonce[12] || ciphertext || tag[16]`。
    fn encrypt_secret(secret: &str, key_base64: &str) -> String {
        let engine = base64::engine::general_purpose::STANDARD;
        let key = engine.decode(key_base64).expect("密钥");
        let cipher = Aes256Gcm::new_from_slice(&key).expect("初始化");
        let nonce = [7_u8; 12];
        let sealed = cipher
            .encrypt(&aes_gcm::Nonce::from(nonce), secret.as_bytes())
            .expect("加密");
        let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);
        let mut payload = nonce.to_vec();
        payload.extend_from_slice(ciphertext);
        payload.extend_from_slice(tag);
        engine.encode(payload)
    }

    #[test]
    fn key_is_32_random_bytes_in_base64() {
        let key = generate_bind_key();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&key)
            .expect("合法 base64");
        assert_eq!(decoded.len(), 32);
        assert_ne!(key, generate_bind_key());
    }

    #[test]
    fn connect_url_carries_task_and_source() {
        let url = build_connect_url("task-1", "omnicrawl");
        assert_eq!(
            url,
            "https://q.qq.com/qqbot/openclaw/connect.html?task_id=task-1&source=omnicrawl&_wv=2"
        );
        assert!(build_connect_url("任务 1", "第三方 机器人").contains("%E4%BB%BB"));
    }

    #[test]
    fn secret_round_trip_matches_the_official_layout() {
        let key = generate_bind_key();
        let encrypted = encrypt_secret("my-secret", &key);
        assert_eq!(
            decrypt_secret(&encrypted, &key).expect("可解密"),
            "my-secret"
        );
    }

    #[test]
    fn wrong_key_reports_decrypt_failure() {
        let key = generate_bind_key();
        let other = generate_bind_key();
        let encrypted = encrypt_secret("my-secret", &key);
        let error = decrypt_secret(&encrypted, &other).expect_err("应失败");
        assert!(error.0.contains("解密失败"), "{error}");
    }

    #[test]
    fn short_or_malformed_payloads_are_rejected() {
        let key = generate_bind_key();
        let engine = base64::engine::general_purpose::STANDARD;
        let short = engine.encode([1_u8; 20]);
        assert!(decrypt_secret(&short, &key)
            .expect_err("太短")
            .0
            .contains("长度"));
        assert!(decrypt_secret("!!!", &key).is_err());
        assert!(decrypt_secret(&engine.encode([0_u8; 40]), "AAAA").is_err());
    }

    #[test]
    fn bind_flow_creates_task_then_polls_to_completion() {
        let transport = MockHttp::default();
        // 绑定流程用 create_bind_task 自己生成的 key，因此先建任务，再按同一把 key 加密密文。
        let task = create_bind_task(&transport).expect("创建任务");
        *transport.secret.lock().unwrap() = encrypt_secret("resolved-secret", &task.key);
        assert_eq!(
            build_connect_url(&task.task_id, "omnicrawl"),
            "https://q.qq.com/qqbot/openclaw/connect.html?task_id=task-1&source=omnicrawl&_wv=2"
        );
        let first = poll_bind_result(&transport, &task.task_id).expect("轮询");
        assert_eq!(first.status, BindStatus::Pending);
        let second = poll_bind_result(&transport, &task.task_id).expect("轮询");
        assert_eq!(second.status, BindStatus::Completed);
        assert_eq!(second.app_id, "102012345");
        let credentials = credentials_from(&second, &task.key).expect("解密");
        assert_eq!(credentials.app_id, "102012345");
        assert_eq!(credentials.app_secret, "resolved-secret");
        assert_eq!(credentials.user_openid, "F6A8B9C0");
        let requests = transport.requests.lock().unwrap();
        assert!(requests[0].1.contains("\"key\""), "{}", requests[0].1);
    }

    #[test]
    fn platform_errors_are_surfaced() {
        struct Failing;
        impl HttpTransport for Failing {
            fn request(
                &self,
                _method: &str,
                _url: &str,
                _headers: &[(String, String)],
                _body: Option<&[u8]>,
                _timeout: Duration,
            ) -> Result<HttpReply, String> {
                Ok(HttpReply {
                    status: 200,
                    body: br#"{"retcode":-1,"msg":"busy"}"#.to_vec(),
                    headers: Vec::new(),
                })
            }
        }
        let error = create_bind_task(&Failing).expect_err("应失败");
        assert!(error.0.contains("busy"), "{error}");
    }

    #[test]
    fn qr_codes_render_as_block_characters() {
        let art = qr_terminal("https://q.qq.com/qqbot/openclaw/connect.html?task_id=t")
            .expect("应能渲染");
        assert!(art.contains('█') || art.contains('▀') || art.contains('▄'));
    }
}
