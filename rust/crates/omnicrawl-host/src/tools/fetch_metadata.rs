//! `Sec-Fetch-*` 请求头：按「这次请求到底是在干什么」推导，而不是写死导航取值。
//!
//! 这四个头是浏览器告诉服务端的**请求目的**（Fetch Metadata），服务端普遍据此做策略判断
//! （拒绝跨站的资源请求、只放行同站表单等）。写死取值的问题不是「像不像」，而是**说错话**：
//!
//! - `sec-fetch-site` 恒为 `none` 时，一次跨站重定向被描述成「用户直接在地址栏输入的」。
//!   服务端按 `none` 做的判断会与真实浏览器（`cross-site`）不同，可能放行本应拒绝的请求。
//! - `sec-fetch-mode: navigate` 搭配 JSON 请求体在浏览器里不存在：导航是文档获取，
//!   带 body 的是 `cors`/`empty` 那一类。这个组合本身就是自相矛盾。
//!
//! 因此本模块只做一件事：给定「发起方 + 目标 + 方法的用途」，算出该发的四个值。

/// 这次请求的用途。抓取工具只有两类：取文档（导航）与调 API（非导航）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// 顶层文档导航：地址栏输入、跟随链接、跟随重定向。
    Navigation,
    /// 直接调对方接口（`fetch`/XHR 那类），带 JSON 或二进制体。
    Api,
}

/// 推导出的四个头取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchMetadata {
    pub dest: String,
    pub mode: String,
    pub site: String,
    /// 只有导航才发 `Sec-Fetch-User: ?1`（用户主动发起的导航）。
    pub user: bool,
}

impl FetchMetadata {
    /// 按用途、发起方与目标计算。
    ///
    /// `initiator` 为 `None` 表示没有发起方——浏览器在地址栏直接输入就是这个状态，
    /// 此时 `sec-fetch-site` 是 `none`。
    pub fn compute(purpose: Purpose, initiator: Option<&str>, target: &str) -> Self {
        let site = site_of(initiator, target);
        match purpose {
            // 导航取的是文档，`mode` 恒为 `navigate`，`user` 表示这次导航由用户发起。
            Purpose::Navigation => Self {
                dest: "document".to_string(),
                mode: "navigate".to_string(),
                site,
                user: true,
            },
            // 调接口取的是空资源，`mode` 是 `cors`；不是导航，因此没有 `Sec-Fetch-User`。
            Purpose::Api => Self {
                dest: "empty".to_string(),
                mode: "cors".to_string(),
                site,
                user: false,
            },
        }
    }
}

/// `sec-fetch-site` 的四个取值，按发起方与目标的关系判定。
///
/// - `none`：没有发起方（地址栏输入、书签）。
/// - `same-origin`：源完全一致（scheme + host + port）。
/// - `same-site`：同站不同源（同可注册域、不同子域或端口）。
/// - `cross-site`：跨站。
///
/// 与 cookie 的 `SameSite` 共用同一套「站点」近似（见 [`super::cookies`] 的说明），
/// 因此 `a.co.uk` 与 `b.co.uk` 会被判成 `same-site` 而非 `cross-site`。
fn site_of(initiator: Option<&str>, target: &str) -> String {
    let Some(initiator) = initiator else {
        return "none".to_string();
    };
    // 发起方解析不出来时按跨站处理：宁可让服务端更谨慎，也不谎称同站。
    let (Some(source), Some(destination)) = (Endpoint::parse(initiator), Endpoint::parse(target))
    else {
        return "cross-site".to_string();
    };
    if source.origin() == destination.origin() {
        return "same-origin".to_string();
    }
    if registrable_site(&source.host) == registrable_site(&destination.host) {
        return "same-site".to_string();
    }
    "cross-site".to_string()
}

/// 近似可注册域：取最后两段（没有 PSL 表时的保守近似，与 cookie 的判定保持一致）。
fn registrable_site(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    if labels.len() <= 2 {
        return labels.join(".");
    }
    labels[labels.len() - 2..].join(".")
}

/// 参与判定的地址信息：scheme / host / port。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    scheme: String,
    host: String,
    port: u16,
}

impl Endpoint {
    fn parse(url: &str) -> Option<Self> {
        let trimmed = url.trim();
        let (scheme, rest) = trimmed.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let rest = rest.split(['#', '?']).next().unwrap_or(rest);
        let authority = match rest.find('/') {
            Some(index) => &rest[..index],
            None => rest,
        };
        // 去掉用户信息：判定站点用不到凭据。
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = split_authority(authority, &scheme)?;
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    fn origin(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }
}

fn default_port(scheme: &str) -> u16 {
    match scheme {
        "https" => 443,
        "http" => 80,
        _ => 0,
    }
}

fn split_authority<'a>(authority: &'a str, scheme: &str) -> Option<(&'a str, u16)> {
    // IPv6 字面量：`[::1]:8080`。
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(value) => value.parse::<u16>().ok()?.max(1),
            None => default_port(scheme).max(1),
        };
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|character| character.is_ascii_digit()) => {
            Some((host, port.parse::<u16>().ok()?.max(1)))
        }
        _ => Some((authority, default_port(scheme).max(1))),
    }
}

/// 给一跳请求写入 `sec-fetch-*`；调用方已显式给过的值不覆盖。
pub fn apply(headers: &mut Vec<(String, String)>, metadata: &FetchMetadata) {
    // 先算完再写：闭包里借用 `headers` 会与随后的 `push` 冲突。
    let present: [bool; 4] = [
        "sec-fetch-dest",
        "sec-fetch-mode",
        "sec-fetch-site",
        "sec-fetch-user",
    ]
    .map(|name| {
        headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    });
    let [has_dest, has_mode, has_site, has_user] = present;
    if !has_dest {
        headers.push(("sec-fetch-dest".to_string(), metadata.dest.clone()));
    }
    if !has_mode {
        headers.push(("sec-fetch-mode".to_string(), metadata.mode.clone()));
    }
    if !has_site {
        headers.push(("sec-fetch-site".to_string(), metadata.site.clone()));
    }
    if metadata.user && !has_user {
        headers.push(("sec-fetch-user".to_string(), "?1".to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_initiator_is_reported_as_none() {
        let metadata = FetchMetadata::compute(Purpose::Navigation, None, "https://example.com/a");
        assert_eq!(metadata.site, "none");
        assert_eq!(metadata.dest, "document");
        assert_eq!(metadata.mode, "navigate");
    }

    #[test]
    fn identical_origins_are_same_origin() {
        let metadata = FetchMetadata::compute(
            Purpose::Navigation,
            Some("https://example.com/from"),
            "https://example.com/to",
        );
        assert_eq!(metadata.site, "same-origin");
    }

    /// 同站不同源：子域或端口不同都算 `same-site`，不是 `same-origin`。
    #[test]
    fn subdomains_and_ports_are_same_site() {
        for (source, target) in [
            ("https://api.example.com/a", "https://www.example.com/b"),
            ("https://example.com:8443/a", "https://example.com/b"),
        ] {
            let metadata = FetchMetadata::compute(Purpose::Navigation, Some(source), target);
            assert_eq!(metadata.site, "same-site", "{source} -> {target}");
        }
    }

    #[test]
    fn different_registrable_domains_are_cross_site() {
        let metadata = FetchMetadata::compute(
            Purpose::Navigation,
            Some("https://example.com/a"),
            "https://other.com/b",
        );
        assert_eq!(metadata.site, "cross-site");
    }

    /// scheme 不同即源不同，但仍是同一个站点。
    #[test]
    fn scheme_change_is_same_site_not_same_origin() {
        let metadata = FetchMetadata::compute(
            Purpose::Navigation,
            Some("https://example.com/a"),
            "http://example.com/b",
        );
        assert_eq!(metadata.site, "same-site");
    }

    /// 解析不出发起方时按跨站处理：宁可让服务端更谨慎。
    #[test]
    fn an_unparseable_initiator_is_cross_site() {
        let metadata = FetchMetadata::compute(
            Purpose::Navigation,
            Some("not a url"),
            "https://example.com/",
        );
        assert_eq!(metadata.site, "cross-site");
    }

    /// 调接口不能声称自己是导航：`navigate` + JSON body 在浏览器里不存在。
    #[test]
    fn api_calls_do_not_claim_to_be_navigation() {
        let metadata = FetchMetadata::compute(Purpose::Api, None, "https://api.example.com/v1");
        assert_eq!(metadata.dest, "empty");
        assert_eq!(metadata.mode, "cors");
        assert!(!metadata.user, "非导航不该发 Sec-Fetch-User");
    }

    #[test]
    fn apply_writes_all_four_values_for_navigation() {
        let mut headers = Vec::new();
        let metadata = FetchMetadata::compute(Purpose::Navigation, None, "https://example.com/");
        apply(&mut headers, &metadata);
        let value = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or("")
        };
        assert_eq!(value("sec-fetch-dest"), "document");
        assert_eq!(value("sec-fetch-mode"), "navigate");
        assert_eq!(value("sec-fetch-site"), "none");
        assert_eq!(value("sec-fetch-user"), "?1");
    }

    #[test]
    fn apply_leaves_caller_supplied_values_alone() {
        let mut headers = vec![("Sec-Fetch-Site".to_string(), "same-origin".to_string())];
        let metadata = FetchMetadata::compute(Purpose::Api, None, "https://example.com/");
        apply(&mut headers, &metadata);
        let sites: Vec<&String> = headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("sec-fetch-site"))
            .map(|(_, value)| value)
            .collect();
        assert_eq!(sites, vec!["same-origin"], "不该重复写：{headers:?}");
    }

    #[test]
    fn api_calls_get_no_user_header() {
        let mut headers = Vec::new();
        let metadata = FetchMetadata::compute(Purpose::Api, None, "https://example.com/");
        apply(&mut headers, &metadata);
        assert!(
            !headers.iter().any(|(key, _)| key == "sec-fetch-user"),
            "{headers:?}"
        );
    }
}
