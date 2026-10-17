//! OneJev 自部署服务的直连客户端：命名管道（Windows）/ Unix socket，不经 TCP 与 HTTP。
//!
//! 为什么要它：自部署的判定是纯内存计算，`qev serve` 的 uvicorn/FastAPI 那一层对本机调用方
//! 是纯中转开销。worker（`rust/assets/extensions/onejev_worker.py`）直接持有 `qev` 引擎，
//! 本模块按行把请求交给它、按行读回响应。
//!
//! 线格式与 HTTP 那条完全一致（`{model, state, questions}` 进、顶层 `answers` 出），因此上层
//! 解析答案的代码不必区分两种传输；差别只在连接建立与「失败长什么样」。
//!
//! 端点由 `omnicrawl-config` 的 `onejev_endpoint` 统一解析——拉服务与连服务必须用同一个名字。

use std::io::{BufRead, BufReader, Write};

/// 一次直连判定：把 `body_text`（已脱敏）发过去，读回响应正文。
///
/// 返回文本与 HTTP 通路同形（顶层含 `answers`），因此 [`crate::decision_wire::extract_answers`]
/// 直接可用。
pub fn post_body(endpoint: &str, body_text: &str, timeout_seconds: u64) -> Result<String, String> {
    let timeout = std::time::Duration::from_secs(timeout_seconds.max(1));
    #[cfg(windows)]
    let stream = connect_pipe(endpoint, timeout)?;
    #[cfg(not(windows))]
    let stream = connect_socket(endpoint, timeout)?;
    exchange(stream, body_text)
}

/// 一次请求-响应往返：两端都是字节流，按行分帧、都以 `\n` 结尾。
fn exchange<S: std::io::Read + Write>(mut stream: S, body_text: &str) -> Result<String, String> {
    let mut payload = body_text.trim_end_matches(['\r', '\n']).as_bytes().to_vec();
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .map_err(|error| format!("发送决策请求失败：{error}"))?;
    stream
        .flush()
        .map_err(|error| format!("发送决策请求失败：{error}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .map_err(|error| format!("读取决策响应失败：{error}"))?;
    if read == 0 {
        return Err("本地决策服务没有回任何内容（连接被关闭）。".to_string());
    }
    let text = line.trim_end_matches(['\r', '\n']).to_string();
    // worker 的失败信封是 `{"error": "..."}`：翻成可读原因，别让上层去解析「缺少 answers」。
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
        if value.get("answers").is_none() {
            if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
                return Err(format!("决策服务返回错误：{error}"));
            }
        }
    }
    Ok(text)
}

/// 连接命名管道并把它包成 `File`。
///
/// 管道未就绪（服务还在加载权重）时按短间隔重试到超时：这里是唯一「等它一下」的地方，
/// 上层不必为启动窗口再写一遍等待逻辑。
#[cfg(windows)]
fn connect_pipe(endpoint: &str, timeout: std::time::Duration) -> Result<std::fs::File, String> {
    use std::os::windows::io::FromRawHandle;

    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = endpoint.encode_utf16().chain(std::iter::once(0)).collect();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(unsafe { std::fs::File::from_raw_handle(handle as *mut _) });
        }
        let error = std::io::Error::last_os_error();
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "连接本地决策服务失败（{endpoint}）：{error}。服务可能未启动。"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// 连接 Unix 域套接字。
#[cfg(not(windows))]
fn connect_socket(endpoint: &str, timeout: std::time::Duration) -> Result<std::os::unix::net::UnixStream, String> {
    let stream = std::os::unix::net::UnixStream::connect(endpoint).map_err(|error| {
        format!("连接本地决策服务失败（{endpoint}）：{error}。服务可能未启动。")
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    Ok(stream)
}
