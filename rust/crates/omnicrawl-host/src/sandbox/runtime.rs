//! P2：解释器最小可读集合。
//!
//! AppContainer 默认读不到用户目录（`C:\Users\X` 的 ACL 里没有 `ALL APPLICATION PACKAGES`），
//! 而 `node`/`npm`/`python`/`pip` 常装在用户目录下。若不额外授权，沙盒内会出现
//! 「`node` 命令都不存在」。本模块只负责**发现**这些目录，授权与判定仍由
//! [`super::windows`] 与 [`super::Sandbox`] 负责。
//!
//! 三条设计约束（都是实测得出的，不是推测）：
//!
//! 1. **只授目录本身，不授 `AppData` 祖先**。`AppData\Roaming` 里有大量凭据（git 凭据、
//!    各家 CLI 的 token），授它等于把隔离拆了。因此只给 `Roaming\npm`、`Local\Programs\Python`
//!    这类**具体的解释器目录**。
//! 2. **只读取执行**。这些目录不需要可写；写权限留在工作区。
//! 3. **发现失败不算错误**。找不到解释器目录就不授，沙盒照样能起来，只是那条命令会
//!    报「找不到程序」——这比整个沙盒起不来好，也不违背 fail-closed（拒绝的是那条命令）。

use std::path::{Path, PathBuf};

/// 一个要授予只读执行权的位置及其用途。用途会进日志与 `/sandbox`，让人知道开了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRoot {
    pub path: PathBuf,
    pub reason: &'static str,
}

/// 从宿主环境里找出需要授权的解释器根目录。
///
/// 判定依据是 `PATH` 与常见安装位置，**不遍历用户目录**（那既慢又会碰到无权限路径）。
/// 返回顺序固定，便于测试与日志比对。
pub fn discover(env: &dyn Fn(&str) -> Option<String>) -> Vec<RuntimeRoot> {
    let mut found: Vec<RuntimeRoot> = Vec::new();
    let mut push = |path: PathBuf, reason: &'static str| {
        if !path.is_dir() {
            return;
        }
        if found.iter().any(|entry| same_path(&entry.path, &path)) {
            return;
        }
        found.push(RuntimeRoot { path, reason });
    };

    // PATH 里的每一项按名字认领：只认已知解释器目录，不整条 PATH 全授。
    for entry in env("PATH").unwrap_or_default().split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let path = PathBuf::from(entry);
        if let Some(reason) = classify(&path) {
            push(path, reason);
        }
    }

    // PATH 不一定包含「安装根」：`C:\Program Files\nodejs` 在 PATH 里，
    // 但 Python 的 `Scripts` 与用户级安装可能只在别处。
    //
    // **刻意不授 `AppData\Roaming\npm` 与 `AppData\Local\pnpm`**：那是 npm 的全局前缀，
    // 一旦容器能读到它，npm 会把它当成有效 prefix 去解析，然后沿着 `npm config get prefix`
    // 继续访问该目录下的其它东西；实测授它反而让 `npm -v` 从「能用」变成 `EPERM lstat C:\Users`。
    // 少开一个目录既更安全，也更可用。
    let mut extra: Vec<(PathBuf, &'static str)> = Vec::new();
    if let Some(program_files) = env("ProgramFiles").filter(|value| !value.is_empty()) {
        extra.push((PathBuf::from(program_files).join("nodejs"), "node"));
    }
    for key in ["LOCALAPPDATA", "APPDATA"] {
        if let Some(base) = env(key).filter(|value| !value.is_empty()) {
            let base = PathBuf::from(base);
            extra.push((base.join("Programs").join("Python"), "python"));
            extra.push((base.join("Programs").join("nodejs"), "node"));
        }
    }
    for (path, reason) in extra {
        push(path, reason);
    }

    // 去掉嵌套项：授了 `…\Programs\Python` 之后，再授它下面的 `Python312`、`Scripts`
    // 是纯浪费——可继承 ACE 会各自向子树传播一遍。实测这几条重叠项把首次准备从
    // 约 125 秒推到 413 秒。
    keep_outermost(found)
}

/// 沙盒内 PowerShell 的候选，按优先顺序：7.* 最新安装 → `PATH` 上的 `pwsh` → 5.1。
///
/// `pwsh` 的安装目录名就是版本（`7`、`7.4`、`7-preview`），因此按数字段倒序取最新；
/// 同版本里正式版排在 preview 前面。5.1 恒定放在最后——它是回退项，不是首选。
///
/// 只做**发现**，不判断容器内能不能用：AppContainer 里的可用性由
/// [`super::windows::Isolation`] 实测探测，探测不通就继续往后取。
pub fn powershell_candidates(env: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if path.is_file() && !found.iter().any(|entry| same_path(entry, &path)) {
            found.push(path);
        }
    };

    if let Some(root) = env("ProgramFiles").filter(|value| !value.trim().is_empty()) {
        let install_root = PathBuf::from(root).join("PowerShell");
        let mut installed: Vec<(Vec<u32>, bool, PathBuf)> = std::fs::read_dir(&install_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let version = leading_version(&name)?;
                let preview = name.to_ascii_lowercase().contains("preview");
                Some((version, preview, entry.path().join("pwsh.exe")))
            })
            .collect();
        // 版本倒序；同版本正式版优先（`preview` 排在后面）。
        installed.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        for (_, _, executable) in installed {
            push(executable);
        }
    }

    for entry in env("PATH").unwrap_or_default().split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        push(PathBuf::from(entry).join("pwsh.exe"));
    }

    if let Some(system_root) = env("SystemRoot").filter(|value| !value.trim().is_empty()) {
        push(
            PathBuf::from(system_root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        );
    }
    found
}

/// 目录名开头的版本数字段（`7` → `[7]`、`7.4` → `[7, 4]`、`7-preview` → `[7]`）。
fn leading_version(name: &str) -> Option<Vec<u32>> {
    let text = name.trim();
    if !text.starts_with(|character: char| character.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<u32> = text
        .split(|character: char| !character.is_ascii_digit())
        .take_while(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// 去掉被其它条目包含的路径（保留最外层）。
fn keep_outermost(entries: Vec<RuntimeRoot>) -> Vec<RuntimeRoot> {
    let mut kept: Vec<RuntimeRoot> = Vec::new();
    for entry in entries {
        if kept
            .iter()
            .any(|other| contains_path(&other.path, &entry.path))
        {
            continue;
        }
        kept.retain(|other| !contains_path(&entry.path, &other.path));
        kept.push(entry);
    }
    kept
}

/// `parent` 是否是 `child` 的祖先（大小写不敏感，按路径边界比较）。
fn contains_path(parent: &Path, child: &Path) -> bool {
    let parent = normalize(parent);
    let child = normalize(child);
    child.len() > parent.len()
        && child.starts_with(&parent)
        && child.as_bytes().get(parent.len()) == Some(&b'\\')
}

fn normalize(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// 按目录特征判断它是哪个解释器（只看路径，不读文件内容）。
fn classify(path: &Path) -> Option<&'static str> {
    let text = path.to_string_lossy().to_lowercase();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // 全局前缀目录（`AppData\Roaming\npm`、`AppData\Local\pnpm`）不授：
    // 让容器读得到它，npm 就会拿它当有效 prefix 继续往下解析用户目录，
    // 实测反而把原本能用的 `npm -v` 变成 `EPERM lstat C:\Users`。
    if (name == "npm" || name == "pnpm" || name == "yarn")
        && text.contains("\\appdata\\")
        && !text.contains("\\programs\\")
    {
        return None;
    }
    if name == "nodejs" || name == "node" {
        return Some("node");
    }
    if name == "npm" {
        return Some("npm");
    }
    if name == "pnpm" || name == "yarn" {
        return Some("node");
    }
    if name == "python" || text.contains("\\python3") || text.contains("\\python2") {
        return Some("python");
    }
    if name == "anaconda3" || name == "miniconda3" || name == "miniforge3" {
        return Some("python");
    }
    if name == "scripts" && (text.contains("python") || text.contains("anaconda")) {
        return Some("python");
    }
    if name == "windowsapps" {
        return None;
    }
    None
}

/// 路径比较用大小写不敏感（Windows 语义），并去掉结尾分隔符。
fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let right = right
        .to_string_lossy()
        .trim_end_matches('\\')
        .to_lowercase();
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn path_entries_are_classified_by_name() {
        assert_eq!(
            classify(Path::new(r"C:\Program Files\nodejs")),
            Some("node")
        );
        assert_eq!(
            classify(Path::new(r"D:\ProgramData\Anaconda3")),
            Some("python")
        );
        assert_eq!(
            classify(Path::new(
                r"C:\Users\X\AppData\Local\Programs\Python\Python312"
            )),
            Some("python")
        );
        // 不认识的一律不授：宁可解释器起不来，也不乱开读权。
        assert_eq!(classify(Path::new(r"C:\Windows\System32")), None);
        assert_eq!(classify(Path::new(r"C:\Users\X\.ssh")), None);
    }

    #[test]
    fn npm_global_prefix_is_never_granted() {
        // 授 `AppData\Roaming\npm` 会让 npm 拿它当有效 prefix 继续解析用户目录，
        // 实测把原本可用的 `npm -v` 变成 `EPERM lstat C:\Users`——开了反而更不可用。
        assert_eq!(classify(Path::new(r"C:\Users\X\AppData\Roaming\npm")), None);
        assert_eq!(classify(Path::new(r"C:\Users\X\AppData\Local\pnpm")), None);
        // `AppData\Local\Programs\nodejs` 是程序本体，仍然要授。
        assert_eq!(
            classify(Path::new(r"C:\Users\X\AppData\Local\Programs\nodejs")),
            Some("node")
        );
    }

    #[test]
    fn appdata_is_never_granted_as_a_whole() {
        // `AppData` 本身是祖先目录，里面全是凭据；只能授它下面的具体程序目录。
        assert_eq!(classify(Path::new(r"C:\Users\X\AppData\Roaming")), None);
        assert_eq!(classify(Path::new(r"C:\Users\X\AppData")), None);
    }

    #[test]
    fn the_windows_apps_alias_is_never_granted() {
        // `WindowsApps` 里是应用执行别名（多为零字节占位），授它没有意义，
        // 且它位于用户目录深处，不该成为沙盒可读的入口。
        assert_eq!(
            classify(Path::new(r"C:\Users\X\AppData\Local\Microsoft\WindowsApps")),
            None
        );
    }

    #[test]
    fn nested_entries_collapse_to_the_outermost() {
        // 授 `…\Programs\Python` 就够了，它下面的 `Python312`、`Scripts` 都是冗余的。
        let entries = vec![
            RuntimeRoot {
                path: PathBuf::from(r"C:\X\AppData\Local\Programs\Python\Python312"),
                reason: "python",
            },
            RuntimeRoot {
                path: PathBuf::from(r"C:\X\AppData\Local\Programs\Python"),
                reason: "python",
            },
            RuntimeRoot {
                path: PathBuf::from(r"C:\X\AppData\Local\Programs\Python\Python312\Scripts"),
                reason: "python",
            },
        ];
        let kept = keep_outermost(entries);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            kept[0].path,
            PathBuf::from(r"C:\X\AppData\Local\Programs\Python")
        );
    }

    #[test]
    fn sibling_directories_are_kept_apart() {
        // 名字前缀相同但不是子目录（`Python312` vs `Python312x`），不能误并。
        let entries = vec![
            RuntimeRoot {
                path: PathBuf::from(r"C:\X\Python312"),
                reason: "python",
            },
            RuntimeRoot {
                path: PathBuf::from(r"C:\X\Python312x"),
                reason: "python",
            },
        ];
        assert_eq!(keep_outermost(entries).len(), 2);
    }

    #[test]
    fn discovery_only_returns_directories_that_exist() {
        // 用不存在的路径构造环境：结果必须为空，而不是把不存在的目录记成「已授权」。
        let env = env_of(&[
            ("PATH", r"C:\definitely-not-here\nodejs"),
            ("LOCALAPPDATA", r"C:\definitely-not-here\AppData\Local"),
            ("APPDATA", r"C:\definitely-not-here\AppData\Roaming"),
            ("ProgramFiles", r"C:\definitely-not-here"),
        ]);
        assert!(discover(&env).is_empty());
    }

    #[test]
    fn discovery_deduplicates_case_insensitively() {
        // 同一个目录以不同大小写出现在 PATH 与猜测位置里，只能授一次。
        let root = std::env::current_dir().unwrap_or_default();
        let node = root.join("nodejs");
        let _ = std::fs::create_dir_all(&node);
        let text = node.to_string_lossy().to_string();
        let env = env_of(&[
            ("PATH", &text),
            ("ProgramFiles", root.to_string_lossy().as_ref()),
        ]);
        let found = discover(&env);
        let count = found
            .iter()
            .filter(|entry| same_path(&entry.path, &node))
            .count();
        assert!(count <= 1, "{found:?}");
    }

    #[test]
    fn powershell_versions_sort_numerically_not_lexically() {
        // `7.10` 必须排在 `7.9` 前面：字符串比较会给出相反答案。
        let root = std::env::temp_dir().join("omnicrawl-pwsh-order");
        let _ = std::fs::remove_dir_all(&root);
        for name in ["7.9", "7.10", "7", "6"] {
            let directory = root.join("PowerShell").join(name);
            std::fs::create_dir_all(&directory).expect("建目录");
            std::fs::write(directory.join("pwsh.exe"), b"").expect("放可执行文件");
        }
        let root_text = root.to_string_lossy().to_string();
        let env = env_of(&[("ProgramFiles", &root_text)]);
        let found = powershell_candidates(&env);
        let versions: Vec<String> = found
            .iter()
            .filter_map(|path| {
                path.parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().to_string())
            })
            .collect();
        assert_eq!(versions, ["7.10", "7.9", "7", "6"], "{found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn powershell_falls_back_to_windows_powershell_last() {
        // 没有 pwsh 时回退 5.1，且它永远排在最后——不是首选。
        let system_root = PathBuf::from(r"C:\Windows");
        let env = env_of(&[("SystemRoot", &system_root.to_string_lossy())]);
        let found = powershell_candidates(&env);
        // 只在 5.1 真实存在时才断言（CI/无 PowerShell 的机器上结果为空）。
        if found.is_empty() {
            return;
        }
        assert!(
            found
                .last()
                .map(|path| path.ends_with("powershell.exe"))
                .unwrap_or(false),
            "{found:?}"
        );
    }

    #[test]
    fn a_preview_install_loses_to_the_stable_one() {
        let root = std::env::temp_dir().join("omnicrawl-pwsh-preview");
        let _ = std::fs::remove_dir_all(&root);
        for name in ["7.4-preview", "7.4"] {
            let directory = root.join("PowerShell").join(name);
            std::fs::create_dir_all(&directory).expect("建目录");
            std::fs::write(directory.join("pwsh.exe"), b"").expect("放可执行文件");
        }
        let root_text = root.to_string_lossy().to_string();
        let env = env_of(&[("ProgramFiles", &root_text)]);
        let first = powershell_candidates(&env).into_iter().next();
        assert!(
            first
                .as_ref()
                .map(|path| path.to_string_lossy().contains(r"7.4\pwsh.exe"))
                .unwrap_or(false),
            "{first:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
