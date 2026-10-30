//! 脚本收集与地址解析：从 HTML 里挑出可执行脚本，并把外链解析成绝对地址。
//!
//! 从 `engine.rs` 拆出。这一段的判据（协议白名单、站点相对路径的拼法）
//! 与沙箱求值无关，改动时只需要看这一个文件。

use super::*;

/// 收集脚本：按文档顺序保留内联与外链，跳过非 JS 数据块与空体。
///
/// `base` 用于把 `src` 解析成绝对地址；解析失败的外链直接丢弃（浏览器同样不会去取）。
fn collect_scripts(html: &str, base: &str) -> Vec<ScriptRef> {
    let Ok(pattern) = regex::Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script\s*>") else {
        return Vec::new();
    };
    let Ok(type_pattern) = regex::Regex::new(r#"(?i)\btype\s*=\s*["']?([^"'\s>]+)"#) else {
        return Vec::new();
    };
    let Ok(src_pattern) = regex::Regex::new(r#"(?i)\bsrc\s*=\s*["']([^"']+)["']"#) else {
        return Vec::new();
    };
    let mut scripts = Vec::new();
    for capture in pattern.captures_iter(html) {
        let attrs = &capture[1];
        let mut module = false;
        if let Some(kind) = type_pattern
            .captures(attrs)
            .map(|capture| capture[1].to_ascii_lowercase())
        {
            // JSON-LD、模板等数据块（`application/ld+json`、`text/template`）不是脚本。
            if kind.contains("module") {
                module = true;
            } else if !kind.contains("javascript") && !kind.contains("ecmascript") {
                continue;
            }
        }
        let defer = attrs.to_ascii_lowercase().contains("defer");
        if let Some(target) = src_pattern.captures(attrs) {
            if let Some(resolved) = resolve_script_url(base, &target[1]) {
                scripts.push(ScriptRef {
                    kind: ScriptKind::External,
                    body: resolved,
                    defer,
                    module,
                });
            }
            continue;
        }
        let body = capture[2].trim();
        if body.is_empty() || body.len() > MAX_SCRIPT_BYTES {
            continue;
        }
        scripts.push(ScriptRef {
            kind: ScriptKind::Inline,
            body: body.to_string(),
            defer,
            module,
        });
    }
    scripts
}

/// 把 `src` 解析成绝对地址：只接受 `http(s)` 与站点相对路径，拒绝其它协议。
///
/// 外链脚本是唯一会被沙箱「再去取一次」的东西，因此协议白名单在这里就收紧：
/// `javascript:`、`data:`、`file:` 一律不认，避免把本地文件或脚本协议当成取数目标。
fn resolve_script_url(base: &str, target: &str) -> Option<String> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    if target.starts_with("//") {
        let scheme = base.split(':').next().unwrap_or("https");
        return Some(format!("{scheme}:{target}"));
    }
    if let Some((scheme, rest)) = target.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let _ = rest;
        return Some(target.to_string());
    }
    // 带冒号但没有 `//` 的（`javascript:` / `data:` / `mailto:`）不是可取的外链。
    if target.contains(':') {
        return None;
    }
    let (scheme, authority) = split_origin(base)?;
    if target.starts_with('/') {
        return Some(format!("{scheme}//{authority}{target}"));
    }
    // 相对路径：从 base 的目录部分接上。
    let path = base
        .split("://")
        .nth(1)
        .and_then(|rest| rest.find('/').map(|index| &rest[index..]))
        .unwrap_or("/");
    let directory = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    };
    Some(format!("{scheme}//{authority}{directory}{target}"))
}

/// 拆出 `scheme:` 与 `authority`（`host[:port]`）。
fn split_origin(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = format!("{scheme}:");
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    Some((scheme, authority))
}
