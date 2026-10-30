//! 新版本检测：查询 npm registry 上的 `omnicrawl-cli` 最新版本，并与本地产品版本比较。
//!
//! 三个约束决定了实现形态：
//!
//! 1. **不能拖慢启动**：检测走后台线程，调用方拿到的是「立刻返回的句柄」，
//!    界面先按「检查中」渲染，结果回来再补。网络不可用时最坏情况是一条超时，
//!    但这不会发生在启动路径上。
//! 2. **一小时内不重复请求**：结果按 `version_check.json` 落盘缓存，跨进程有效。
//!    进程内还叠一层内存缓存，避免同一进程反复读盘。
//! 3. **任何失败都必须静默**：检测是附加信息，不能因为网络不可达、registry 改版、
//!    缓存损坏就把「有新版本」变成启动噪音。所有错误都收敛成 `Err`，由调用方决定不说。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// npm 包名：产品版本就发布在这个包上（`packages/cli/package.json`）。
pub const PACKAGE_NAME: &str = "omnicrawl-cli";

/// npm registry 上该包的元数据地址。
const REGISTRY_URL: &str = "https://registry.npmjs.org/omnicrawl-cli";

/// 缓存有效期：一小时内复用同一份结果，不重复联网。
pub const CACHE_TTL_SECONDS: i64 = 3600;

/// 单次请求超时。检测是后台附加动作，宁可放弃也不要挂住线程。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 缓存文件名（放在 `~/.OmniCrawl` 下，与其它本机状态同级）。
const CACHE_FILENAME: &str = "version_check.json";

/// 一次检测的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// 本地产品版本。
    pub current: String,
    /// registry 上的最新版本。
    pub latest: String,
    /// 是否确实存在更新的版本。
    pub has_update: bool,
}

impl UpdateStatus {
    fn new(current: &str, latest: &str) -> Self {
        Self {
            current: current.to_string(),
            latest: latest.to_string(),
            has_update: is_newer(latest, current),
        }
    }

    /// 一句话提示；没有更新时返回 `None`（调用方据此不显示任何东西）。
    pub fn notice(&self) -> Option<String> {
        if !self.has_update {
            return None;
        }
        Some(format!(
            "新版本 {} 可用（当前 {}），运行 npm i -g {PACKAGE_NAME} 升级",
            self.latest, self.current
        ))
    }
}

/// 磁盘上的缓存条目：结果 + 写入时刻，用来判断是否还在有效期内。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CacheEntry {
    checked_at: i64,
    status: UpdateStatus,
}

/// 缓存的读写口：目录由调用方给（通常是 `~/.OmniCrawl`），不在这里猜路径。
#[derive(Debug, Clone)]
pub struct UpdateCache {
    path: PathBuf,
}

impl UpdateCache {
    /// 在 `dir` 下用默认文件名建缓存句柄（不建目录、不碰磁盘）。
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(CACHE_FILENAME),
        }
    }

    /// 显式指定缓存文件（测试用）。
    pub fn at_file(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读一份**仍在有效期内的**缓存。
    ///
    /// 过期、损坏、字段缺失一律当成没有缓存：过期的结果会被当成结论继续显示，
    /// 那比不显示更糟。
    pub fn read_fresh(&self, now: i64) -> Option<UpdateStatus> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let entry: CacheEntry = serde_json::from_str(&text).ok()?;
        // 时钟回拨会让 `checked_at` 落在未来：这种记录不能当成「刚查过」，
        // 否则时间戳一旦跑到前头，缓存就会一直有效下去。
        if entry.checked_at > now {
            return None;
        }
        let age = now.saturating_sub(entry.checked_at);
        if age > CACHE_TTL_SECONDS {
            return None;
        }
        // 缓存里的当前版本必须与本次运行的版本一致：升级之后旧缓存会把
        // 「已是最新」继续报一小时的假结论。
        if entry.status.current != omnicrawl_version::VERSION {
            return None;
        }
        Some(entry.status)
    }

    /// 写入缓存；失败不报错（缓存只是优化，写不进去最多下次再查）。
    pub fn write(&self, status: &UpdateStatus, now: i64) {
        let entry = CacheEntry {
            checked_at: now,
            status: status.clone(),
        };
        let Ok(text) = serde_json::to_string(&entry) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.path, text);
    }
}

/// 取当前时间戳（秒）。缓存过期判定一律走这里，测试可以传固定值。
pub fn now_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 查一次 registry 并返回结果（不读写缓存）。
///
/// 失败原因只用于诊断，不面向终端用户：检测失败等于「没有更新」，不需要解释。
pub fn fetch_latest(current: &str) -> Result<UpdateStatus, String> {
    // 与 omnicrawl-extensions 的 npm 安装器同一套 ureq 形态：
    // agent 由 config_builder 建，逐请求用 `.config()` 设超时。
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .get(REGISTRY_URL)
        .config()
        .timeout_connect(Some(REQUEST_TIMEOUT))
        .timeout_recv_response(Some(REQUEST_TIMEOUT))
        .timeout_recv_body(Some(REQUEST_TIMEOUT))
        .build()
        // npm 认这个 Accept 才回 install-v1 精简元数据（不含整份 versions）。
        .header("Accept", "application/vnd.npm.install-v1+json")
        .call()
        .map_err(|error| format!("请求 registry 失败：{error}"))?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(format!("registry 返回 HTTP {status}"));
    }
    let body = response
        .into_body()
        .read_to_string()
        .map_err(|error| format!("读取 registry 响应失败：{error}"))?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| format!("registry 响应不是合法 JSON：{error}"))?;
    let latest = value
        .get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(|latest| latest.as_str())
        .ok_or_else(|| "registry 响应里没有 dist-tags.latest".to_string())?;
    Ok(UpdateStatus::new(current, latest))
}

/// 带缓存的检测：一小时内直接复用磁盘结果。
///
/// 这是同步入口，供后台线程调用；界面侧用 [`UpdateCheck`] 拿异步结果。
pub fn check_with_cache(cache: &UpdateCache) -> Result<UpdateStatus, String> {
    let now = now_seconds();
    if let Some(status) = cache.read_fresh(now) {
        return Ok(status);
    }
    let status = fetch_latest(omnicrawl_version::VERSION)?;
    cache.write(&status, now);
    Ok(status)
}

/// 后台检测句柄：`spawn` 立刻返回，结果用 [`UpdateCheck::poll`] 取。
///
/// 不用 channel + 阻塞 recv：界面每帧都是非阻塞的，拿 `JoinHandle::is_finished`
/// 做一次无锁探测就够，也省掉一个线程同步原语。
pub struct UpdateCheck {
    handle: Option<std::thread::JoinHandle<Result<UpdateStatus, String>>>,
    result: Option<Result<UpdateStatus, String>>,
}

impl UpdateCheck {
    /// 起一个后台线程跑 `check_with_cache`。
    pub fn spawn(cache: UpdateCache) -> Self {
        let handle = std::thread::Builder::new()
            .name("omnicrawl-update-check".to_string())
            .spawn(move || check_with_cache(&cache));
        Self {
            // 线程起不来（资源耗尽）不是致命错误：当成「没有检测结果」。
            handle: handle.ok(),
            result: None,
        }
    }

    /// 已就绪的结果：`None` 表示还在检测中。
    ///
    /// 第一次就绪后再调用会一直返回同一份 `Some`，调用方不必清理状态。
    pub fn poll(&mut self) -> Option<&Result<UpdateStatus, String>> {
        if self.result.is_none() {
            let handle = self.handle.take()?;
            if !handle.is_finished() {
                self.handle = Some(handle);
                return None;
            }
            // 线程 panic 也收敛成错误：检测永远不该让调用方看见展开。
            let joined = handle
                .join()
                .unwrap_or_else(|_| Err("版本检测线程异常退出".to_string()));
            self.result = Some(joined);
        }
        self.result.as_ref()
    }
}

/// 语义化版本比较：`candidate` 是否比 `current` 新。
///
/// 只处理 `MAJOR.MINOR.PATCH` 与可选预发布后缀（`-beta.1`）：npm registry 的
/// `dist-tags.latest` 只会给正式版，但缓存文件可能被手改，这里按容错处理——
/// 解析不出的段当 0，预发布版比同号正式版旧。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (candidate_core, candidate_pre) = split_prerelease(candidate);
    let (current_core, current_pre) = split_prerelease(current);
    let candidate_parts = parse_version_core(candidate_core);
    let current_parts = parse_version_core(current_core);
    for index in 0..3 {
        let left = candidate_parts.get(index).copied().unwrap_or(0);
        let right = current_parts.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    // 主版本号相同：没有预发布后缀的那个更新（1.0.0 > 1.0.0-beta）。
    match (candidate_pre, current_pre) {
        (None, None) => false,
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (Some(left), Some(right)) => left > right,
    }
}

/// 拆出 `1.2.3-beta.1` 的主干与预发布后缀。
fn split_prerelease(version: &str) -> (&str, Option<&str>) {
    let version = version.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    }
}

/// 主干版本号转数字；非数字段当 0（容错，不报错）。
fn parse_version_core(core: &str) -> Vec<u64> {
    core.split('.')
        .map(|part| part.trim().parse::<u64>().unwrap_or(0))
        .collect()
}
