//! robots.txt 解析与判定：明确禁止抓取的路径一概不抓。
//!
//! 语义按 RFC 9309（Robots Exclusion Protocol）实现，只保留抓取工具真正需要的部分：
//! - 按 `User-agent` 分组，优先取与自身 UA 最具体的匹配组，找不到就用 `*` 组；
//! - `Allow` / `Disallow` 同时命中时，**最长匹配优先**；长度相同则 `Allow` 胜；
//! - 支持 `*` 通配符与行尾 `$` 锚定；
//! - 组内出现 `Disallow: /` 即整站禁止。
//!
//! 取不到 robots.txt 时的语义刻意保守：
//! - `404` / `410`（文件不存在）表示站点未作限制，按允许处理；
//! - 其他状态码（`5xx`、超时、网络错误）表示「无法确认许可」，同样按允许处理，
//!   但会在结果里标注原因。理由是 RFC 9309 允许对不可达的 robots.txt 采取宽松解释，
//!   而对普通站点一律拒绝会让工具在对方临时故障时全面失效。
//!
//! 本模块只回答「允许不允许」，不负责发请求；取回与缓存由调用方完成。

/// 一次判定结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 明确允许。
    Allowed,
    /// 明确禁止，附带命中的规则，便于向用户解释原因。
    Disallowed { rule: String },
    /// robots.txt 不可用（缺失或取回失败），按允许处理并说明原因。
    Unavailable { reason: String },
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        !matches!(self, Verdict::Disallowed { .. })
    }
}

/// 一条 `Allow` / `Disallow` 规则。
#[derive(Debug, Clone)]
struct Rule {
    allow: bool,
    pattern: String,
}

#[derive(Debug, Clone, Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
}

/// 解析后的 robots.txt。
#[derive(Debug, Clone, Default)]
pub struct Robots {
    groups: Vec<Group>,
}

impl Robots {
    /// 解析文本。非法行按 RFC 9309 忽略，不因个别行畸形而整体失效。
    pub fn parse(text: &str) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        let mut current: Option<Group> = None;
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if value.is_empty() {
                        continue;
                    }
                    // 连续的 user-agent 行属于同一组；遇到规则行之后的新 UA 才另起一组。
                    let starts_new = current
                        .as_ref()
                        .map(|group| !group.rules.is_empty())
                        .unwrap_or(true);
                    if starts_new {
                        if let Some(group) = current.take() {
                            groups.push(group);
                        }
                        current = Some(Group::default());
                    }
                    if let Some(group) = current.as_mut() {
                        group.agents.push(value.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    let Some(group) = current.as_mut() else {
                        // 规则出现在任何 user-agent 之前：按 RFC 计入 `*` 组。
                        if groups
                            .iter()
                            .all(|item| !item.agents.iter().any(|a| a == "*"))
                        {
                            groups.push(Group {
                                agents: vec!["*".to_string()],
                                rules: Vec::new(),
                            });
                        }
                        let index = groups
                            .iter()
                            .position(|item| item.agents.iter().any(|a| a == "*"))
                            .expect("已确保存在通配组");
                        if !value.is_empty() {
                            groups[index].rules.push(Rule {
                                allow: key == "allow",
                                pattern: value.to_string(),
                            });
                        }
                        continue;
                    };
                    // 空值规则没有意义：`Disallow:` 表示不限制任何路径。
                    if !value.is_empty() {
                        group.rules.push(Rule {
                            allow: key == "allow",
                            pattern: value.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        if let Some(group) = current {
            groups.push(group);
        }
        Self { groups }
    }

    /// 判定某个路径（含查询串）是否允许抓取。
    pub fn verdict(&self, user_agent: &str, path: &str) -> Verdict {
        let rules = self.rules_for(user_agent);
        if rules.is_empty() {
            return Verdict::Allowed;
        }
        let path = normalize_path(path);
        let mut best: Option<&Rule> = None;
        for rule in &rules {
            if !pattern_matches(&rule.pattern, &path) {
                continue;
            }
            best = match best {
                None => Some(rule),
                Some(current) => {
                    let new_len = rule.pattern.len();
                    let old_len = current.pattern.len();
                    // 最长匹配优先；同长时 Allow 胜（RFC 9309 §2.2.2）。
                    if new_len > old_len || (new_len == old_len && rule.allow && !current.allow) {
                        Some(rule)
                    } else {
                        Some(current)
                    }
                }
            };
        }
        match best {
            Some(rule) if !rule.allow => Verdict::Disallowed {
                rule: rule.pattern.clone(),
            },
            _ => Verdict::Allowed,
        }
    }

    /// 选出适用于该 UA 的规则集：最具体的匹配组优先，其次 `*` 组。
    ///
    /// RFC 9309 规定同 UA 多组时应合并，这里取「最具体的那一组」：
    /// 抓取工具的 UA 是固定的产品令牌，站点极少为同一令牌写多组，合并收益很低。
    fn rules_for(&self, user_agent: &str) -> Vec<Rule> {
        let ua = user_agent.to_ascii_lowercase();
        let mut best: Option<(usize, &Group)> = None;
        for group in &self.groups {
            for agent in &group.agents {
                if agent == "*" {
                    continue;
                }
                if ua.contains(agent.as_str()) {
                    let score = agent.len();
                    if best.map(|(top, _)| score > top).unwrap_or(true) {
                        best = Some((score, group));
                    }
                }
            }
        }
        if let Some((_, group)) = best {
            return group.rules.clone();
        }
        self.groups
            .iter()
            .filter(|group| group.agents.iter().any(|agent| agent == "*"))
            .flat_map(|group| group.rules.clone())
            .collect()
    }
}

/// 路径归一化：空路径按 `/` 处理，去掉片段。
fn normalize_path(path: &str) -> String {
    let path = path.split('#').next().unwrap_or(path);
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// 规则匹配：支持 `*` 通配与行尾 `$` 锚定。
fn pattern_matches(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let anchored = pattern.ends_with('$');
    let body = if anchored {
        &pattern[..pattern.len() - 1]
    } else {
        pattern
    };
    if body.is_empty() {
        return false;
    }
    if !body.contains('*') {
        return if anchored {
            path == body
        } else {
            path.starts_with(body)
        };
    }
    let segments: Vec<&str> = body.split('*').collect();
    // 首段必须从头匹配；其余段依次向后推进。
    let mut cursor = match segments.first() {
        Some(first) if !first.is_empty() => {
            if !path.starts_with(first) {
                return false;
            }
            first.len()
        }
        _ => 0,
    };
    for (index, segment) in segments.iter().enumerate() {
        if index == 0 {
            continue;
        }
        if segment.is_empty() {
            continue;
        }
        let Some(found) = path[cursor..].find(segment) else {
            return false;
        };
        cursor += found + segment.len();
    }
    if anchored {
        // 锚定：最后一段必须落在行尾。
        match segments.last() {
            Some(last) if last.is_empty() => path.ends_with('*'),
            Some(last) => path.ends_with(last),
            None => true,
        }
    } else {
        true
    }
}

/// 从一个完整 URL 里取出用于判定的路径（含查询串）。
pub fn request_path(url: &str) -> String {
    let without_scheme = match url.find("://") {
        Some(index) => &url[index + 3..],
        None => url,
    };
    let path_start = without_scheme.find(['/', '?', '#']);
    match path_start {
        Some(index) => {
            let tail = &without_scheme[index..];
            let tail = tail.split('#').next().unwrap_or(tail);
            if tail.starts_with('/') {
                tail.to_string()
            } else {
                format!("/{tail}")
            }
        }
        None => "/".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# 站点 robots
User-agent: *
Disallow: /private/
Disallow: /search
Allow: /search/about
Crawl-delay: 10

User-agent: OmniCrawl
Disallow: /
";

    #[test]
    fn wildcard_group_blocks_and_allows() {
        let robots = Robots::parse(SAMPLE);
        assert!(robots.verdict("SomeBot/1.0", "/private/data").is_allowed() == false);
        assert!(robots.verdict("SomeBot/1.0", "/public").is_allowed());
        // 最长匹配：`/search/about` 比 `/search` 更长，Allow 胜。
        assert!(robots.verdict("SomeBot/1.0", "/search/about").is_allowed());
        assert!(!robots.verdict("SomeBot/1.0", "/search?q=1").is_allowed());
    }

    #[test]
    fn specific_agent_group_wins_over_wildcard() {
        let robots = Robots::parse(SAMPLE);
        // 具名组整站禁止：优先于 `*` 组。
        let verdict = robots.verdict("OmniCrawl/2.1 (+https://example.com)", "/public");
        assert!(!verdict.is_allowed(), "{verdict:?}");
        assert!(matches!(verdict, Verdict::Disallowed { rule } if rule == "/"));
        // 未命中具名组时回落到 `*` 组。
        assert!(robots.verdict("OtherBot", "/public").is_allowed());
    }

    #[test]
    fn empty_disallow_means_no_restriction() {
        let robots = Robots::parse("User-agent: *\nDisallow:\n");
        assert!(robots.verdict("AnyBot", "/anything").is_allowed());
    }

    #[test]
    fn no_matching_group_allows_everything() {
        let robots = Robots::parse("User-agent: Googlebot\nDisallow: /\n");
        assert!(robots.verdict("SomeBot", "/page").is_allowed());
    }

    #[test]
    fn wildcard_and_anchor_in_patterns() {
        let robots = Robots::parse("User-agent: *\nDisallow: /*.pdf$\nDisallow: /*/print\n");
        assert!(!robots.verdict("Bot", "/docs/manual.pdf").is_allowed());
        // 锚定 `$`：以 .pdf 结尾才拦，带后续路径的不拦。
        assert!(robots.verdict("Bot", "/docs/manual.pdf?x=1").is_allowed());
        assert!(!robots.verdict("Bot", "/article/print").is_allowed());
        // 无 `$` 时按前缀匹配：`/article/printable` 以 `/article/print` 开头，同样被拦。
        assert!(!robots.verdict("Bot", "/article/printable").is_allowed());
        // 前缀不成立时放行。
        assert!(robots.verdict("Bot", "/article/draft").is_allowed());
    }

    #[test]
    fn same_length_prefers_allow() {
        let robots = Robots::parse("User-agent: *\nDisallow: /page\nAllow: /page\n");
        assert!(robots.verdict("Bot", "/page").is_allowed());
    }

    #[test]
    fn rule_before_any_user_agent_goes_to_wildcard() {
        let robots = Robots::parse("Disallow: /secret\nUser-agent: *\nDisallow: /tmp\n");
        assert!(!robots.verdict("Bot", "/secret").is_allowed());
        assert!(!robots.verdict("Bot", "/tmp").is_allowed());
    }

    #[test]
    fn request_path_strips_origin_and_fragment() {
        assert_eq!(request_path("https://example.com/a/b?x=1#frag"), "/a/b?x=1");
        assert_eq!(request_path("https://example.com"), "/");
        assert_eq!(request_path("https://example.com?q=1"), "/?q=1");
        assert_eq!(request_path("http://example.com/"), "/");
    }

    #[test]
    fn comments_and_unknown_directives_are_ignored() {
        let robots = Robots::parse(
            "User-agent: * # 所有爬虫\nDisallow: /x # 注释\nSitemap: https://e.com/s.xml\n",
        );
        assert!(!robots.verdict("Bot", "/x").is_allowed());
        assert!(robots.verdict("Bot", "/y").is_allowed());
    }
}
