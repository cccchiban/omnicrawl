//! Client Hints 协商（RFC 8942 的 `Accept-CH`）：服务端声明之后才发对应的提示头。
//!
//! 为什么必须协商而不是一律发送：
//! - 真实浏览器只在站点通过 `Accept-CH`（或 `Critical-CH`）声明后，才在**后续请求**里
//!   带上这些高熵提示头。没被要求就先发，等于自报「我不是普通浏览器」。
//! - `sec-ch-ua-platform-version` 属高熵提示，一律预发是最典型的自动化特征之一。
//!
//! 因此本模块只做两件事：记住「哪个 origin 声明了哪些提示」，以及在后续请求里按声明补头。
//! 没有声明就不发——宁可少发，也不发对方没要的。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// 高熵提示的上限：避免被异常站点用超长列表撑爆内存。
const MAX_TOKENS_PER_ORIGIN: usize = 32;
/// 记住的 origin 上限：够覆盖一次工具调用涉及的站点，超出整体清空。
const MAX_CACHED_ORIGINS: usize = 256;

/// 本实现能够**如实提供值**的提示头。
///
/// 只列真有值可给的项：`sec-ch-ua-arch`/`bitness`/`model`/`full-version` 会随设备与系统
/// 变化，本实现没有对应的真实来源，凭空编一个比不发更糟——因此不列在这里。
/// 声明了却发不出去，会让协商状态与实际请求不一致，这正是要避免的。
const SUPPORTED_HINTS: [&str; 4] = [
    "sec-ch-ua",
    "sec-ch-ua-mobile",
    "sec-ch-ua-platform",
    "sec-ch-ua-platform-version",
];

fn store() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static STORE: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 协商用的 origin 键：`scheme://host[:port]`。
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

/// 从响应头里读出 `Accept-CH` / `Critical-CH` 声明并记住（针对该响应对应的 origin）。
///
/// 两个头都可能出现多次，且值可以是逗号分隔列表。
pub fn record(from_url: &str, headers: &[(String, String)]) {
    let Some(origin) = origin_of(from_url) else {
        return;
    };
    let declared: Vec<String> = headers
        .iter()
        .filter(|(name, _)| {
            name.eq_ignore_ascii_case("accept-ch") || name.eq_ignore_ascii_case("critical-ch")
        })
        .flat_map(|(_, value)| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    if declared.is_empty() {
        return;
    }
    if let Ok(mut store) = store().lock() {
        if store.len() >= MAX_CACHED_ORIGINS && !store.contains_key(&origin) {
            store.clear();
        }
        let entry = store.entry(origin).or_default();
        for token in declared {
            if entry.len() >= MAX_TOKENS_PER_ORIGIN {
                break;
            }
            entry.insert(token);
        }
    }
}

/// 该 URL 所属 origin 已声明、且本实现能如实提供的提示头。
///
/// 是否**发得出去**还取决于浏览器族：Firefox 没有 Client Hints 体系（`client_hints` 为
/// `None`），Safari 也没有。因此这里只回答「服务端要了、本实现也有值」，
/// 最终由 `client_hint_value` 按族决定给不给值。
pub fn hints_for(url: &str) -> Vec<String> {
    let Some(origin) = origin_of(url) else {
        return Vec::new();
    };
    let declared = store()
        .lock()
        .ok()
        .and_then(|store| store.get(&origin).cloned())
        .unwrap_or_default();
    SUPPORTED_HINTS
        .iter()
        .filter(|hint| declared.contains(**hint))
        .map(|hint| (*hint).to_string())
        .collect()
}

/// 平台版本：Windows 取内核主版本（10/11 内核均是 10.0.0）。
///
/// 浏览器这里给的是**平台的真实版本**，不是随便编的数字；本实现在编译期确定目标平台，
/// 因此只能给出该平台对应的固定值。非 Windows 目标返回 `None`——宁可不发。
pub fn platform_version(family: &str) -> Option<&'static str> {
    // 只对声明了 Client Hints 的族生效（Safari 没有这套头）。
    if family == "safari" {
        return None;
    }
    if cfg!(target_os = "windows") {
        Some("\"10.0.0\"")
    } else if cfg!(target_os = "macos") {
        Some("\"14.0.0\"")
    } else {
        None
    }
}

/// 清空协商状态（测试用；生产路径不需要）。
#[cfg(test)]
pub(crate) fn reset() {
    if let Ok(mut store) = store().lock() {
        store.clear();
    }
}

/// 测试串行锁：协商状态是进程级全局变量，用例并发跑时彼此的 `reset()` 会互相清空，
/// 表现为「单独跑通过、全量跑挂」。凡读写该状态的用例都必须先拿这把锁。
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn hints_are_sent_only_after_the_site_declares_them() {
        let _serial = super::test_guard();
        reset();
        let url = "https://hints-a.example.com/page";
        // 未声明前一瓶不发。
        assert!(hints_for(url).is_empty());

        record(url, &[header("accept-ch", "Sec-CH-UA-Platform-Version")]);
        assert_eq!(hints_for(url), vec!["sec-ch-ua-platform-version"]);
    }

    #[test]
    fn declarations_are_scoped_to_a_single_origin() {
        let _serial = super::test_guard();
        reset();
        record(
            "https://hints-b.example.com/page",
            &[header("accept-ch", "sec-ch-ua-platform-version")],
        );
        assert_eq!(
            hints_for("https://hints-b.example.com/other"),
            vec!["sec-ch-ua-platform-version"]
        );
        // 另一个 origin 没被声明，不得跟着发。
        assert!(hints_for("https://hints-c.example.com/page").is_empty());
        // 同一主机的不同 scheme 也是不同 origin。
        assert!(hints_for("http://hints-b.example.com/page").is_empty());
    }

    #[test]
    fn critical_ch_counts_as_a_declaration() {
        let _serial = super::test_guard();
        reset();
        let url = "https://hints-d.example.com/page";
        record(url, &[header("critical-ch", "sec-ch-ua-platform-version")]);
        assert_eq!(hints_for(url), vec!["sec-ch-ua-platform-version"]);
    }

    #[test]
    fn unsupported_hints_are_never_sent_even_if_declared() {
        let _serial = super::test_guard();
        reset();
        let url = "https://hints-e.example.com/page";
        record(
            url,
            &[header(
                "accept-ch",
                "sec-ch-ua-platform-version, sec-ch-prefers-color-scheme, width",
            )],
        );
        // 本实现能如实提供的只有 platform-version。
        assert_eq!(hints_for(url), vec!["sec-ch-ua-platform-version"]);
    }

    /// 协商状态与「真能发出的头」必须一致：声明了却发不出去，等于状态说谎。
    ///
    /// 这条不变量由真实站点触发：Bing 会声明 `Sec-CH-UA-Arch`/`Bitness`/`Model`/
    /// `Full-Version`，而本实现没有这些值——因此它们不该进 `SUPPORTED_HINTS`。
    ///
    /// 同时按族校验：Firefox/Safari 没有 Client Hints 体系，即便服务端声明了也不发。
    #[test]
    fn every_supported_hint_has_a_real_value_to_send() {
        let _serial = super::test_guard();
        reset();
        let url = "https://hints-invariant.example.com/page";
        // 让所有支持的提示都被声明。
        let declared = SUPPORTED_HINTS.join(", ");
        record(url, &[header("accept-ch", &declared)]);
        let hints = hints_for(url);
        assert_eq!(hints.len(), SUPPORTED_HINTS.len(), "支持列表应全部生效");

        // 有 Client Hints 体系的族：每一项都必须给出真实值。
        for family in ["chrome", "edge"] {
            for hint in &hints {
                assert!(
                    crate::tools::tls_transport::client_hint_value(hint, family).is_some(),
                    "{family} 声明了 {hint} 却给不出值"
                );
            }
        }
        // 没有该体系的族：不应发出任何高熵提示。
        for family in ["firefox", "safari"] {
            let sent: Vec<&String> = hints
                .iter()
                .filter(|hint| {
                    crate::tools::tls_transport::client_hint_value(hint, family).is_some()
                })
                .collect();
            assert!(
                sent.is_empty(),
                "{family} 没有 Client Hints 体系，却会发出：{sent:?}"
            );
        }
    }

    #[test]
    fn multiple_declarations_are_merged_and_deduplicated() {
        let _serial = super::test_guard();
        reset();
        let url = "https://hints-f.example.com/page";
        record(url, &[header("accept-ch", "sec-ch-ua")]);
        record(
            url,
            &[
                header("accept-ch", "sec-ch-ua-platform-version"),
                header("accept-ch", "sec-ch-ua"),
            ],
        );
        let hints = hints_for(url);
        assert_eq!(hints, vec!["sec-ch-ua", "sec-ch-ua-platform-version"]);
    }

    #[test]
    fn empty_declaration_changes_nothing() {
        let _serial = super::test_guard();
        reset();
        let url = "https://hints-g.example.com/page";
        record(url, &[header("accept-ch", "")]);
        assert!(hints_for(url).is_empty());
    }

    #[test]
    fn platform_version_is_absent_for_safari() {
        let _serial = super::test_guard();
        assert!(platform_version("safari").is_none());
        // 其余族在当前目标平台上有值（Claude 在 Windows 上跑，只断言不 panic 的形态）。
        for family in ["chrome", "edge", "firefox"] {
            let value = platform_version(family);
            if cfg!(any(target_os = "windows", target_os = "macos")) {
                assert!(value.is_some(), "{family} 应当有平台版本");
                assert!(value.unwrap().starts_with('"'), "{family} 值需要带引号");
            }
        }
    }
}
