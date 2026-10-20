//! HSTS（RFC 6797）：站点声明过 `Strict-Transport-Security` 之后，后续明文访问一律升级到 https。
//!
//! 为什么必须做：真实浏览器对声明过 HSTS 的站点**不再发出任何明文请求**。本实现若不升级，
//! 抓取同一站点时会持续走 http，得到的是一段真实浏览器根本不会经历的交互（可能是 301
//! 跳转、可能是站点的明文降级页），抓到的正文与浏览器看到的不同。
//!
//! 三条安全属性，缺一条就不是 HSTS：
//! - **只从 https 响应学习**：明文响应里的 STS 头必须忽略，否则链路上的中间人可以随手注入
//!   一条 `max-age=31536000` 把目标锁死。
//! - **升级后不回退**：升级到 https 后若握手失败，如实报错，绝不悄悄退回明文。
//! - **`max-age=0` 是撤销**：收到即删除策略，不是「不过期」。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 记住的主机上限：够覆盖一次工具调用涉及的站点，超出整体清空。
const MAX_CACHED_HOSTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Policy {
    /// 过期时刻（Unix 秒）。存绝对时间而不是 `max-age`：`max-age` 要用时才知道过没过期。
    expires_at: u64,
    include_subdomains: bool,
}

fn store() -> &'static Mutex<HashMap<String, Policy>> {
    static STORE: OnceLock<Mutex<HashMap<String, Policy>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// 一条 `Strict-Transport-Security` 头的语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    /// `max-age` 的秒数。
    pub max_age: u64,
    pub include_subdomains: bool,
}

/// 解析 STS 头；没有合法 `max-age` 时返回 `None`（整条头按无效忽略）。
///
/// `max-age` 是唯一必需项：缺少它或写得不是非负整数，整条头都无效——不能只忽略那一个
/// 指令，因为「没有 max-age」的 HSTS 头在规范里就是无效头。
pub fn parse_directive(value: &str) -> Option<Directive> {
    let mut max_age: Option<u64> = None;
    let mut include_subdomains = false;
    for token in value.split(';') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (name, argument) = match token.split_once('=') {
            Some((name, argument)) => (name.trim(), Some(argument.trim())),
            None => (token, None),
        };
        if name.eq_ignore_ascii_case("max-age") {
            let argument = argument?;
            // 规范只允许非负整数；带引号的值不是合法语法，按无效处理。
            if argument.is_empty() || !argument.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            max_age = Some(argument.parse::<u64>().unwrap_or(u64::MAX));
        } else if name.eq_ignore_ascii_case("includesubdomains") {
            include_subdomains = true;
        }
        // `preload` 只影响能否进浏览器内置的预加载列表，不改变运行时语义，因此不记录。
    }
    Some(Directive {
        max_age: max_age?,
        include_subdomains,
    })
}

/// 从一次响应里学习 HSTS 策略。
///
/// 明文来源直接忽略（见模块文档）；同一响应里出现多条时取最后一条有效值，
/// 与浏览器逐条处理的行为一致。
pub fn record(from_url: &str, headers: &[(String, String)]) {
    let Some((scheme, _)) = from_url.split_once("://") else {
        return;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return;
    }
    let Some(host) = host_of(from_url) else {
        return;
    };
    let mut learned: Option<Directive> = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("strict-transport-security") {
            continue;
        }
        if let Some(directive) = parse_directive(value) {
            learned = Some(directive);
        }
    }
    let Some(directive) = learned else {
        return;
    };
    let Ok(mut store) = store().lock() else {
        return;
    };
    if directive.max_age == 0 {
        // `max-age=0` 是撤销：把这条策略删掉，而不是记一条立刻过期的条目。
        store.remove(&host);
        return;
    }
    if store.len() >= MAX_CACHED_HOSTS && !store.contains_key(&host) {
        store.clear();
    }
    store.insert(
        host,
        Policy {
            expires_at: now_seconds().saturating_add(directive.max_age),
            include_subdomains: directive.include_subdomains,
        },
    );
}

/// 该主机是否处于 HSTS 之下。
pub fn is_protected(host: &str) -> bool {
    let host = normalize_host(host);
    if host.is_empty() {
        return false;
    }
    let Ok(store) = store().lock() else {
        return false;
    };
    if let Some(policy) = store.get(&host) {
        // 过期的策略等于不存在。
        if policy.expires_at > now_seconds() {
            return true;
        }
    }
    // 逐级向上找 `includeSubDomains` 的祖先策略；`notexample.com` 不匹配 `example.com`。
    let mut rest = host.as_str();
    while let Some(index) = rest.find('.') {
        rest = &rest[index + 1..];
        if let Some(policy) = store.get(rest) {
            if policy.include_subdomains && policy.expires_at > now_seconds() {
                return true;
            }
        }
    }
    false
}

/// 需要升级时返回改写后的 https 地址，否则 `None`。
///
/// 端口规则同浏览器：明文的默认端口 80 升级后落到 https 的默认端口（不再显式写出来），
/// 非默认端口原样保留——`http://example.com:8080/` 升级后仍是 `:8080`。
pub fn upgrade(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    let (userinfo, host, port) = split_authority(authority)?;
    if !is_protected(host) {
        return None;
    }
    let mut upgraded = String::from("https://");
    if let Some(userinfo) = userinfo {
        upgraded.push_str(userinfo);
        upgraded.push('@');
    }
    // 主机名按 URL 序列化规则小写：浏览器把 `HTTP://EXAMPLE.COM/` 规范成小写主机。
    upgraded.push_str(&host.to_ascii_lowercase());
    match port {
        // 80 是明文的默认端口，升级后不该继续显式声明它。
        Some(80) | None => {}
        Some(port) => {
            upgraded.push(':');
            upgraded.push_str(&port.to_string());
        }
    }
    upgraded.push_str(tail);
    Some(upgraded)
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (_, host, _) = split_authority(&rest[..authority_end])?;
    let host = normalize_host(host);
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// 拆出 `userinfo@host:port`；端口不是合法数字时整段判为不可解析。
fn split_authority(authority: &str) -> Option<(Option<&str>, &str, Option<u16>)> {
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(index) => (Some(&authority[..index]), &authority[index + 1..]),
        None => (None, authority),
    };
    if hostport.is_empty() {
        return None;
    }
    // IPv6 字面量：`[::1]` 里的冒号不是端口分隔符。
    if hostport.starts_with('[') {
        let end = hostport.find(']')?;
        let host = &hostport[..=end];
        let after = &hostport[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(raw) => Some(raw.parse::<u16>().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        return Some((userinfo, host, port));
    }
    match hostport.rsplit_once(':') {
        Some((host, raw)) => {
            if host.is_empty() {
                return None;
            }
            Some((userinfo, host, Some(raw.parse::<u16>().ok()?)))
        }
        None => Some((userinfo, hostport, None)),
    }
}

/// 清空策略（测试用；生产路径不需要）。
#[cfg(test)]
pub(crate) fn reset() {
    if let Ok(mut store) = store().lock() {
        store.clear();
    }
}

/// 测试串行锁：策略是进程级全局状态，并发用例的 `reset()` 会互相清空。
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sts(value: &str) -> Vec<(String, String)> {
        vec![("strict-transport-security".to_string(), value.to_string())]
    }

    #[test]
    fn a_declared_site_is_upgraded_on_the_next_plain_request() {
        let _serial = test_guard();
        reset();
        record("https://hsts-a.example.com/page", &sts("max-age=31536000"));
        assert_eq!(
            upgrade("http://hsts-a.example.com/page?x=1").as_deref(),
            Some("https://hsts-a.example.com/page?x=1")
        );
        // 未声明的主机不受影响。
        assert_eq!(upgrade("http://hsts-plain.example.com/"), None);
        // 已经是 https 的不需要升级。
        assert_eq!(upgrade("https://hsts-a.example.com/page"), None);
    }

    /// 明文响应里的 STS 头必须忽略：否则链路上的中间人可以注入策略把目标锁死。
    #[test]
    fn a_plaintext_declaration_is_ignored() {
        let _serial = test_guard();
        reset();
        record("http://hsts-b.example.com/page", &sts("max-age=31536000"));
        assert_eq!(upgrade("http://hsts-b.example.com/page"), None);
    }

    #[test]
    fn include_subdomains_covers_children_but_not_lookalikes() {
        let _serial = test_guard();
        reset();
        record(
            "https://hsts-c.example.com/",
            &sts("max-age=600; includeSubDomains"),
        );
        assert_eq!(
            upgrade("http://api.hsts-c.example.com/x").as_deref(),
            Some("https://api.hsts-c.example.com/x")
        );
        // `nothsts-c.example.com` 的结尾不是 `.hsts-c.example.com`。
        assert_eq!(upgrade("http://nothsts-c.example.com/x"), None);
    }

    #[test]
    fn subdomains_are_not_covered_without_the_flag() {
        let _serial = test_guard();
        reset();
        record("https://hsts-d.example.com/", &sts("max-age=600"));
        assert_eq!(upgrade("http://api.hsts-d.example.com/x"), None);
        assert!(upgrade("http://hsts-d.example.com/x").is_some());
    }

    /// `max-age=0` 是撤销，不是「立刻过期的一条策略」——后者会让子域的旧策略继续生效。
    #[test]
    fn a_zero_max_age_revokes_the_policy() {
        let _serial = test_guard();
        reset();
        record("https://hsts-e.example.com/", &sts("max-age=31536000"));
        assert!(upgrade("http://hsts-e.example.com/").is_some());
        record("https://hsts-e.example.com/", &sts("max-age=0"));
        assert_eq!(upgrade("http://hsts-e.example.com/"), None);
    }

    /// 端口规则：80 升级后落到默认端口，非默认端口原样保留。
    #[test]
    fn the_port_survives_the_upgrade_except_the_plaintext_default() {
        let _serial = test_guard();
        reset();
        record("https://hsts-f.example.com/", &sts("max-age=600"));
        assert_eq!(
            upgrade("http://hsts-f.example.com:80/x").as_deref(),
            Some("https://hsts-f.example.com/x")
        );
        assert_eq!(
            upgrade("http://hsts-f.example.com:8080/x").as_deref(),
            Some("https://hsts-f.example.com:8080/x")
        );
        assert_eq!(
            upgrade("http://HSTS-F.example.com/x").as_deref(),
            Some("https://hsts-f.example.com/x"),
            "主机名要归一化后再重建"
        );
    }

    #[test]
    fn directives_without_a_valid_max_age_are_ignored_entirely() {
        assert_eq!(parse_directive("includeSubDomains"), None);
        assert_eq!(parse_directive("max-age=abc"), None);
        assert_eq!(parse_directive("max-age=\"600\""), None);
        assert_eq!(parse_directive("max-age=-1"), None);
        assert_eq!(
            parse_directive("max-age=600; includeSubDomains; preload"),
            Some(Directive {
                max_age: 600,
                include_subdomains: true
            })
        );
        // 大小写不敏感。
        assert_eq!(
            parse_directive("MAX-AGE=60; INCLUDESUBDOMAINS")
                .map(|directive| directive.include_subdomains),
            Some(true)
        );
    }

    /// 一条无效头不能抹掉同一响应里另一条有效头。
    #[test]
    fn an_invalid_declaration_does_not_cancel_a_valid_one() {
        let _serial = test_guard();
        reset();
        record(
            "https://hsts-g.example.com/",
            &[
                ("strict-transport-security".to_string(), "bogus".to_string()),
                (
                    "strict-transport-security".to_string(),
                    "max-age=600".to_string(),
                ),
            ],
        );
        assert!(upgrade("http://hsts-g.example.com/").is_some());
    }

    #[test]
    fn ipv6_literals_are_parsed_as_hosts() {
        let _serial = test_guard();
        reset();
        record("https://[::1]:8443/", &sts("max-age=600"));
        assert_eq!(
            upgrade("http://[::1]:8443/x").as_deref(),
            Some("https://[::1]:8443/x")
        );
        // 冒号不是端口分隔符，主机必须整体识别。
        assert!(split_authority("[::1]").is_some());
        assert_eq!(split_authority("[::1]").map(|item| item.1), Some("[::1]"));
    }
}
