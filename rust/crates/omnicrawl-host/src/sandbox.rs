//! 沙盒策略运行期（P0：只有**判定**，没有隔离）。
//!
//! 设计见 `rust/assets/docs/sandbox_windows_design.md`。本模块在 P0 阶段承担三件事：
//!
//! 1. 把 `[sandbox]` 配置折算成可查询的策略（[`Sandbox`]）；
//! 2. 判定「某个路径 / 某个网络目标**会不会**被拦」（[`Sandbox::check_path`] /
//!    [`Sandbox::check_target`]）；
//! 3. 在 dry-run（`mode = "off"`）下把判定结果记进[`Sandbox::decisions`]，供 `/sandbox`
//!    展示「本会拦什么」。
//!
//! **P0 不改变任何执行行为**：无论判定结果如何，命令照常以宿主身份运行。
//! 真隔离在 P1/P3 落地；届时同一条判定链会变成"先判定、再据此决定 grant/deny"。

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use omnicrawl_config::features::sandbox::{NetPolicy, SandboxConfig};

/// 一次判定的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 在允许范围内。
    Allow,
    /// 会被拦下。
    Deny(String),
    /// 需要审批（网络 `ask` 策略下）。
    Ask(String),
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// `/sandbox` 与日志里的一行展示。
    pub fn label(&self) -> String {
        match self {
            Self::Allow => "放行".to_string(),
            Self::Deny(reason) => format!("拦截（{reason}）"),
            Self::Ask(reason) => format!("待审批（{reason}）"),
        }
    }
}

/// 判定类别，用于 `/sandbox` 分组与日志前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    Net,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "读",
            Self::Write => "写",
            Self::Net => "网络",
        }
    }
}

/// 一条 dry-run 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub kind: Kind,
    /// 被检查的对象：路径或 `host:port`。
    pub subject: String,
    pub verdict: Verdict,
}

/// 沙盒策略的运行期句柄。
///
/// 默认（配置缺省或 `enabled = false`）是**空操作**：所有判定都返回 `Allow`，
/// 不记录任何东西——这样关闭沙盒时对现有行为与性能零影响。
#[derive(Debug)]
pub struct Sandbox {
    config: SandboxConfig,
    root: PathBuf,
    temp_dir: PathBuf,
    decisions: Mutex<Vec<Decision>>,
    /// 记录条数上限：dry-run 可能跑一整天，不能无限涨。
    max_decisions: usize,
}

/// dry-run 记录条数上限。超出后只保留前 N 条并标记"已截断"。
pub const MAX_DECISIONS: usize = 512;

impl Sandbox {
    /// 关闭状态：不判定、不记录。
    pub fn disabled(root: impl Into<PathBuf>, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            config: SandboxConfig::default(),
            root: root.into(),
            temp_dir: temp_dir.into(),
            decisions: Mutex::new(Vec::new()),
            max_decisions: MAX_DECISIONS,
        }
    }

    /// 按配置构造。`root` 是工作区根（不是进程当前目录）。
    pub fn new(
        config: SandboxConfig,
        root: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config,
            root: root.into(),
            temp_dir: temp_dir.into(),
            decisions: Mutex::new(Vec::new()),
            max_decisions: MAX_DECISIONS,
        }
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// 是否开着（dry-run 或真隔离都算开）。
    pub fn is_on(&self) -> bool {
        self.config.enabled
    }

    /// 是否真隔离。
    pub fn isolates(&self) -> bool {
        self.config.isolates()
    }

    /// 是否 dry-run。
    pub fn is_dry_run(&self) -> bool {
        self.config.is_dry_run()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 工作区内的专用临时目录（沙盒进程的 `TEMP` 指这里）。
    pub fn sandbox_temp(&self) -> PathBuf {
        self.temp_dir.join("sandbox")
    }

    // ── 判定 ─────────────────────────────────────────────────

    /// 判定一个路径能不能读。
    pub fn check_read(&self, path: &Path) -> Verdict {
        let verdict = if !self.is_on()
            || self.is_inside_allowed(
                path,
                self.config
                    .writable
                    .iter()
                    .chain(self.config.readable.iter()),
            ) {
            Verdict::Allow
        } else {
            Verdict::Deny(format!(
                "{} 不在沙盒可读范围（工作区与 `[sandbox.fs]` 之外）",
                path.display()
            ))
        };
        self.record(Kind::Read, path_text(path), &verdict);
        verdict
    }

    /// 判定一个路径能不能写。
    ///
    /// 注意 P0 里这是**软**判定：真正拦住写入的是 P1/P2 的 ACL。这里只负责"本会拦"。
    pub fn check_write(&self, path: &Path) -> Verdict {
        let verdict = if !self.is_on() || self.is_inside_allowed(path, self.config.writable.iter())
        {
            Verdict::Allow
        } else {
            Verdict::Deny(format!(
                "{} 不在沙盒可写范围（工作区与 `[sandbox.fs] writable` 之外）",
                path.display()
            ))
        };
        self.record(Kind::Write, path_text(path), &verdict);
        verdict
    }

    /// 判定一个网络目标（`host:port`）。
    pub fn check_target(&self, target: &str) -> Verdict {
        let verdict = self.target_verdict(target);
        self.record(Kind::Net, target.to_string(), &verdict);
        verdict
    }

    fn target_verdict(&self, target: &str) -> Verdict {
        if !self.is_on() {
            return Verdict::Allow;
        }
        let (host, _port) = split_target(target);
        // 回环永远放行：本地 dev server、沙盒代理本身都在回环上。
        if is_loopback(host) {
            return Verdict::Allow;
        }
        // deny 优先于任何放行规则。
        if matches_any(host, &self.config.net_deny) {
            return Verdict::Deny(format!("命中 `[sandbox.net] deny`：{host}"));
        }
        match self.config.net_policy {
            NetPolicy::Offline => Verdict::Deny("网络策略 offline".to_string()),
            NetPolicy::Online => Verdict::Allow,
            NetPolicy::Allowlist => {
                if matches_any(host, &self.config.net_allow) {
                    Verdict::Allow
                } else {
                    Verdict::Deny(format!("不在 `[sandbox.net] allow` 白名单：{host}"))
                }
            }
            NetPolicy::Ask => Verdict::Ask(format!("网络策略 ask：{target}")),
        }
    }

    /// 路径是否落在允许集合内。相对路径按工作区根解析——**不是**进程当前目录。
    fn is_inside_allowed<'a>(&self, path: &Path, extra: impl Iterator<Item = &'a String>) -> bool {
        let resolved = self.resolve(path);
        if is_within(&resolved, &self.root)
            || is_within(&resolved, &self.temp_dir)
            || is_within(&resolved, &self.sandbox_temp())
        {
            return true;
        }
        extra
            .map(|entry| self.resolve(Path::new(entry)))
            .any(|allowed| is_within(&resolved, &allowed))
    }

    /// 相对路径按工作区根展开；`..` 在这里就被折掉，避免 `..\..\etc` 绕过前缀比较。
    fn resolve(&self, path: &Path) -> PathBuf {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        normalize(&joined)
    }

    // ── dry-run 记录 ─────────────────────────────────────────

    fn record(&self, kind: Kind, subject: String, verdict: &Verdict) {
        // 关闭状态下不记录：保持"开关关了就零影响"。
        if !self.is_on() {
            return;
        }
        if verdict.is_allowed() && !self.config.net_log_all && kind == Kind::Net {
            return;
        }
        let Ok(mut decisions) = self.decisions.lock() else {
            return;
        };
        if decisions.len() >= self.max_decisions {
            return;
        }
        decisions.push(Decision {
            kind,
            subject,
            verdict: verdict.clone(),
        });
    }

    /// 取出并清空 dry-run 记录（`/sandbox` 与日志用）。
    pub fn take_decisions(&self) -> Vec<Decision> {
        self.decisions
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    /// 只看不取。
    pub fn decisions(&self) -> Vec<Decision> {
        self.decisions
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 只保留会被拦/待审批的条目——这是用户真正关心的"本会拦什么"。
    pub fn blocked_summary(&self) -> Vec<Decision> {
        self.decisions()
            .into_iter()
            .filter(|decision| !decision.verdict.is_allowed())
            .collect()
    }

    /// 一句话统计，`/sandbox` 用。
    pub fn dry_run_summary(&self) -> String {
        let decisions = self.decisions();
        if decisions.is_empty() {
            return "dry-run 暂无判定记录。".to_string();
        }
        let blocked = decisions
            .iter()
            .filter(|decision| !decision.verdict.is_allowed())
            .count();
        format!(
            "dry-run 已记录 {} 条判定，其中 {} 条会被拦或待审批。",
            decisions.len(),
            blocked
        )
    }

    // ── 命令级判定（P0 的对外入口）────────────────────────────

    /// 对一条 shell 命令做 dry-run 判定，返回要附在输出里的提示。
    ///
    /// 沙盒关闭或真隔离模式下都返回空串：关闭时无事可报，真隔离时拦截图件本身就够
    /// （P1 之后错误的措辞会是真实故障，而不是"本会拦"）。
    pub fn dry_run_notice_for_command(&self, command: &str) -> String {
        if !self.is_dry_run() {
            return String::new();
        }
        let mut found: Vec<Decision> = Vec::new();
        for path in command_paths(command) {
            // 命令里的路径既可能被读、也可能被写。两者都判一遍：
            // 只报"写"会漏掉「读走 ~/.ssh」这类外泄路径，而那恰恰最要紧。
            for (kind, verdict) in [
                (Kind::Read, self.check_read(Path::new(&path))),
                (Kind::Write, self.check_write(Path::new(&path))),
            ] {
                if !verdict.is_allowed() {
                    found.push(Decision {
                        kind,
                        subject: path.clone(),
                        verdict,
                    });
                }
            }
        }
        for target in command_targets(command) {
            let verdict = self.check_target(&target);
            if !verdict.is_allowed() {
                found.push(Decision {
                    kind: Kind::Net,
                    subject: target,
                    verdict,
                });
            }
        }
        if found.is_empty() {
            return String::new();
        }
        // 去重：同一条命令里重复出现的路径不必重复提示。
        let mut lines: Vec<String> = Vec::new();
        for decision in found {
            let entry = format!(
                "  - {} {}：{}",
                decision.kind.as_str(),
                decision.subject,
                decision.verdict.label()
            );
            if !lines.contains(&entry) {
                lines.push(entry);
            }
        }
        format!(
            "\n\n[sandbox dry-run] 若开启 appcontainer 隔离，本命令会涉及以下受限对象：\n{}",
            lines.join("\n")
        )
    }
}

/// 从命令文本里粗取"看起来像路径"的片段。
///
/// 这是**启发式**，不解析 shell 语法：dry-run 的目的是让用户提前看到"大概会拦什么"，
/// 漏报一些、多报一些都能接受（不漏报关键路径即可）。P1 之后真正的边界由 ACL 保证，
/// 不依赖这里的准确性。
fn command_paths(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in command.split(|value: char| {
        value.is_whitespace()
            || matches!(
                value,
                '"' | '\'' | '|' | '&' | ';' | '(' | ')' | '<' | '>' | ','
            )
    }) {
        let token = raw.trim();
        if token.is_empty() || token.len() < 3 {
            continue;
        }
        if !looks_like_path(token) {
            continue;
        }
        out.push(token.to_string());
    }
    out
}

/// 是否像一个文件路径（绝对路径、盘符、或带分隔符的相对路径）。
fn looks_like_path(token: &str) -> bool {
    let bytes = token.as_bytes();
    // 盘符式：`C:\...`、`D:/...`
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        let letter = bytes[0];
        if letter.is_ascii_alphabetic() {
            return true;
        }
    }
    // UNC：`\\server\share`
    if token.starts_with(r"\\") {
        return true;
    }
    // 其余只在明显带分隔符时才算路径，避免把 `--flag=value` 之类全当路径。
    token.contains('\\') || token.contains('/')
}

/// 从命令文本里粗取网络目标（`host:port` 或裸域名形式的 URL）。
fn command_targets(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in command.split_whitespace() {
        let token =
            token.trim_matches(|value: char| matches!(value, '"' | '\'' | '(' | ')' | ',' | ';'));
        if token.is_empty() {
            continue;
        }
        if let Some(target) = target_of_url(token) {
            out.push(target);
            continue;
        }
        // 形如 `host:port`（带数字端口）也当成网络目标。
        let (host, port) = split_target(token);
        if port.is_some() && !host.is_empty() && !looks_like_path(token) {
            out.push(token.to_string());
        }
    }
    out
}

/// 从 URL 取 `host:port`；非 http(s)/ws(s) 一律 `None`。
fn target_of_url(token: &str) -> Option<String> {
    let rest = ["https://", "http://", "wss://", "ws://"]
        .iter()
        .find_map(|prefix| token.strip_prefix(prefix))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }
    let has_port = split_target(authority).1.is_some();
    Some(if has_port {
        authority.to_string()
    } else {
        format!("{authority}:443")
    })
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// 折掉 `.` 与 `..`（不访问文件系统，纯词法）。
///
/// 前缀比较必须在词法归一之后做，否则 `工作区\..\Windows` 会被误判为"在工作区内"。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 根之后无处可退时保留，交给后续比较去判断（不会变成"在内部"）。
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` 是否等于 `base` 或在其之下（大小写不敏感，Windows 语义）。
fn is_within(path: &Path, base: &Path) -> bool {
    let path = normalize(path);
    let base = normalize(base);
    let mut path_components = path.components();
    let mut base_components = base.components();
    loop {
        match (base_components.next(), path_components.next()) {
            (None, _) => return true,
            (Some(expected), Some(actual)) => {
                let expected = expected.as_os_str().to_string_lossy().to_lowercase();
                let actual = actual.as_os_str().to_string_lossy().to_lowercase();
                if expected != actual {
                    return false;
                }
            }
            (Some(_), None) => return false,
        }
    }
}

/// 拆 `host:port`；IPv6 字面量按 `[::1]:8080` 处理。
fn split_target(target: &str) -> (&str, Option<&str>) {
    if let Some(rest) = target.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &rest[..end];
            let port = rest[end + 1..].strip_prefix(':');
            return (host, port);
        }
    }
    match target.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|value| value.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (target, None),
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim().to_lowercase();
    host == "localhost" || host == "::1" || host == "0.0.0.0" || host.starts_with("127.")
}

/// 域名匹配：精确相等，或 `*.suffix` 的后缀匹配。
///
/// `*.example.com` **不**匹配裸 `example.com`，避免 `evil-example.com` 这类混淆
/// （它与 `example.com` 只差一个连字符，后缀匹配会误放行）。
fn matches_any(host: &str, patterns: &[String]) -> bool {
    let host = host.trim().to_lowercase();
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().to_lowercase();
        if let Some(suffix) = pattern.strip_prefix("*.") {
            host.ends_with(&format!(".{suffix}")) && host.len() > suffix.len() + 1
        } else {
            host == pattern
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::sandbox::SandboxMode;

    fn enabled(mode: SandboxMode) -> SandboxConfig {
        SandboxConfig {
            enabled: true,
            mode,
            ..SandboxConfig::default()
        }
    }

    fn sandbox(config: SandboxConfig) -> Sandbox {
        Sandbox::new(
            config,
            PathBuf::from(r"D:\work"),
            PathBuf::from(r"D:\work\.omnicrawl\.agent_tmp"),
        )
    }

    #[test]
    fn disabled_sandbox_allows_everything_and_records_nothing() {
        let sandbox = Sandbox::disabled(r"D:\work", r"D:\work\.omnicrawl\.agent_tmp");
        assert!(sandbox
            .check_read(Path::new(r"C:\Users\X\.ssh\id_rsa"))
            .is_allowed());
        assert!(sandbox.check_write(Path::new(r"C:\Windows\x")).is_allowed());
        assert!(sandbox.check_target("evil.example.com:443").is_allowed());
        assert!(sandbox.decisions().is_empty(), "关闭时不应留下任何记录");
    }

    #[test]
    fn writes_inside_the_workspace_are_allowed() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        assert!(sandbox.check_write(Path::new("输出/a.png")).is_allowed());
        assert!(sandbox
            .check_write(Path::new(r"D:\work\src\main.rs"))
            .is_allowed());
    }

    #[test]
    fn writes_outside_the_workspace_are_denied_in_dry_run() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let verdict = sandbox.check_write(Path::new(r"C:\Users\X\.ssh\authorized_keys"));
        assert!(!verdict.is_allowed());
        assert!(verdict.label().contains("拦截"), "{}", verdict.label());
        // dry-run 记录下来了，这正是用户要看的东西。
        assert_eq!(sandbox.blocked_summary().len(), 1);
    }

    #[test]
    fn traversal_cannot_escape_the_workspace() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        // 词法归一之后 `工作区\..\Windows` 落在工作区外，必须被拦。
        assert!(!sandbox
            .check_write(Path::new(r"D:\work\..\Windows\System32\evil.dll"))
            .is_allowed());
        assert!(!sandbox
            .check_write(Path::new(r"D:\work\..\..\other\file.txt"))
            .is_allowed());
    }

    #[test]
    fn prefix_lookalikes_are_not_treated_as_inside() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        // `D:\workshop` 不是 `D:\work` 的子目录，朴素前缀比较会误放行。
        assert!(!sandbox
            .check_write(Path::new(r"D:\workshop\a.txt"))
            .is_allowed());
    }

    #[test]
    fn path_comparison_is_case_insensitive() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        assert!(sandbox
            .check_write(Path::new(r"d:\WORK\Src\Main.RS"))
            .is_allowed());
    }

    #[test]
    fn extra_writable_entries_widen_the_write_set() {
        let mut config = enabled(SandboxMode::Off);
        config.writable = vec![r"D:\data".to_string()];
        let sandbox = sandbox(config);
        assert!(sandbox
            .check_write(Path::new(r"D:\data\a.csv"))
            .is_allowed());
        assert!(!sandbox
            .check_write(Path::new(r"D:\data2\a.csv"))
            .is_allowed());
    }

    #[test]
    fn readable_entries_do_not_grant_write() {
        let mut config = enabled(SandboxMode::Off);
        config.readable = vec![r"C:\tools".to_string()];
        let sandbox = sandbox(config);
        assert!(sandbox
            .check_read(Path::new(r"C:\tools\node.exe"))
            .is_allowed());
        assert!(!sandbox
            .check_write(Path::new(r"C:\tools\node.exe"))
            .is_allowed());
    }

    #[test]
    fn loopback_is_always_allowed() {
        let mut config = enabled(SandboxMode::Off);
        config.net_policy = NetPolicy::Offline;
        let sandbox = sandbox(config);
        assert!(sandbox.check_target("127.0.0.1:3000").is_allowed());
        assert!(sandbox.check_target("localhost:8080").is_allowed());
        assert!(sandbox.check_target("[::1]:9000").is_allowed());
        // 非回环在 offline 下被拦。
        assert!(!sandbox.check_target("example.com:443").is_allowed());
    }

    #[test]
    fn ask_policy_reports_pending_approval() {
        let mut config = enabled(SandboxMode::Off);
        config.net_policy = NetPolicy::Ask;
        let sandbox = sandbox(config);
        let verdict = sandbox.check_target("api.example.com:443");
        assert!(matches!(verdict, Verdict::Ask(_)), "{verdict:?}");
        assert!(verdict.label().contains("待审批"));
    }

    #[test]
    fn deny_beats_allowlist() {
        let mut config = enabled(SandboxMode::Off);
        config.net_policy = NetPolicy::Allowlist;
        config.net_allow = vec!["*.example.com".to_string()];
        config.net_deny = vec!["secret.example.com".to_string()];
        let sandbox = sandbox(config);
        assert!(sandbox.check_target("api.example.com:443").is_allowed());
        assert!(!sandbox.check_target("secret.example.com:443").is_allowed());
    }

    #[test]
    fn suffix_patterns_do_not_match_the_bare_domain() {
        let mut config = enabled(SandboxMode::Off);
        config.net_policy = NetPolicy::Allowlist;
        config.net_allow = vec!["*.example.com".to_string()];
        let sandbox = sandbox(config);
        assert!(sandbox.check_target("a.example.com:443").is_allowed());
        // `*.example.com` 不覆盖裸域，也不覆盖"只差一个连字符"的混淆域名。
        assert!(!sandbox.check_target("example.com:443").is_allowed());
        assert!(!sandbox.check_target("evil-example.com:443").is_allowed());
        assert!(!sandbox
            .check_target("a.example.com.evil.com:443")
            .is_allowed());
    }

    #[test]
    fn online_policy_allows_everything() {
        let mut config = enabled(SandboxMode::Off);
        config.net_policy = NetPolicy::Online;
        let sandbox = sandbox(config);
        assert!(sandbox.check_target("anything.example:443").is_allowed());
    }

    #[test]
    fn log_all_off_skips_allowed_network_entries() {
        let mut config = enabled(SandboxMode::Off);
        config.net_policy = NetPolicy::Online;
        config.net_log_all = false;
        let sandbox = sandbox(config);
        sandbox.check_target("allowed.example:443");
        assert!(sandbox.decisions().is_empty(), "放行的网络请求不该记录");
        sandbox.check_target("x.example:443");
        // 仍然全放行，所以依然不记录。
        assert!(sandbox.decisions().is_empty());
    }

    #[test]
    fn local_denials_are_always_recorded() {
        let mut config = enabled(SandboxMode::Off);
        config.net_policy = NetPolicy::Offline;
        config.net_log_all = false;
        let sandbox = sandbox(config);
        sandbox.check_target("api.example:443");
        assert_eq!(sandbox.decisions().len(), 1, "被拒的网络请求必须留痕");
    }

    #[test]
    fn decisions_are_capped() {
        let mut sandbox = sandbox(enabled(SandboxMode::Off));
        sandbox.max_decisions = 3;
        for _ in 0..10 {
            sandbox.check_write(Path::new(r"C:\outside\a.txt"));
        }
        assert_eq!(sandbox.decisions().len(), 3, "记录条数要有上限");
    }

    #[test]
    fn take_decisions_clears_the_buffer() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        sandbox.check_write(Path::new(r"C:\outside\a.txt"));
        assert_eq!(sandbox.take_decisions().len(), 1);
        assert!(sandbox.decisions().is_empty());
    }

    #[test]
    fn isolation_flag_follows_mode() {
        assert!(!sandbox(enabled(SandboxMode::Off)).isolates());
        assert!(sandbox(enabled(SandboxMode::AppContainer)).isolates());
        assert!(sandbox(enabled(SandboxMode::Off)).is_dry_run());
    }

    #[test]
    fn sandbox_temp_lives_under_the_temp_dir() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        // 专用临时目录本身可写，否则沙盒进程连临时文件都建不了。
        assert!(sandbox
            .check_write(&sandbox.sandbox_temp().join("a.tmp"))
            .is_allowed());
    }

    #[test]
    fn summary_counts_only_blocked_entries() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        sandbox.check_write(Path::new("inside.txt"));
        sandbox.check_write(Path::new(r"C:\outside.txt"));
        let summary = sandbox.dry_run_summary();
        assert!(summary.contains("2 条判定"), "{summary}");
        assert!(summary.contains("1 条会被拦"), "{summary}");
    }

    #[test]
    fn target_parsing_handles_ports_and_ipv6() {
        assert_eq!(
            split_target("example.com:443"),
            ("example.com", Some("443"))
        );
        assert_eq!(split_target("example.com"), ("example.com", None));
        assert_eq!(split_target("[::1]:8080"), ("::1", Some("8080")));
        // 非数字端口不算端口，整体当主机名（避免把 IPv6 裸地址拆坏）。
        assert_eq!(split_target("a:b"), ("a:b", None));
    }

    #[test]
    fn a_command_touching_outside_paths_gets_a_dry_run_notice() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let notice =
            sandbox.dry_run_notice_for_command(r"copy C:\Users\X\.ssh\id_rsa .\stolen.txt");
        assert!(notice.contains("dry-run"), "{notice}");
        assert!(notice.contains("id_rsa"), "应指出具体受限对象：{notice}");
        assert!(notice.contains("拦截"), "{notice}");
    }

    #[test]
    fn reading_a_secret_outside_the_workspace_is_reported_as_a_read() {
        // `copy` 是「读源文件、写到目标」：源路径属于**读**。只报「写」会漏掉外泄路径。
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let notice = sandbox.dry_run_notice_for_command(r"copy C:\Users\X\.ssh\id_rsa .\x.txt");
        assert!(notice.contains("读"), "应报为读，而不是只报写：{notice}");
        assert!(notice.contains("id_rsa"), "{notice}");
    }

    #[test]
    fn a_path_both_read_and_written_is_reported_twice() {
        // 工作区外的路径既不读也不写，两个限制都该说清楚（一条读、一条写）。
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let notice = sandbox.dry_run_notice_for_command(r"copy C:\outside\a.txt C:\outside\b.txt");
        let reads = notice.lines().filter(|line| line.contains("读 ")).count();
        let writes = notice.lines().filter(|line| line.contains("写 ")).count();
        assert_eq!(reads, 2, "两个源路径都该报读：{notice}");
        assert_eq!(writes, 2, "两个目标路径都该报写：{notice}");
    }

    #[test]
    fn a_command_staying_inside_gets_no_notice() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let notice = sandbox.dry_run_notice_for_command(r"python .\build.py 输出\a.png");
        assert!(notice.is_empty(), "工作区内不该打扰：{notice}");
    }

    #[test]
    fn a_command_hitting_the_network_gets_a_notice() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let notice = sandbox.dry_run_notice_for_command("curl https://api.example.com/v1/data");
        assert!(notice.contains("网络"), "{notice}");
        assert!(notice.contains("api.example.com"), "{notice}");
    }

    #[test]
    fn loopback_requests_are_not_reported() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let notice = sandbox.dry_run_notice_for_command("curl http://127.0.0.1:3000/health");
        assert!(notice.is_empty(), "回环不该打扰：{notice}");
    }

    #[test]
    fn a_disabled_sandbox_never_produces_a_notice() {
        let sandbox = Sandbox::disabled(r"D:\work", r"D:\work\.omnicrawl\.agent_tmp");
        assert!(sandbox
            .dry_run_notice_for_command(r"copy C:\Windows\a .\b")
            .is_empty());
    }

    #[test]
    fn an_isolating_sandbox_does_not_emit_dry_run_wording() {
        // 真隔离时用"本会拦"的措辞会误导：那时是真实故障，由系统报错。
        let sandbox = sandbox(enabled(SandboxMode::AppContainer));
        assert!(sandbox
            .dry_run_notice_for_command(r"copy C:\Users\X\a .\b")
            .is_empty());
    }

    #[test]
    fn notice_deduplicates_repeated_subjects() {
        let sandbox = sandbox(enabled(SandboxMode::Off));
        let notice = sandbox.dry_run_notice_for_command(r"copy C:\outside\a.txt C:\outside\a.txt");
        // 同一对象最多报两条（读一条、写一条），不因重复出现而线性增长。
        let hits = notice
            .lines()
            .filter(|line| line.trim_start().starts_with("- ") && line.contains("a.txt"))
            .count();
        assert_eq!(hits, 2, "同一对象只报读/写各一次：{notice}");
    }

    #[test]
    fn url_targets_default_to_https_port() {
        assert_eq!(
            target_of_url("https://api.example.com/v1").as_deref(),
            Some("api.example.com:443")
        );
        assert_eq!(
            target_of_url("http://api.example.com:8080/v1").as_deref(),
            Some("api.example.com:8080")
        );
        assert_eq!(target_of_url("ftp://api.example.com"), None);
    }

    #[test]
    fn path_lookalikes_are_not_treated_as_paths() {
        // `--limit=10` 这类不该被误判成路径并触发无关提示。
        assert!(!looks_like_path("--limit=10"));
        assert!(!looks_like_path("--output=result"));
        assert!(looks_like_path(r"C:\data\a.csv"));
        assert!(looks_like_path("./build.py"));
        assert!(looks_like_path(r"\\server\share\a.txt"));
    }
}
