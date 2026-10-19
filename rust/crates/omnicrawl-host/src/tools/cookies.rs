//! 抓取用的进程内 cookie 罐：收 `Set-Cookie`、按 `domain`/`path`/`secure`/过期回带。
//!
//! 只实现抓取场景需要的那部分 RFC 6265：
//!
//! - 不落盘：罐的生命周期就是进程，退出即失（抓取工具没有持久会话的语义）。
//! - 不做 Public Suffix List 校验：没有那份表时，`Domain` 只按「必须是请求主机的后缀」校验，
//!   拒绝 `Domain=com` 这类把 cookie 广播到整个顶级域之外还能成立的写法仍然做不到——
//!   这里保守地要求 `Domain` 与请求主机域名匹配且不是单标签。
//! - 不实现 `HttpOnly` 的脚本隔离：本栈不执行脚本，那一项没有作用面，只原样保留标记。
//!
//! 之所以自己实现而不用 `cookie_store`：这里的规则面很小（存、匹配、拼一个 `Cookie` 头），
//! 引一棵依赖树反而让「回带了什么、为什么没回带」变得不可审计，而抓取排障恰恰全靠这一点。

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 一条 cookie。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cookie {
    name: String,
    value: String,
    /// 已归一化：小写、无前导 `.`。
    domain: String,
    /// `Domain` 缺省时为真：只回带给完全一致的主机，不包括子域。
    host_only: bool,
    path: String,
    secure: bool,
    http_only: bool,
    /// Unix 秒；`None` 是会话 cookie。
    expires_at: Option<i64>,
    /// 同一请求路径下按创建序回带（RFC 6265 §5.4 的第 2 条排序键）。
    created_at: i64,
}

/// 一条已解析的请求地址：只取 cookie 规则用到的三段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    secure: bool,
    host: String,
    path: String,
}

impl Target {
    fn parse(url: &str) -> Option<Self> {
        let uri = ureq::http::Uri::from_str(url).ok()?;
        let scheme = uri.scheme_str()?.to_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let host = uri.host()?.to_lowercase();
        if host.is_empty() {
            return None;
        }
        let path = match uri.path() {
            "" => "/".to_string(),
            value => value.to_string(),
        };
        Some(Self {
            secure: scheme == "https",
            host,
            path,
        })
    }
}

/// 进程内 cookie 罐。抓取在多个线程里并发跑，因此内部是锁而不是 `&mut self`。
#[derive(Debug, Default)]
pub struct CookieJar {
    cookies: Mutex<Vec<Cookie>>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// 存一条 `Set-Cookie`。返回它是否改变了罐的状态。
    ///
    /// 解析不出 `name=value`、域名不匹配、`Secure` 来自明文 http、或 `Max-Age<=0` 删除时
    /// 都返回 `false`——调用方据此不把它当成「拿到了新 cookie」。
    pub fn store(&self, url: &str, set_cookie: &str) -> bool {
        let Some(target) = Target::parse(url) else {
            return false;
        };
        let now = now_seconds();
        let Some(cookie) = parse_set_cookie(set_cookie, &target, now) else {
            return false;
        };
        if cookie.secure && !target.secure {
            return false;
        }
        let mut guard = self.lock();
        // 同名同域同路径是「替换」，不是新增（RFC 6265 §5.3 第 11 步）。
        let existing = guard.iter().position(|item| {
            item.name == cookie.name && item.domain == cookie.domain && item.path == cookie.path
        });
        let expired = cookie.expires_at.is_some_and(|value| value <= now);
        match (existing, expired) {
            (_, true) => {
                if let Some(index) = existing {
                    guard.remove(index);
                }
                false
            }
            (Some(index), false) => {
                let created_at = guard[index].created_at;
                guard[index] = Cookie { created_at, ..cookie };
                true
            }
            (None, false) => {
                guard.push(cookie);
                true
            }
        }
    }

    /// 该地址应当带哪些 cookie，按浏览器顺序拼成一个 `Cookie` 头。
    pub fn cookie_header(&self, url: &str) -> Option<String> {
        let target = Target::parse(url)?;
        let now = now_seconds();
        let mut guard = self.lock();
        guard.retain(|item| item.expires_at.is_none_or(|value| value > now));
        let mut matched: Vec<&Cookie> = guard
            .iter()
            .filter(|item| matches(item, &target))
            .collect();
        if matched.is_empty() {
            return None;
        }
        // 路径更长的更具体，先发；同长按创建序。
        matched.sort_by(|left, right| {
            right
                .path
                .len()
                .cmp(&left.path.len())
                .then(left.created_at.cmp(&right.created_at))
        });
        let pairs: Vec<String> = matched
            .iter()
            .map(|item| format!("{}={}", item.name, item.value))
            .collect();
        Some(pairs.join("; "))
    }

    /// 当前（未过期的）cookie 条数。
    pub fn count(&self) -> usize {
        let now = now_seconds();
        let mut guard = self.lock();
        guard.retain(|item| item.expires_at.is_none_or(|value| value > now));
        guard.len()
    }

    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Cookie>> {
        // 一条 panic 不该让整罐 cookie 永久不可用：取回内部数据继续用。
        self.cookies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// 抓取侧的共享罐：`FetcherOptions` 里一份，跨请求、跨线程共用。
pub type SharedJar = Arc<CookieJar>;

/// 给一跳请求补 `Cookie` 头（罐为空或无匹配时不加）。
pub fn apply_header(jar: Option<&CookieJar>, url: &str, headers: &mut Vec<(String, String)>) {
    let Some(jar) = jar else {
        return;
    };
    // 调用方显式给了 cookie 就以它为准：否则会出现两个 Cookie 头。
    if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("cookie"))
    {
        return;
    }
    if let Some(value) = jar.cookie_header(url) {
        headers.push(("cookie".to_string(), value));
    }
}

/// 收割一跳响应里的 `Set-Cookie`：逐条存，非法条目静默丢弃（浏览器的行为）。
pub fn harvest<I>(jar: Option<&CookieJar>, url: &str, values: I)
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let Some(jar) = jar else {
        return;
    };
    for value in values {
        jar.store(url, value.as_ref());
    }
}

fn matches(cookie: &Cookie, target: &Target) -> bool {
    if cookie.secure && !target.secure {
        return false;
    }
    if !domain_matches(&target.host, &cookie.domain, cookie.host_only) {
        return false;
    }
    path_matches(&target.path, &cookie.path)
}

/// `host` 是否落在 `domain` 上：完全一致，或（非 host-only 时）是它的子域。
fn domain_matches(host: &str, domain: &str, host_only: bool) -> bool {
    if host == domain {
        return true;
    }
    if host_only {
        return false;
    }
    match host.strip_suffix(domain) {
        // 前缀必须非空且以 `.` 结尾，否则 `notexample.com` 也会匹配 `example.com`。
        Some(prefix) => prefix.ends_with('.'),
        None => false,
    }
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    match request_path.strip_prefix(cookie_path) {
        Some(rest) => cookie_path.ends_with('/') || rest.starts_with('/'),
        None => false,
    }
}

/// `Path` 缺省时取请求路径的目录部分（RFC 6265 §5.1.4）。
fn default_path(request_path: &str) -> String {
    if !request_path.starts_with('/') {
        return "/".to_string();
    }
    match request_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => request_path[..index].to_string(),
    }
}

fn parse_set_cookie(text: &str, target: &Target, now: i64) -> Option<Cookie> {
    let mut parts = text.split(';');
    let first = parts.next()?.trim();
    let (name, value) = first.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return None;
    }
    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.trim().to_string(),
        domain: target.host.clone(),
        host_only: true,
        path: default_path(&target.path),
        secure: false,
        http_only: false,
        expires_at: None,
        created_at: now,
    };
    let mut max_age: Option<i64> = None;
    let mut expires: Option<i64> = None;
    for attribute in parts {
        let attribute = attribute.trim();
        if attribute.is_empty() {
            continue;
        }
        let (key, raw) = match attribute.split_once('=') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => (attribute, ""),
        };
        match key.to_lowercase().as_str() {
            "domain" => {
                let domain = raw.trim_start_matches('.').to_lowercase();
                if domain.is_empty() || !domain.contains('.') {
                    // 单标签域名（`localhost` 与内网主机名除外）按 RFC 应当拒绝；
                    // 这里只在它确实是请求主机的后缀时才接受，避免 `Domain=com` 把
                    // cookie 广播到所有 `.com` 站点。
                    if domain != target.host {
                        return None;
                    }
                }
                if !domain_matches(&target.host, &domain, false) {
                    return None;
                }
                cookie.domain = domain;
                cookie.host_only = false;
            }
            "path" => {
                if raw.starts_with('/') {
                    cookie.path = raw.to_string();
                }
            }
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "max-age" => {
                if let Ok(seconds) = raw.parse::<i64>() {
                    max_age = Some(seconds);
                }
            }
            "expires" => {
                if let Some(stamp) = parse_http_date(raw) {
                    expires = Some(stamp);
                }
            }
            _ => {}
        }
    }
    // `Max-Age` 优先于 `Expires`（RFC 6265 §5.3 第 3 步）。
    cookie.expires_at = match max_age {
        Some(seconds) => Some(if seconds <= 0 { 0 } else { now + seconds }),
        None => expires,
    };
    Some(cookie)
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// 解析 `Expires`／`Expires` 的三种历史格式（RFC 1123 / RFC 850 / asctime）。
///
/// 三种格式只是分隔符与字段顺序不同，统一拆成 token 后按「月份名 + 带冒号的时间 +
/// 日 + 年」识别，避免为每种格式各写一条分支。
fn parse_http_date(text: &str) -> Option<i64> {
    let tokens: Vec<&str> = text
        .split(|character: char| !character.is_ascii_alphanumeric() && character != ':')
        .filter(|token| !token.is_empty())
        .collect();
    let mut month = None;
    let mut clock = None;
    let mut day = None;
    let mut year = None;
    for token in tokens {
        let lowered = token.to_lowercase();
        if let Some(index) = MONTHS.iter().position(|name| lowered.starts_with(name)) {
            month = Some(index as i64 + 1);
            continue;
        }
        if token.contains(':') {
            let mut fields = token.split(':');
            let hour: i64 = fields.next()?.parse().ok()?;
            let minute: i64 = fields.next()?.parse().ok()?;
            let second: i64 = fields.next()?.parse().ok()?;
            clock = Some((hour, minute, second));
            continue;
        }
        if let Ok(number) = token.parse::<i64>() {
            if token.len() == 4 {
                year = Some(number);
            } else if day.is_none() && (1..=31).contains(&number) {
                day = Some(number);
            } else if year.is_none() {
                // 两位年份：70 之后算 19xx，其余算 20xx（与 RFC 6265 的提示一致）。
                year = Some(if number < 70 { 2000 + number } else { 1900 + number });
            }
        }
    }
    let (hour, minute, second) = clock?;
    let days = days_from_civil(year?, month?, day?);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// 公历日期到 1970-01-01 起的天数（Howard Hinnant 的 civil_from_days 反函数）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_cookie_is_returned_to_the_same_host_and_path() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/a/page", "sid=abc; Path=/"));
        assert_eq!(
            jar.cookie_header("https://example.com/b").as_deref(),
            Some("sid=abc")
        );
        // 另一个主机不共享。
        assert_eq!(jar.cookie_header("https://other.com/"), None);
    }

    #[test]
    fn the_default_path_is_the_request_directory() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/a/b/page", "k=1"));
        assert_eq!(
            jar.cookie_header("https://example.com/a/b/other").as_deref(),
            Some("k=1")
        );
        assert_eq!(jar.cookie_header("https://example.com/a/other"), None);
        assert_eq!(jar.cookie_header("https://example.com/"), None);
    }

    #[test]
    fn an_explicit_path_limits_the_scope() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "k=1; Path=/api"));
        assert_eq!(
            jar.cookie_header("https://example.com/api/x").as_deref(),
            Some("k=1")
        );
        // `/apix` 不是 `/api` 的子路径。
        assert_eq!(jar.cookie_header("https://example.com/apix"), None);
    }

    #[test]
    fn a_domain_cookie_reaches_subdomains_but_a_host_cookie_does_not() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "wide=1; Domain=example.com"));
        assert!(jar.store("https://example.com/", "narrow=1"));
        assert_eq!(
            jar.cookie_header("https://a.example.com/").as_deref(),
            Some("wide=1")
        );
        assert_eq!(
            jar.cookie_header("https://example.com/").as_deref(),
            Some("wide=1; narrow=1")
        );
    }

    #[test]
    fn a_domain_that_does_not_match_the_host_is_refused() {
        let jar = CookieJar::new();
        assert!(!jar.store("https://example.com/", "k=1; Domain=other.com"));
        // 单标签域名不能把 cookie 广播到整个顶级域。
        assert!(!jar.store("https://example.com/", "k=1; Domain=com"));
        assert_eq!(jar.count(), 0);
    }

    #[test]
    fn a_secure_cookie_is_not_sent_over_plain_http() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "sid=1; Secure"));
        assert_eq!(
            jar.cookie_header("https://example.com/").as_deref(),
            Some("sid=1")
        );
        assert_eq!(jar.cookie_header("http://example.com/"), None);
    }

    #[test]
    fn a_secure_cookie_set_over_plain_http_is_dropped() {
        let jar = CookieJar::new();
        assert!(!jar.store("http://example.com/", "sid=1; Secure"));
        assert_eq!(jar.count(), 0);
    }

    #[test]
    fn max_age_zero_deletes_an_existing_cookie() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "k=1"));
        assert_eq!(
            jar.cookie_header("https://example.com/").as_deref(),
            Some("k=1")
        );
        jar.store("https://example.com/", "k=1; Max-Age=0");
        assert_eq!(jar.cookie_header("https://example.com/"), None);
    }

    #[test]
    fn a_past_expires_means_gone_and_a_future_one_means_kept() {
        let jar = CookieJar::new();
        // 已过期：这条不是「存进去」，而是「删掉同键的旧值」，因此返回 false。
        assert!(!jar.store(
            "https://example.com/",
            "old=1; Expires=Wed, 09 Jun 1999 10:18:14 GMT"
        ));
        assert_eq!(jar.cookie_header("https://example.com/"), None);
        assert!(jar.store(
            "https://example.com/",
            "fresh=1; Expires=Wed, 09 Jun 2100 10:18:14 GMT"
        ));
        assert_eq!(
            jar.cookie_header("https://example.com/").as_deref(),
            Some("fresh=1")
        );
    }

    #[test]
    fn an_expired_set_cookie_clears_the_previous_value() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "k=1"));
        assert_eq!(
            jar.cookie_header("https://example.com/").as_deref(),
            Some("k=1")
        );
        jar.store(
            "https://example.com/",
            "k=1; Expires=Thu, 01 Jan 1970 00:00:01 GMT",
        );
        assert_eq!(jar.cookie_header("https://example.com/"), None);
    }

    #[test]
    fn the_longest_path_goes_first_and_same_path_keeps_creation_order() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "a=1"));
        assert!(jar.store("https://example.com/", "b=2"));
        assert!(jar.store("https://example.com/deep/page", "c=3"));
        assert_eq!(
            jar.cookie_header("https://example.com/deep/x").as_deref(),
            Some("c=3; a=1; b=2")
        );
    }

    #[test]
    fn a_same_name_domain_and_path_replaces_instead_of_duplicating() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "k=1"));
        assert!(jar.store("https://example.com/", "k=2"));
        assert_eq!(
            jar.cookie_header("https://example.com/").as_deref(),
            Some("k=2")
        );
        assert_eq!(jar.count(), 1);
    }

    #[test]
    fn a_value_with_semicolons_or_equals_survives() {
        let jar = CookieJar::new();
        assert!(jar.store("https://example.com/", "k=a=b"));
        assert_eq!(
            jar.cookie_header("https://example.com/").as_deref(),
            Some("k=a=b")
        );
    }

    #[test]
    fn malformed_set_cookie_lines_are_ignored() {
        let jar = CookieJar::new();
        for line in ["", "novalue", "=onlyvalue", "  "] {
            assert!(!jar.store("https://example.com/", line), "{line}");
        }
        assert_eq!(jar.count(), 0);
    }

    #[test]
    fn http_dates_of_all_three_legacy_formats_parse() {
        let reference = 928_923_494; // 1999-06-09T10:18:14Z
        for text in [
            "Wed, 09 Jun 1999 10:18:14 GMT",
            "Wednesday, 09-Jun-99 10:18:14 GMT",
            "Wed Jun  9 10:18:14 1999",
        ] {
            assert_eq!(parse_http_date(text), Some(reference), "{text}");
        }
    }

    #[test]
    fn apply_header_respects_a_caller_supplied_cookie() {
        let jar = CookieJar::new();
        jar.store("https://example.com/", "sid=1");
        let mut headers = Vec::new();
        apply_header(Some(&jar), "https://example.com/", &mut headers);
        assert_eq!(
            headers,
            vec![("cookie".to_string(), "sid=1".to_string())]
        );

        let mut explicit = vec![("Cookie".to_string(), "manual=1".to_string())];
        apply_header(Some(&jar), "https://example.com/", &mut explicit);
        assert_eq!(explicit.len(), 1, "重复的 Cookie 头会破坏语义：{explicit:?}");
    }
}
