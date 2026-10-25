//! 宿主接入点的落点验证：`install_agent_temp_workspace` 必须真的建目录、补清理、
//! 并在关闭时停掉调度线程。
//!
//! 这一层此前完全没有消费者，只有 `workspace_temp_parity.rs` 在直接调用底层 API。
//! 因此这里补的是「接进来之后行为对不对」，而不是再抄一遍对照数据集。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_workspace::temp::{
    install_agent_temp_workspace, DEFAULT_AGENT_TEMP_SUBDIRECTORIES, LAST_CLEANUP_FILENAME,
};

static SERIAL: AtomicU64 = AtomicU64::new(0);

/// 用例根目录：`home/` 放配置（配置固定读 `~/.OmniCrawl/config.toml`），`workspace/` 是被
/// 接入的工作区。两者必须分开——工作区里也有 `.omnicrawl`，混在一起在 Windows 上会因
/// 大小写不敏感而互相顶掉。
struct Case {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
}

impl Case {
    fn new(name: &str) -> Self {
        let serial = SERIAL.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "ocl-agent-temp-{name}-{}-{serial}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(home.join(".OmniCrawl")).expect("建配置目录");
        std::fs::create_dir_all(&workspace).expect("建工作区");
        Self {
            root,
            home,
            workspace,
        }
    }

    fn write_config(&self, text: &str) {
        std::fs::write(self.home.join(".OmniCrawl").join("config.toml"), text).expect("写配置");
    }

    fn environment(&self) -> ConfigEnvironment {
        ConfigEnvironment::new(&self.home, std::env::consts::OS)
    }

    fn temp_root(&self) -> PathBuf {
        self.workspace.join(".omnicrawl").join(".agent_tmp")
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 展示路径用平台分隔符（与 Python `str(Path)` 一致），因此比较时统一折成 `/`。
fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

fn install(case: &Case) -> omnicrawl_workspace::AgentTempInstallation {
    install_agent_temp_workspace(&case.environment(), &case.workspace).expect("应当装配出句柄")
}

/// 默认配置：建出分类子目录与说明文件，并因「从未清理过」立刻补清理一次。
#[test]
fn installing_creates_the_categorised_directories() {
    let case = Case::new("install");
    case.write_config("");
    let mut installation = install(&case);

    assert_eq!(
        slashed(&installation.display_path()),
        ".omnicrawl/.agent_tmp",
        "展示路径必须相对工作区，与提示词和工具落盘一致"
    );
    assert!(installation.note.is_none(), "{:?}", installation.note);

    let temp = case.temp_root();
    for subdir in DEFAULT_AGENT_TEMP_SUBDIRECTORIES {
        assert!(temp.join(subdir).is_dir(), "缺少分类子目录：{subdir}");
    }
    assert!(temp.join("README.md").is_file(), "说明文件应当写到根上");
    assert!(
        temp.join(LAST_CLEANUP_FILENAME).is_file(),
        "补清理必须落下时间戳，否则每次启动都会当成到期"
    );

    installation.close();
}

/// 清理边界只在临时目录内部：目录外的用户文件不受影响。
#[test]
fn cleanup_stays_inside_the_temp_directory() {
    let case = Case::new("cleanup");
    case.write_config("");
    // 第一次装落下「上次清理」基线，第二次才会因为过期而补清理。
    install(&case).close();

    let temp = case.temp_root();
    std::fs::write(temp.join("files").join("junk.txt"), "x").expect("造产物");
    let outside = case.workspace.join("keep-me.txt");
    std::fs::write(&outside, "用户文件").expect("造工作区文件");
    // 间隔调到 1 小时并把时间戳推回两小时前：不依赖默认的 24 小时。
    case.write_config("[agent_temp]\ncleanup_interval_hours = 1\n");
    let marker = temp.join(LAST_CLEANUP_FILENAME);
    std::fs::write(&marker, "").expect("写标记");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&marker)
        .expect("打开标记");
    file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60))
        .expect("改标记时间");
    drop(file);

    let mut installation = install(&case);
    assert!(
        installation.cleaned_entries > 0,
        "距上次清理超过间隔时必须补清理"
    );
    assert!(
        !temp.join("files").join("junk.txt").exists(),
        "临时产物应当被清掉"
    );
    assert!(outside.is_file(), "临时目录外的文件不得被删");
    assert!(temp.join("files").is_dir(), "清理后要重建分类子目录");

    installation.close();
}

/// `[agent_temp] enabled = false` 时完全不接入：不建目录、不返句柄。
#[test]
fn a_disabled_section_installs_nothing() {
    let case = Case::new("disabled");
    case.write_config("[agent_temp]\nenabled = false\n");
    assert!(
        install_agent_temp_workspace(&case.environment(), &case.workspace).is_none(),
        "关掉之后不得建目录"
    );
    assert!(!case.temp_root().exists(), "关掉之后不得留下临时目录");
}

/// 自定义 `directory` 要真的被采用——这是「配置不再是摆设」的直接证据。
#[test]
fn the_configured_directory_is_honoured() {
    let case = Case::new("custom");
    case.write_config("[agent_temp]\ndirectory = \".cache/work\"\n");
    let mut installation = install(&case);
    assert_eq!(slashed(&installation.display_path()), ".cache/work");
    assert!(case.workspace.join(".cache").join("work").is_dir());
    installation.close();
}

/// 配置写坏时降级到默认目录而不是拒绝启动。
#[test]
fn a_broken_section_falls_back_to_the_defaults() {
    let case = Case::new("broken");
    case.write_config("[agent_temp]\ndirectory = 42\n");
    let mut installation = install(&case);
    assert_eq!(slashed(&installation.display_path()), ".omnicrawl/.agent_tmp");
    assert!(
        installation.note.is_some(),
        "降级原因必须如实带出来，否则用户不知道配置没生效"
    );
    installation.close();
}

/// 路径逃出工作区时不得接入：清理越界是这条代码最不能犯的错。
#[test]
fn a_directory_outside_the_workspace_is_refused() {
    let case = Case::new("escape");
    case.write_config("[agent_temp]\ndirectory = \"../outside\"\n");
    assert!(
        install_agent_temp_workspace(&case.environment(), &case.workspace).is_none(),
        "越界目录必须拒绝接入"
    );
}
