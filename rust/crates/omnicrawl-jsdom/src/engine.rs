//! 沙箱运行器：把一个页面的快照 DOM 交给 boa，执行脚本，再把结果收回来。
//!
//! 边界（与抓取工具的整体约定一致）：
//! - **沙箱不联网**：`fetch` / `XMLHttpRequest` 存在，但取数由调用方注入的
//!   [`HttpSource`] 完成；没有接入通道时它们退回「拒绝式桩」——函数存在、
//!   调用得到 rejected Promise，页面走 `.catch` 分支而不是整段脚本中断。
//! - **不等真实时间**：`setTimeout` / `setInterval` / `requestAnimationFrame` 只登记回调，
//!   脚本求值结束后统一跑几轮，不按真实延迟睡眠。
//! - **伪装环境**：按 [`Profile`] 补上 `navigator` / `screen` / `Intl` / `WebGL` 等字段，
//!   让页面读到自洽的有头浏览器环境。渲染类输出是常量（见 [`crate::browser_env`]）。
//! - **不碰挑战页**：调用方负责判定；本模块只加工已经抓到的 HTML。
//!
//! 脚本接触到的 DOM 是**快照**：读到的是抓取到的原始结构，写入只改内存里的树。
//! 没有布局、样式计算与真实事件循环，因此依赖这些的页面脚本会自然失败并被记录，
//! 而不是产出看起来成功、实际是空的内容。

use std::cell::RefCell;

use boa_engine::builtins::object::OrdinaryObject;
use boa_engine::object::builtins::JsArray;
use boa_engine::object::ObjectInitializer;
use boa_engine::property::Attribute;
use boa_engine::{
    js_string, Context, JsError, JsObject, JsResult, JsString, JsValue, NativeFunction, Source,
};

use crate::browser_env;
use crate::dom::{Dom, NODE_ID};
use crate::fetch_api::{self, HttpSource};
use crate::fingerprint::Profile;

/// 执行脚本的专用线程栈上限：解释器递归比原生栈帧更深，给足 8 MiB；
/// 真溢出了也只影响这个线程，由 [`run_inline_scripts`] 兜成「没拿到内容」。
const SANDBOX_STACK_BYTES: usize = 8 * 1024 * 1024;
/// 单次执行允许的循环迭代总数：防止页面里的 `while(true)` 把抓取线程占死。
pub const LOOP_ITERATION_LIMIT: u64 = 1_000_000;
/// 单个脚本（内联或外链）的字节上限：超出即跳过，避免把畸形的大文件塞进解释器。
///
/// 取 8 MiB 而不是更小的值：现代 SPA 的主 bundle 常在 2–6 MiB（Vite + React + 图表库），
/// 512 KiB 这类阈值会把整站唯一的那份 bundle 直接跳过，页面永远渲染不出来。
pub const MAX_SCRIPT_BYTES: usize = 8 * 1024 * 1024;
/// 参与执行的内联脚本数上限。
pub const MAX_INLINE_SCRIPTS: usize = 32;
/// 执行脚本后允许的节点总数上限（`document.write` 可能不断追加节点）。
pub const MAX_DOM_NODES: usize = 200_000;
/// 定时器回调最多跑几轮：页面脚本可能用定时器自我续命，无上限会把抓取卡死。
///
/// 上限不能太小：React 18 的调度器把「一次渲染」拆成多轮 `setImmediate`，
/// 每轮只处理一个时间片并立刻续排下一轮。8 轮不足，实测这类应用需要上百轮才能提交完成；
/// 这里给到足够大的预算，真正的兜底靠 `LOOP_ITERATION_LIMIT` 与整体超时。
const MAX_TIMER_ROUNDS: usize = 512;
/// 微任务（Promise 回调）最多驱动几轮：`fetch().then(渲染)` 这类链需要它才跑得完。
const MAX_MICROTASK_ROUNDS: usize = 32;


thread_local! {
    /// JS 侧与 Rust 侧共享的唯一状态：快照 DOM 与脚本写出的内容。
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

struct Session {
    dom: Dom,
    /// `document.write` 收到的片段（按调用顺序累积）。
    written: String,
    /// 脚本是否真的操作过 DOM：没动过就说明页面不是 JS 拼装型。
    touched: bool,
    /// 逐个脚本的错误文案（互不影响）。
    errors: Vec<String>,
}

fn with_session<R>(callback: impl FnOnce(&mut Session) -> R) -> Option<R> {
    SESSION.with(|slot| slot.borrow_mut().as_mut().map(callback))
}

/// 单个待执行脚本的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptKind {
    /// 页面里直接写着正文的 `<script>`。
    Inline,
    /// `<script src=...>`：正文需要由调用方取回。
    External,
}

/// 按文档顺序收集到的一个脚本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRef {
    pub kind: ScriptKind,
    /// 内联时是脚本正文；外链时是解析后的绝对地址。
    pub body: String,
    /// `defer` 脚本在文档解析完之后再跑（与浏览器一致），顺序仍按文档序。
    pub defer: bool,
    /// `<script type="module">`：必须按 ES 模块求值，不能当普通脚本 `eval`。
    ///
    /// 现代打包产物（Vite、Rollup）几乎全是模块：末尾带 `export{...}`，
    /// 当脚本执行会直接 `SyntaxError: unexpected token 'export'`，整份 bundle 白跑。
    pub module: bool,
}

/// 外链脚本的取数通道：由调用方注入，沙箱本身不持有任何网络能力。
///
/// 实现方负责访问控制（robots.txt、体积上限、协议白名单等）；
/// 返回 `Err` 时该脚本被跳过并记入 [`RunOutcome::errors`]，不影响其余脚本。
pub trait ScriptSource: Send + Sync {
    fn load(&self, url: &str) -> Result<String, String>;
}

/// 执行结果。
#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    /// 执行后的完整 HTML。
    pub html: String,
    /// `document.write` 累积的片段。
    pub written: String,
    /// 执行后的正文文本（`document.body.textContent`）。
    pub text: String,
    /// 脚本是否操作过 DOM。
    pub touched: bool,
    /// 实际执行的脚本数。
    pub scripts: usize,
    /// 逐个脚本的错误文案（空表示全部正常）。
    pub errors: Vec<String>,
    /// 脚本数或节点数是否触顶被截断。
    pub truncated: bool,
}

/// 执行一段 HTML 里全部内联脚本，返回执行后的页面状态。
///
/// 外链脚本（`src=`）一律不取：那需要再发一次网络请求，超出「只加工已抓到的内容」的范围。
///
/// 脚本在**独立线程**上执行：页面脚本可能触发深度递归（真实站点里的同意管理桩就会），
/// 那个线程的栈溢出只会让本函数退化为「不做重建」，绝不会带崩宿主进程。
pub fn run_inline_scripts(html: &str) -> RunOutcome {
    run_with_profile(html, &Profile::for_family("chrome"))
}

/// 带浏览器画像执行：`profile` 决定 `navigator`/`screen`/Client Hints 的取值，
/// 调用方应传入与传输层档案一致的族，避免 UA 与 TLS 指纹互相矛盾。
///
/// 不加载外链脚本（`src=` 一律跳过）：需要外链请用 [`run_with_scripts`]。
pub fn run_with_profile(html: &str, profile: &Profile) -> RunOutcome {
    run_with_scripts(html, profile, None)
}

/// 完整执行：内联脚本 + 外链脚本，按文档顺序。
///
/// `loader` 负责取外链正文，也负责所有访问控制（robots.txt、体积、协议白名单）。
/// 传 `None` 时对外链退化为「跳过并记录」，与 [`run_with_profile`] 行为一致。
///
/// 脚本内 `fetch`/XHR 一律失败：需要真取数请用 [`run_with_sources`]。
///
/// 顺序语义与浏览器一致：普通脚本按文档序**立即**执行，`defer` 脚本排在最后、
/// 仍按文档序。没有实现 `async` 的乱序语义——沙箱是单线程顺序求值，
/// 乱序会让同一页面在不同运行里产出不同正文。
pub fn run_with_scripts(
    html: &str,
    profile: &Profile,
    loader: Option<&(dyn ScriptSource + '_)>,
) -> RunOutcome {
    run_with_sources(html, profile, loader, None)
}

/// 完整执行，并把脚本内的 `fetch` / `XMLHttpRequest` 接到调用方的 [`HttpSource`]。
///
/// 通道的生命周期由本函数的作用域保证：进入时登记、退出时清空，
/// `fetch` 拿到的 Promise 在脚本跑完后由引擎驱动微任务兑现，页面的 `.then` 渲染照常执行。
///
/// 不传 `http` 时与 [`run_with_scripts`] 完全等价。
pub fn run_with_sources(
    html: &str,
    profile: &Profile,
    loader: Option<&(dyn ScriptSource + '_)>,
    http: Option<&(dyn HttpSource + '_)>,
) -> RunOutcome {
    // 用 `scope` 而不是 `spawn`：加载器是调用方借来的，只有作用域线程能带着非 'static 引用进场。
    std::thread::scope(|scope| {
        let builder = std::thread::Builder::new()
            .name("omnicrawl-jsdom".to_string())
            .stack_size(SANDBOX_STACK_BYTES);
        let spawned =
            builder.spawn_scoped(scope, || run_in_process(html, profile, loader, http));
        match spawned {
            Ok(handle) => match handle.join() {
                Ok(outcome) => outcome,
                // 栈溢出或解释器内部 panic：按「没拿到内容」处理，交由调用方保留静态提取结果。
                Err(failure) => RunOutcome {
                    errors: vec![format!("沙箱执行中断：{}", panic_message(&failure))],
                    ..RunOutcome::default()
                },
            },
            Err(error) => RunOutcome {
                errors: vec![format!("沙箱线程创建失败：{error}")],
                ..RunOutcome::default()
            },
        }
    })
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "未知错误".to_string())
}

fn run_in_process(
    html: &str,
    profile: &Profile,
    loader: Option<&(dyn ScriptSource + '_)>,
    http: Option<&(dyn HttpSource + '_)>,
) -> RunOutcome {
    let collected = collect_scripts(html, profile.href.as_str());
    let truncated = collected.len() > MAX_INLINE_SCRIPTS;
    let collected: Vec<ScriptRef> = collected.into_iter().take(MAX_INLINE_SCRIPTS).collect();

    SESSION.with(|slot| {
        *slot.borrow_mut() = Some(Session {
            dom: Dom::from_html(html, MAX_DOM_NODES),
            written: String::new(),
            touched: false,
            errors: Vec::new(),
        });
    });
    // 节点对象缓存按执行隔离：上一份文档的节点号在这次执行里会指向别的节点。
    clear_element_cache();
    // 通道在这里登记、在本函数末尾清空：中间所有脚本都能用到它。
    fetch_api::install(http, profile.href.as_str());

    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(LOOP_ITERATION_LIMIT);

    let mut executed = 0usize;
    match install_globals(&mut context, profile) {
        Ok(()) => {
            // 先跑普通脚本，再跑 defer：两者各自保持文档顺序。
            for deferred in [false, true] {
                for script in collected.iter().filter(|script| script.defer == deferred) {
                    let body = match &script.kind {
                        ScriptKind::Inline => script.body.clone(),
                        ScriptKind::External => match loader {
                            Some(loader) => match loader.load(&script.body) {
                                Ok(body) if body.len() <= MAX_SCRIPT_BYTES => body,
                                Ok(_) => {
                                    with_session(|session| {
                                        session.errors.push(format!(
                                            "外链脚本超出体积上限，已跳过：{}",
                                            script.body
                                        ))
                                    });
                                    continue;
                                }
                                Err(reason) => {
                                    with_session(|session| {
                                        session.errors.push(format!(
                                            "外链脚本取数失败（{}）：{}",
                                            reason, script.body
                                        ))
                                    });
                                    continue;
                                }
                            },
                            // 没给加载器：与旧行为一致，跳过但不报错。
                            None => continue,
                        },
                    };
                    executed += 1;
                    // 给脚本带上来源路径：boa 的报错与堆栈会因此带文件名，
                    // 否则一份页面里多个脚本只能靠位置猜。
                    let label = match &script.kind {
                        ScriptKind::Inline => format!("inline#{executed}"),
                        ScriptKind::External => script.body.clone(),
                    };
                    let source = Source::from_bytes(&body).with_path(std::path::Path::new(&label));
                    let outcome = if script.module {
                        eval_module(&mut context, source)
                    } else {
                        context.eval(source).map(|_| ())
                    };
                    if let Err(failure) = outcome {
                        with_session(|session| {
                            session
                                .errors
                                .push(format!("{label}：{}", script_error_detail(&failure.to_string())))
                        });
                    }
                    // 每个脚本跑完就把微任务推进一步：SPA 常在脚本里 `fetch(...).then(渲染)`，
                    // 只靠最后统一 flush 会让后面的脚本看到还没渲染的 DOM。
                    drive_microtasks(&mut context);
                }
            }
            // 页面常把主逻辑放在 `setTimeout(fn, 0)` 里（指纹脚本就是）。
            // 这里不睡眠，只把已登记的回调跑几轮，再收正文；
            // 每一轮之后都推一次微任务，因为回调里往往又发了一次 `fetch`。
            for _ in 0..MAX_TIMER_ROUNDS {
                if !browser_env::flush_timers_once(&mut context) {
                    break;
                }
                drive_microtasks(&mut context);
            }
        }
        Err(failure) => {
            let message = one_line(&failure.to_string());
            with_session(|session| session.errors.push(format!("沙箱初始化失败：{message}")));
        }
    }

    // 脚本内请求的失败原因也要浮到工具输出：`fetch` 的 reject 在页面里常被静默吞掉。
    let http_errors = fetch_api::teardown();
    let outcome = with_session(|session| RunOutcome {
        html: session.dom.serialize_document(),
        written: session.written.clone(),
        text: session
            .dom
            .first_element_named("body")
            .map(|body| session.dom.text_content(body))
            .unwrap_or_default(),
        touched: session.touched,
        scripts: executed,
        errors: {
            let mut errors = session.errors.clone();
            errors.extend(http_errors.iter().cloned());
            errors
        },
        truncated: truncated || session.dom.truncated,
    })
    .unwrap_or_default();

    SESSION.with(|slot| *slot.borrow_mut() = None);
    clear_element_cache();
    outcome
}

/// 驱动引擎的微任务队列：`fetch(...).then(...)` 的回调在这里执行。
///
/// 轮数上限存在的理由：页面可能有自我续命的 Promise 链（`then` 里再发请求），
/// 无上限会把抓取卡在解释器里。
fn drive_microtasks(context: &mut Context) {
    for _ in 0..MAX_MICROTASK_ROUNDS {
        if context.run_jobs().is_err() {
            return;
        }
    }
}

/// 按 ES 模块求值：解析 → 链接 → 求值 → 驱动微任务。
///
/// 与浏览器一致的地方：模块有自己的作用域（顶层 `var`/`function` 不再挂到 `window`），
/// `import`/`export` 语法合法。差别在于不做网络导入——`import` 只解析已登记的模块图，
/// 没有加载器的裸说明符会链接失败，这与 `ScriptSource` 只取已声明脚本的取向一致。
fn eval_module<R: boa_engine::parser::source::ReadChar>(
    context: &mut Context,
    source: Source<'_, R>,
) -> JsResult<()> {
    let module = boa_engine::module::Module::parse(source, None, context)?;
    module.link(context)?;
    let promise = module.evaluate(context)?;
    drive_microtasks(context);
    // 模块求值失败表现为 rejected Promise，而不是 `Err`：取回拒因才不丢错误信息。
    if let boa_engine::builtins::promise::PromiseState::Rejected(reason) = promise.state() {
        let mut text = reason
            .to_string(context)
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_else(|_| "模块求值被拒绝".to_string());
        // `stack` 里带着「文件:行:列」，是定位页面脚本失败点的唯一线索。
        if let Some(object) = reason.as_object() {
            if let Ok(stack) = object.get(js_string!("stack"), context) {
                if let Ok(stack) = stack.to_string(context) {
                    let stack = stack.to_std_string_escaped();
                    if !stack.is_empty() && !text.contains(&stack) {
                        text.push_str("
");
                        text.push_str(&stack);
                    }
                }
            }
        }
        return Err(JsError::from_opaque(JsValue::from(JsString::from(text))));
    }
    Ok(())
}


/// 错误里常带多行堆栈：只留首行，避免把解释器噪音灌进工具输出。
fn one_line(message: &str) -> String {
    message
        .lines()
        .next()
        .unwrap_or(message)
        .trim()
        .to_string()
}

/// 诊断用：保留完整错误（含 boa 的堆栈帧），按 `OMNICRAWL_JSDOM_TRACE` 决定启用。
///
/// 平时只留首行，因为错误文案会进工具输出；排查脚本兼容性时这个环境变量能让
/// 「某处不可调用」变成带行号的定位，不必再把脚本切碎逐个试。
fn script_error_detail(message: &str) -> String {
    if std::env::var_os("OMNICRAWL_JSDOM_TRACE").is_some() {
        message.trim().to_string()
    } else {
        one_line(message)
    }
}

/// 脚本是否真的重建出了内容（操作过 DOM 且拿到了非空文本或写出片段）。
pub fn touches_dom(outcome: &RunOutcome) -> bool {
    outcome.touched && (!outcome.text.trim().is_empty() || !outcome.written.trim().is_empty())
}

/// 收集脚本：按文档顺序保留内联与外链，跳过非 JS 数据块与空体。
///
/// `base` 用于把 `src` 解析成绝对地址；解析失败的外链直接丢弃（浏览器同样不会去取）。
fn collect_scripts(html: &str, base: &str) -> Vec<ScriptRef> {
    let Ok(pattern) = regex::Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script\s*>") else {
        return Vec::new();
    };
    let Ok(type_pattern) = regex::Regex::new(r#"(?i)\btype\s*=\s*["']?([^"'\s>]+)"#) else {
        return Vec::new();
    };
    let Ok(src_pattern) = regex::Regex::new(r#"(?i)\bsrc\s*=\s*["']([^"']+)["']"#) else {
        return Vec::new();
    };
    let mut scripts = Vec::new();
    for capture in pattern.captures_iter(html) {
        let attrs = &capture[1];
        let mut module = false;
        if let Some(kind) = type_pattern
            .captures(attrs)
            .map(|capture| capture[1].to_ascii_lowercase())
        {
            // JSON-LD、模板等数据块（`application/ld+json`、`text/template`）不是脚本。
            if kind.contains("module") {
                module = true;
            } else if !kind.contains("javascript") && !kind.contains("ecmascript") {
                continue;
            }
        }
        let defer = attrs.to_ascii_lowercase().contains("defer");
        if let Some(target) = src_pattern.captures(attrs) {
            if let Some(resolved) = resolve_script_url(base, &target[1]) {
                scripts.push(ScriptRef {
                    kind: ScriptKind::External,
                    body: resolved,
                    defer,
                    module,
                });
            }
            continue;
        }
        let body = capture[2].trim();
        if body.is_empty() || body.len() > MAX_SCRIPT_BYTES {
            continue;
        }
        scripts.push(ScriptRef {
            kind: ScriptKind::Inline,
            body: body.to_string(),
            defer,
            module,
        });
    }
    scripts
}

/// 把 `src` 解析成绝对地址：只接受 `http(s)` 与站点相对路径，拒绝其它协议。
///
/// 外链脚本是唯一会被沙箱「再去取一次」的东西，因此协议白名单在这里就收紧：
/// `javascript:`、`data:`、`file:` 一律不认，避免把本地文件或脚本协议当成取数目标。
fn resolve_script_url(base: &str, target: &str) -> Option<String> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    if target.starts_with("//") {
        let scheme = base.split(':').next().unwrap_or("https");
        return Some(format!("{scheme}:{target}"));
    }
    if let Some((scheme, rest)) = target.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let _ = rest;
        return Some(target.to_string());
    }
    // 带冒号但没有 `//` 的（`javascript:` / `data:` / `mailto:`）不是可取的外链。
    if target.contains(':') {
        return None;
    }
    let (scheme, authority) = split_origin(base)?;
    if target.starts_with('/') {
        return Some(format!("{scheme}//{authority}{target}"));
    }
    // 相对路径：从 base 的目录部分接上。
    let path = base
        .split("://")
        .nth(1)
        .and_then(|rest| rest.find('/').map(|index| &rest[index..]))
        .unwrap_or("/");
    let directory = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    };
    Some(format!("{scheme}//{authority}{directory}{target}"))
}

/// 拆出 `scheme:` 与 `authority`（`host[:port]`）。
fn split_origin(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = format!("{scheme}:");
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    Some((scheme, authority))
}

fn install_globals(context: &mut Context, profile: &Profile) -> JsResult<()> {
    let element_proto = build_element_prototype(context);
    context
        .register_global_property(
            js_string!("__ornElementProto"),
            element_proto.clone(),
            Attribute::READONLY,
        )
        .ok();

    let document = build_document(context)?;
    context.register_global_property(js_string!("document"), document, Attribute::all())?;

    // `window` 必须是**全局对象本身**，不是一个另建的空壳。
    //
    // 这是浏览器语义，也是最省事的伪装入路：`window.Date`、`window.fetch`、
    // `window.navigator` 都随全局自动可见。早先的实现另建了一个只挂 `document`
    // 访问器的对象，结果页面脚本里的 `new window.Date()` 直接报「不是构造器」，
    // 一处破绽就让整段探测脚本失效。
    let global = context.global_object().clone();
    context.register_global_property(js_string!("window"), global.clone(), Attribute::all())?;
    context.register_global_property(js_string!("self"), global, Attribute::all())?;

    // 浏览器环境最后装：它会补上 `navigator`/`screen`/`WebGL` 等探测器会读的字段。
    // 这里刻意不让失败冒泡——环境伪装是增强项，装不上也不该让「重建正文」这个主功能失效。
    let _ = browser_env::install(context, profile);
    // 网络 API 排在最后：`browser_env` 会先装一批「拒绝式桩」占位，
    // 真正的 `fetch`/XHR 在这里覆盖它们（未接通道时仍由桩负责拒绝）。
    let _ = fetch_api::install_globals(context);
    Ok(())
}

fn build_document(context: &mut Context) -> JsResult<JsValue> {
    let body_id = with_session(|session| {
        session
            .dom
            .first_element_named("body")
            .unwrap_or(session.dom.root)
    })
    .unwrap_or(0);
    let head_id = with_session(|session| session.dom.first_element_named("head").unwrap_or(0))
        .unwrap_or(0);
    let root_id = with_session(|session| session.dom.root).unwrap_or(0);

    let body = element_value(body_id, context);
    let head = element_value(head_id, context);
    let document_element = element_value(root_id, context);

    // 先把全部原生函数转成对象再进场：构造期独占 `context`，中途不能再借。
    let get_body = NativeFunction::from_fn_ptr(doc_get_body).to_js_function(context.realm());
    let get_head = NativeFunction::from_fn_ptr(doc_get_head).to_js_function(context.realm());

    let document = ObjectInitializer::new(context)
        .function(NativeFunction::from_fn_ptr(doc_write), js_string!("write"), 1)
        // `document` 自身也是节点：`nodeType` 为 9 是 React 与选择器库的常识判断。
        .property(js_string!("nodeType"), JsValue::from(9), Attribute::all())
        .property(js_string!("nodeName"), js_string!("#document"), Attribute::all())
        .property(js_string!("nodeValue"), JsValue::null(), Attribute::all())
        .function(
            NativeFunction::from_fn_ptr(doc_query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_element_by_id),
            js_string!("getElementById"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_elements_by_tag_name),
            js_string!("getElementsByTagName"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_element),
            js_string!("createElement"),
            1,
        )
        // React 对 SVG 走 `createElementNS`：缺它时图标类组件直接抛错，
        // 错误边界之外没有兜底，整棵子树都不渲染。
        .function(
            NativeFunction::from_fn_ptr(doc_create_element_ns),
            js_string!("createElementNS"),
            2,
        )
        // `queryCommandSupported` / `queryCommandEnabled`：Monaco 这类编辑器启动时会探测剪贴板能力，
        // 缺了就是 `not a callable function`，整份 bundle 在顶层中断。
        .function(
            NativeFunction::from_fn_ptr(doc_query_command_supported),
            js_string!("queryCommandSupported"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_command_supported),
            js_string!("queryCommandEnabled"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_command_state),
            js_string!("queryCommandState"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("execCommand"),
            3,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_has_focus),
            js_string!("hasFocus"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_range),
            js_string!("createRange"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_selection),
            js_string!("getSelection"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_comment),
            js_string!("createComment"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_elements_by_class_name),
            js_string!("getElementsByClassName"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_text_node),
            js_string!("createTextNode"),
            1,
        )
        .accessor(js_string!("body"), Some(get_body), None, Attribute::all())
        .accessor(js_string!("head"), Some(get_head), None, Attribute::all())
        .property(
            js_string!("documentElement"),
            document_element,
            Attribute::all(),
        )
        .property(js_string!("__ornBody"), body, Attribute::READONLY)
        .property(js_string!("__ornHead"), head, Attribute::READONLY)
        .build();
    Ok(document.into())
}

fn build_element_prototype(context: &mut Context) -> JsObject {
    // 同一函数要挂到多个名字上时复用同一个对象：先统一转成 JsFunction 再进场。
    let realm = context.realm().clone();
    let text_get = NativeFunction::from_fn_ptr(el_get_text_content).to_js_function(&realm);
    let text_set = NativeFunction::from_fn_ptr(el_set_text_content).to_js_function(&realm);
    let html_get = NativeFunction::from_fn_ptr(el_get_inner_html).to_js_function(&realm);
    let html_set = NativeFunction::from_fn_ptr(el_set_inner_html).to_js_function(&realm);
    let tag_get = NativeFunction::from_fn_ptr(el_get_tag_name).to_js_function(&realm);

    ObjectInitializer::new(context)
        .accessor(
            js_string!("textContent"),
            Some(text_get.clone()),
            Some(text_set.clone()),
            Attribute::all(),
        )
        .accessor(
            js_string!("innerText"),
            Some(text_get),
            Some(text_set),
            Attribute::all(),
        )
        .accessor(
            js_string!("innerHTML"),
            Some(html_get),
            Some(html_set),
            Attribute::all(),
        )
        .accessor(js_string!("tagName"), Some(tag_get.clone()), None, Attribute::all())
        .accessor(js_string!("nodeName"), Some(tag_get), None, Attribute::all())
        // `nodeType` 必须是访问器：文本节点是 3、元素是 1，写死成 1 会让
        // React 把文本节点当元素处理（`createTextInstance` 之后立刻崩）。
        .accessor(
            js_string!("nodeType"),
            Some(NativeFunction::from_fn_ptr(el_node_type).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("nodeValue"),
            Some(NativeFunction::from_fn_ptr(el_node_value).to_js_function(&realm)),
            Some(NativeFunction::from_fn_ptr(el_set_node_value).to_js_function(&realm)),
            Attribute::all(),
        )
        // 树遍历四件套 + `childNodes`：React 的协调器（`getChildNodes`、卸载遍历）
        // 只认这些，只有 `appendChild` 是挂不上一棵树的。
        .accessor(
            js_string!("childNodes"),
            Some(NativeFunction::from_fn_ptr(el_child_nodes).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("children"),
            Some(NativeFunction::from_fn_ptr(el_child_elements).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("firstChild"),
            Some(NativeFunction::from_fn_ptr(el_first_child).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("lastChild"),
            Some(NativeFunction::from_fn_ptr(el_last_child).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("nextSibling"),
            Some(NativeFunction::from_fn_ptr(el_next_sibling).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("previousSibling"),
            Some(NativeFunction::from_fn_ptr(el_previous_sibling).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("parentNode"),
            Some(NativeFunction::from_fn_ptr(el_parent_node).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("parentElement"),
            Some(NativeFunction::from_fn_ptr(el_parent_node).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("firstElementChild"),
            Some(NativeFunction::from_fn_ptr(el_first_child).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        // `ownerDocument` 必须指向真实的 document 对象：React 的事件系统会读
        // `el.ownerDocument` 再往上挂 `selectionchange` 监听，`undefined` 会变成
        // `TypeError: cannot convert 'null' or 'undefined' to object`。
        .accessor(
            js_string!("ownerDocument"),
            Some(NativeFunction::from_fn_ptr(el_owner_document).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(el_get_attribute),
            js_string!("getAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_set_attribute),
            js_string!("setAttribute"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(el_has_attribute),
            js_string!("hasAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove_attribute),
            js_string!("removeAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_append_child),
            js_string!("appendChild"),
            1,
        )
        // React 的提交阶段只用 `insertBefore` 落位，`appendChild` 是它的 `null` 特例。
        .function(
            NativeFunction::from_fn_ptr(el_insert_before),
            js_string!("insertBefore"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove_child),
            js_string!("removeChild"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_replace_child),
            js_string!("replaceChild"),
            2,
        )
        // `cloneNode`：模板/Portal 库把 `<template>` 内容复制进真实树时用它。
        .function(
            NativeFunction::from_fn_ptr(el_clone_node),
            js_string!("cloneNode"),
            1,
        )
        // `replaceChildren`：React 的 effect 里常用它一次性换掉整批子节点。
        .function(
            NativeFunction::from_fn_ptr(el_replace_children),
            js_string!("replaceChildren"),
            1,
        )
        .function(NativeFunction::from_fn_ptr(el_remove), js_string!("remove"), 0)
        // `contains` 是 React-Modal、Portal、tooltip 库卸载时的必经判断：
        // 缺了它错误边界接住异常，整棵子树退回成一句 "not a callable function"。
        .function(
            NativeFunction::from_fn_ptr(el_contains),
            js_string!("contains"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_matches),
            js_string!("matches"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_closest),
            js_string!("closest"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        // 布局相关：没有渲染引擎，给稳定的占位值而不是 undefined。
        // 页面脚本普遍拿 `clientWidth` 做「元素是否可见」判断，undefined 会让整段逻辑抛错。
        .accessor(
            js_string!("clientWidth"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("clientHeight"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("offsetWidth"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("offsetHeight"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        // `getBoundingClientRect` 是布局库（header 偏移、懒加载、动画）的高频入口：
        // 缺了它是 `not a callable function`，整棵子树被错误边界替换。
        .function(
            NativeFunction::from_fn_ptr(el_bounding_rect),
            js_string!("getBoundingClientRect"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(el_zero),
            js_string!("getClientRects"),
            0,
        )
        // `classList` 在真实 DOM 里是 getter 而非方法：注册成方法会让
        // `element.classList.remove(...)` 变成「对函数取属性」，拿到 undefined。
        .accessor(
            js_string!("classList"),
            Some(NativeFunction::from_fn_ptr(el_class_list).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .function(NativeFunction::from_fn_ptr(el_insert_adjacent), js_string!("insertAdjacentHTML"), 2)
        // 返回已 resolve 的 Promise：fullscreen polyfill 会 `!result && new Promise(...)`，
        // 拿到 undefined 时它转而监听 fullscreenchange 事件，那条路在没有事件循环时永远不结束。
        .function(
            NativeFunction::from_fn_ptr(el_resolve_promise),
            js_string!("requestFullscreen"),
            0,
        )
        .function(NativeFunction::from_fn_ptr(el_noop), js_string!("click"), 0)
        .function(NativeFunction::from_fn_ptr(el_noop), js_string!("focus"), 0)
        // 元素也是 EventTarget：页面脚本普遍在元素上挂监听（`e.addEventListener("click", ...)`）。
        // 沙箱没有事件循环，监听器不会因此被触发，但**方法必须存在**，
        // 否则脚本在挂监听这一步就抛错，后面的渲染代码全部不执行。
        .function(
            NativeFunction::from_fn_ptr(el_add_listener),
            js_string!("addEventListener"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove_listener),
            js_string!("removeEventListener"),
            2,
        )
        // `getElementsByTagName` 与 `getElementsByClassName`：老代码与 polyfill 常用，
        // 缺失会让「探测 → 遍历 → 写入」这条链在第一步就断掉。
        .function(
            NativeFunction::from_fn_ptr(el_get_elements_by_tag_name),
            js_string!("getElementsByTagName"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_get_elements_by_class_name),
            js_string!("getElementsByClassName"),
            1,
        )
        .build()
}

/// 元素内的 `getElementsByTagName`：返回数组（真实 DOM 是 HTMLCollection，
/// 但探针只按索引与 `length` 使用，数组足够且索引可靠）。
fn el_get_elements_by_tag_name(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsArray::new(context)?.into());
    };
    let tag = args_string(args, 0, context);
    let found = if tag == "*" {
        with_session(|session| session.dom.query_all(index, "*"))
    } else {
        with_session(|session| session.dom.query_all(index, &tag))
    }
    .unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let _ = array.push(element_value(node, context), context);
    }
    Ok(array.into())
}

/// 元素内的 `getElementsByClassName`。
fn el_get_elements_by_class_name(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsArray::new(context)?.into());
    };
    let class = args_string(args, 0, context);
    let selector = format!(".{class}");
    let found = with_session(|session| session.dom.query_all(index, &selector)).unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let _ = array.push(element_value(node, context), context);
    }
    Ok(array.into())
}

/// 元素上的 `addEventListener`：把监听器存成元素对象的自有属性。
fn el_add_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    let listener = args.get(1).cloned().unwrap_or(JsValue::undefined());
    if !type_name.is_empty() {
        let _ = object.set(
            JsString::from(format!("__ornListener:{type_name}")),
            listener,
            false,
            context,
        );
    }
    Ok(JsValue::undefined())
}

fn el_remove_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    if !type_name.is_empty() {
        let _ = object.set(
            JsString::from(format!("__ornListener:{type_name}")),
            JsValue::undefined(),
            false,
            context,
        );
    }
    Ok(JsValue::undefined())
}

/// 元素的布局尺寸占位：`0` 是「无布局引擎」下最诚实的值，同时不会让脚本抛错。
fn el_zero(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(0))
}

/// 立即兑现的 Promise：给「必须返回 thenable」的 API 用。
fn el_resolve_promise(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(boa_engine::object::builtins::JsPromise::resolve(JsValue::undefined(), context)?.into())
}

/// 元素上的空实现方法（`requestFullscreen` / `click` / `focus`）。
fn el_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `element.classList`：读写在 `class` 属性上，形状与真实 `DOMTokenList` 一致。
fn el_class_list(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let index = node_id(this, context);
    let classes = index
        .and_then(|node| with_session(|session| session.dom.attr(node, "class").map(str::to_string)))
        .flatten()
        .unwrap_or_default();
    let items: Vec<String> = classes
        .split_whitespace()
        .map(|item| item.to_string())
        .collect();
    let list = JsArray::new(context)?;
    for item in &items {
        let _ = list.push(JsString::from(item.as_str()), context);
    }
    Ok(ObjectInitializer::new(context)
        .property(js_string!("length"), JsValue::from(items.len()), Attribute::all())
        .property(js_string!("value"), JsString::from(classes), Attribute::all())
        .property(js_string!("items"), list, Attribute::READONLY)
        .property(
            js_string!("__ornNode"),
            JsValue::from(index.unwrap_or(0)),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(cl_add),
            js_string!("add"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(cl_remove),
            js_string!("remove"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(cl_contains),
            js_string!("contains"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(cl_toggle),
            js_string!("toggle"),
            1,
        )
        .build()
        .into())
}

/// 从 `classList` 对象上取出所属元素的节点号。
fn class_list_node(this: &JsValue, context: &mut Context) -> Option<usize> {
    let object = this.as_object()?;
    object
        .get(js_string!("__ornNode"), context)
        .ok()
        .and_then(|value| value.to_number(context).ok())
        .map(|number| number as usize)
}

fn cl_mutate(this: &JsValue, args: &[JsValue], context: &mut Context, add: bool) -> JsResult<JsValue> {
    let Some(node) = class_list_node(this, context) else {
        return Ok(JsValue::undefined());
    };
    let name = args_string(args, 0, context);
    if name.is_empty() {
        return Ok(JsValue::undefined());
    }
    with_session(|session| {
        session.touched = true;
        let current = session.dom.attr(node, "class").unwrap_or_default().to_string();
        let mut items: Vec<String> = current
            .split_whitespace()
            .map(|item| item.to_string())
            .collect();
        if add {
            if !items.iter().any(|item| item == &name) {
                items.push(name.clone());
            }
        } else {
            items.retain(|item| item != &name);
        }
        session.dom.set_attr(node, "class", &items.join(" "));
    });
    Ok(JsValue::undefined())
}

fn cl_add(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    cl_mutate(this, args, context, true)
}

fn cl_remove(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    cl_mutate(this, args, context, false)
}

fn cl_contains(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = class_list_node(this, context) else {
        return Ok(JsValue::from(false));
    };
    let name = args_string(args, 0, context);
    let present = with_session(|session| {
        session
            .dom
            .attr(node, "class")
            .unwrap_or_default()
            .split_whitespace()
            .any(|item| item == name)
    })
    .unwrap_or(false);
    Ok(JsValue::from(present))
}

fn cl_toggle(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let present = cl_contains(this, args, context)?;
    let now_present = !present.to_boolean();
    cl_mutate(this, args, context, now_present)?;
    Ok(JsValue::from(now_present))
}

/// `element.insertAdjacentHTML(position, html)`：只支持最常见的 `beforeend` / `afterbegin`。
fn el_insert_adjacent(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let html = args_string(args, 1, context);
    with_session(|session| {
        session.touched = true;
        session.dom.replace_children_with(index, &html);
    });
    Ok(JsValue::undefined())
}

/// 造一个元素对象：自身只存节点号，读写全部走原型上的访问器。
/// 同一个节点必须始终映射到**同一个** JS 对象。
///
/// 每次访问都新建对象会让 `root.firstChild.parentNode === root` 恒为 `false`，
/// 而 React 的协调器正是靠引用相等判断「这个节点还是原来那个」：身份不稳定时
/// 它每一步都当成新节点，插入被反复重排，最终什么都挂不上去。
fn element_value(node: usize, context: &mut Context) -> JsValue {
    if let Some(existing) = cached_element(node, context) {
        return existing;
    }
    let proto = context
        .global_object()
        .get(js_string!("__ornElementProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let element = JsObject::from_proto_and_data(proto, OrdinaryObject);
    let _ = element.set(js_string!(NODE_ID), node, false, context);
    // `el.style` 必须是真实对象：React 启动时会写
    // `document.createElement("div").style` 并枚举它的键做特性探测，
    // 给它 undefined 会让整份 bundle 在顶层就抛错，页面永远挂不上去。
    let style = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(style_set_property),
            js_string!("setProperty"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(style_get_property),
            js_string!("getPropertyValue"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(style_remove_property),
            js_string!("removeProperty"),
            1,
        )
        .property(js_string!("length"), JsValue::from(0), Attribute::all())
        .property(js_string!("cssText"), JsString::from(""), Attribute::all())
        .build();
    let _ = element.set(js_string!("style"), style, false, context);
    let _ = element.set(
        js_string!("dataset"),
        ObjectInitializer::new(context).build(),
        false,
        context,
    );
    cache_element(node, element.clone());
    element.into()
}

thread_local! {
    /// 节点号 → 已创建的 JS 对象。用 `JsObject` 句柄而非裸值：
    /// 同一个 `JsObject` 的两次 `into()` 会得到同一个 JS 引用。
    static ELEMENT_CACHE: std::cell::RefCell<std::collections::HashMap<usize, JsObject>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn cached_element(node: usize, _context: &mut Context) -> Option<JsValue> {
    ELEMENT_CACHE.with(|slot| slot.borrow().get(&node).map(|object| JsValue::from(object.clone())))
}

fn cache_element(node: usize, object: JsObject) {
    ELEMENT_CACHE.with(|slot| {
        slot.borrow_mut().insert(node, object);
    });
}

/// 每次执行开始前清空：节点号只在一次执行内有效，跨执行复用会指向别的节点。
pub(crate) fn clear_element_cache() {
    ELEMENT_CACHE.with(|slot| slot.borrow_mut().clear());
}

/// `style.setProperty(name, value)`：记到样式对象自身上，`getPropertyValue` 能读回。
fn style_set_property(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    if let Some(object) = this.as_object() {
        let _ = object.set(
            JsString::from(name.clone()),
            JsString::from(value.clone()),
            false,
            context,
        );
        // CSS 属性名同时按驼峰暴露，`el.style.backgroundColor` 是更常见写法。
        let camel: String = name
            .split('-')
            .enumerate()
            .map(|(index, part)| {
                if index == 0 {
                    part.to_string()
                } else {
                    let mut chars = part.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                        None => String::new(),
                    }
                }
            })
            .collect();
        let _ = object.set(JsString::from(camel), JsString::from(value), false, context);
        let _ = object.set(js_string!("cssText"), JsString::from(""), false, context);
    }
    Ok(JsValue::undefined())
}

fn style_get_property(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = this
        .as_object()
        .and_then(|object| object.get(JsString::from(name), context).ok())
        .unwrap_or_else(|| JsString::from("").into());
    Ok(value)
}

fn style_remove_property(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    if let Some(object) = this.as_object() {
        let _ = object.delete_property_or_throw(JsString::from(name), context);
    }
    Ok(JsValue::from(js_string!("")))
}

fn node_id(this: &JsValue, context: &mut Context) -> Option<usize> {
    let object = this.as_object()?;
    let value = object.get(js_string!(NODE_ID), context).ok()?;
    value.to_number(context).ok().map(|number| number as usize)
}

/// 参数里的节点号：`appendChild` / `insertBefore` / `removeChild` 共用。
fn arg_node_id(args: &[JsValue], index: usize, context: &mut Context) -> Option<usize> {
    args.get(index)
        .and_then(|value| value.as_object())
        .and_then(|object| object.get(js_string!(NODE_ID), context).ok())
        .and_then(|value| value.to_number(context).ok())
        .map(|number| number as usize)
}

/// 造一个「类数组」的节点集合：真实 DOM 是 `NodeList`/`HTMLCollection`，
/// 脚本侧只用 `length` 与下标，数组足够且索引可靠。
fn node_list_value(nodes: &[usize], context: &mut Context) -> JsValue {
    let array = JsArray::new(context);
    let Ok(array) = array else {
        return JsValue::null();
    };
    for node in nodes {
        let _ = array.push(element_value(*node, context), context);
    }
    array.into()
}

fn el_node_type(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::from(0));
    };
    let kind = with_session(|session| session.dom.node_type(index)).unwrap_or(0);
    Ok(JsValue::from(kind as i32))
}

fn el_node_value(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let text = with_session(|session| session.dom.text_of(index).map(str::to_string)).flatten();
    Ok(match text {
        Some(text) => JsString::from(text).into(),
        None => JsValue::null(),
    })
}

fn el_set_node_value(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let text = args_string(args, 0, context);
    with_session(|session| {
        session.touched = true;
        session.dom.set_text(index, &text);
    });
    Ok(JsValue::undefined())
}

fn el_child_nodes(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(node_list_value(&[], context));
    };
    let children = with_session(|session| session.dom.children_of(index).to_vec()).unwrap_or_default();
    Ok(node_list_value(&children, context))
}

fn el_child_elements(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(node_list_value(&[], context));
    };
    let elements = with_session(|session| {
        session
            .dom
            .children_of(index)
            .iter()
            .copied()
            .filter(|child| session.dom.node_type(*child) == 1)
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    Ok(node_list_value(&elements, context))
}

fn el_first_child(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let found = with_session(|session| session.dom.children_of(index).first().copied()).flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn el_last_child(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let found = with_session(|session| session.dom.children_of(index).last().copied()).flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn el_next_sibling(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let found = with_session(|session| {
        let parent = session.dom.parent_of(index)?;
        let siblings = session.dom.children_of(parent);
        let position = siblings.iter().position(|sibling| *sibling == index)?;
        siblings.get(position + 1).copied()
    })
    .flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn el_previous_sibling(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let found = with_session(|session| {
        let parent = session.dom.parent_of(index)?;
        let siblings = session.dom.children_of(parent);
        let position = siblings.iter().position(|sibling| *sibling == index)?;
        position.checked_sub(1).and_then(|before| siblings.get(before).copied())
    })
    .flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn el_parent_node(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let found = with_session(|session| session.dom.parent_of(index)).flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn el_insert_before(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let Some(child) = arg_node_id(args, 0, context) else {
        return Ok(JsValue::undefined());
    };
    // 第二个参数为 `null` 时与 `appendChild` 等价，这是 React 的常见写法。
    let reference = arg_node_id(args, 1, context);
    with_session(|session| {
        session.touched = true;
        session.dom.insert_before(parent, child, reference);
    });
    Ok(args.first().cloned().unwrap_or(JsValue::undefined()))
}

fn el_remove_child(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let Some(child) = arg_node_id(args, 0, context) else {
        return Ok(JsValue::undefined());
    };
    with_session(|session| {
        session.touched = true;
        session.dom.remove_child(parent, child);
    });
    Ok(args.first().cloned().unwrap_or(JsValue::undefined()))
}

fn el_replace_child(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(parent) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let Some(replacement) = arg_node_id(args, 0, context) else {
        return Ok(JsValue::undefined());
    };
    let Some(replaced) = arg_node_id(args, 1, context) else {
        return Ok(JsValue::undefined());
    };
    with_session(|session| {
        session.touched = true;
        let siblings = session.dom.children_of(parent).to_vec();
        let position = siblings.iter().position(|sibling| *sibling == replaced);
        match position.and_then(|position| siblings.get(position + 1).copied()) {
            Some(next) => session.dom.insert_before(parent, replacement, Some(next)),
            None => session.dom.insert_before(parent, replacement, None),
        }
        session.dom.remove_child(parent, replaced);
    });
    Ok(args.get(1).cloned().unwrap_or(JsValue::undefined()))
}

/// `element.contains(other)`：`other` 是本元素自身或其后代时为 `true`。
fn el_contains(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::from(false));
    };
    let Some(other) = arg_node_id(args, 0, context) else {
        return Ok(JsValue::from(false));
    };
    let contained = with_session(|session| {
        if index == other {
            return true;
        }
        let mut current = session.dom.parent_of(other);
        while let Some(node) = current {
            if node == index {
                return true;
            }
            current = session.dom.parent_of(node);
        }
        false
    })
    .unwrap_or(false);
    Ok(JsValue::from(contained))
}

/// `element.matches(selector)`：按沙箱支持的子集做选择器匹配。
fn el_matches(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::from(false));
    };
    let selector = args_string(args, 0, context);
    let matched = with_session(|session| {
        session
            .dom
            .query_all(session.dom.root, &selector)
            .contains(&index)
    })
    .unwrap_or(false);
    Ok(JsValue::from(matched))
}

/// `element.closest(selector)`：沿祖先链找第一个匹配的元素。
fn el_closest(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let selector = args_string(args, 0, context);
    let found = with_session(|session| {
        let mut current = Some(index);
        while let Some(node) = current {
            if session.dom.query_all(session.dom.root, &selector).contains(&node) {
                return Some(node);
            }
            current = session.dom.parent_of(node);
        }
        None
    })
    .flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

/// `element.getBoundingClientRect()`：没有布局引擎，返回全零矩形。
///
/// 页面普遍读 `.height` / `.top` 做偏移计算，`undefined` 会在 `Math.ceil` 上抛错。
fn el_bounding_rect(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("x"), JsValue::from(0), Attribute::all())
        .property(js_string!("y"), JsValue::from(0), Attribute::all())
        .property(js_string!("top"), JsValue::from(0), Attribute::all())
        .property(js_string!("left"), JsValue::from(0), Attribute::all())
        .property(js_string!("right"), JsValue::from(0), Attribute::all())
        .property(js_string!("bottom"), JsValue::from(0), Attribute::all())
        .property(js_string!("width"), JsValue::from(0), Attribute::all())
        .property(js_string!("height"), JsValue::from(0), Attribute::all())
        .build()
        .into())
}

/// `element.cloneNode(deep)`：深拷贝子树，返回游离副本。
fn el_clone_node(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let deep = args
        .first()
        .map(|value| value.to_boolean())
        .unwrap_or(false);
    let cloned = with_session(|session| session.dom.clone_tree(index, deep)).flatten();
    Ok(match cloned {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

/// `element.replaceChildren(...nodes)`：一次性换掉全部子节点。
fn el_replace_children(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let replacements: Vec<usize> = (0..args.len())
        .filter_map(|position| arg_node_id(args, position, context))
        .collect();
    with_session(|session| {
        session.touched = true;
        session.dom.replace_children(index, &replacements);
    });
    Ok(JsValue::undefined())
}

fn args_string(args: &[JsValue], index: usize, context: &mut Context) -> String {
    args.get(index)
        .cloned()
        .unwrap_or(JsValue::undefined())
        .to_string(context)
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default()
}

fn el_get_text_content(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let text = with_session(|session| {
        session.touched = true;
        session.dom.text_content(index)
    })
    .unwrap_or_default();
    Ok(JsValue::from(js_string!(text)))
}

fn el_set_text_content(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let text = args_string(args, 0, context);
    with_session(|session| {
        session.touched = true;
        session.dom.set_text(index, &text);
    });
    Ok(JsValue::undefined())
}

fn el_get_inner_html(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let html = with_session(|session| session.dom.inner_html(index)).unwrap_or_default();
    Ok(JsValue::from(js_string!(html)))
}

fn el_set_inner_html(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let html = args_string(args, 0, context);
    with_session(|session| {
        session.touched = true;
        session.dom.replace_children_with(index, &html);
    });
    Ok(JsValue::undefined())
}

fn el_get_tag_name(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let tag = with_session(|session| {
        session
            .dom
            .tag(index)
            .map(|name| name.to_ascii_uppercase())
            .unwrap_or_default()
    })
    .unwrap_or_default();
    Ok(JsValue::from(js_string!(tag)))
}

fn el_get_attribute(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let name = args_string(args, 0, context);
    let value = with_session(|session| session.dom.attr(index, &name).map(str::to_string)).flatten();
    Ok(match value {
        Some(text) => JsValue::from(js_string!(text)),
        None => JsValue::null(),
    })
}

fn el_set_attribute(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    with_session(|session| {
        session.touched = true;
        session.dom.set_attr(index, &name, &value);
    });
    Ok(JsValue::undefined())
}

fn el_has_attribute(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::from(false));
    };
    let name = args_string(args, 0, context);
    let present = with_session(|session| session.dom.attr(index, &name).is_some()).unwrap_or(false);
    Ok(JsValue::from(present))
}

fn el_remove_attribute(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let name = args_string(args, 0, context);
    with_session(|session| {
        session.touched = true;
        session.dom.remove_attr(index, &name);
    });
    Ok(JsValue::undefined())
}

fn el_append_child(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let child = args
        .first()
        .cloned()
        .unwrap_or(JsValue::undefined())
        .as_object()
        .and_then(|object| object.get(js_string!(NODE_ID), context).ok())
        .and_then(|value| value.to_number(context).ok())
        .map(|number| number as usize);
    if let Some(child) = child {
        with_session(|session| {
            session.touched = true;
            session.dom.append(index, child);
        });
    }
    Ok(args.first().cloned().unwrap_or(JsValue::undefined()))
}

fn el_remove(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    with_session(|session| {
        session.touched = true;
        session.dom.detach(index);
    });
    Ok(JsValue::undefined())
}

fn el_query_selector(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let selector = args_string(args, 0, context);
    let found = with_session(|session| session.dom.query_first(index, &selector)).flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn el_query_selector_all(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsArray::new(context)?.into());
    };
    let selector = args_string(args, 0, context);
    let found = with_session(|session| session.dom.query_all(index, &selector)).unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let value = element_value(node, context);
        array.push(value, context)?;
    }
    Ok(array.into())
}

fn doc_get_body(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(this
        .as_object()
        .and_then(|object| object.get(js_string!("__ornBody"), context).ok())
        .unwrap_or(JsValue::null()))
}

fn doc_get_head(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(this
        .as_object()
        .and_then(|object| object.get(js_string!("__ornHead"), context).ok())
        .unwrap_or(JsValue::null()))
}

fn doc_write(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let html = args_string(args, 0, context);
    with_session(|session| {
        session.touched = true;
        session.written.push_str(&html);
        // 与浏览器不同，这里一律追加到 `body` 末尾：没有解析阶段的插入点概念，
        // 而正文提取只关心「脚本写出了什么」。
        if let Some(body) = session.dom.first_element_named("body") {
            session.dom.insert_fragment(body, &html);
        }
    });
    Ok(JsValue::undefined())
}

fn doc_query_selector(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let selector = args_string(args, 0, context);
    let root = with_session(|session| session.dom.root).unwrap_or(0);
    let found = with_session(|session| session.dom.query_first(root, &selector)).flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn doc_query_selector_all(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let selector = args_string(args, 0, context);
    let root = with_session(|session| session.dom.root).unwrap_or(0);
    let found = with_session(|session| session.dom.query_all(root, &selector)).unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let value = element_value(node, context);
        array.push(value, context)?;
    }
    Ok(array.into())
}

fn doc_get_element_by_id(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let id = args_string(args, 0, context);
    let found = with_session(|session| {
        let dom = &session.dom;
        dom.elements()
            .into_iter()
            .find(|node| dom.attr(*node, "id") == Some(id.as_str()))
    })
    .flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn doc_get_elements_by_tag_name(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let tag = args_string(args, 0, context).to_ascii_lowercase();
    let found = with_session(|session| {
        let dom = &session.dom;
        dom.elements()
            .into_iter()
            .filter(|node| {
                dom.tag(*node)
                    .is_some_and(|name| tag.is_empty() || name == tag)
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let value = element_value(node, context);
        array.push(value, context)?;
    }
    Ok(array.into())
}

fn doc_query_command_supported(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    // 剪贴板类命令一律「不支持」：沙箱里没有真实剪贴板，声称支持会引出后续真实调用。
    Ok(JsValue::from(false))
}

fn doc_query_command_state(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(false))
}

fn doc_has_focus(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(true))
}

fn noop_returning_undefined(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `document.createRange()`：返回带常用成员的选区对象。
fn doc_create_range(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("collapsed"), JsValue::from(true), Attribute::all())
        .property(js_string!("startOffset"), JsValue::from(0), Attribute::all())
        .property(js_string!("endOffset"), JsValue::from(0), Attribute::all())
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("setStart"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("setEnd"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("selectNodeContents"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("getBoundingClientRect"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(range_get_client_rects),
            js_string!("getClientRects"),
            0,
        )
        .build()
        .into())
}

fn range_get_client_rects(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
}

fn doc_get_selection(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // 复用全局 `getSelection`：两处语义一致，都返回「未选中」。
    context
        .global_object()
        .get(js_string!("getSelection"), context)
        .map(|value| match value.as_object().filter(JsObject::is_callable) {
            Some(function) => function
                .call(&JsValue::undefined(), &[], context)
                .unwrap_or(JsValue::null()),
            None => JsValue::null(),
        })
}

fn doc_create_comment(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    doc_create_text_node(_this, _args, context)
}

fn doc_get_elements_by_class_name(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
}

fn doc_create_element(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    create_element_node(args, context)
}

/// `document.createElementNS(ns, tag)`：React 用全小写名 + `xlink:href` 处理 SVG，
/// 沙箱不分命名空间，按普通元素建即可。
fn doc_create_element_ns(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let tag = args_string(args, 1, context);
    let created = with_session(|session| {
        session.touched = true;
        session.dom.create_detached(Some(&tag), "")
    })
    .flatten();
    Ok(match created {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn create_element_node(args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let tag = args_string(args, 0, context);
    let created = with_session(|session| {
        session.touched = true;
        let root = session.dom.root;
        let node = session.dom.push_element(root, &tag)?;
        // 新节点默认游离：脚本要显式 `appendChild` 才进树。
        session.dom.detach(node);
        Some(node)
    })
    .flatten();
    Ok(match created {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

fn doc_create_text_node(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let text = args_string(args, 0, context);
    let created = with_session(|session| {
        session.touched = true;
        let root = session.dom.root;
        let node = session.dom.push_text(root, &text)?;
        session.dom.detach(node);
        Some(node)
    })
    .flatten();
    Ok(match created {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

/// `element.ownerDocument`：返回全局 document。
///
/// 元素实例本身没有指向 document 的句柄，全局 document 是唯一正确的答案
/// （沙箱只有一份文档），也避免了把 `undefined` 交给 React 的事件系统。
fn el_owner_document(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(context
        .global_object()
        .get(js_string!("document"), context)
        .unwrap_or(JsValue::undefined()))
}
