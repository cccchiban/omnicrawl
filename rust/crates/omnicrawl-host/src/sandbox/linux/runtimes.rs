//! 解释器安装根的发现：让 `node`/`python`/`cargo` 在沙盒里真的能跑。
//!
//! 为什么需要这一步：`system_read_paths()` 只覆盖 `/usr`、`/usr/local`、`/opt`。
//! 但用户版本的 Node、Rust、Python 通常装在 `$HOME` 下——nvm 在 `~/.nvm/versions/node/vN`、
//! rustup 在 `~/.rustup/toolchains/<tc>`、pyenv 在 `~/.pyenv/versions/<v>`。Landlock 是
//! 白名单语义，"没列出"等于拒绝，于是这些解释器在沙盒里表现为**不存在**（`ENOENT` 或
//! `EACCES`），而不是一条能看懂的拒绝。
//!
//! 但**不能因此授 `$HOME`**：`~/.ssh`、`~/.npmrc`（含 auth token）、`~/.git-credentials`
//! 全在里面。所以这里做的是"精确到安装根"的授权，并带两道闸：
//!
//! 1. **绝不把安装根爬到 `$HOME`**——爬升以 `bin/` 的父目录为界，且结果必须严格在 `$HOME` 之下。
//! 2. **根目录若直接含敏感文件就不授它本身**，改为授它的安全子目录（`~/.cargo` 是典型：
//!    它含 `credentials.toml`，只能授 `bin`/`registry`/`git`）。
//!
//! 第 2 条与 Windows 侧一致的取舍：`runtime.rs` 的注释里写着"授它等于把隔离拆了"。

use std::path::{Path, PathBuf};

/// 一个要授只读执行权限的安装根。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    /// 分类名，只用于诊断（`node` / `python` / `cargo` / `rustup` …）。
    pub kind: &'static str,
    /// 真正要授权的路径。可能是安装根，也可能是它的安全子目录（见 [`Install::paths`]）。
    pub roots: Vec<PathBuf>,
}

/// 直接含它就说明目录里有凭据，不能整目录授权。
///
/// 只看**直接子项**（一层），不做递归扫描：递归既慢又没必要——凭据总是放在固定的
/// 位置，而把 `~/.nvm/versions/node/v20` 整棵扫一遍毫无收益。
///
/// `config.toml` 必须在内：`~/.cargo/config.toml` 可以写 `[registries.*] token`，
/// 与 `credentials.toml` 是同一类东西。只盯着 `credentials.toml` 会漏。
const SENSITIVE_NAMES: [&str; 12] = [
    "credentials.toml",
    "credentials",
    "config.toml",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pgpass",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "config.json",
    "auth.json",
];

/// 安装根带凭据时，退而授权这些子目录。
///
/// 只对 `cargo` 有意义：`~/.cargo/bin` 有可执行文件，`registry`/`git` 有依赖源码缓存，
/// 而 `credentials.toml` 在根上、不在这些里面。其余种类没有已知的这类需求，
/// 直接放弃授权（用户可在 `[sandbox.fs] readable` 里自行放行）。
const FALLBACK_CHILDREN: [(&str, [&str; 3]); 1] = [("cargo", ["bin", "registry", "git"])];

/// 要探测的可执行文件名与分类。
///
/// `rustc` 不在列表里：真实 toolchain 在 `~/.rustup/toolchains/<tc>/bin`，而 `PATH` 上
/// 的 `rustc` 是 `~/.cargo/bin` 里的 shim。发现 shim 就够了——toolchain 由
/// [`RUSTUP_HOME_CHILD`] 一并补上。
const CANDIDATES: [(&str, &str); 5] = [
    ("node", "node"),
    ("python3", "python"),
    ("python", "python"),
    ("cargo", "cargo"),
    ("pip3", "python"),
];

/// rustup 的 toolchain 根（相对 `$HOME`）。
///
/// `~/.cargo/bin/cargo` 是 rustup 的 shim，它靠 `RUSTUP_HOME`（默认 `~/.rustup`）找到
/// `settings.toml` 与 toolchain。不授这个目录，`cargo` 会报 "no toolchain installed"——
/// 于是"为了安全"把 Rust 整个弄坏。它本身不含凭据（`settings.toml` 只是配置）。
const RUSTUP_HOME_CHILD: &str = ".rustup";

/// 从 `PATH` 发现可用的解释器安装根。
///
/// `env` 注入是为了可测：测试用假 `PATH` 指向自己造的目录树。
pub fn discover(env: &dyn Fn(&str) -> Option<String>) -> Vec<Install> {
    let Some(path) = env("PATH") else {
        return Vec::new();
    };
    // `$HOME` 也要规范化：解释器路径经过 `canonicalize`，若 `/tmp` 或家目录本身是符号链接，
    // 未规范化的 `$HOME` 会让"是否在家目录之下"的边界判断整片失效。
    let home = env("HOME")
        .map(PathBuf::from)
        .map(|home| std::fs::canonicalize(&home).unwrap_or(home));
    let mut found: Vec<Install> = Vec::new();

    for entry in std::env::split_paths(&path) {
        for (binary, kind) in CANDIDATES {
            let candidate = entry.join(binary);
            if !is_executable(&candidate) {
                continue;
            }
            let Some(exe) = std::fs::canonicalize(&candidate).ok() else {
                continue;
            };
            let Some(root) = install_root(&exe) else {
                continue;
            };
            // 系统路径已经由 `system_read_paths()` 覆盖，再列一遍是噪声。
            if is_system_root(&root) {
                continue;
            }
            let Some(roots) = grantable(root, kind, home.as_deref()) else {
                continue;
            };
            if found.iter().any(|existing| existing.roots == roots) {
                continue;
            }
            found.push(Install { kind, roots });
        }
    }
    found
}

/// 从可执行文件推安装根：只在它位于 `bin/` 下时才认。
///
/// - `~/.nvm/versions/node/v20/bin/node` → `~/.nvm/versions/node/v20`
/// - `~/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo` → 该 toolchain 目录
/// - `~/.pyenv/versions/3.12.0/bin/python3` → 版本目录
///
/// 不在 `bin/` 下就不猜：猜错的代价是授出一个过大的目录。
fn install_root(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    bin.parent().map(Path::to_path_buf)
}

/// `root` 能否整目录授权；能就返回它，不能则退回安全子目录（可能为空）。
fn grantable(root: PathBuf, kind: &str, home: Option<&Path>) -> Option<Vec<PathBuf>> {
    // 闸 1：安装根必须严格在 `$HOME` 之下，且不能就是 `$HOME` 本身。
    // 没有 `$HOME` 时（极端环境）也拒绝，因为此时无法判断边界。
    let home = home?;
    if !root.starts_with(home) || root == home {
        // 例如 `/opt/node`：不在家目录下，交给 `system_read_paths()` 的 `/opt` 覆盖。
        return None;
    }
    // 闸 2：根目录直接含凭据 → 只授安全子目录。
    let mut roots = if contains_sensitive(&root) {
        let allowed = FALLBACK_CHILDREN
            .iter()
            .find(|(name, _)| *name == kind)
            .map(|(_, children)| *children)?;
        allowed
            .iter()
            .map(|child| root.join(child))
            .filter(|path| path.exists() && !contains_sensitive(path))
            .collect::<Vec<PathBuf>>()
    } else {
        vec![root]
    };

    // `~/.cargo/bin/cargo` 是 rustup shim：真正的 toolchain 在 `~/.rustup`。
    // 不给它就报 "no toolchain installed"，于是"为了安全"把 Rust 整个弄坏。
    // `~/.rustup` 不含凭据（`settings.toml` 只是配置），按只读补上。
    //
    // 前提是 `cargo` 本身确实被授权了：`roots` 为空说明连 `bin` 都不存在，
    // 此时补授 rustup 只会白放开一个目录而 cargo 仍然跑不了。
    if kind == "cargo" && !roots.is_empty() {
        let rustup_home = home.join(RUSTUP_HOME_CHILD);
        if rustup_home.is_dir()
            && rustup_home.starts_with(home)
            && !contains_sensitive(&rustup_home)
        {
            roots.push(rustup_home);
        }
    }

    roots.sort_unstable();
    roots.dedup();
    (!roots.is_empty()).then_some(roots)
}

/// 目录的直接子项里是否有敏感文件名。
fn contains_sensitive(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // 读不到就按"有"处理：宁可少授权，也不要在看不见目录内容时放行。
        return true;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        SENSITIVE_NAMES
            .iter()
            .any(|sensitive| name.to_string_lossy() == *sensitive)
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// `system_read_paths()` 已覆盖的根，没必要重复授权。
///
/// `Path::starts_with` 按组件比较（`/usr-local` 不匹配 `/usr`），所以这里可以直接用。
fn is_system_root(root: &Path) -> bool {
    ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt"]
        .iter()
        .any(|system| root == Path::new(system) || root.starts_with(system))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn env_of(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn scratch(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "omnicrawl-runtimes-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("应能建临时目录");
        base
    }

    /// 造一个假的解释器：`<root>/bin/<name>` 可执行。
    fn fake_binary(root: &Path, name: &str) {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("应能建 bin");
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\n").expect("应能写");
        let mut permissions = std::fs::metadata(&path).expect("元数据").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("应能设权限");
    }

    /// nvm 布局：版本目录被完整授权。
    #[test]
    fn nvm_version_directory_is_granted() {
        let base = scratch("nvm");
        let home = base.join("home");
        let version = home.join(".nvm/versions/node/v20.11.0");
        fake_binary(&version, "node");

        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", version.join("bin").display().to_string()),
        ]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].kind, "node");
        assert_eq!(found[0].roots, vec![version]);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `~/.cargo` 含 `credentials.toml`：绝不授整个目录，只授 bin/registry/git。
    ///
    /// 这条是本次发现逻辑的核心安全断言。授 `~/.cargo` 等于把发布令牌交给沙盒。
    #[test]
    fn cargo_root_with_credentials_is_split() {
        let base = scratch("cargo");
        let home = base.join("home");
        let cargo_root = home.join(".cargo");
        fake_binary(&cargo_root, "cargo");
        std::fs::create_dir_all(cargo_root.join("registry")).expect("应能建");
        std::fs::create_dir_all(cargo_root.join("git")).expect("应能建");
        std::fs::write(cargo_root.join("credentials.toml"), "token = \"secret\"\n")
            .expect("应能写");

        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", cargo_root.join("bin").display().to_string()),
        ]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            !found[0].roots.contains(&cargo_root),
            "含凭据的根绝不能被整目录授权：{:?}",
            found[0].roots
        );
        for expected in ["bin", "registry", "git"] {
            assert!(
                found[0].roots.contains(&cargo_root.join(expected)),
                "缺少 {expected}：{:?}",
                found[0].roots
            );
            assert!(
                !found[0]
                    .roots
                    .contains(&cargo_root.join(expected).join("credentials.toml")),
                "凭据文件本身不能被授权"
            );
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 安装根绝不能爬到 `$HOME`：`$HOME/bin/node` 的根是 `$HOME`，必须整个放弃。
    ///
    /// 爬过头会把 `~/.ssh`、`~/.npmrc` 一起放出去——这比"node 跑不起来"严重得多。
    #[test]
    fn install_root_never_climbs_to_home() {
        let base = scratch("home");
        let home = base.join("home");
        fake_binary(&home, "node");

        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", home.join("bin").display().to_string()),
        ]));
        assert!(
            found.is_empty(),
            "把 $HOME 当安装根必须放弃：{found:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `~/.cargo/bin/cargo` 是 rustup shim：`~/.rustup` 也必须只读授权，
    /// 否则 cargo 报 "no toolchain installed"，Rust 整个不可用。
    #[test]
    fn rustup_home_is_granted_alongside_cargo() {
        let base = scratch("rustup");
        let home = base.join("home");
        let cargo_root = home.join(".cargo");
        fake_binary(&cargo_root, "cargo");
        std::fs::create_dir_all(cargo_root.join("registry")).expect("应能建");
        std::fs::write(cargo_root.join("credentials.toml"), "token = \"secret\"\n")
            .expect("应能写");
        let rustup_home = home.join(".rustup");
        std::fs::create_dir_all(rustup_home.join("toolchains/stable/bin")).expect("应能建");
        std::fs::write(rustup_home.join("settings.toml"), "default_toolchain = \"stable\"\n")
            .expect("应能写");

        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", cargo_root.join("bin").display().to_string()),
        ]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].roots.contains(&rustup_home),
            "缺少 ~/.rustup，cargo 会报 no toolchain installed：{:?}",
            found[0].roots
        );
        // 但凭据仍然不能出现在任何被授权的路径里。
        assert!(
            !found[0].roots.contains(&cargo_root),
            "含 credentials.toml 的 ~/.cargo 不能被整目录授权"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `~/.cargo/config.toml` 也可能写 registry token，同样是凭据。
    ///
    /// 只盯 `credentials.toml` 会漏：两者都是 crates.io 发布凭据的载体。
    #[test]
    fn cargo_config_toml_also_blocks_the_whole_root() {
        let base = scratch("cargo-config");
        let home = base.join("home");
        let cargo_root = home.join(".cargo");
        fake_binary(&cargo_root, "cargo");
        std::fs::create_dir_all(cargo_root.join("registry")).expect("应能建");
        // 只有 config.toml，没有 credentials.toml。
        std::fs::write(
            cargo_root.join("config.toml"),
            "[registries.crates-io]\ntoken = \"secret\"\n",
        )
        .expect("应能写");

        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", cargo_root.join("bin").display().to_string()),
        ]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            !found[0].roots.contains(&cargo_root),
            "含 config.toml 的 ~/.cargo 不能被整目录授权：{:?}",
            found[0].roots
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn system_roots_are_skipped() {
        for root in ["/usr", "/usr/local", "/opt", "/bin"] {
            assert!(is_system_root(Path::new(root)), "{root}");
        }
        assert!(!is_system_root(Path::new("/home/u/.pyenv/versions/3.12.0")));
    }

    #[test]
    fn install_root_requires_a_bin_parent() {
        // 不在 bin/ 下就不猜。
        assert_eq!(
            install_root(Path::new("/home/u/.nvm/versions/node/v20/node")),
            None
        );
        assert_eq!(
            install_root(Path::new(
                "/home/u/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/cargo"
            )),
            Some(PathBuf::from(
                "/home/u/.rustup/toolchains/stable-x86_64-unknown-linux-gnu"
            ))
        );
    }

    /// pyenv 布局与 nvm 同形，根是版本目录。
    #[test]
    fn pyenv_version_directory_is_granted() {
        let base = scratch("pyenv");
        let home = base.join("home");
        let version = home.join(".pyenv/versions/3.12.0");
        fake_binary(&version, "python3");

        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", version.join("bin").display().to_string()),
        ]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].roots, vec![version]);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 不在 PATH 里的解释器不该被发现。
    #[test]
    fn missing_binaries_are_ignored() {
        let base = scratch("missing");
        let home = base.join("home");
        std::fs::create_dir_all(&home).expect("应能建");

        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", home.display().to_string()),
        ]));
        assert!(found.is_empty(), "{found:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 同一路径被多个 PATH 条目命中时只留一份。
    #[test]
    fn duplicate_paths_collapse() {
        let base = scratch("dedupe");
        let home = base.join("home");
        let version = home.join(".nvm/versions/node/v20.11.0");
        fake_binary(&version, "node");
        let bin = version.join("bin");

        let path = format!("{}:{}", bin.display(), bin.display());
        let found = discover(&env_of(vec![
            ("HOME", home.display().to_string()),
            ("PATH", path),
        ]));
        assert_eq!(found.len(), 1, "{found:?}");

        let _ = std::fs::remove_dir_all(&base);
    }
}
