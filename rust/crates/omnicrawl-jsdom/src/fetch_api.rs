//! 脚本内发起的网络请求：`fetch` 与 `XMLHttpRequest` 在沙箱里的实现。
//!
//! 沙箱本身仍然不持有网络能力。本模块只做翻译：把 JS 侧调用变成一次 [`HttpRequest`]，
//! 交给调用方注入的 [`HttpSource`] 执行，再把结果按浏览器的形状（`Response` / XHR 实例）
//! 交回脚本。访问控制（协议白名单、robots.txt、跨站限制、次数与体积上限）全在实现方。
//!
//! 为什么必须有这一层：SPA 外壳（`<div id="root">` + bundle）的正文完全由脚本拉取接口后
//! 渲染。没有 `fetch` 时脚本第一步就拿到 rejected Promise，DOM 永远是空的。
//!
//! 与真实浏览器一致的部分：`fetch` 返回 Promise，`.then` 回调在引擎驱动微任务时执行；
//! `Response.status/ok/headers/json()/text()` 字段齐全；XHR 有 `readyState` 常量与
//! `onload` / `onreadystatechange` 派发。
//!
//! 刻意不做的部分：不做 CORS 预检、不做缓存与 `AbortController` 取消、不按真实延迟异步派发
//! （XHR 在 `send()` 返回前同步派发，否则页面在沙箱里永远等不到回调）。

use std::cell::RefCell;

use boa_engine::builtins::object::OrdinaryObject;
use boa_engine::object::builtins::{JsArray, JsPromise};
use boa_engine::object::ObjectInitializer;
use boa_engine::property::{Attribute, PropertyKey};
use boa_engine::{
    js_string, Context, JsError, JsObject, JsResult, JsString, JsValue, NativeFunction,
};

/// 单次响应体的字节上限：超过就按失败处理，避免把巨型接口塞进解释器。
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// 单次执行允许的脚本内请求数。
pub const MAX_REQUESTS: usize = 24;
/// 单次执行允许接收的总字节数。
pub const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// 脚本侧发起的一次请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// 绝对地址：相对地址已由沙箱按页面地址补全。
    pub url: String,
    pub method: String,
    /// 脚本显式设置的请求头，按脚本给出的顺序原样传递。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 调用方给出的响应：只保留脚本能读到的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    /// 响应头按出现顺序保留（同名头允许重复）。
    pub headers: Vec<(String, String)>,
    /// 已解码的响应体。
    pub body: Vec<u8>,
    /// 是否发生过重定向：填 `Response.redirected`。
    pub redirected: bool,
    /// 最终地址（跟过重定向后与原请求不同）：填 `Response.url` 与 `responseURL`。
    pub url: String,
}

impl HttpResponse {
    /// 按名字取第一个头（大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// 脚本发起请求时的取数通道：由调用方注入，沙箱本身不持有任何网络能力。
///
/// 实现方负责全部访问控制。返回 `Err` 时该请求被当成网络错误
/// （`fetch` reject、XHR 触发 `onerror`），不影响页面上其余脚本。
pub trait HttpSource: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String>;
}

thread_local! {
    /// 本次执行的取数通道与预算，用裸指针做生命周期擦除。
    ///
    /// `fetch` 是原生函数，只能靠 thread_local 拿到通道；生命周期由
    /// [`install`]/[`teardown`] 在同一个函数体内兜住。沙箱每次跑在独立线程上，
    /// 因此不存在跨线程共享。
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    source: *const (dyn HttpSource + 'static),
    /// 页面地址：相对 URL 以它为基准补全。
    base: String,
    requests: usize,
    bytes: usize,
    /// 请求失败的原因：进 `RunOutcome::errors` 供排障。
    errors: Vec<String>,
}

/// 登记取数通道；`None` 表示脚本内请求一律失败（与没有网络能力的旧行为一致）。
pub(crate) fn install(source: Option<&(dyn HttpSource + '_)>, base: &str) {
    let source = source.map(|source| {
        let raw: *const (dyn HttpSource + '_) = source;
        // SAFETY: 只做生命周期擦除，实际生命周期由 `run_with_sources` 的作用域保证。
        unsafe {
            std::mem::transmute::<*const (dyn HttpSource + '_), *const (dyn HttpSource + 'static)>(
                raw,
            )
        }
    });
    STATE.with(|slot| {
        *slot.borrow_mut() = source.map(|source| State {
            source,
            base: base.to_string(),
            requests: 0,
            bytes: 0,
            errors: Vec::new(),
        });
    });
}

/// 清空登记状态，返回脚本内请求产生的错误清单。
pub(crate) fn teardown() -> Vec<String> {
    STATE
        .with(|slot| slot.borrow_mut().take())
        .map(|state| state.errors)
        .unwrap_or_default()
}

fn record_error(message: String) {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.errors.push(message);
        }
    });
}

/// 执行一次请求：做预算检查、补全地址，再交给通道。
fn perform(request: HttpRequest) -> Result<HttpResponse, String> {
    let (source, base) = STATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|state| (state.source, state.base.clone()))
            .ok_or_else(|| "沙箱没有接入取数通道".to_string())
    })?;
    let mut request = request;
    request.url = absolutize(&base, &request.url);
    if !(request.url.starts_with("http://") || request.url.starts_with("https://")) {
        return Err(format!("非 http(s) 协议：{}", request.url));
    }
    let over_requests = STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return true;
        };
        state.requests += 1;
        state.requests > MAX_REQUESTS
    });
    if over_requests {
        return Err(format!("脚本内请求数超过上限（{MAX_REQUESTS}）"));
    }
    // SAFETY: 通道的存活期由 `install`/`teardown` 包住，其间不会失效。
    let response = unsafe { &*source }.send(&request)?;
    if response.body.len() > MAX_RESPONSE_BYTES {
        return Err(format!("响应体超过 {MAX_RESPONSE_BYTES} 字节"));
    }
    let over_bytes = STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return true;
        };
        state.bytes += response.body.len();
        state.bytes > MAX_TOTAL_BYTES
    });
    if over_bytes {
        return Err("脚本内请求接收的总字节数超出上限".to_string());
    }
    Ok(response)
}

/// 把脚本给的地址补成绝对地址，规则与 `new URL(target, base)` 够用部分一致。
fn absolutize(base: &str, target: &str) -> String {
    let target = target.trim();
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    if target.starts_with("//") {
        let scheme = base.split(':').next().unwrap_or("https");
        return format!("{scheme}:{target}");
    }
    let Some((scheme, rest)) = base.split_once("://") else {
        return target.to_string();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if target.starts_with('/') {
        return format!("{scheme}://{authority}{target}");
    }
    let path = rest
        .find('/')
        .map(|index| &rest[index..])
        .unwrap_or("/")
        .split(['?', '#'])
        .next()
        .unwrap_or("/");
    let directory = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    };
    format!("{scheme}://{authority}{directory}{target}")
}

/// 装上 `fetch` / `Headers` / `Response` / `XMLHttpRequest`。
///
/// 必须在 `browser_env::install_network_stubs` 之后调用。没登记取数通道时直接返回，
/// 让那批「拒绝式桩」留在原位——页面脚本照旧拿到 rejected Promise，行为与旧版本一致。
pub(crate) fn install_globals(context: &mut Context) -> JsResult<()> {
    if !connected() {
        return Ok(());
    }
    install_headers(context)?;
    install_response(context)?;
    install_xhr(context)?;
    let function = NativeFunction::from_fn_ptr(js_fetch).to_js_function(context.realm());
    context.register_global_property(js_string!("fetch"), function, Attribute::all())?;
    Ok(())
}

fn connected() -> bool {
    STATE.with(|slot| slot.borrow().is_some())
}

fn install_headers(context: &mut Context) -> JsResult<()> {
    let prototype = ObjectInitializer::new(context)
        .function(native(headers_get), js_string!("get"), 1)
        .function(native(headers_has), js_string!("has"), 1)
        .function(native(headers_set), js_string!("set"), 2)
        .function(native(headers_append), js_string!("append"), 2)
        .function(native(headers_delete), js_string!("delete"), 1)
        .function(native(headers_for_each), js_string!("forEach"), 1)
        .function(native(headers_keys), js_string!("keys"), 0)
        .function(native(headers_values), js_string!("values"), 0)
        .function(native(headers_entries), js_string!("entries"), 0)
        .build();
    let constructor =
        boa_engine::object::FunctionObjectBuilder::new(context.realm(), native(headers_construct))
            .name(js_string!("Headers"))
            .constructor(true)
            .build();
    let _ = constructor.set(js_string!("prototype"), prototype.clone(), false, context);
    let _ = prototype.set(js_string!("constructor"), constructor.clone(), false, context);
    context.register_global_property(js_string!("Headers"), constructor, Attribute::all())?;
    context.register_global_property(
        js_string!("__ornHeadersProto"),
        prototype,
        Attribute::READONLY,
    )?;
    Ok(())
}

fn install_response(context: &mut Context) -> JsResult<()> {
    let prototype = ObjectInitializer::new(context)
        .function(native(response_text), js_string!("text"), 0)
        .function(native(response_json), js_string!("json"), 0)
        .function(native(response_clone), js_string!("clone"), 0)
        .build();
    let constructor =
        boa_engine::object::FunctionObjectBuilder::new(context.realm(), native(response_construct))
            .name(js_string!("Response"))
            .constructor(true)
            .build();
    let _ = constructor.set(js_string!("prototype"), prototype.clone(), false, context);
    let _ = prototype.set(js_string!("constructor"), constructor.clone(), false, context);
    context.register_global_property(js_string!("Response"), constructor, Attribute::all())?;
    context.register_global_property(
        js_string!("__ornResponseProto"),
        prototype,
        Attribute::READONLY,
    )?;
    Ok(())
}

fn install_xhr(context: &mut Context) -> JsResult<()> {
    let prototype = ObjectInitializer::new(context)
        .function(native(xhr_open), js_string!("open"), 3)
        .function(
            native(xhr_set_request_header),
            js_string!("setRequestHeader"),
            2,
        )
        .function(native(xhr_send), js_string!("send"), 1)
        .function(native(xhr_abort), js_string!("abort"), 0)
        .function(native(noop), js_string!("overrideMimeType"), 1)
        .function(
            native(xhr_get_all_response_headers),
            js_string!("getAllResponseHeaders"),
            0,
        )
        .function(
            native(xhr_get_response_header),
            js_string!("getResponseHeader"),
            1,
        )
        .function(native(xhr_add_listener), js_string!("addEventListener"), 2)
        .function(native(xhr_remove_listener), js_string!("removeEventListener"), 2)
        .function(native(xhr_dispatch), js_string!("dispatchEvent"), 1)
        .build();
    let ready_states = [
        ("UNSENT", 0),
        ("OPENED", 1),
        ("HEADERS_RECEIVED", 2),
        ("LOADING", 3),
        ("DONE", 4),
    ];
    for (name, value) in ready_states {
        let _ = prototype.set(js_string!(name), JsValue::from(value), false, context);
    }
    let constructor =
        boa_engine::object::FunctionObjectBuilder::new(context.realm(), native(xhr_construct))
            .name(js_string!("XMLHttpRequest"))
            .constructor(true)
            .build();
    let _ = constructor.set(js_string!("prototype"), prototype.clone(), false, context);
    let _ = prototype.set(js_string!("constructor"), constructor.clone(), false, context);
    // 常量同时挂在构造器上：`xhr.readyState === XMLHttpRequest.DONE` 两种写法都要能读到。
    for (name, value) in ready_states {
        let _ = constructor.set(js_string!(name), JsValue::from(value), false, context);
    }
    context.register_global_property(js_string!("XMLHttpRequest"), constructor, Attribute::all())?;
    context.register_global_property(js_string!("__ornXhrProto"), prototype, Attribute::READONLY)?;
    Ok(())
}

// ---------------------------------------------------------------- fetch

fn js_fetch(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let request = match parse_fetch(args, context) {
        Ok(request) => request,
        Err(reason) => {
            record_error(format!("fetch 参数无效：{reason}"));
            let payload = JsValue::from(JsString::from(format!("TypeError: {reason}")));
            return Ok(JsPromise::reject(JsError::from_opaque(payload), context)?.into());
        }
    };
    match perform(request) {
        Ok(response) => {
            let value = response_object(&response, context)?;
            // 已兑现的 Promise：`.then` 回调用由引擎在脚本跑完后驱动。
            Ok(JsPromise::resolve(value, context)?.into())
        }
        Err(reason) => {
            record_error(format!("fetch 失败：{reason}"));
            // 浏览器里网络错误是 rejected Promise，页面走 `.catch` 分支而不是整段中断。
            let payload = JsValue::from(JsString::from(format!(
                "TypeError: Failed to fetch（{reason}）"
            )));
            Ok(JsPromise::reject(JsError::from_opaque(payload), context)?.into())
        }
    }
}

fn parse_fetch(args: &[JsValue], context: &mut Context) -> Result<HttpRequest, String> {
    let target = args.first().cloned().unwrap_or(JsValue::undefined());
    // `fetch(input, init)`：input 可以是字符串，也可以是 `Request` 实例。
    let (url, mut request) = match target.as_object().filter(|_| !target.is_string()) {
        Some(object) => {
            let url = string_property(&object, "url", context);
            let method = string_property(&object, "method", context);
            let headers = object
                .get(js_string!("headers"), context)
                .ok()
                .map(|value| header_pairs(&value, context))
                .unwrap_or_default();
            let body = string_property(&object, "body", context).into_bytes();
            (
                url.clone(),
                HttpRequest {
                    url,
                    method: normalize_method(&method),
                    headers,
                    body,
                },
            )
        }
        None => {
            let url = target
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default();
            (
                url.clone(),
                HttpRequest {
                    url,
                    method: "GET".to_string(),
                    headers: Vec::new(),
                    body: Vec::new(),
                },
            )
        }
    };
    if url.trim().is_empty() {
        return Err("URL 为空".to_string());
    }
    if let Some(init) = args.get(1).and_then(JsValue::as_object) {
        let method = string_property(&init, "method", context);
        if !method.is_empty() {
            request.method = normalize_method(&method);
        }
        if let Ok(headers) = init.get(js_string!("headers"), context) {
            if !headers.is_undefined() && !headers.is_null() {
                request.headers = header_pairs(&headers, context);
            }
        }
        if let Ok(body) = init.get(js_string!("body"), context) {
            if !body.is_undefined() && !body.is_null() {
                let text = body
                    .to_string(context)
                    .map(|text| text.to_std_string_escaped())
                    .unwrap_or_default();
                request.body = text.into_bytes();
            }
        }
    }
    Ok(request)
}

fn normalize_method(method: &str) -> String {
    let method = method.trim();
    if method.is_empty() || method == "undefined" {
        "GET".to_string()
    } else {
        method.to_ascii_uppercase()
    }
}

/// `Response` 实例：字段名与浏览器一致，正文另存在 `__ornBody` 上。
fn response_object(response: &HttpResponse, context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornResponseProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let object = JsObject::from_proto_and_data(proto, OrdinaryObject);
    let headers = headers_object(&response.headers, context)?;
    let body = response.text_lossy();
    let fields = [
        ("status", JsValue::from(response.status)),
        (
            "statusText",
            JsValue::from(JsString::from(response.status_text.clone())),
        ),
        ("ok", JsValue::from((200..300).contains(&response.status))),
        ("url", JsValue::from(JsString::from(response.url.clone()))),
        ("redirected", JsValue::from(response.redirected)),
        ("type", JsValue::from(js_string!("basic"))),
        ("bodyUsed", JsValue::from(false)),
        ("headers", headers),
        ("__ornBody", JsValue::from(JsString::from(body))),
    ];
    for (name, value) in fields {
        let _ = object.set(js_string!(name), value, false, context);
    }
    Ok(object.into())
}

fn response_construct(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornResponseProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let object = JsObject::from_proto_and_data(proto, OrdinaryObject);
    let body = args
        .first()
        .filter(|value| !value.is_undefined())
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let headers = headers_object(&[], context)?;
    let fields = [
        ("status", JsValue::from(200)),
        ("statusText", JsValue::from(js_string!("OK"))),
        ("ok", JsValue::from(true)),
        ("url", JsValue::from(js_string!(""))),
        ("redirected", JsValue::from(false)),
        ("type", JsValue::from(js_string!("default"))),
        ("bodyUsed", JsValue::from(false)),
        ("headers", headers),
        ("__ornBody", JsValue::from(JsString::from(body))),
    ];
    for (name, value) in fields {
        let _ = object.set(js_string!(name), value, false, context);
    }
    Ok(object.into())
}

fn response_text(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let body = consume_body(this, context);
    Ok(JsPromise::resolve(JsString::from(body), context)?.into())
}

fn response_json(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let body = consume_body(this, context);
    match json_parse(&body, context) {
        Ok(value) => Ok(JsPromise::resolve(value, context)?.into()),
        Err(_) => {
            let payload = JsValue::from(js_string!("SyntaxError: 响应体不是合法 JSON"));
            Ok(JsPromise::reject(JsError::from_opaque(payload), context)?.into())
        }
    }
}

fn response_clone(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let proto = context
        .global_object()
        .get(js_string!("__ornResponseProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let clone = JsObject::from_proto_and_data(proto, OrdinaryObject);
    for name in [
        "status",
        "statusText",
        "ok",
        "url",
        "redirected",
        "type",
        "bodyUsed",
        "headers",
        "__ornBody",
    ] {
        if let Ok(value) = object.get(js_string!(name), context) {
            let _ = clone.set(js_string!(name), value, false, context);
        }
    }
    Ok(clone.into())
}

fn consume_body(this: &JsValue, context: &mut Context) -> String {
    let Some(object) = this.as_object() else {
        return String::new();
    };
    let body = object
        .get(js_string!("__ornBody"), context)
        .ok()
        .and_then(|value| value.to_string(context).ok())
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default();
    let _ = object.set(js_string!("bodyUsed"), JsValue::from(true), false, context);
    body
}

/// 用引擎自己的 `JSON.parse`：解析器行为与页面看到的一致，也不必自己写一遍。
fn json_parse(text: &str, context: &mut Context) -> JsResult<JsValue> {
    let json = context
        .global_object()
        .get(js_string!("JSON"), context)?;
    let Some(json) = json.as_object() else {
        return Err(JsError::from_opaque(JsValue::from(js_string!(
            "JSON 不可用"
        ))));
    };
    let parse = json.get(js_string!("parse"), context)?;
    let Some(parse) = parse.as_object().filter(JsObject::is_callable) else {
        return Err(JsError::from_opaque(JsValue::from(js_string!(
            "JSON.parse 不可用"
        ))));
    };
    parse.call(
        &JsValue::undefined(),
        &[JsValue::from(JsString::from(text))],
        context,
    )
}

// ------------------------------------------------------------- Headers

/// 头表在 JS 对象上的存储：扁平的 `[名, 值, 名, 值, ...]` 数组。
///
/// 用扁平数组而不是嵌套数组：读写都只需按 2 的步长走一遍，
/// 也避免在 `set`/`append` 里反复包装内层数组。
const HEADER_STORE: &str = "__ornHeaderPairs";

fn headers_object(pairs: &[(String, String)], context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornHeadersProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let object = JsObject::from_proto_and_data(proto, OrdinaryObject);
    store_pairs(&object, pairs, context)?;
    Ok(object.into())
}

fn store_pairs(object: &JsObject, pairs: &[(String, String)], context: &mut Context) -> JsResult<()> {
    let array = JsArray::new(context)?;
    for (name, value) in pairs {
        let _ = array.push(JsString::from(name.clone()), context);
        let _ = array.push(JsString::from(value.clone()), context);
    }
    let _ = object.set(js_string!(HEADER_STORE), array, false, context);
    Ok(())
}

fn header_pairs(value: &JsValue, context: &mut Context) -> Vec<(String, String)> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    // `Headers` 实例：直接读回自己的存储。
    if let Ok(store) = object.get(js_string!(HEADER_STORE), context) {
        let pairs = read_flat_pairs(&store, context);
        if !pairs.is_empty() {
            return pairs;
        }
    }
    // 普通对象或数组：按自有可枚举键读。
    let Ok(keys) = object.own_property_keys(context) else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for key in keys {
        let name = match &key {
            PropertyKey::String(text) => text.to_std_string_escaped(),
            PropertyKey::Index(index) => index.get().to_string(),
            PropertyKey::Symbol(_) => continue,
        };
        if name.starts_with("__orn") {
            continue;
        }
        let Ok(item) = object.get(key.clone(), context) else {
            continue;
        };
        if item.is_undefined() || item.is_callable() {
            continue;
        }
        let text = item
            .to_string(context)
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_default();
        pairs.push((name, text));
    }
    pairs
}

fn read_flat_pairs(value: &JsValue, context: &mut Context) -> Vec<(String, String)> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let Ok(array) = JsArray::from_object(object) else {
        return Vec::new();
    };
    let Ok(length) = array.length(context) else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    let mut index = 0;
    while index + 1 < length as usize {
        let name = array
            .at(index as i64, context)
            .ok()
            .and_then(|value| value.to_string(context).ok())
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_default();
        let value = array
            .at(index as i64 + 1, context)
            .ok()
            .and_then(|value| value.to_string(context).ok())
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_default();
        pairs.push((name, value));
        index += 2;
    }
    pairs
}

fn own_pairs(this: &JsValue, context: &mut Context) -> Vec<(String, String)> {
    let Some(object) = this.as_object() else {
        return Vec::new();
    };
    let stored = object
        .get(js_string!(HEADER_STORE), context)
        .unwrap_or(JsValue::undefined());
    read_flat_pairs(&stored, context)
}

fn rewrite_pairs(this: &JsValue, pairs: &[(String, String)], context: &mut Context) {
    if let Some(object) = this.as_object() {
        let _ = store_pairs(&object, pairs, context);
    }
}

fn headers_construct(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let pairs = args
        .first()
        .map(|value| header_pairs(value, context))
        .unwrap_or_default();
    headers_object(&pairs, context)
}

fn headers_get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let found = own_pairs(this, context)
        .into_iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(&name))
        .map(|(_, value)| value);
    Ok(match found {
        Some(value) => JsValue::from(JsString::from(value)),
        None => JsValue::null(),
    })
}

fn headers_has(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let found = own_pairs(this, context)
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case(&name));
    Ok(JsValue::from(found))
}

fn headers_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    let mut pairs = own_pairs(this, context);
    pairs.retain(|(key, _)| !key.eq_ignore_ascii_case(&name));
    pairs.push((name, value));
    rewrite_pairs(this, &pairs, context);
    Ok(JsValue::undefined())
}

fn headers_append(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    let mut pairs = own_pairs(this, context);
    pairs.push((name, value));
    rewrite_pairs(this, &pairs, context);
    Ok(JsValue::undefined())
}

fn headers_delete(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let pairs: Vec<(String, String)> = own_pairs(this, context)
        .into_iter()
        .filter(|(key, _)| !key.eq_ignore_ascii_case(&name))
        .collect();
    rewrite_pairs(this, &pairs, context);
    Ok(JsValue::undefined())
}

fn headers_for_each(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(callback) = args.first().and_then(JsValue::as_object) else {
        return Ok(JsValue::undefined());
    };
    if !callback.is_callable() {
        return Ok(JsValue::undefined());
    }
    for (name, value) in own_pairs(this, context) {
        let params = [
            JsValue::from(JsString::from(value)),
            JsValue::from(JsString::from(name)),
        ];
        let _ = callback.call(this, &params, context);
    }
    Ok(JsValue::undefined())
}

fn headers_keys(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let array = JsArray::new(context)?;
    for (name, _) in own_pairs(this, context) {
        array.push(JsString::from(name), context)?;
    }
    Ok(array.into())
}

fn headers_values(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let array = JsArray::new(context)?;
    for (_, value) in own_pairs(this, context) {
        array.push(JsString::from(value), context)?;
    }
    Ok(array.into())
}

fn headers_entries(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let outer = JsArray::new(context)?;
    for (name, value) in own_pairs(this, context) {
        let entry = JsArray::new(context)?;
        entry.push(JsString::from(name), context)?;
        entry.push(JsString::from(value), context)?;
        outer.push(entry, context)?;
    }
    Ok(outer.into())
}

// ------------------------------------------------------- XMLHttpRequest

const XHR_LISTENERS: &str = "__ornXhrListeners";
const XHR_PAIRS: &str = "__ornXhrPairs";

fn xhr_construct(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornXhrProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let object = JsObject::from_proto_and_data(proto, OrdinaryObject);
    let fields = [
        ("readyState", JsValue::from(0)),
        ("status", JsValue::from(0)),
        ("statusText", JsValue::from(js_string!(""))),
        ("responseType", JsValue::from(js_string!(""))),
        ("response", JsValue::null()),
        ("responseText", JsValue::from(js_string!(""))),
        ("responseURL", JsValue::from(js_string!(""))),
        ("responseXML", JsValue::null()),
        ("timeout", JsValue::from(0)),
        ("withCredentials", JsValue::from(false)),
        ("__ornXhrMethod", JsValue::from(js_string!("GET"))),
        ("__ornXhrUrl", JsValue::from(js_string!(""))),
    ];
    for (name, value) in fields {
        let _ = object.set(js_string!(name), value, false, context);
    }
    let headers = headers_object(&[], context)?;
    let _ = object.set(js_string!(XHR_PAIRS), headers, false, context);
    let listeners = JsArray::new(context)?;
    let _ = object.set(js_string!(XHR_LISTENERS), listeners, false, context);
    Ok(object.into())
}

fn xhr_open(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let method = normalize_method(&args_string(args, 0, context));
    let url = args_string(args, 1, context);
    let _ = object.set(
        js_string!("__ornXhrMethod"),
        JsValue::from(JsString::from(method)),
        false,
        context,
    );
    let _ = object.set(
        js_string!("__ornXhrUrl"),
        JsValue::from(JsString::from(url)),
        false,
        context,
    );
    let _ = object.set(js_string!("readyState"), JsValue::from(1), false, context);
    Ok(JsValue::undefined())
}

fn xhr_set_request_header(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let mut pairs = object
        .get(js_string!(XHR_PAIRS), context)
        .ok()
        .map(|value| header_pairs(&value, context))
        .unwrap_or_default();
    pairs.push((name, value));
    let headers = headers_object(&pairs, context)?;
    let _ = object.set(js_string!(XHR_PAIRS), headers, false, context);
    Ok(JsValue::undefined())
}

fn xhr_send(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let request = HttpRequest {
        url: string_property(&object, "__ornXhrUrl", context),
        method: normalize_method(&string_property(&object, "__ornXhrMethod", context)),
        headers: object
            .get(js_string!(XHR_PAIRS), context)
            .ok()
            .map(|value| header_pairs(&value, context))
            .unwrap_or_default(),
        body: args
            .first()
            .filter(|value| !value.is_undefined() && !value.is_null())
            .map(|value| {
                value
                    .to_string(context)
                    .map(|text| text.to_std_string_escaped())
                    .unwrap_or_default()
                    .into_bytes()
            })
            .unwrap_or_default(),
    };
    let response_type = string_property(&object, "responseType", context);
    match perform(request) {
        Ok(response) => {
            let text = response.text_lossy();
            let response_value = if response_type == "json" && !text.is_empty() {
                json_parse(&text, context).unwrap_or(JsValue::null())
            } else {
                JsValue::from(JsString::from(text.clone()))
            };
            let fields = [
                ("status", JsValue::from(response.status)),
                (
                    "statusText",
                    JsValue::from(JsString::from(response.status_text.clone())),
                ),
                ("readyState", JsValue::from(4)),
                ("responseText", JsValue::from(JsString::from(text))),
                ("response", response_value),
                ("responseURL", JsValue::from(JsString::from(response.url.clone()))),
            ];
            for (name, value) in fields {
                let _ = object.set(js_string!(name), value, false, context);
            }
            let headers = headers_object(&response.headers, context)?;
            let _ = object.set(js_string!(XHR_PAIRS), headers, false, context);
            // 同步派发：页面普遍在 `send()` 之后立刻读 `responseText` / `status`，
            // 异步派发会让整段渲染逻辑在沙箱里永远等不到。
            for event in ["readystatechange", "load", "loadend"] {
                dispatch_xhr(&object, event, context);
            }
        }
        Err(reason) => {
            record_error(format!("XMLHttpRequest 失败：{reason}"));
            let fields = [
                ("readyState", JsValue::from(4)),
                ("status", JsValue::from(0)),
                ("statusText", JsValue::from(js_string!(""))),
            ];
            for (name, value) in fields {
                let _ = object.set(js_string!(name), value, false, context);
            }
            for event in ["readystatechange", "error", "loadend"] {
                dispatch_xhr(&object, event, context);
            }
        }
    }
    Ok(JsValue::undefined())
}

/// 派发一次 XHR 事件：既调 `on<name>` 属性，也调 `addEventListener` 登记的处理器。
fn dispatch_xhr(object: &JsObject, type_name: &str, context: &mut Context) {
    let target = JsValue::from(object.clone());
    let event = ObjectInitializer::new(context)
        .property(
            js_string!("type"),
            JsString::from(type_name),
            Attribute::all(),
        )
        .property(js_string!("target"), target.clone(), Attribute::all())
        .property(js_string!("currentTarget"), target.clone(), Attribute::all())
        .build();
    let event_value = JsValue::from(event);
    let handler_name = format!("on{type_name}");
    if let Ok(handler) = object.get(js_string!(handler_name.as_str()), context) {
        if let Some(function) = handler.as_object().filter(JsObject::is_callable) {
            let _ = function.call(&target, std::slice::from_ref(&event_value), context);
        }
    }
    let listeners = object
        .get(js_string!(XHR_LISTENERS), context)
        .ok()
        .and_then(|value| JsArray::from_object(value.as_object()?).ok());
    let Some(listeners) = listeners else {
        return;
    };
    let mut index = 0;
    while index + 1 < listeners.length(context).unwrap_or(0) as usize {
        let kind = listeners
            .at(index as i64, context)
            .ok()
            .and_then(|value| value.to_string(context).ok())
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_default();
        let function = listeners
            .at(index as i64 + 1, context)
            .unwrap_or(JsValue::undefined());
        if kind == type_name {
            if let Some(function) = function.as_object().filter(JsObject::is_callable) {
                let _ = function.call(&target, std::slice::from_ref(&event_value), context);
            }
        }
        index += 2;
    }
}

fn xhr_add_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let kind = args_string(args, 0, context);
    let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let Ok(listeners) = object.get(js_string!(XHR_LISTENERS), context) else {
        return Ok(JsValue::undefined());
    };
    let Some(listeners) = listeners
        .as_object()
        .and_then(|value| JsArray::from_object(value).ok())
    else {
        return Ok(JsValue::undefined());
    };
    listeners.push(JsString::from(kind), context)?;
    listeners.push(handler, context)?;
    Ok(JsValue::undefined())
}

fn xhr_remove_listener(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let kind = args_string(args, 0, context);
    let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let Ok(listeners) = object.get(js_string!(XHR_LISTENERS), context) else {
        return Ok(JsValue::undefined());
    };
    let Some(listeners) = listeners.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Ok(listeners) = JsArray::from_object(listeners) else {
        return Ok(JsValue::undefined());
    };
    let length = listeners.length(context).unwrap_or(0) as usize;
    let mut kept: Vec<(String, JsValue)> = Vec::new();
    let mut index = 0;
    while index + 1 < length {
        let entry_kind = listeners
            .at(index as i64, context)
            .ok()
            .and_then(|value| value.to_string(context).ok())
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_default();
        let entry_handler = listeners
            .at(index as i64 + 1, context)
            .unwrap_or(JsValue::undefined());
        let same = entry_kind == kind && same_object(&entry_handler, &handler, context);
        if !same {
            kept.push((entry_kind, entry_handler));
        }
        index += 2;
    }
    let rebuilt = JsArray::new(context)?;
    for (kind, handler) in kept {
        rebuilt.push(JsString::from(kind), context)?;
        rebuilt.push(handler, context)?;
    }
    let _ = object.set(js_string!(XHR_LISTENERS), rebuilt, false, context);
    Ok(JsValue::undefined())
}

fn same_object(left: &JsValue, right: &JsValue, context: &mut Context) -> bool {
    match (left.as_object(), right.as_object()) {
        (Some(left), Some(right)) => std::ptr::eq(left.as_ref(), right.as_ref()),
        _ => {
            let left = left.to_string(context).map(|text| text.to_std_string_escaped());
            let right = right.to_string(context).map(|text| text.to_std_string_escaped());
            left.is_ok() && left == right
        }
    }
}

fn xhr_dispatch(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(true))
}

fn xhr_abort(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn xhr_get_all_response_headers(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let text = own_pairs(this, context)
        .into_iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    Ok(JsValue::from(JsString::from(text)))
}

fn xhr_get_response_header(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let found = own_pairs(this, context)
        .into_iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(&name))
        .map(|(_, value)| value);
    Ok(match found {
        Some(value) => JsValue::from(JsString::from(value)),
        None => JsValue::null(),
    })
}

// -------------------------------------------------------------- 工具函数

fn noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn native(function: boa_engine::native_function::NativeFunctionPointer) -> NativeFunction {
    NativeFunction::from_fn_ptr(function)
}

fn args_string(args: &[JsValue], index: usize, context: &mut Context) -> String {
    args.get(index)
        .cloned()
        .unwrap_or(JsValue::undefined())
        .to_string(context)
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default()
}

fn string_property(object: &JsObject, name: &str, context: &mut Context) -> String {
    object
        .get(js_string!(name), context)
        .ok()
        .filter(|value| !value.is_undefined() && !value.is_null())
        .and_then(|value| value.to_string(context).ok())
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default()
}
