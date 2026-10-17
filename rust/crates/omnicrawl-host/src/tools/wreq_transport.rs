//! fetcher 专用的浏览器指纹传输：wreq（BoringSSL）+ wreq-util 设备档案。
//!
//! ## 为什么不能复用 ureq 传输
//!
//! [`super::web_transport::UreqWebTransport`] 是 ureq + rustls：TLS ClientHello 与
//! HTTP/2 设置都是固定的库指纹，任何做 TLS 指纹校验（JA3/JA4）的站点都会直接拒绝。
//! 而 Python 侧并不是「可选增强」——`omnicrawl/net/fetcher.py` 把这件事委托给 curl_cffi
//! 的 `impersonate` 参数（`_create_session`，第 169 行），且默认值就是 `chrome`
//! （`_normalize_impersonate` 对空值与未知值都回退 `chrome`）。
//!
//! 因此这里用 wreq（BoringSSL 后端）加 wreq-util 的浏览器档案对齐同一语义：指纹模拟
//! 走的是真实浏览器设备档案（TLS 扩展顺序、密码套件、ALPN、HTTP/2 SETTINGS 顺序与
//! 伪头顺序），而不是靠字符串拼凑。
//!
//! ## 线程模型
//!
//! 本版本的 wreq 只有异步 API（不含 blocking 模块），而宿主工具执行体是阻塞式的，
//! 且 `omnicrawl-api` 自身就跑在 tokio 上。若在此处调用 `Runtime::block_on`，一旦
//! 调用方线程已处于某个 runtime 中就 panic（"Cannot start a runtime from within a
//! runtime"）。所以改为**一条专用线程持有 current_thread runtime**，调用方通过 channel
//! 提交任务并同步等待回复——无论调用方位于什么线程都安全，代价是一次跨线程往返。

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};

use serde_json::Value;
use wreq_util::Profile;

use super::web_transport::{WebError, WebErrorKind, WebRequest, WebResponse, WebTransport};

/// Python `_IMPERSONATE_OPTIONS` 的顺序（`omnicrawl/net/fetcher.py:62`）。
///
/// Python 用 `name == option or name.startswith(option)` 做前缀匹配，所以 `chrome124`
/// 会落到 `chrome`。四个名字互不为前缀，因此顺序不影响结果，保留原顺序只为对照方便。
pub const IMPERSONATE_OPTIONS: [&str; 4] = ["chrome", "firefox", "safari", "edge"];

/// 未显式给 `impersonate` 时的档案：对齐 Python `_normalize_impersonate` 的兜底。
pub const DEFAULT_IMPERSONATE: &str = "chrome";

/// 归一化 `impersonate` 参数，语义对齐 Python `_normalize_impersonate`。
///
/// Python 实现是 `str(value or "chrome").strip().lower()` 加前缀匹配，未知值回退
/// `chrome`。其中 `value or "chrome"` 走的是 Python 真值语义：`None`、`False`、`0`
/// 与 `""` 都算假值，会落到默认 `chrome`；`True` 则先 `str()` 成 `"True"` 再参与
/// 前缀匹配（同样匹配不上而回退 `chrome`）。这里逐条对齐，不图省事只判 `None`。
pub fn normalize_impersonate(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Number(number)) => {
            // 0 在 Python 里是假值，会走 `or "chrome"`；其余数字按 str() 表示。
            if number.as_f64() == Some(0.0) {
                String::new()
            } else {
                number.to_string()
            }
        }
        // None / Bool(false) / 数字 0 / 空串：Python 的假值分支 → 默认 chrome。
        _ => String::new(),
    };
    let name = raw.trim().to_lowercase();
    for option in IMPERSONATE_OPTIONS {
        if name == option || name.starts_with(option) {
            return option.to_string();
        }
    }
    "chrome".to_string()
}

/// 把归一化后的档案名映射到 wreq-util 的设备档案。
///
/// wreq-util 的 [`Profile`] 是 `#[non_exhaustive]` 枚举，只有带版本号的变体、没有
/// `FromStr`，所以族名与版本的对应关系必须在这里硬编码。这里取各族**最新**档案来
/// 代表 Python 侧不带版本号的 `chrome`/`firefox`/`safari`/`edge`。
///
/// 这是已记录的偏差：curl_cffi 的同名别名指向的具体版本可能不同，而且升级
/// wreq-util 后这里要手动跟进（编译器不会提醒有新档案）。
fn profile_for(name: &str) -> Profile {
    match name {
        "firefox" => Profile::Firefox151,
        "safari" => Profile::Safari26,
        "edge" => Profile::Edge148,
        // 归一化保证只会是四个已知名字之一；`chrome` 与兜底走同一分支。
        _ => Profile::Chrome149,
    }
}

/// 复用键：档案、证书校验、代理与重定向上限都会改变客户端的连接层配置，必须分开缓存。
///
/// 超时**不**进键：它在每次请求上单独设置（`RequestBuilder::timeout`），同一客户端
/// 可以服务不同超时的请求。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    profile: Profile,
    insecure: bool,
    proxy: Option<String>,
    max_redirects: u32,
}

/// 客户端缓存上限。实际用到的组合很少（代理地址通常只有系统代理一个），这里只是防止
/// 异常输入把缓存撑爆；超限时整体清空而不是淘汰单条，实现简单且不会有陈旧连接堆积。
const MAX_CACHED_CLIENTS: usize = 16;

/// 浏览器指纹传输：把请求交给专用 runtime 线程执行。
pub struct WreqWebTransport {
    jobs: Sender<Job>,
}

/// 一次抓取任务：请求 + 回传通道。
struct Job {
    request: WebRequest,
    reply: Sender<Result<WebResponse, WebError>>,
}

impl WreqWebTransport {
    /// 取得共享的传输线程句柄。
    ///
    /// 这里不做「每次构造就起一条线程」：`RegistryOptions` 里的 `FetcherOptions`
    /// 会在功能开关切换（`apply_feature` 重建工具表）时被重新构造，若每次起新线程
    /// 就会持续泄漏。宿主只需要一条传输线程，所以用 `OnceLock` 让所有实例共用它。
    pub fn new() -> Self {
        Self {
            jobs: transport_sender(),
        }
    }
}

/// 进程级唯一的传输线程句柄（首次调用时启动线程）。
fn transport_sender() -> Sender<Job> {
    static SENDER: std::sync::OnceLock<Sender<Job>> = std::sync::OnceLock::new();
    SENDER
        .get_or_init(|| {
            let (jobs, receiver) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("omnicrawl-wreq".to_string())
                .spawn(move || transport_loop(receiver))
                .expect("浏览器指纹传输线程应当能创建");
            jobs
        })
        .clone()
}

impl Default for WreqWebTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl WebTransport for WreqWebTransport {
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
        let (reply, result) = mpsc::channel();
        self.jobs
            .send(Job {
                request: request.clone(),
                reply,
            })
            .map_err(|_| WebError::new(WebErrorKind::Other, TRANSPORT_GONE))?;
        // 传输线程若在任务执行中 panic，recv 会立刻出错而不是永久阻塞。
        result
            .recv()
            .map_err(|_| WebError::new(WebErrorKind::Other, TRANSPORT_GONE))?
    }

    /// 只补档案没带的导航头，其余一律交给设备档案。
    ///
    /// wreq 把档案头作为客户端默认头，而请求头会**覆盖**默认头，所以这里多写一个档案已经
    /// 生成过的头（UA、`sec-ch-ua*`、`sec-fetch-dest/mode/site`、`accept`、`accept-language`）
    /// 就等于把档案的真实值换成本地手写值，HTTP 层立刻与 TLS 层自相矛盾。档案缺的只有
    /// 两个顶层导航头：`Upgrade-Insecure-Requests`（Chromium/Firefox/Safari 的 GET 导航都带）
    /// 与 `Sec-Fetch-User`（只有 Chromium 与 Firefox 带，Safari 不发）。
    ///
    /// 刻意不发 `Connection` 与 `Accept-Encoding`：档案走 HTTP/2，`Connection` 是 h2 禁止的
    /// 连接专有头；`Accept-Encoding` 一旦发出就必须能解压（wreq 侧要开 gzip/br/zstd 特性），
    /// 当前依赖树没开，发了会拿到压缩字节流。
    fn navigation_headers(
        &self,
        user_agent: &str,
        impersonate: Option<&str>,
    ) -> Vec<(String, String)> {
        // 档案自带 UA；`user_agent` 只服务于没有档案的传输。
        let _ = user_agent;
        let family = impersonate.unwrap_or("chrome");
        let mut headers = vec![("Upgrade-Insecure-Requests".to_string(), "1".to_string())];
        if family != "safari" {
            headers.push(("Sec-Fetch-User".to_string(), "?1".to_string()));
        }
        headers
    }
}

const TRANSPORT_GONE: &str = "浏览器指纹传输线程不可用。";

/// 传输线程主体：建 current_thread runtime，串行处理队列中的任务。
///
/// 串行是刻意的：并发由调用方提供（fetcher 用 `std::thread::scope` 起线程池），
/// 这里只要一个 runtime 就够，避免把并发的等待变成运行时内部的调度问题。
fn transport_loop(receiver: Receiver<Job>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            // 建不起 runtime 不能让调用方永远阻塞：逐个回报失败后退出。
            let message = format!("无法创建浏览器指纹传输运行时：{error}");
            while let Ok(job) = receiver.recv() {
                let _ = job
                    .reply
                    .send(Err(WebError::new(WebErrorKind::Other, message.clone())));
            }
            return;
        }
    };
    let mut clients = ClientCache::default();
    while let Ok(job) = receiver.recv() {
        let outcome = runtime.block_on(execute(&mut clients, &job.request));
        let _ = job.reply.send(outcome);
    }
}

/// 按 [`ClientKey`] 复用 wreq 客户端：构建时要装配整套 TLS/HTTP2 指纹，代价不低。
#[derive(Default)]
struct ClientCache {
    clients: HashMap<ClientKey, wreq::Client>,
}

impl ClientCache {
    fn get(&mut self, request: &WebRequest) -> Result<wreq::Client, WebError> {
        let name = request
            .impersonate
            .clone()
            .unwrap_or_else(|| "chrome".to_string());
        let key = ClientKey {
            profile: profile_for(&name),
            insecure: request.insecure,
            proxy: request.proxy.clone(),
            max_redirects: request.max_redirects,
        };
        if let Some(client) = self.clients.get(&key) {
            return Ok(client.clone());
        }
        let client = build_client(&key)?;
        if self.clients.len() >= MAX_CACHED_CLIENTS {
            self.clients.clear();
        }
        self.clients.insert(key, client.clone());
        Ok(client)
    }
}

fn build_client(key: &ClientKey) -> Result<wreq::Client, WebError> {
    // 显式绑定 Windows：宿主就是 Windows，系统代理、时区与语言环境都是 Windows，且档案里
    // 只有**头部**随平台变化（TLS 与 HTTP/2 选项与平台无关），所以固定平台不会削弱指纹，
    // 只让 UA / sec-ch-ua-platform 与本机真实环境一致。Safari 档案没有平台变体，绑定被忽略。
    let emulation = wreq_util::Emulation::builder()
        .profile(key.profile)
        .platform(wreq_util::Platform::Windows)
        .build();
    let mut builder = wreq::Client::builder()
        .emulation(emulation)
        // 跟随重定向的上限对齐 Python 的 httpx 默认（本项目常量 MAX_REDIRECTS）。
        .redirect(wreq::redirect::Policy::limited(key.max_redirects as usize))
        // insecure=true 关掉证书链校验，对齐 Python 的 verify=False。
        .tls_cert_verification(!key.insecure);
    if let Some(address) = &key.proxy {
        let proxy = wreq::Proxy::all(address.clone()).map_err(|error| {
            WebError::new(WebErrorKind::Other, format!("代理地址无效：{error}"))
        })?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|error| {
        WebError::new(
            WebErrorKind::Other,
            format!("无法构建浏览器指纹客户端：{error}"),
        )
    })
}

/// 在 runtime 上执行一次请求，产出与 ureq 传输完全一致的 [`WebResponse`] 形状。
async fn execute(cache: &mut ClientCache, request: &WebRequest) -> Result<WebResponse, WebError> {
    let client = cache.get(request)?;
    let method = wreq::Method::from_bytes(request.method.as_bytes())
        .map_err(|error| WebError::new(WebErrorKind::Other, format!("HTTP 方法无效：{error}")))?;
    let mut builder = client.request(method, request.url.clone());
    let mut headers = wreq::header::HeaderMap::new();
    for (name, value) in &request.headers {
        let header_name =
            wreq::header::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                WebError::new(WebErrorKind::Other, format!("请求头名无效：{error}"))
            })?;
        let header_value = wreq::header::HeaderValue::from_str(value).map_err(|error| {
            WebError::new(WebErrorKind::Other, format!("请求头值无效：{error}"))
        })?;
        // append 而非 insert：同一请求头出现多次时不能被后者覆盖。
        headers.append(header_name, header_value);
    }
    builder = builder
        .headers(headers)
        // 超时逐请求设置，与客户端缓存解耦。
        .timeout(request.timeout);
    if !request.body.is_empty() {
        builder = builder.body(request.body.clone());
    }

    let response = builder.send().await.map_err(classify)?;
    let status = response.status().as_u16();
    // 自动重定向之后的最终地址（Python 侧对应 response.url）。
    let final_url = response.uri().to_string();
    let body = response.bytes().await.map_err(classify)?.to_vec();
    Ok(WebResponse {
        status,
        final_url,
        body,
    })
}

/// 把 wreq 的错误映射到内核统一的错误分类，供 `fetcher::friendly_error` 转成中文文案。
///
/// 分类与 ureq 传输保持同一套语义：超时、连接失败、TLS 失败各有专属提示，其余归入
/// `Other` 并原样带上底层文本（便于排障，不吞信息）。
fn classify(error: wreq::Error) -> WebError {
    let kind = if error.is_timeout() {
        WebErrorKind::Timeout
    } else if error.is_connect() {
        WebErrorKind::Connect
    } else {
        let message = error.to_string().to_lowercase();
        // wreq/boring 的证书错误没有稳定的结构化分类，只能按关键词识别。
        if message.contains("certificate")
            || message.contains("cert")
            || message.contains("unknown ca")
            || message.contains("tls")
            || message.contains("ssl")
        {
            WebErrorKind::Tls
        } else {
            WebErrorKind::Other
        }
    };
    WebError::new(kind, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wreq::IntoEmulation;

    #[test]
    fn impersonate_normalization_mirrors_python() {
        // 大小写与前缀（curl_cffi 支持 chrome124 这类带版本后缀的写法）。
        assert_eq!(normalize_impersonate(Some(&json!("chrome124"))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!("Chrome"))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(" FIREFOX "))), "firefox");
        assert_eq!(normalize_impersonate(Some(&json!("firefox133"))), "firefox");
        assert_eq!(normalize_impersonate(Some(&json!("safari"))), "safari");
        assert_eq!(normalize_impersonate(Some(&json!("edge"))), "edge");
        // 未知值、空值、缺失与 Python 假值一律回退 chrome。
        assert_eq!(normalize_impersonate(Some(&json!("curl"))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(""))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!("   "))), "chrome");
        assert_eq!(normalize_impersonate(None), "chrome");
        assert_eq!(normalize_impersonate(Some(&Value::Null)), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(0))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(false))), "chrome");
        // 非零数字与 true 先 str()，匹配不上再回退。
        assert_eq!(normalize_impersonate(Some(&json!(1))), "chrome");
        assert_eq!(normalize_impersonate(Some(&json!(true))), "chrome");
    }

    #[test]
    fn profiles_cover_all_python_options() {
        // 四个族名都必须有档案，且 chrome 兜底不能落到别的族。
        assert_eq!(profile_for("chrome"), Profile::Chrome149);
        assert_eq!(profile_for("firefox"), Profile::Firefox151);
        assert_eq!(profile_for("safari"), Profile::Safari26);
        assert_eq!(profile_for("edge"), Profile::Edge148);
        assert_eq!(profile_for("unknown"), Profile::Chrome149);
    }

    /// 锚定「档案为准」这条契约：档案自己就生成整套桌面浏览器导航头（UA 与 Client Hints
    /// 同源于同一个版本），所以传输层不能再写一遍——写一遍就会把档案值顶掉，让 HTTP 层
    /// 声称的版本与 TLS 层的档案版本不一致。
    #[test]
    fn profile_emulation_supplies_the_browser_header_set() {
        let emulation = wreq_util::Emulation::builder()
            .profile(Profile::Chrome149)
            .platform(wreq_util::Platform::Windows)
            .build()
            .into_emulation();
        let headers = &emulation.headers;
        let value = |name: &str| {
            headers
                .get(name)
                .and_then(|item| item.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        // 与 TLS/HTTP2 指纹同源的 UA 与 Client Hints。
        assert!(
            value("user-agent").contains("Chrome/149"),
            "{}",
            value("user-agent")
        );
        assert!(value("sec-ch-ua").contains("149"), "{}", value("sec-ch-ua"));
        assert_eq!(value("sec-ch-ua-platform"), "\"Windows\"");
        assert_eq!(value("sec-ch-ua-mobile"), "?0");
        // 顶层导航头。
        assert_eq!(value("sec-fetch-dest"), "document");
        assert_eq!(value("sec-fetch-mode"), "navigate");
        assert_eq!(value("sec-fetch-site"), "none");
        assert!(
            value("accept").starts_with("text/html"),
            "{}",
            value("accept")
        );
        // h2 禁止连接专有头，档案本来也不发。
        assert_eq!(value("connection"), "");
        // 这两个是档案**不**提供的，正是 `navigation_headers` 要补的部分。
        assert_eq!(value("upgrade-insecure-requests"), "");
        assert_eq!(value("sec-fetch-user"), "");
    }

    /// 传输层只补档案缺的导航头，且不能重复写档案已经生成的头。
    #[test]
    fn navigation_headers_only_fill_what_profiles_omit() {
        let transport = WreqWebTransport::new();
        let chrome = transport.navigation_headers("ignored", Some("chrome"));
        assert!(chrome.contains(&("Upgrade-Insecure-Requests".to_string(), "1".to_string())));
        assert!(chrome.contains(&("Sec-Fetch-User".to_string(), "?1".to_string())));

        // 手写这些头会把档案值顶掉：UA/Client Hints/部分导航头由档案提供；
        // Connection 是 h2 违规头；Accept-Encoding 一发就必须能解压。
        const PROFILE_OWNED: [&str; 11] = [
            "user-agent",
            "sec-ch-ua",
            "sec-ch-ua-platform",
            "sec-ch-ua-mobile",
            "accept",
            "accept-language",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "connection",
            "accept-encoding",
        ];
        for (name, _) in &chrome {
            let lowered = name.to_lowercase();
            assert!(
                !PROFILE_OWNED.contains(&lowered.as_str()),
                "不该重复写档案拥有的头：{name}"
            );
        }

        // Safari 不发 Sec-Fetch-User。
        let safari = transport.navigation_headers("ignored", Some("safari"));
        assert!(!safari
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("sec-fetch-user")));
        assert!(safari.contains(&("Upgrade-Insecure-Requests".to_string(), "1".to_string())));

        // 缺省族名按 chrome 处理（与 `fetch_one` 的默认值一致）。
        let fallback = transport.navigation_headers("ignored", None);
        assert!(fallback.contains(&("Sec-Fetch-User".to_string(), "?1".to_string())));
    }
}
