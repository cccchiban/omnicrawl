//! TLS 会话恢复的接入验证：`[tls_resumption]` 现在真的会被读，并且只在 `enabled` 为真时
//! 才构造缓存。
//!
//! 这条路径此前端到端不可达——配置段的读写函数全仓零调用，而 `FetcherOptions` 的注释却
//! 声称「由配置控制」。这里钉住的是「注释与事实一致」。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_host::tools::fetcher::tls_session_cache_from_config;

static SERIAL: AtomicU64 = AtomicU64::new(0);

struct Case {
    root: PathBuf,
}

impl Case {
    fn new(name: &str) -> Self {
        let serial = SERIAL.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "ocl-tls-resumption-{name}-{}-{serial}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".OmniCrawl")).expect("建配置目录");
        Self { root }
    }

    fn write_config(&self, text: &str) {
        std::fs::write(self.root.join(".OmniCrawl").join("config.toml"), text).expect("写配置");
    }

    fn environment(&self) -> ConfigEnvironment {
        ConfigEnvironment::new(&self.root, std::env::consts::OS)
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 缺段与显式关闭都必须走完整握手：会话恢复改的是密钥调度，不能默认开。
#[test]
fn the_cache_stays_off_unless_enabled() {
    let case = Case::new("off");
    case.write_config("");
    assert!(
        tls_session_cache_from_config(&case.environment()).is_none(),
        "缺段时不得建缓存"
    );

    case.write_config("[tls_resumption]\nenabled = false\n");
    assert!(
        tls_session_cache_from_config(&case.environment()).is_none(),
        "显式关闭时不得建缓存"
    );
}

/// 开启后容量要来自 `cache_capacity`，而不是永远用默认值。
#[test]
fn the_configured_capacity_reaches_the_cache() {
    let case = Case::new("on");
    case.write_config("[tls_resumption]\nenabled = true\ncache_capacity = 32\n");
    let cache = tls_session_cache_from_config(&case.environment()).expect("开启后应当建缓存");
    assert_eq!(cache.capacity(), 32, "容量必须取自配置");
}

/// 容量为 0 按默认值处理而不是「不许缓存」——否则开启恢复会变成一句空话。
#[test]
fn a_zero_capacity_falls_back_to_the_default() {
    let case = Case::new("zero");
    case.write_config("[tls_resumption]\nenabled = true\ncache_capacity = 0\n");
    let cache = tls_session_cache_from_config(&case.environment()).expect("开启后应当建缓存");
    assert_eq!(cache.capacity(), 256);
}

/// 配置写坏时按关闭处理：宁可完整握手，也不猜。
#[test]
fn a_broken_section_disables_resumption() {
    let case = Case::new("broken");
    case.write_config("[tls_resumption]\nenabled = \"yes\"\n");
    assert!(
        tls_session_cache_from_config(&case.environment()).is_none(),
        "类型错误必须降级为关闭"
    );
}
