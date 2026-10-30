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
use std::rc::Rc;

use boa_engine::builtins::object::OrdinaryObject;
use boa_engine::module::IdleModuleLoader;
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

mod canvas;
mod document;
mod document_ops;
mod element_ops;
mod element_proto;
mod sandbox;
mod scripts;

pub(super) use canvas::{
    canvas_element_dimension, el_canvas_get_height, el_canvas_get_width, el_canvas_set_height,
    el_canvas_set_width, el_get_context, el_to_data_url, set_canvas_dimension,
};
use document::build_document;
pub(super) use document_ops::{
    create_element_node, doc_create_comment, doc_create_element, doc_create_element_ns,
    doc_create_range, doc_create_text_node, doc_get_body, doc_get_element_by_id,
    doc_get_elements_by_class_name, doc_get_elements_by_tag_name, doc_get_head, doc_get_selection,
    doc_has_focus, doc_query_command_state, doc_query_command_supported, doc_query_selector,
    doc_query_selector_all, doc_write, el_owner_document, noop_returning_undefined,
    range_get_client_rects,
};
pub(super) use element_ops::{
    cl_add, cl_contains, cl_mutate, cl_remove, cl_toggle, class_list_node, el_add_listener,
    el_class_list, el_get_elements_by_class_name, el_get_elements_by_tag_name, el_insert_adjacent,
    el_noop, el_remove_listener, el_resolve_promise, el_zero,
};
use element_proto::build_element_prototype;
pub(super) use sandbox::run_in_process;
pub(super) use scripts::{collect_scripts, resolve_script_url, split_origin};

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
        let spawned = builder.spawn_scoped(scope, || run_in_process(html, profile, loader, http));
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

/// 收尾：拆掉 fetch 通道、按当前会话状态汇总结果、清空会话与节点缓存。
fn finish(executed: usize, truncated: bool) -> RunOutcome {
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

/// 按 ES 模块求值：解析 → 加载 → 链接 → 求值 → 驱动微任务。
///
/// 与浏览器一致的地方：模块有自己的作用域（顶层 `var`/`function` 不再挂到 `window`），
/// `import`/`export` 语法合法。差别在于不做网络导入——`import` 只解析已登记的模块图，
/// 没有加载器的裸说明符会加载失败并记为脚本错误，这与 `ScriptSource` 只取已声明脚本的取向一致。
fn eval_module<R: boa_engine::parser::source::ReadChar>(
    context: &mut Context,
    source: Source<'_, R>,
) -> JsResult<()> {
    let module = boa_engine::module::Module::parse(source, None, context)?;
    // 必须先加载模块图再链接：`link` 会按 `requested_modules` 直接索引已加载表，
    // 跳过这一步时任何 `import`/`export ... from` 都会命中空表并让 boa 在 link 内 panic
    // （release 配置是 `panic = "abort"`，线程 join 兜不住，整个抓取进程会被打断）。
    let loaded = module.load(context);
    drive_microtasks(context);
    match loaded.state() {
        boa_engine::builtins::promise::PromiseState::Fulfilled(_) => {}
        boa_engine::builtins::promise::PromiseState::Rejected(reason) => {
            return Err(module_rejection(reason, context));
        }
        // 加载没跑完（作业自身出错导致队列被清空）：`link` 此时仍会索引空表，
        // 因此宁可报脚本错误，也不把未完成的模块图交给链接阶段。
        boa_engine::builtins::promise::PromiseState::Pending => {
            return Err(JsError::from_opaque(JsValue::from(JsString::from(
                "模块依赖加载未完成",
            ))));
        }
    }
    module.link(context)?;
    let promise = module.evaluate(context)?;
    drive_microtasks(context);
    // 模块求值失败表现为 rejected Promise，而不是 `Err`：取回拒因才不丢错误信息。
    if let boa_engine::builtins::promise::PromiseState::Rejected(reason) = promise.state() {
        return Err(module_rejection(reason, context));
    }
    Ok(())
}

/// 把模块的拒因转成脚本错误：文案之外尽量带上 `stack`（带「文件:行:列」）。
fn module_rejection(reason: JsValue, context: &mut Context) -> JsError {
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
                    text.push_str(
                        "
",
                    );
                    text.push_str(&stack);
                }
            }
        }
    }
    JsError::from_opaque(JsValue::from(JsString::from(text)))
}

/// 错误里常带多行堆栈：只留首行，避免把解释器噪音灌进工具输出。
fn one_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_string()
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
    ELEMENT_CACHE.with(|slot| {
        slot.borrow()
            .get(&node)
            .map(|object| JsValue::from(object.clone()))
    })
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
fn style_set_property(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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

fn style_get_property(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = this
        .as_object()
        .and_then(|object| object.get(JsString::from(name), context).ok())
        .unwrap_or_else(|| JsString::from("").into());
    Ok(value)
}

fn style_remove_property(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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
    let children =
        with_session(|session| session.dom.children_of(index).to_vec()).unwrap_or_default();
    Ok(node_list_value(&children, context))
}

fn el_child_elements(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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

fn el_previous_sibling(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let found = with_session(|session| {
        let parent = session.dom.parent_of(index)?;
        let siblings = session.dom.children_of(parent);
        let position = siblings.iter().position(|sibling| *sibling == index)?;
        position
            .checked_sub(1)
            .and_then(|before| siblings.get(before).copied())
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
            if session
                .dom
                .query_all(session.dom.root, &selector)
                .contains(&node)
            {
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
fn el_bounding_rect(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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
fn el_replace_children(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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
    let value =
        with_session(|session| session.dom.attr(index, &name).map(str::to_string)).flatten();
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

