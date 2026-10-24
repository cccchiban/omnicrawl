//! `Referer` 头的计算：按 `Referrer-Policy` 决定把发起方的多少信息交给目标。
//!
//! 为什么必须做：真实浏览器点链接、跟随重定向、发子资源请求都会带 `Referer`，一个二级请求
//! 却不带它是很突出的特征。但这件事的价值不在「像不像浏览器」，而在**语义正确**：`Referer`
//! 受同源策略约束，跨源时默认只交源（scheme + host + port），同源时才交完整路径。写错方向
//! 会把发起方页面的路径（可能含会话标识、内网结构）泄给第三方。
//!
//! 默认策略是 `strict-origin-when-cross-origin`——这是当前所有主流浏览器的默认值：
//! 同源交全路径，跨源只交源，且**从 https 降到 http 时什么都不交**（防止把安全页面的
//! URL 通过明文链路漏出去）。
//!
//! 服务端可以用 `Referrer-Policy` 响应头声明自己希望怎么被传递，本模块按响应头更新策略
//! （与浏览器一致，见 [`policy_from_response`]）。唯一不接受的取值是 `unsafe-url`：
//! 那个档位要求把完整 URL 交给任意目标、连降级都不设防，本实现不提供。

/// 规范化后的策略取值（不含 `unsafe-url`，见 [`Policy::parse`]）。
///
/// 声明顺序就是**严格程度**：越靠前越严。这个顺序来自 Referrer Policy 规范里
/// 「从最严到最松」的列举，不要凭直觉排——`strict-origin`（同源也只发源）比
/// `strict-origin-when-cross-origin`（同源发完整 URL）更严，两者写反会让收窄判定失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Policy {
    /// 任何情况都不发。
    NoReferrer,
    /// 只在同源时发，且发完整 URL（跨源完全不发）。
    SameOrigin,
    /// 只发源，且降级（https→http）时不发。
    StrictOrigin,
    /// 同源发完整，跨源发源，降级时不发。
    StrictOriginWhenCrossOrigin,
    /// 只发源（降级也发）。
    Origin,
    /// 同源发完整，跨源发源，降级也发。
    OriginWhenCrossOrigin,
    /// 完整 URL 都发，只有降级时不发。
    NoReferrerWhenDowngrade,
}

impl Policy {
    /// 浏览器默认策略。
    pub const DEFAULT: Policy = Policy::StrictOriginWhenCrossOrigin;

    /// 从响应头或 meta 的原始取值解析；认不出时返回 `None`（调用方沿用当前策略）。
    ///
    /// `unsafe-url` 明确不实现：它会把完整 URL（含查询串、可能含会话标识）交给任意目标，
    /// 连降级都不设防，安全上不可接受。
    pub fn parse(raw: &str) -> Option<Policy> {
        // 取值可能是一个逗号分隔列表（`strict-origin, origin`），取第一项有效值。
        let token = raw
            .split(',')
            .map(str::trim)
            .find(|item| !item.is_empty())?
            .to_ascii_lowercase();
        Some(match token.as_str() {
            "no-referrer" => Policy::NoReferrer,
            "same-origin" => Policy::SameOrigin,
            "strict-origin" => Policy::StrictOrigin,
            "strict-origin-when-cross-origin" => Policy::StrictOriginWhenCrossOrigin,
            "origin" => Policy::Origin,
            "origin-when-cross-origin" => Policy::OriginWhenCrossOrigin,
            "no-referrer-when-downgrade" => Policy::NoReferrerWhenDowngrade,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Policy::NoReferrer => "no-referrer",
            Policy::SameOrigin => "same-origin",
            Policy::StrictOrigin => "strict-origin",
            Policy::StrictOriginWhenCrossOrigin => "strict-origin-when-cross-origin",
            Policy::Origin => "origin",
            Policy::OriginWhenCrossOrigin => "origin-when-cross-origin",
            Policy::NoReferrerWhenDowngrade => "no-referrer-when-downgrade",
        }
    }
}

/// 按当前策略与目标计算出该发的 `Referer` 值；不该发时返回 `None`。
///
/// `initiator` 是发起方地址（不含片段），`target` 是本次请求的目标地址。
pub fn referer_for(policy: Policy, initiator: &str, target: &str) -> Option<String> {
    if policy == Policy::NoReferrer {
        return None;
    }
    let source = Endpoint::parse(initiator)?;
    let destination = Endpoint::parse(target)?;
    let same_origin = source.origin() == destination.origin();
    let downgrade = source.scheme == "https" && destination.scheme == "http";

    let value = match policy {
        Policy::NoReferrer => return None,
        Policy::SameOrigin => {
            if !same_origin {
                return None;
            }
            // 同源时交完整 URL——「只要源」是 `strict-origin` 的职责，不是这个策略的。
            source.path_and_query()
        }
        Policy::Origin => source.origin(),
        Policy::StrictOrigin => {
            // 降级时连源都不交：否则明文链路会暴露「用户从哪个 https 站点来」。
            if downgrade {
                return None;
            }
            source.origin()
        }
        Policy::StrictOriginWhenCrossOrigin => {
            if downgrade {
                return None;
            }
            if same_origin {
                source.path_and_query()
            } else {
                source.origin()
            }
        }
        Policy::OriginWhenCrossOrigin => {
            if same_origin {
                source.path_and_query()
            } else {
                source.origin()
            }
        }
        Policy::NoReferrerWhenDowngrade => {
            // 浏览器历史默认：完整 URL 都发，只有降级时退化为不发。
            if downgrade {
                return None;
            }
            source.path_and_query()
        }
    };
    Some(value)
}

/// 用响应头里的新策略更新当前策略，返回更新后的值。
///
/// 直接采纳站点的声明（这是浏览器的行为：`Referrer-Policy` 是站点对自己的 URL
/// 怎么被传递的要求）。只有一种例外——`unsafe-url` 不被接受：它要求把完整 URL
/// 交给任意目标、连降级都不设防，本实现不提供这个档位，因此保持原策略。
pub fn adopt(current: Policy, raw: &str) -> Policy {
    Policy::parse(raw).unwrap_or(current)
}

/// 参与策略判定的一段地址：只需要 scheme / host / port / path+query。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    scheme: String,
    host: String,
    port: u16,
    /// 已带上查询串，不含片段。
    path_and_query: String,
}

impl Endpoint {
    fn parse(url: &str) -> Option<Self> {
        let trimmed = url.trim();
        let (scheme, rest) = trimmed.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        // 去掉片段：`Referer` 从不包含 `#fragment`。
        let rest = rest.split('#').next().unwrap_or(rest);
        let (authority, path_and_query) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        // 去掉用户信息：`Referer` 不得携带凭据。
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = split_authority(authority, &scheme)?;
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme,
            // IPv6 字面量的方括号是地址语法的一部分，输出时必须还原，否则 `https://::1:8443`
            // 不是合法 URL（`::1` 里的冒号会和端口分隔符混淆）。
            host: if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_ascii_lowercase()
            },
            port,
            path_and_query: path_and_query.to_string(),
        })
    }

    /// `scheme://host[:port]`；默认端口不写（浏览器就是这么输出的）。
    fn origin(&self) -> String {
        let default_port = default_port(&self.scheme);
        if self.port == default_port {
            format!("{}://{}", self.scheme, self.host)
        } else {
            format!("{}://{}:{}", self.scheme, self.host, self.port)
        }
    }

    fn path_and_query(&self) -> String {
        format!("{}{}", self.origin(), self.path_and_query)
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
    // IPv6 字面量：`[::1]:8080`。方括号里的冒号不是端口分隔符。
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

/// 给一跳请求补 `Referer`；不该发或调用方已显式指定时不动。
pub fn apply_header(
    policy: Policy,
    initiator: Option<&str>,
    target: &str,
    headers: &mut Vec<(String, String)>,
) {
    let Some(initiator) = initiator else {
        return;
    };
    // 调用方显式给了 `Referer` 就以它为准（与 Cookie 的处理一致）。
    if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("referer"))
    {
        return;
    }
    if let Some(value) = referer_for(policy, initiator, target) {
        headers.push(("referer".to_string(), value));
    }
}

/// 从响应头读 `Referrer-Policy` 并收窄当前策略（多次出现时按顺序依次应用）。
pub fn policy_from_response(current: Policy, headers: &[(String, String)]) -> Policy {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("referrer-policy"))
        .fold(current, |accumulated, (_, value)| adopt(accumulated, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_matches_browsers() {
        assert_eq!(Policy::DEFAULT.as_str(), "strict-origin-when-cross-origin");
    }

    #[test]
    fn same_origin_sends_the_full_path() {
        assert_eq!(
            referer_for(
                Policy::DEFAULT,
                "https://example.com/a/b?q=1",
                "https://example.com/c"
            )
            .as_deref(),
            Some("https://example.com/a/b?q=1")
        );
    }

    #[test]
    fn cross_origin_sends_only_the_origin() {
        assert_eq!(
            referer_for(
                Policy::DEFAULT,
                "https://example.com/a/b?q=1",
                "https://other.com/x"
            )
            .as_deref(),
            Some("https://example.com")
        );
    }

    /// 跨子域在「源」这一层就不同：`a.example.com` 与 `example.com` 是两个源。
    #[test]
    fn a_different_subdomain_is_cross_origin() {
        assert_eq!(
            referer_for(
                Policy::DEFAULT,
                "https://a.example.com/x",
                "https://b.example.com/y"
            )
            .as_deref(),
            Some("https://a.example.com")
        );
    }

    /// 默认策略下 https → http 一个字节都不发：否则明文链路会漏出安全页面的 URL。
    #[test]
    fn https_to_http_sends_nothing_under_the_default_policy() {
        assert_eq!(
            referer_for(
                Policy::DEFAULT,
                "https://example.com/a",
                "http://example.com/b"
            ),
            None
        );
    }

    /// `origin-when-cross-origin` 比默认宽松：降级时仍发源。
    #[test]
    fn origin_when_cross_origin_still_sends_on_downgrade() {
        assert_eq!(
            referer_for(
                Policy::OriginWhenCrossOrigin,
                "https://example.com/a",
                "http://example.com/b"
            )
            .as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn strict_origin_drops_the_path_across_sites_and_blocks_downgrade() {
        assert_eq!(
            referer_for(
                Policy::StrictOrigin,
                "https://example.com/a?q=1",
                "https://other.com/"
            )
            .as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            referer_for(
                Policy::StrictOrigin,
                "https://example.com/a",
                "http://other.com/"
            ),
            None
        );
    }

    #[test]
    fn no_referrer_and_same_origin_behave_as_documented() {
        assert_eq!(
            referer_for(
                Policy::NoReferrer,
                "https://example.com/a",
                "https://example.com/b"
            ),
            None
        );
        assert_eq!(
            referer_for(
                Policy::SameOrigin,
                "https://example.com/a?q=1",
                "https://other.com/"
            ),
            None
        );
        assert_eq!(
            referer_for(
                Policy::SameOrigin,
                "https://example.com/a?q=1",
                "https://example.com/"
            ),
            Some("https://example.com/a?q=1".to_string())
        );
    }

    /// 默认端口不写进 origin；非默认端口要写，否则两个不同源会算出同一个值。
    #[test]
    fn default_ports_are_omitted_but_custom_ports_are_kept() {
        assert_eq!(
            referer_for(
                Policy::Origin,
                "https://example.com:443/a",
                "https://other.com/"
            )
            .as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            referer_for(
                Policy::Origin,
                "https://example.com:8443/a",
                "https://other.com/"
            )
            .as_deref(),
            Some("https://example.com:8443")
        );
    }

    /// 凭据与片段绝不能出现在 `Referer` 里。
    #[test]
    fn credentials_and_fragments_never_leak() {
        let value = referer_for(
            Policy::Origin,
            "https://user:secret@example.com/a#section",
            "https://other.com/",
        )
        .expect("应当有值");
        assert_eq!(value, "https://example.com");
        assert!(!value.contains("secret"), "{value}");
        assert!(!value.contains('#'), "{value}");
    }

    #[test]
    fn policies_parse_and_ignore_unsafe_url() {
        assert_eq!(Policy::parse("no-referrer"), Some(Policy::NoReferrer));
        assert_eq!(
            Policy::parse("Strict-Origin-When-Cross-Origin"),
            Some(Policy::StrictOriginWhenCrossOrigin)
        );
        // 列表取第一项有效值。
        assert_eq!(
            Policy::parse("strict-origin, origin"),
            Some(Policy::StrictOrigin)
        );
        assert_eq!(
            Policy::parse("unsafe-url"),
            None,
            "不实现会泄露全 URL 的策略"
        );
        assert_eq!(Policy::parse(""), None);
        assert_eq!(Policy::parse("bogus"), None);
    }

    /// 响应头声明照常采纳（含比默认更宽松的取值），只有 `unsafe-url` 与无法识别的值保持原样。
    #[test]
    fn response_headers_update_the_policy_except_unsafe_url() {
        assert_eq!(
            adopt(Policy::DEFAULT, "no-referrer"),
            Policy::NoReferrer,
            "更严应当生效"
        );
        assert_eq!(
            adopt(Policy::DEFAULT, "no-referrer-when-downgrade"),
            Policy::NoReferrerWhenDowngrade,
            "更宽的取值同样生效：站点有权决定自己的 URL 怎么被传递"
        );
        assert_eq!(
            adopt(Policy::DEFAULT, "unsafe-url"),
            Policy::DEFAULT,
            "不提供会泄露全 URL 的档位"
        );
        assert_eq!(
            adopt(Policy::DEFAULT, "bogus"),
            Policy::DEFAULT,
            "无法识别时保持原样"
        );
    }

    /// 严格程度排序是收窄/采纳判定的基础，且极易凭直觉写反：
    /// `strict-origin`（同源也只发源）比 `strict-origin-when-cross-origin`（同源发完整 URL）更严。
    #[test]
    fn policies_are_ordered_from_strictest_to_loosest() {
        let ordered = [
            Policy::NoReferrer,
            Policy::SameOrigin,
            Policy::StrictOrigin,
            Policy::StrictOriginWhenCrossOrigin,
            Policy::Origin,
            Policy::OriginWhenCrossOrigin,
            Policy::NoReferrerWhenDowngrade,
        ];
        for pair in ordered.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{} 应当比 {} 更严",
                pair[0].as_str(),
                pair[1].as_str()
            );
        }
        // 同源情形的实际差异：`strict-origin` 只发源，默认策略发完整 URL。
        assert_eq!(
            referer_for(
                Policy::StrictOrigin,
                "https://example.com/a?q=1",
                "https://example.com/b"
            )
            .as_deref(),
            Some("https://example.com")
        );
    }

    /// 知乎实测下发 `no-referrer-when-downgrade`：它比默认宽松，但必须被识别而非当成垃圾值。
    #[test]
    fn no_referrer_when_downgrade_is_recognized() {
        assert_eq!(
            Policy::parse("no-referrer-when-downgrade"),
            Some(Policy::NoReferrerWhenDowngrade)
        );
        // 不降级时发完整 URL。
        assert_eq!(
            referer_for(
                Policy::NoReferrerWhenDowngrade,
                "https://example.com/a?q=1",
                "https://other.com/b"
            )
            .as_deref(),
            Some("https://example.com/a?q=1")
        );
        // 降级时不发。
        assert_eq!(
            referer_for(
                Policy::NoReferrerWhenDowngrade,
                "https://example.com/a?q=1",
                "http://other.com/b"
            ),
            None
        );
    }

    #[test]
    fn policy_from_response_applies_every_header_in_order() {
        let headers = vec![
            ("referrer-policy".to_string(), "origin".to_string()),
            ("Referrer-Policy".to_string(), "no-referrer".to_string()),
        ];
        assert_eq!(
            policy_from_response(Policy::DEFAULT, &headers),
            Policy::NoReferrer
        );
    }

    #[test]
    fn apply_header_respects_a_caller_supplied_referer() {
        let mut headers = vec![("Referer".to_string(), "https://mine.example/".to_string())];
        apply_header(
            Policy::DEFAULT,
            Some("https://example.com/a"),
            "https://example.com/b",
            &mut headers,
        );
        assert_eq!(headers.len(), 1, "不该出现两个 Referer：{headers:?}");
    }

    #[test]
    fn no_initiator_means_no_referer() {
        let mut headers = Vec::new();
        apply_header(Policy::DEFAULT, None, "https://example.com/", &mut headers);
        assert!(headers.is_empty());
    }

    #[test]
    fn ipv6_literals_keep_their_brackets_and_port() {
        assert_eq!(
            referer_for(Policy::Origin, "https://[::1]:8443/a", "https://other.com/").as_deref(),
            Some("https://[::1]:8443")
        );
    }
}
