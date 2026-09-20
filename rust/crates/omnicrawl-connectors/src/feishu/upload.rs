//! 飞书文件上传：`multipart/form-data` 组装，配合 `im/v1/images` 与 `im/v1/files` 使用。
//!
//! Python 侧走 SDK 的 `CreateImageRequest` / `CreateFileRequest`，内核直接拼 HTTP：
//! 字段顺序与 SDK 一致（先文本字段、后文件字段），文件部分用
//! `application/octet-stream`（SDK 内部不暴露细粒度 MIME，取通用类型）。
//! `file_type` 仍复用 `files::FILE_TYPE_MAP` 与 Telegram 侧的路径后缀工具，不另起一套。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::files::FILE_TYPE_MAP;
use crate::telegram::files::path_suffix;

const DEFAULT_FILE_TYPE: &str = "stream";
const FILE_PART_CONTENT_TYPE: &str = "application/octet-stream";

/// 上传用的 `file_type`：按后缀大小写折叠后查表，未命中回落 `stream`。
pub fn file_type_for(file_name: &str) -> &'static str {
    let suffix = path_suffix(file_name).to_lowercase();
    FILE_TYPE_MAP
        .iter()
        .find(|(candidate, _)| *candidate == suffix)
        .map(|(_, file_type)| *file_type)
        .unwrap_or(DEFAULT_FILE_TYPE)
}

/// 生成边界串：时间戳 + 进程内序号，同一次进程里的两次上传不会撞车。
pub fn new_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        "omnicrawl-{nanos:x}-{:x}",
        COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

/// 组装 multipart 正文：先文本字段（按给定顺序），再文件字段，最后收尾边界。
pub fn multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file_field: &str,
    file_name: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{file_name}\"\r\nContent-Type: {FILE_PART_CONTENT_TYPE}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// 上传请求的 `Content-Type` 头。
pub fn multipart_content_type(boundary: &str) -> String {
    format!("multipart/form-data; boundary={boundary}")
}
