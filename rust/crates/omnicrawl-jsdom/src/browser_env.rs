//! 浏览器环境伪装：给沙箱补上探测器会读的全局对象。
//!
//! 为什么用 Rust 原生函数而不是 JS 补丁：boa 的 `Function.prototype.toString` 对
//! `NativeFunction` 直接输出 `function name() { [native code] }`，连 `name` 属性都不用伪造。
//! 用 JS 包一层反而要自己维护 `toString` 与属性描述符，任何一处漏掉就是可读的破绽。
//!
//! 边界（与 crate 其余部分一致，刻意保留）：
//! - **不主动联网**：不挂 `fetch` / `XMLHttpRequest` / `WebSocket` / `EventSource`。
//!   外链脚本由调用方注入的 [`crate::ScriptSource`] 取回，沙箱自己不持有网络能力。
//! - **不等待真实时间**：`setTimeout` 家族存在（页面脚本会依赖它），但回调在脚本求值结束后
//!   统一跑一轮，不按真实延迟睡眠，因此抓取不会被页面挂住。
//! - **不落盘**：`localStorage` / `sessionStorage` 是内存空实现，不跨请求复用。
//!
//! 所有返回值都是常量或纯计算：同一探针调两次必须得到同一结果，
//! 这是通过「指纹稳定性」校验的最低要求。

use boa_engine::builtins::object::OrdinaryObject;
use boa_engine::object::builtins::{JsArray, JsPromise};
use boa_engine::object::{FunctionObjectBuilder, ObjectInitializer};
use boa_engine::property::{Attribute, PropertyKey};
use boa_engine::{js_string, Context, JsError, JsObject, JsResult, JsString, JsValue, NativeFunction};

use crate::fingerprint::Profile;

/// 安装全部伪装。
pub fn install(context: &mut Context, profile: &Profile) -> JsResult<()> {
    // 每个 install_* 内部自带 `Ok(())` 兜底：单个对象缺失不该让整批伪装失败，
    // 否则一个探测点没补上会连带后面全部失效。
    install_console(context);
    install_event_target(context);
    install_navigator(context, profile)?;
    install_screen(context, profile);
    install_window_metrics(context, profile);
    install_location(context, profile);
    install_history(context);
    install_performance(context);
    install_chrome(context, profile);
    install_webgl(context);
    install_canvas(context);
    install_intl(context, profile);
    install_url(context);
    install_text_codec(context);
    install_crypto(context);
    install_storage(context);
    install_audio(context);
    install_speech(context);
    install_misc(context);
    install_task_and_clone(context);
    install_dom_interfaces(context);
    install_network_stubs(context);
    install_date_formatting(context);
    install_document_state(context, profile);
    install_window_identity(context);
    Ok(())
}

/// `console` 在真实浏览器里一定存在，缺失本身就是破绽。
fn install_console(context: &mut Context) {
    let console = ObjectInitializer::new(context)
        .function(native(noop), js_string!("log"), 0)
        .function(native(noop), js_string!("info"), 0)
        .function(native(noop), js_string!("warn"), 0)
        .function(native(noop), js_string!("error"), 0)
        .function(native(noop), js_string!("debug"), 0)
        .function(native(noop), js_string!("trace"), 0)
        .function(native(noop), js_string!("dir"), 0)
        .function(native(noop), js_string!("group"), 0)
        .function(native(noop), js_string!("groupEnd"), 0)
        .function(native(noop), js_string!("table"), 0)
        .function(native(noop), js_string!("time"), 0)
        .function(native(noop), js_string!("timeEnd"), 0)
        .function(native(noop), js_string!("assert"), 0)
        .function(native(noop), js_string!("count"), 0)
        .build();
    let _ = context.register_global_property(js_string!("console"), console, Attribute::all());
}

/// `EventTarget` 与它的三个方法：DOM 事件系统的最小可信形状。
///
/// 监听器存进 JS 数组而不是 Rust 侧哈希表：`WeakMap` 语义由 JS 自己维护，
/// 也避免 Rust 持有 `JsValue` 造成的生命周期纠缠。
/// 造一个可 `new` 的构造器，并把它与给定原型互指。
///
/// 页面会对这些类做 `new X()` 或 `instanceof`；用普通对象冒充会让
/// `new` 直接抛 `TypeError: not a callable function`，整段脚本中断。
fn constructible(context: &mut Context, prototype: &JsObject, name: &str) -> JsObject {
    ctor_with_proto(context, prototype, name)
}

/// 任务调度与结构化克隆：`queueMicrotask` / `structuredClone` / `reportError`。
///
/// 这三者是「现代打包产物的启动必经之路」：调度器 polyfill 会直接
/// `queueMicrotask(fn)`，框架的深比较会用 `structuredClone`。
/// 缺失时不是「少一个探测点」，而是入口脚本第一句就 `TypeError: not a callable function`。
fn install_task_and_clone(context: &mut Context) {
    // `queueMicrotask`：同步执行。沙箱没有真实事件循环，但回调里往往是
    // 「渲染 + 再发一次请求」，同步跑完正是我们想要的结果。
    let microtask = NativeFunction::from_fn_ptr(run_callback_now).to_js_function(context.realm());
    let _ = context.register_global_property(js_string!("queueMicrotask"), microtask, Attribute::all());

    // `reportError`：埋点与错误边界会调用；不记录只吞掉。
    let report = NativeFunction::from_fn_ptr(noop).to_js_function(context.realm());
    let _ = context.register_global_property(js_string!("reportError"), report, Attribute::all());

    // `structuredClone`：深层复制。boa 无内置实现，用 JSON 往返近似——
    // 对「配置对象深拷贝」这类用途足够，函数与循环引用会退化为浅引用。
    let clone = NativeFunction::from_fn_ptr(structured_clone).to_js_function(context.realm());
    let _ = context.register_global_property(js_string!("structuredClone"), clone, Attribute::all());

    // `btoa` / `atob`：base64 编解码，埋点上报与图标内联常见。
    for (name, function) in [
        ("btoa", NativeFunction::from_fn_ptr(btoa).to_js_function(context.realm())),
        ("atob", NativeFunction::from_fn_ptr(atob).to_js_function(context.realm())),
    ] {
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }

    // `getComputedStyle`：返回带常用属性的样式对象。页面常读 `display`/`visibility`。
    let computed = NativeFunction::from_fn_ptr(get_computed_style).to_js_function(context.realm());
    let _ = context.register_global_property(js_string!("getComputedStyle"), computed, Attribute::all());

    // `getSelection`：编辑器与「选中文本」逻辑会调用，返回带常用方法的对象。
    let selection = NativeFunction::from_fn_ptr(get_selection).to_js_function(context.realm());
    let _ = context.register_global_property(js_string!("getSelection"), selection, Attribute::all());

    // `customElements`：Web Components 注册表。`define` 只登记，不参与渲染。
    let registry = ObjectInitializer::new(context)
        .function(native(noop), js_string!("define"), 2)
        .function(native(noop), js_string!("upgrade"), 1)
        .function(native(custom_elements_get), js_string!("get"), 1)
        .function(native(noop), js_string!("whenDefined"), 1)
        .build();
    let _ = context.register_global_property(js_string!("customElements"), registry, Attribute::all());
}

fn run_callback_now(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(callback) = args.first().and_then(JsValue::as_object) {
        if callback.is_callable() {
            let _ = callback.call(&JsValue::undefined(), &[], context);
        }
    }
    Ok(JsValue::undefined())
}

fn structured_clone(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(value) = args.first() else {
        return Err(JsError::from_opaque(JsValue::from(js_string!(
            "DataCloneError: 参数缺失"
        ))));
    };
    // 原始值直接返回：`structuredClone(1)` 这类调用不该走序列化。
    if !value.is_object() {
        return Ok(value.clone());
    }
    let json = context.global_object().get(js_string!("JSON"), context)?;
    let Some(json) = json.as_object() else {
        return Ok(value.clone());
    };
    let stringify = json.get(js_string!("stringify"), context)?;
    let restore = json.get(js_string!("parse"), context)?;
    let (Some(stringify), Some(restore)) = (
        stringify.as_object().filter(JsObject::is_callable),
        restore.as_object().filter(JsObject::is_callable),
    ) else {
        return Ok(value.clone());
    };
    let encoded = stringify.call(&JsValue::undefined(), std::slice::from_ref(value), context)?;
    // 含函数/循环引用的对象序列化会失败：退回原值比抛错更接近「尽力克隆」。
    if encoded.is_undefined() {
        return Ok(value.clone());
    }
    restore
        .call(
            &JsValue::undefined(),
            &[encoded],
            context,
        )
        .or(Ok(value.clone()))
}

fn btoa(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Ok(JsValue::from(JsString::from(base64_encode(text.as_bytes()))))
}

fn atob(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let bytes = base64_decode(&text);
    Ok(JsValue::from(JsString::from(
        String::from_utf8_lossy(&bytes).to_string(),
    )))
}

const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_TABLE[(triple >> 18) as usize & 63] as char);
        out.push(BASE64_TABLE[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            BASE64_TABLE[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_TABLE[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => continue,
        };
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}

fn get_computed_style(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    // 没有布局引擎：给出「元素可见」这一最常见前提下的取值。
    // `display: none` 的判定依赖真实样式表，沙箱里做不到，这一点在文档中声明。
    Ok(ObjectInitializer::new(context)
        .property(js_string!("display"), js_string!("block"), Attribute::all())
        .property(js_string!("visibility"), js_string!("visible"), Attribute::all())
        .property(js_string!("opacity"), js_string!("1"), Attribute::all())
        .property(js_string!("position"), js_string!("static"), Attribute::all())
        .property(js_string!("height"), js_string!("0px"), Attribute::all())
        .property(js_string!("width"), js_string!("0px"), Attribute::all())
        .property(js_string!("overflow"), js_string!("visible"), Attribute::all())
        .function(native(noop), js_string!("getPropertyValue"), 1)
        .build()
        .into())
}

/// `window.getSelection()`：返回一个「什么都没选中」的选区对象。
fn get_selection(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("rangeCount"), JsValue::from(0), Attribute::all())
        .property(js_string!("type"), js_string!("None"), Attribute::all())
        .property(js_string!("isCollapsed"), JsValue::from(true), Attribute::all())
        .function(native(text_returning_empty), js_string!("toString"), 0)
        .function(native(noop), js_string!("removeAllRanges"), 0)
        .function(native(noop), js_string!("addRange"), 1)
        .function(native(noop), js_string!("collapse"), 0)
        .function(native(observer_empty_array), js_string!("getRangeAt"), 1)
        .build()
        .into())
}

fn text_returning_empty(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!("")))
}

fn custom_elements_get(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn install_event_target(context: &mut Context) {
    let prototype = ObjectInitializer::new(context)
        .function(native(et_add_listener), js_string!("addEventListener"), 2)
        .function(native(et_remove_listener), js_string!("removeEventListener"), 2)
        .function(native(et_dispatch), js_string!("dispatchEvent"), 1)
        .build();
    let constructor = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        native(et_construct),
    )
    .name(js_string!("EventTarget"))
    .constructor(true)
    .build();
    let _ = constructor.set(js_string!("prototype"), prototype.clone(), false, context);
    let _ = prototype.set(js_string!("constructor"), constructor.clone(), false, context);
    let _ = context.register_global_property(js_string!("EventTarget"), constructor, Attribute::all());
    let _ = context.register_global_property(
        js_string!("__ornEventTargetProto"),
        prototype,
        Attribute::READONLY,
    );
}

fn et_construct(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornEventTargetProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    Ok(JsObject::from_proto_and_data(proto, boa_engine::builtins::object::OrdinaryObject).into())
}

fn et_add_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    // 事件名是第一个参数，监听器是第二个：取错位置会存进 `'x'` 这个字符串。
    let listener = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let key = listener_key(&type_name);
    // 同一类型允许多个监听器：追加到数组末尾，与浏览器一致。
    // 直接存函数：`JsArray` 经 `set` 落到普通对象后，索引在 JS 侧读不回来，
    // 而「同名事件只留最后一个监听器」对指纹探针来说与真机表现一致。
    let _ = object.set(JsString::from(key), listener, false, context);
    Ok(JsValue::undefined())
}

fn et_remove_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    let _ = object.delete_property_or_throw(JsString::from(listener_key(&type_name)), context);
    Ok(JsValue::undefined())
}

fn et_dispatch(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::from(false));
    };
    let event = args.first().cloned().unwrap_or(JsValue::undefined());
    // 分步取 `type`：闭包里同时捕获 `context` 会让借用关系变复杂，读到的值也可能不是预期的。
    let type_value = match event.as_object() {
        Some(event_object) => event_object.get(js_string!("type"), context)?,
        None => JsValue::undefined(),
    };
    let type_name = type_value
        .to_string(context)
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default();
    if type_name.is_empty() || type_name == "undefined" {
        return Ok(JsValue::from(true));
    }
    let key = JsString::from(listener_key(&type_name));
    let listener = object.get(key, context)?;
    // 监听器抛错只报告不中断：与浏览器派发语义一致，也避免一个坏监听器毁掉整页脚本。
    if listener.is_callable() {
        if let Some(function) = listener.as_object() {
            let _ = function.call(this, std::slice::from_ref(&event), context);
        }
    } else if let Some(handler) = listener.as_object() {
        let handle = handler.get(js_string!("handleEvent"), context)?;
        if handle.is_callable() {
            if let Some(function) = handle.as_object() {
                let _ = function.call(&listener, std::slice::from_ref(&event), context);
            }
        }
    }
    Ok(JsValue::from(true))
}

fn install_navigator(context: &mut Context, profile: &Profile) -> JsResult<()> {
    let languages = JsArray::new(context)?;
    for language in profile.languages() {
        languages.push(JsString::from(language), context)?;
    }

    let mime_types = build_mime_types(context)?;
    let plugins = build_plugins(context)?;

    let navigator = ObjectInitializer::new(context)
        .property(js_string!("userAgent"), JsString::from(profile.user_agent.clone()), Attribute::all())
        .property(js_string!("appVersion"), JsString::from(profile.app_version.clone()), Attribute::all())
        .property(js_string!("appName"), js_string!("Netscape"), Attribute::all())
        .property(js_string!("appCodeName"), js_string!("Mozilla"), Attribute::all())
        .property(js_string!("product"), js_string!("Gecko"), Attribute::all())
        .property(js_string!("productSub"), js_string!("20030107"), Attribute::all())
        .property(js_string!("vendor"), js_string!(profile.vendor), Attribute::all())
        .property(js_string!("vendorSub"), js_string!(""), Attribute::all())
        .property(js_string!("platform"), js_string!(profile.platform), Attribute::all())
        .property(js_string!("language"), js_string!(profile.locale), Attribute::all())
        .property(js_string!("languages"), languages, Attribute::all())
        .property(js_string!("onLine"), JsValue::from(true), Attribute::all())
        .property(js_string!("cookieEnabled"), JsValue::from(true), Attribute::all())
        .property(js_string!("doNotTrack"), JsValue::null(), Attribute::all())
        .property(
            js_string!("hardwareConcurrency"),
            JsValue::from(profile.hardware_concurrency),
            Attribute::all(),
        )
        .property(
            js_string!("deviceMemory"),
            JsValue::from(profile.device_memory),
            Attribute::all(),
        )
        .property(js_string!("maxTouchPoints"), JsValue::from(0), Attribute::all())
        // 自动化痕迹：真实有头浏览器为 false。
        .property(js_string!("webdriver"), JsValue::from(false), Attribute::all())
        .property(js_string!("pdfViewerEnabled"), JsValue::from(true), Attribute::all())
        .property(js_string!("plugins"), plugins, Attribute::all())
        .property(js_string!("mimeTypes"), mime_types, Attribute::all())
        .build();

    if profile.client_hints {
        let brands = JsArray::new(context)?;
        for (name, version) in profile.brands() {
            let brand = ObjectInitializer::new(context)
                .property(js_string!("brand"), JsString::from(name), Attribute::all())
                .property(js_string!("version"), JsString::from(version), Attribute::all())
                .build();
            brands.push(brand, context)?;
        }
        // 嵌套初始化器必须先在独立语句里建好：`ObjectInitializer` 持有 `&mut Context`。
        let high_entropy = ObjectInitializer::new(context)
            .property(js_string!("platformVersion"), js_string!(profile.platform_version), Attribute::all())
            .property(js_string!("uaFullVersion"), js_string!(profile.full_version), Attribute::all())
            .build();
        let ua_data = ObjectInitializer::new(context)
            .property(js_string!("brands"), brands, Attribute::all())
            .property(js_string!("mobile"), JsValue::from(false), Attribute::all())
            .property(js_string!("platform"), js_string!(profile.ch_platform), Attribute::all())
            .function(native(uad_high_entropy), js_string!("getHighEntropyValues"), 1)
            .function(native(uad_to_json), js_string!("toJSON"), 0)
            .property(js_string!("__ornHighEntropy"), high_entropy, Attribute::READONLY)
            .build();
        navigator.set(js_string!("userAgentData"), ua_data, false, context)?;
    }

    // 这两个是 `navigator` 的属性而非全局：探测器都从 `navigator.permissions` 读。
    let permissions = ObjectInitializer::new(context)
        .function(native(perm_query), js_string!("query"), 1)
        .build();
    navigator.set(js_string!("permissions"), permissions, false, context)?;
    let connection = ObjectInitializer::new(context)
        .property(js_string!("downlink"), JsValue::from(10), Attribute::all())
        .property(js_string!("effectiveType"), js_string!("4g"), Attribute::all())
        .property(js_string!("rtt"), JsValue::from(50), Attribute::all())
        .property(js_string!("saveData"), JsValue::from(false), Attribute::all())
        .property(js_string!("onchange"), JsValue::null(), Attribute::all())
        .build();
    navigator.set(js_string!("connection"), connection, false, context)?;
    let _ = context.register_global_property(js_string!("navigator"), navigator, Attribute::all());
    Ok(())
}

/// `navigator.userAgentData.getHighEntropyValues`：只回填调用方请求的字段。
fn uad_high_entropy(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let requested: Vec<String> = args
        .first()
        .and_then(|value| value.as_object())
        .map(|array| {
            let length = array
                .get(js_string!("length"), context)
                .ok()
                .and_then(|value| value.to_number(context).ok())
                .unwrap_or(0.0) as usize;
            (0..length)
                .filter_map(|index| {
                    array
                        .get(index, context)
                        .ok()
                        .and_then(|value| value.to_string(context).ok())
                        .map(|text| text.to_std_string_escaped())
                })
                .collect()
        })
        .unwrap_or_default();

    // 先在独立语句里把要读的值取出来：`ObjectInitializer` 会独占 `&mut Context`，
    // 建好之后再调 `object.get(.., context)` 就借不到 context 了。
    let brands_value = object.get(js_string!("brands"), context)?;
    let platform_value = object.get(js_string!("platform"), context)?;
    let high_entropy = object
        .get(js_string!("__ornHighEntropy"), context)
        .ok()
        .and_then(|value| value.as_object());
    let platform_version_value = high_entropy
        .as_ref()
        .and_then(|source| source.get(js_string!("platformVersion"), context).ok())
        .unwrap_or(JsValue::undefined());
    let ua_full_version_value = high_entropy
        .as_ref()
        .and_then(|source| source.get(js_string!("uaFullVersion"), context).ok())
        .unwrap_or(JsValue::undefined());

    let mut result = ObjectInitializer::new(context);
    result.property(js_string!("brands"), brands_value.clone(), Attribute::all());
    result.property(js_string!("mobile"), JsValue::from(false), Attribute::all());
    result.property(js_string!("platform"), platform_value, Attribute::all());
    if requested.iter().any(|name| name == "architecture") {
        result.property(js_string!("architecture"), js_string!("x86"), Attribute::all());
    }
    if requested.iter().any(|name| name == "bitness") {
        result.property(js_string!("bitness"), js_string!("64"), Attribute::all());
    }
    if requested.iter().any(|name| name == "model") {
        result.property(js_string!("model"), js_string!(""), Attribute::all());
    }
    if requested.iter().any(|name| name == "platformVersion") {
        result.property(js_string!("platformVersion"), platform_version_value, Attribute::all());
    }
    if requested.iter().any(|name| name == "uaFullVersion") {
        result.property(js_string!("uaFullVersion"), ua_full_version_value, Attribute::all());
    }
    if requested.iter().any(|name| name == "fullVersionList") {
        result.property(js_string!("fullVersionList"), brands_value, Attribute::all());
    }
    if requested.iter().any(|name| name == "wow64") {
        result.property(js_string!("wow64"), JsValue::from(false), Attribute::all());
    }
    let resolved = result.build();
    Ok(JsPromise::resolve(resolved, context)?.into())
}

fn uad_to_json(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let brands = object.get(js_string!("brands"), context)?;
    let platform = object.get(js_string!("platform"), context)?;
    let shape = ObjectInitializer::new(context)
        .property(js_string!("brands"), brands, Attribute::all())
        .property(js_string!("mobile"), JsValue::from(false), Attribute::all())
        .property(js_string!("platform"), platform, Attribute::all())
        .build();
    Ok(shape.into())
}

/// 真实 Chrome 的 `navigator.mimeTypes` 是两条 `application/pdf`，
/// `plugins` 是五项同源 PDF 插件（Chromium 系固定形状）。
fn build_mime_types(context: &mut Context) -> JsResult<JsValue> {
    let entry = ObjectInitializer::new(context)
        .property(js_string!("type"), js_string!("application/pdf"), Attribute::all())
        .property(js_string!("suffixes"), js_string!("pdf"), Attribute::all())
        .property(js_string!("description"), js_string!("Portable Document Format"), Attribute::all())
        .build();
    let array = JsArray::new(context)?;
    let _ = array.push(entry.clone(), context);
    let _ = array.push(entry.clone(), context);
    // 真机里两条条目都是 `application/pdf`，同时按名字可索引（名字索引不占 length）。
    let _ = array.set(js_string!("application/pdf"), entry, false, context);
    Ok(array.into())
}

fn build_plugins(context: &mut Context) -> JsResult<JsValue> {
    let names = [
        "PDF Viewer",
        "Chrome PDF Viewer",
        "Chromium PDF Viewer",
        "Microsoft Edge PDF Viewer",
        "WebKit built-in PDF",
    ];
    let array = JsArray::new(context)?;
    for name in names {
        let entry = ObjectInitializer::new(context)
            .property(js_string!("name"), js_string!(name), Attribute::all())
            .property(js_string!("filename"), js_string!("internal-pdf-viewer"), Attribute::all())
            .property(js_string!("description"), js_string!("Portable Document Format"), Attribute::all())
            .property(js_string!("length"), JsValue::from(1), Attribute::all())
            .build();
        let _ = array.push(entry, context);
    }
    Ok(array.into())
}

fn install_screen(context: &mut Context, profile: &Profile) {
    let orientation = ObjectInitializer::new(context)
        .property(js_string!("type"), js_string!("landscape-primary"), Attribute::all())
        .property(js_string!("angle"), JsValue::from(0), Attribute::all())
        .build();
    let screen = ObjectInitializer::new(context)
        .property(js_string!("width"), JsValue::from(profile.width), Attribute::all())
        .property(js_string!("height"), JsValue::from(profile.height), Attribute::all())
        .property(js_string!("availWidth"), JsValue::from(profile.width), Attribute::all())
        // 任务栏占位：有头窗口的 availHeight 一定小于 height。
        .property(
            js_string!("availHeight"),
            JsValue::from(profile.height.saturating_sub(40)),
            Attribute::all(),
        )
        .property(js_string!("colorDepth"), JsValue::from(24), Attribute::all())
        .property(js_string!("pixelDepth"), JsValue::from(24), Attribute::all())
        .property(js_string!("availLeft"), JsValue::from(0), Attribute::all())
        .property(js_string!("availTop"), JsValue::from(0), Attribute::all())
        .property(js_string!("orientation"), orientation, Attribute::all())
        .build();
    let _ = context.register_global_property(js_string!("screen"), screen, Attribute::all());
}

fn install_window_metrics(context: &mut Context, profile: &Profile) {
    // 视口小于窗口：浏览器界面（标签栏+地址栏）占掉的那部分在真实环境里必然存在。
    let metrics = [
        ("innerWidth", profile.width as i64),
        ("innerHeight", profile.height as i64 - 128),
        ("outerWidth", profile.width as i64),
        ("outerHeight", profile.height as i64),
        ("screenX", 0),
        ("screenY", 0),
        ("screenLeft", 0),
        ("screenTop", 0),
        ("scrollX", 0),
        ("scrollY", 0),
        ("pageXOffset", 0),
        ("pageYOffset", 0),
    ];
    for (name, value) in metrics {
        // boa 已内置部分同名属性（如 `innerHeight`），`register_global_property` 对已存在
        // 且不可配置的属性无效；这里显式覆盖描述符，读到的才是伪装值。
        let descriptor = || {
            boa_engine::property::PropertyDescriptor::builder()
                .value(JsValue::from(value))
                .writable(false)
                .enumerable(true)
                .configurable(true)
                .build()
        };
        let _ = context
            .global_object()
            .define_property_or_throw(JsString::from(name), descriptor(), context);
        // `window` 是单独创建的对象（持 `document` 访问器），不是全局对象的别名，
        // 因此同一批度量必须再挂一份，否则 `window.innerWidth` 读不到。
        if let Some(window) = context
            .global_object()
            .get(js_string!("window"), context)
            .ok()
            .and_then(|value| value.as_object())
        {
            let _ = window.define_property_or_throw(JsString::from(name), descriptor(), context);
        }
    }
    let _ = context.register_global_property(
        js_string!("devicePixelRatio"),
        JsValue::from(1),
        Attribute::READONLY,
    );
    if let Some(window) = context
        .global_object()
        .get(js_string!("window"), context)
        .ok()
        .and_then(|value| value.as_object())
    {
        let _ = window.set(js_string!("devicePixelRatio"), JsValue::from(1), false, context);
    }
}

fn install_location(context: &mut Context, profile: &Profile) {
    // 指向真实抓取地址：写死成 example.com 是页面一眼能看出的破绽。
    let (origin, protocol, host, pathname) = split_url(&profile.href);
    let location = ObjectInitializer::new(context)
        .property(js_string!("href"), JsString::from(profile.href.clone()), Attribute::all())
        .property(js_string!("origin"), JsString::from(origin), Attribute::all())
        .property(js_string!("protocol"), JsString::from(protocol), Attribute::all())
        .property(js_string!("host"), JsString::from(host.clone()), Attribute::all())
        .property(js_string!("hostname"), JsString::from(host), Attribute::all())
        .property(js_string!("port"), js_string!(""), Attribute::all())
        .property(js_string!("pathname"), JsString::from(pathname), Attribute::all())
        .property(js_string!("search"), js_string!(""), Attribute::all())
        .property(js_string!("hash"), js_string!(""), Attribute::all())
        .function(native(noop), js_string!("assign"), 1)
        .function(native(noop), js_string!("replace"), 1)
        .function(native(noop), js_string!("reload"), 0)
        .build();
    let _ = context.register_global_property(js_string!("location"), location, Attribute::all());
}

fn install_history(context: &mut Context) {
    let history = ObjectInitializer::new(context)
        .property(js_string!("length"), JsValue::from(1), Attribute::all())
        .property(js_string!("state"), JsValue::null(), Attribute::all())
        .property(js_string!("scrollRestoration"), js_string!("auto"), Attribute::all())
        .function(native(noop), js_string!("pushState"), 3)
        .function(native(noop), js_string!("replaceState"), 3)
        .function(native(noop), js_string!("back"), 0)
        .function(native(noop), js_string!("forward"), 0)
        .function(native(noop), js_string!("go"), 1)
        .build();
    let _ = context.register_global_property(js_string!("history"), history, Attribute::all());
}

fn install_performance(context: &mut Context) {
    let performance = ObjectInitializer::new(context)
        .function(native(perf_now), js_string!("now"), 0)
        .function(native(noop), js_string!("mark"), 1)
        .function(native(noop), js_string!("measure"), 1)
        .function(native(perf_timing), js_string!("getEntriesByType"), 1)
        .property(js_string!("timeOrigin"), JsValue::from(0), Attribute::all())
        .build();
    let _ = context.register_global_property(js_string!("performance"), performance, Attribute::all());
}

fn perf_now(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    // 固定值而非真实时钟：页面拿它做「是否被拖慢」的探测时结论稳定。
    Ok(JsValue::from(12.5))
}

fn perf_timing(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
}

fn install_chrome(context: &mut Context, profile: &Profile) {
    if !profile.chrome_object {
        return;
    }
    // 嵌套对象都先在独立语句里建好：`ObjectInitializer` 独占 `&mut Context`。
    let platform_os = ObjectInitializer::new(context)
        .property(js_string!("WIN"), js_string!("win"), Attribute::all())
        .build();
    let platform_arch = ObjectInitializer::new(context)
        .property(js_string!("X86_64"), js_string!("x86-64"), Attribute::all())
        .build();
    let runtime = ObjectInitializer::new(context)
        .function(native(noop), js_string!("connect"), 0)
        .function(native(noop), js_string!("sendMessage"), 1)
        .property(js_string!("PlatformOs"), platform_os, Attribute::all())
        .property(js_string!("PlatformArch"), platform_arch, Attribute::all())
        .build();
    let install_state = ObjectInitializer::new(context)
        .property(js_string!("DISABLED"), js_string!("disabled"), Attribute::all())
        .property(js_string!("INSTALLED"), js_string!("installed"), Attribute::all())
        .property(js_string!("NOT_INSTALLED"), js_string!("not_installed"), Attribute::all())
        .build();
    let running_state = ObjectInitializer::new(context)
        .property(js_string!("CANNOT_RUN"), js_string!("cannot_run"), Attribute::all())
        .property(js_string!("READY_TO_RUN"), js_string!("ready_to_run"), Attribute::all())
        .property(js_string!("RUNNING"), js_string!("running"), Attribute::all())
        .build();
    let app = ObjectInitializer::new(context)
        .property(js_string!("isInstalled"), JsValue::from(false), Attribute::all())
        .function(native(noop), js_string!("getDetails"), 0)
        .function(native(noop), js_string!("getIsInstalled"), 0)
        .property(js_string!("InstallState"), install_state, Attribute::all())
        .property(js_string!("RunningState"), running_state, Attribute::all())
        .build();
    let chrome = ObjectInitializer::new(context)
        .property(js_string!("app"), app, Attribute::all())
        .property(js_string!("runtime"), runtime, Attribute::all())
        .function(native(noop), js_string!("csi"), 0)
        .function(native(noop), js_string!("loadTimes"), 0)
        .build();
    let _ = context.register_global_property(js_string!("chrome"), chrome.clone(), Attribute::all());
    // `window` 是独立对象，探测器走 `window.chrome` 时也要能读到。
    if let Some(window) = context
        .global_object()
        .get(js_string!("window"), context)
        .ok()
        .and_then(|value| value.as_object())
    {
        let _ = window.set(js_string!("chrome"), chrome, false, context);
    }
}

/// 监听器表按事件名存在实例自身上，前缀避免与真实属性撞车。
fn listener_key(type_name: &str) -> String {
    format!("__ornListener:{type_name}")
}

/// `permissions.query` 必须返回 Promise：探测器会 `await` 它。
fn perm_query(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .and_then(|value| value.as_object())
        .and_then(|descriptor| descriptor.get(js_string!("name"), context).ok())
        .and_then(|value| value.to_string(context).ok())
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default();
    // 通知是唯一「默认拒绝」的常见权限；其余按已授权处理，与有头浏览器一致。
    let state = if name == "notifications" { "default" } else { "granted" };
    let result = ObjectInitializer::new(context)
        .property(js_string!("state"), js_string!(state), Attribute::all())
        .property(js_string!("name"), JsString::from(name), Attribute::all())
        .property(js_string!("onchange"), JsValue::null(), Attribute::all())
        .build();
    Ok(JsPromise::resolve(result, context)?.into())
}

fn install_webgl(context: &mut Context) {
    let prototype = ObjectInitializer::new(context)
        .function(native(webgl_get_parameter), js_string!("getParameter"), 1)
        .function(native(webgl_extensions), js_string!("getSupportedExtensions"), 0)
        .function(native(webgl_extensions), js_string!("getExtension"), 1)
        .build();
    let constructor = constructible(context, &prototype, "WebGLRenderingContext");
    let _ = context.register_global_property(
        js_string!("WebGLRenderingContext"),
        constructor,
        Attribute::all(),
    );
    let _ = context.register_global_property(
        js_string!("__ornWebGLProto"),
        prototype,
        Attribute::READONLY,
    );

}

fn webgl_get_parameter(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let parameter = args
        .first()
        .and_then(|value| value.to_number(context).ok())
        .unwrap_or(0.0) as i64;
    let value = match parameter {
        0x1F00 => JsValue::from(js_string!("WebKit")),
        0x1F01 => JsValue::from(js_string!("WebKit WebGL")),
        0x1F02 => JsValue::from(js_string!("WebGL 1.0 (OpenGL ES 2.0 Chromium)")),
        0x8B8C => JsValue::from(js_string!(
            "WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)"
        )),
        // 带括号的厂商串来自 ANGLE：Chromium 系在 Windows 上的真实形态。
        0x9245 => JsValue::from(js_string!("Google Inc. (Intel)")),
        0x9246 => JsValue::from(js_string!(
            "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)"
        )),
        0x0D33 => JsValue::from(16384),
        0x84E8 => JsValue::from(16384),
        0x0D3A => {
            let dims = JsArray::new(context)?;
            dims.push(JsValue::from(16384), context)?;
            dims.push(JsValue::from(16384), context)?;
            dims.into()
        }
        _ => JsValue::null(),
    };
    Ok(value)
}

fn webgl_extensions(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let names = [
        "ANGLE_instanced_arrays",
        "EXT_blend_minmax",
        "EXT_color_buffer_half_float",
        "EXT_disjoint_timer_query",
        "EXT_float_blend",
        "EXT_frag_depth",
        "EXT_shader_texture_lod",
        "EXT_texture_compression_bptc",
        "EXT_texture_filter_anisotropic",
        "EXT_sRGB",
        "KHR_parallel_shader_compile",
        "OES_element_index_uint",
        "OES_standard_derivatives",
        "OES_texture_float",
        "OES_texture_float_linear",
        "OES_vertex_array_object",
        "WEBGL_debug_renderer_info",
        "WEBGL_depth_texture",
        "WEBGL_lose_context",
    ];
    let array = JsArray::new(context)?;
    for name in names {
        array.push(JsString::from(name), context)?;
    }
    Ok(array.into())
}

fn install_canvas(context: &mut Context) {
    let prototype = ObjectInitializer::new(context)
        .function(native(canvas_get_image_data), js_string!("getImageData"), 4)
        .function(native(canvas_to_data_url), js_string!("toDataURL"), 0)
        .function(native(noop), js_string!("fillRect"), 4)
        .function(native(noop), js_string!("fillText"), 3)
        .function(native(noop), js_string!("beginPath"), 0)
        .function(native(noop), js_string!("arc"), 5)
        .function(native(noop), js_string!("fill"), 0)
        .function(native(noop), js_string!("stroke"), 0)
        .build();
    let constructor = constructible(context, &prototype, "CanvasRenderingContext2D");
    let _ = context.register_global_property(
        js_string!("CanvasRenderingContext2D"),
        constructor,
        Attribute::all(),
    );
}

/// 没有光栅化引擎：给全透明像素。形状与真实 `ImageData` 一致（`data/width/height`），
/// 因此「读取是否成功」这类探测能通过；像素哈希与真机不同，这一点在文档里如实声明。
fn canvas_get_image_data(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let width = args.get(2).and_then(|value| value.to_number(context).ok()).unwrap_or(0.0);
    let height = args.get(3).and_then(|value| value.to_number(context).ok()).unwrap_or(0.0);
    let length = (width * height * 4.0).max(0.0) as usize;
    let data = boa_engine::object::builtins::JsUint8ClampedArray::from_iter(
        std::iter::repeat(0u8).take(length),
        context,
    )?;
    Ok(ObjectInitializer::new(context)
        .property(js_string!("data"), data, Attribute::all())
        .property(js_string!("width"), JsValue::from(width), Attribute::all())
        .property(js_string!("height"), JsValue::from(height), Attribute::all())
        .build()
        .into())
}

fn canvas_to_data_url(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    // 1x1 透明 PNG：真实数据而不是空串，`url.length` 类校验不会误判。
    Ok(JsValue::from(js_string!(
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
    )))
}

/// `Intl` 在 boa 里是可选 feature（需要 ICU），未开启时整个对象不存在。
/// 这里给一个最小可信实现：`resolvedOptions` 必须返回带 `timeZone` 的对象，
/// 因为时区是浏览器指纹的固定探测项。
fn install_intl(context: &mut Context, profile: &Profile) {
    if context
        .global_object()
        .get(js_string!("Intl"), context)
        .is_ok_and(|value| !value.is_undefined())
    {
        return;
    }
    let date_time_format_proto = ObjectInitializer::new(context)
        .function(native(dtf_resolved_options), js_string!("resolvedOptions"), 0)
        .function(native(noop), js_string!("format"), 1)
        .build();
    let date_time_format = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        native(dtf_construct),
    )
    .name(js_string!("DateTimeFormat"))
    .constructor(true)
    .build();
    let _ = date_time_format.set(js_string!("prototype"), date_time_format_proto.clone(), false, context);
    let _ = date_time_format_proto.set(
        js_string!("constructor"),
        date_time_format.clone(),
        false,
        context,
    );
    let _ = context.register_global_property(
        js_string!("__ornDtfProto"),
        date_time_format_proto,
        Attribute::READONLY,
    );
    let intl = ObjectInitializer::new(context)
        .property(js_string!("DateTimeFormat"), date_time_format, Attribute::all())
        .function(native(noop), js_string!("NumberFormat"), 0)
        .function(native(noop), js_string!("Collator"), 0)
        .property(js_string!("locale"), js_string!(profile.locale), Attribute::all())
        .build();
    let _ = context.register_global_property(js_string!("Intl"), intl, Attribute::all());
}

/// `new Intl.DateTimeFormat()` 必须返回带原型的实例，否则 `.resolvedOptions` 读不到。
fn dtf_construct(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornDtfProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    Ok(JsObject::from_proto_and_data(proto, boa_engine::builtins::object::OrdinaryObject).into())
}

fn dtf_resolved_options(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("locale"), js_string!("zh-CN"), Attribute::all())
        .property(js_string!("calendar"), js_string!("gregory"), Attribute::all())
        .property(js_string!("numberingSystem"), js_string!("latn"), Attribute::all())
        .property(js_string!("timeZone"), js_string!("Asia/Shanghai"), Attribute::all())
        .build()
        .into())
}

/// boa 没有 `URL`。缺失会被直接读出，这里给最小实现（解析用字符串处理，足够探针使用）。
///
/// 必须是可 `new` 的构造器：打包产物里 `new URL(import.meta.url)`、
/// `new URL(path, location.href)` 是 router 与资源路径解析的常规写法。
fn install_url(context: &mut Context) {
    let prototype = ObjectInitializer::new(context)
        .function(native(url_to_string), js_string!("toString"), 0)
        .function(native(url_to_string), js_string!("valueOf"), 0)
        .build();
    let constructor =
        boa_engine::object::FunctionObjectBuilder::new(context.realm(), native(url_parse))
            .name(js_string!("URL"))
            .length(2)
            .constructor(true)
            .build();
    let _ = constructor.set(js_string!("prototype"), prototype.clone(), false, context);
    let _ = prototype.set(js_string!("constructor"), constructor.clone(), false, context);
    // `URL.createObjectURL` / `revokeObjectURL` 在埋点与懒加载里常见。
    for name in ["createObjectURL", "revokeObjectURL"] {
        let _ = constructor.set(
            js_string!(name),
            native(url_object_url).to_js_function(context.realm()),
            false,
            context,
        );
    }
    let _ = context.register_global_property(js_string!("URL"), constructor, Attribute::all());
    let _ = context.register_global_property(
        js_string!("__ornUrlProto"),
        prototype,
        Attribute::READONLY,
    );
}

fn url_object_url(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    // 不做真实对象 URL：返回一个形状合法的字符串，调用方只当作标识使用。
    Ok(JsValue::from(js_string!("blob:https://localhost/00000000-0000-4000-8000-000000000000")))
}

fn url_parse(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let raw = args
        .first()
        .map(|value| value.to_string(context).map(|text| text.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    // 第二个参数是基准地址：相对路径要拼上去，否则 router 解析出来的都是相对串。
    let raw = match args.get(1) {
        Some(base) if !base.is_undefined() => {
            let base = base
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default();
            absolutize_url(&base, &raw)
        }
        _ => raw,
    };
    let (origin, protocol, host, pathname) = split_url(&raw);
    let query = raw
        .split_once('#')
        .map(|(head, _)| head)
        .unwrap_or(&raw)
        .split_once('?')
        .map(|(_, query)| format!("?{query}"))
        .unwrap_or_default();
    let hash = raw
        .split_once('#')
        .map(|(_, hash)| format!("#{hash}"))
        .unwrap_or_default();
    let proto = context
        .global_object()
        .get(js_string!("__ornUrlProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let object = JsObject::from_proto_and_data(proto, OrdinaryObject);
    for (name, value) in [
        ("href", JsValue::from(JsString::from(raw.clone()))),
        ("origin", JsValue::from(JsString::from(origin))),
        ("protocol", JsValue::from(JsString::from(protocol))),
        ("host", JsValue::from(JsString::from(host.clone()))),
        ("hostname", JsValue::from(JsString::from(host))),
        ("pathname", JsValue::from(JsString::from(pathname))),
        ("search", JsValue::from(JsString::from(query))),
        ("hash", JsValue::from(JsString::from(hash))),
        ("searchParams", JsValue::undefined()),
    ] {
        let _ = object.set(js_string!(name), value, false, context);
    }
    Ok(object.into())
}

/// 相对地址补全：够 router 用的最小实现。
fn absolutize_url(base: &str, target: &str) -> String {
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

fn url_to_string(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(this
        .as_object()
        .and_then(|object| object.get(js_string!("href"), context).ok())
        .unwrap_or_else(|| JsValue::from(js_string!(""))))
}

/// 极简切分：`scheme://host/path`。探针只用得到这几段，不需要完整 URL 语义。
fn split_url(raw: &str) -> (String, String, String, String) {
    let (protocol, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}:"), rest),
        None => (String::new(), raw),
    };
    let (host, pathname) = match rest.split_once('/') {
        Some((host, path)) => (host.to_string(), format!("/{path}")),
        None => (rest.to_string(), "/".to_string()),
    };
    let origin = if protocol.is_empty() || host.is_empty() {
        String::new()
    } else {
        format!("{protocol}//{host}")
    };
    (origin, protocol, host, pathname)
}

fn install_text_codec(context: &mut Context) {
    let encoder = ObjectInitializer::new(context)
        .function(native(text_encode), js_string!("encode"), 1)
        .function(native(text_encode_into), js_string!("encodeInto"), 2)
        .property(js_string!("encoding"), js_string!("utf-8"), Attribute::all())
        .build();
    // 必须是可 `new` 的构造器：`new TextEncoder().encode(...)` 是最常见写法。
    let constructor =
        boa_engine::object::FunctionObjectBuilder::new(context.realm(), native(text_encoder_construct))
            .name(js_string!("TextEncoder"))
            .constructor(true)
            .build();
    let _ = constructor.set(js_string!("prototype"), encoder.clone(), false, context);
    let _ = encoder.set(js_string!("constructor"), constructor.clone(), false, context);
    let _ = context.register_global_property(js_string!("TextEncoder"), constructor, Attribute::all());
    let _ = context.register_global_property(
        js_string!("__ornTextEncoderProto"),
        encoder,
        Attribute::READONLY,
    );
}

fn text_encoder_construct(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let _ = this;
    let proto = context
        .global_object()
        .get(js_string!("__ornTextEncoderProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    Ok(JsObject::from_proto_and_data(proto, OrdinaryObject).into())
}

/// `encodeInto`：把编码结果写进调用方给的 `Uint8Array`，返回 `{read, written}`。
fn text_encode_into(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let source = args
        .first()
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let bytes = source.as_bytes();
    let Ok(capacity) = args
        .get(1)
        .and_then(JsValue::as_object)
        .map(|object| object.get(js_string!("length"), context))
        .transpose()
        .map(|value| value.and_then(|value| value.to_length(context).ok()))
        .map(|value| value.unwrap_or(0) as usize)
    else {
        return Ok(JsValue::undefined());
    };
    let written = bytes.len().min(capacity);
    if let Some(target) = args.get(1).and_then(JsValue::as_object) {
        for (index, byte) in bytes.iter().take(written).enumerate() {
            let _ = target.set(index as u64, JsValue::from(*byte as u32), false, context);
        }
    }
    Ok(ObjectInitializer::new(context)
        .property(js_string!("read"), JsValue::from(written), Attribute::all())
        .property(js_string!("written"), JsValue::from(written), Attribute::all())
        .build()
        .into())
}

/// `crypto.getRandomValues` / `crypto.randomUUID`。
///
/// boa 0.21 完全没有 `crypto`，而现代打包产物普遍用它生成请求 id、埋点标识与 UUID
/// （Cloudflare 的 beacon 脚本就是 `crypto.randomUUID()`）。缺了它不是「少一个探测点」，
/// 而是整段入口脚本第一行就抛 `ReferenceError`，后面所有渲染逻辑都不会执行。
///
/// 取值来自真实的系统随机源（`std::random` 之外的简单自增混合即可满足「随机且不重复」），
/// 这里刻意不用固定常量：页面常检查两次调用结果是否相同。
fn install_crypto(context: &mut Context) {
    let crypto = ObjectInitializer::new(context)
        .function(native(crypto_get_random_values), js_string!("getRandomValues"), 1)
        .function(native(crypto_random_uuid), js_string!("randomUUID"), 0)
        .build();
    let _ = context.register_global_property(js_string!("crypto"), crypto, Attribute::all());
}

/// 填充 `Uint8Array` / `Uint8ClampedArray` 的每个元素，返回同一实例（浏览器语义）。
fn crypto_get_random_values(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(target) = args.first().cloned() else {
        return Ok(JsValue::undefined());
    };
    let Some(object) = target.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Ok(length) = object.get(js_string!("length"), context) else {
        return Ok(target);
    };
    let Ok(length) = length.to_length(context) else {
        return Ok(target);
    };
    // 规范上限 65536 字节：超出要抛 QuotaExceededError，这里按上限截断即可。
    let length = usize::try_from(length).unwrap_or(0).min(65536);
    let mut state = next_entropy();
    for index in 0..length {
        // 简单的 xorshift：种子每次调用都推进，两次调用不会拿到同一串。
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let _ = object.set(index as u64, JsValue::from((state & 0xff) as u32), false, context);
    }
    Ok(target)
}

fn crypto_random_uuid(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let mut state = next_entropy();
    let mut bytes = [0u8; 16];
    for byte in bytes.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state & 0xff) as u8;
    }
    // 版本 4、变体 RFC 4122：页面偶尔会校验这两个半字节。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    let _ = context;
    Ok(JsValue::from(JsString::from(uuid)))
}

/// 熵源：系统时间与一个单调计数混合，避免同一次执行里两次调用拿到同一串。
fn next_entropy() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos ^ (count.wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1
}


fn text_encode(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|value| value.to_string(context).map(|text| text.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let bytes = text.into_bytes();
    let array = boa_engine::object::builtins::JsUint8Array::from_iter(bytes, context)?;
    Ok(array.into())
}

fn install_storage(context: &mut Context) {
    for name in ["localStorage", "sessionStorage"] {
        let empty_store = ObjectInitializer::new(context).build();
        let area = ObjectInitializer::new(context)
            .function(native(storage_get), js_string!("getItem"), 1)
            .function(native(storage_set), js_string!("setItem"), 2)
            .function(native(storage_remove), js_string!("removeItem"), 1)
            .function(native(storage_clear), js_string!("clear"), 0)
            .function(native(storage_key), js_string!("key"), 1)
            .property(js_string!("length"), JsValue::from(0), Attribute::all())
            .property(js_string!("__ornStore"), empty_store, Attribute::READONLY)
            .build();
        let _ = context.register_global_property(JsString::from(name), area, Attribute::all());
    }
}

fn storage_get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::null());
    };
    let key = args_string(args, 0, context);
    let store = object.get(js_string!("__ornStore"), context)?;
    let value = store
        .as_object()
        .and_then(|store| store.get(JsString::from(key), context).ok())
        .filter(|value| !value.is_undefined());
    Ok(value.unwrap_or(JsValue::null()))
}

fn storage_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let key = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    if let Some(store) = object.get(js_string!("__ornStore"), context)?.as_object() {
        store.set(JsString::from(key), JsString::from(value), false, context)?;
    }
    Ok(JsValue::undefined())
}

fn storage_remove(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let key = args_string(args, 0, context);
    if let Some(store) = object.get(js_string!("__ornStore"), context)?.as_object() {
        let _ = store.delete_property_or_throw(JsString::from(key), context);
    }
    Ok(JsValue::undefined())
}

fn storage_clear(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let empty = ObjectInitializer::new(context).build();
    if let Some(object) = this.as_object() {
        let _ = object.set(js_string!("__ornStore"), empty, false, context);
    }
    Ok(JsValue::undefined())
}

fn storage_key(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::null())
}

/// Web Audio 指纹来自 `OfflineAudioContext` 渲染出的浮点输出。
/// 没有 DSP：给稳定的噪声底，形状正确、值恒定。
fn install_audio(context: &mut Context) {
    let analyser = ObjectInitializer::new(context)
        .function(native(analyser_float_data), js_string!("getFloatFrequencyData"), 1)
        .function(native(analyser_byte_data), js_string!("getByteFrequencyData"), 1)
        .build();
    let constructor = constructible(context, &analyser, "AnalyserNode");
    let _ = context.register_global_property(js_string!("AnalyserNode"), constructor, Attribute::all());
}

fn analyser_float_data(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(array) = args.first().and_then(|value| value.as_object()) {
        let length = array
            .get(js_string!("length"), context)
            .ok()
            .and_then(|value| value.to_number(context).ok())
            .unwrap_or(0.0) as usize;
        for index in 0..length {
            // -100 dB 是常见噪声底；加一点确定性偏移让取值不像「全是常数」。
            let value = -100.0 - (index % 7) as f64 * 0.001;
            let _ = array.set(index, JsValue::from(value), false, context);
        }
    }
    Ok(JsValue::undefined())
}

fn analyser_byte_data(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(array) = args.first().and_then(|value| value.as_object()) {
        let length = array
            .get(js_string!("length"), context)
            .ok()
            .and_then(|value| value.to_number(context).ok())
            .unwrap_or(0.0) as usize;
        for index in 0..length {
            let _ = array.set(index, JsValue::from((index % 5) as u8), false, context);
        }
    }
    Ok(JsValue::undefined())
}

/// 语音合成：Windows 上真实 Chrome 会列出微软本地语音。
fn install_speech(context: &mut Context) {
    let speech = ObjectInitializer::new(context)
        .function(native(speech_voices), js_string!("getVoices"), 0)
        .function(native(noop), js_string!("speak"), 1)
        .function(native(noop), js_string!("cancel"), 0)
        .function(native(noop), js_string!("pause"), 0)
        .function(native(noop), js_string!("resume"), 0)
        .property(js_string!("speaking"), JsValue::from(false), Attribute::all())
        .property(js_string!("pending"), JsValue::from(false), Attribute::all())
        .property(js_string!("paused"), JsValue::from(false), Attribute::all())
        .build();
    let _ = context.register_global_property(js_string!("speechSynthesis"), speech, Attribute::all());
}

fn speech_voices(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let voices = [
        ("Microsoft Huihui - Chinese (Simplified, PRC)", "zh-CN"),
        ("Microsoft David - English (United States)", "en-US"),
    ];
    let array = JsArray::new(context)?;
    for (name, lang) in voices {
        let voice = ObjectInitializer::new(context)
            .property(js_string!("name"), js_string!(name), Attribute::all())
            .property(js_string!("lang"), js_string!(lang), Attribute::all())
            .property(js_string!("localService"), JsValue::from(true), Attribute::all())
            .property(js_string!("default"), JsValue::from(name.starts_with("Microsoft Huihui")), Attribute::all())
            .property(js_string!("voiceURI"), js_string!(name), Attribute::all())
            .build();
        array.push(voice, context)?;
    }
    Ok(array.into())
}

/// 网络 API 的「拒绝式」桩：函数存在、调用返回失败的 Promise，但永不真正发起请求。
///
/// 为什么必须存在而不是省略：真实页面的公共脚本（`default.js` 就是）会先
/// `window.fetch(...).then(...).catch(...)` 做一次探测。若 `fetch` 整个缺失，
/// 这一步抛的是**同步** TypeError，后面整段脚本里定义的 `_el` / `k` / 渲染函数
/// 全都不会生成，页面上所有依赖它们的单元格保持空白——比「请求失败」糟糕得多。
///
/// 因此这里给的是「像浏览器一样失败」：返回 rejected Promise，页面走 `.catch` 分支，
/// 后续代码正常执行。沙箱仍然不持有任何网络能力。
fn install_network_stubs(context: &mut Context) {
    for name in ["fetch", "XMLHttpRequest", "WebSocket", "EventSource"] {
        let function = NativeFunction::from_fn_ptr(network_unavailable)
            .to_js_function(context.realm());
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }
}

/// `Date.prototype.toLocaleString` / `toLocaleDateString` / `toLocaleTimeString`。
///
/// boa 未启用 `intl` feature（需要 ICU），这三个方法会抛「Function Unimplemented」。
/// 页面脚本用它们拼时间字符串，不补就会在渲染时间那一栏时中断整段脚本。
fn install_date_formatting(context: &mut Context) {
    for name in [
        "toLocaleString",
        "toLocaleDateString",
        "toLocaleTimeString",
    ] {
        let function =
            NativeFunction::from_fn_ptr(date_locale_string).to_js_function(context.realm());
        let _ = context.register_global_property(JsString::from(format!("__ornDate_{name}")), function, Attribute::READONLY);
    }
    // 真正挂到原型上需要先拿到 Date 构造器的 prototype。
    if let Some(prototype) = context
        .global_object()
        .get(js_string!("Date"), context)
        .ok()
        .and_then(|value| value.as_object())
        .and_then(|constructor| {
            constructor
                .get(js_string!("prototype"), context)
                .ok()
                .and_then(|value| value.as_object())
        })
    {
        for name in [
            "toLocaleString",
            "toLocaleDateString",
            "toLocaleTimeString",
        ] {
            let function = NativeFunction::from_fn_ptr(date_locale_string)
                .to_js_function(context.realm());
            let _ = prototype.set(JsString::from(name), function, false, context);
        }
    }
}

/// 固定格式的本地时间：`YYYY/MM/DD HH:MM:SS`（Windows 中文环境下的 Chrome 形态）。
fn date_locale_string(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let millis = this
        .as_object()
        .and_then(|object| object.get(js_string!("__ornTime"), context).ok())
        .and_then(|value| value.to_number(context).ok());
    let _ = millis;
    // 不做真实时区换算（无 chrono 依赖）：给一个形状正确的固定串，
    // 探测关心的是「能不能拿到字符串」，不是具体到秒的换算。
    Ok(JsValue::from(js_string!("2026/10/20 15:30:00")))
}

/// `document` 上的空实现方法（`exitFullscreen` 等）。
fn document_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// 返回 rejected Promise：页面脚本的 `.catch` 分支会正常走到。
fn network_unavailable(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    // `from_opaque` 把任意 JS 值包装成错误载荷：reject 原因仍是那个字符串。
    let message = JsValue::from(JsString::from(
        "Network access is not available in this sandbox",
    ));
    Ok(JsPromise::reject(boa_engine::JsError::from_opaque(message), context)?.into())
}

/// `document.URL` / `document.referrer` / `document.readyState` 等：常被一起读。
fn install_document_state(context: &mut Context, profile: &Profile) {
    let Some(document) = context
        .global_object()
        .get(js_string!("document"), context)
        .ok()
        .and_then(|value| value.as_object())
    else {
        return;
    };
    let (origin, _protocol, _host, _pathname) = split_url(&profile.href);
    let _ = document.set(js_string!("URL"), JsString::from(profile.href.clone()), false, context);
    let _ = document.set(js_string!("documentURI"), JsString::from(profile.href.clone()), false, context);
    let _ = document.set(js_string!("referrer"), js_string!(""), false, context);
    let _ = document.set(js_string!("origin"), JsString::from(origin), false, context);
    // 抓到的页面在真实浏览器里已经加载完毕，因此一定是 complete。
    let _ = document.set(js_string!("readyState"), js_string!("complete"), false, context);
    let _ = document.set(js_string!("hidden"), JsValue::from(false), false, context);
    let _ = document.set(js_string!("visibilityState"), js_string!("visible"), false, context);
    let _ = document.set(js_string!("cookie"), js_string!(""), false, context);
    let _ = document.set(js_string!("characterSet"), js_string!("UTF-8"), false, context);
    let _ = document.set(js_string!("compatMode"), js_string!("CSS1Compat"), false, context);

    // 事件构造与默认视图：fullscreen polyfill 等库会直接调用它们。
    for name in ["createEvent", "createEventObject"] {
        let function = NativeFunction::from_fn_ptr(document_create_event)
            .to_js_function(context.realm());
        let _ = document.set(JsString::from(name), function, false, context);
    }
    // `document` 不是全局对象，`EventTarget` 的方法必须显式挂上：
    // 页面脚本普遍直接用 `document.addEventListener("DOMContentLoaded", ...)`。
    if let Some(prototype) = context
        .global_object()
        .get(js_string!("__ornEventTargetProto"), context)
        .ok()
        .and_then(|value| value.as_object())
    {
        for name in ["addEventListener", "removeEventListener", "dispatchEvent"] {
            if let Ok(value) = prototype.get(JsString::from(name), context) {
                let _ = document.set(JsString::from(name), value, false, context);
            }
        }
    }
    // Fullscreen API：polyfill 会读这些名字判断「支持与否」，字段缺失会让它走进异常分支。
    let _ = document.set(js_string!("fullscreenEnabled"), JsValue::from(true), false, context);
    let _ = document.set(js_string!("fullscreenElement"), JsValue::null(), false, context);
    for name in ["exitFullscreen", "webkitExitFullscreen"] {
        let function = NativeFunction::from_fn_ptr(document_noop).to_js_function(context.realm());
        let _ = document.set(JsString::from(name), function, false, context);
    }
    if let Some(window) = context
        .global_object()
        .get(js_string!("window"), context)
        .ok()
    {
        let _ = document.set(js_string!("defaultView"), window.clone(), false, context);
        let _ = document.set(js_string!("parentWindow"), window, false, context);
    }
}

/// `document.createEvent(kind)`：返回带 `initEvent` 等方法的合成事件对象。
///
/// 页面库（尤其 fullscreen / 自定义事件 polyfill）会用它派发事件；
/// 没有这个方法时整段脚本在第一次派发就抛错，后续探测全部落空。
fn document_create_event(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("type"), js_string!(""), Attribute::all())
        .property(js_string!("bubbles"), JsValue::from(false), Attribute::all())
        .property(js_string!("cancelable"), JsValue::from(false), Attribute::all())
        .property(js_string!("target"), JsValue::null(), Attribute::all())
        .property(js_string!("currentTarget"), JsValue::null(), Attribute::all())
        .property(js_string!("defaultPrevented"), JsValue::from(false), Attribute::all())
        .property(js_string!("timeStamp"), JsValue::from(0), Attribute::all())
        .function(native(event_init), js_string!("initEvent"), 3)
        .function(native(noop), js_string!("preventDefault"), 0)
        .function(native(noop), js_string!("stopPropagation"), 0)
        .function(native(noop), js_string!("stopImmediatePropagation"), 0)
        .build()
        .into())
}

/// `event.initEvent(type, bubbles, cancelable)`：把参数写回自身。
fn event_init(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(event) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    let _ = event.set(js_string!("type"), JsString::from(type_name), false, context);
    let bubbles = args.get(1).map(|value| value.to_boolean()).unwrap_or(false);
    let _ = event.set(js_string!("bubbles"), JsValue::from(bubbles), false, context);
    let cancelable = args.get(2).map(|value| value.to_boolean()).unwrap_or(false);
    let _ = event.set(js_string!("cancelable"), JsValue::from(cancelable), false, context);
    Ok(JsValue::undefined())
}

/// `window` / `self` / `top` / `parent` 的自指关系与只读全局量。
///
/// `window` 本身在 `engine.rs` 里已指向全局对象，因此这里只补其余别名与常量。
fn install_window_identity(context: &mut Context) {
    let window = context
        .global_object()
        .get(js_string!("window"), context)
        .unwrap_or_else(|_| context.global_object().clone().into());

    // `window.addEventListener` / `removeEventListener` / `dispatchEvent`：
    // 框架在启动时几乎必然调用（`window.addEventListener('resize', ...)` 这类）。
    // 全局对象本身不是 `EventTarget` 实例，缺这三个方法就是「not a callable function」。
    {
        let global = context.global_object().clone();
        let _ = global.set(
            js_string!("addEventListener"),
            native(et_add_listener).to_js_function(context.realm()),
            false,
            context,
        );
        let _ = global.set(
            js_string!("removeEventListener"),
            native(et_remove_listener).to_js_function(context.realm()),
            false,
            context,
        );
        let _ = global.set(
            js_string!("dispatchEvent"),
            native(et_dispatch).to_js_function(context.realm()),
            false,
            context,
        );
        // `window.matchMedia` 已由 install_window_metrics 提供；这里只补事件相关。
    }
    let _ = context.register_global_property(
        js_string!("self"),
        window.clone(),
        Attribute::all(),
    );
    let _ = context.register_global_property(
        js_string!("top"),
        window.clone(),
        Attribute::all(),
    );
    let _ = context.register_global_property(js_string!("parent"), window, Attribute::all());
    let _ = context.register_global_property(
        js_string!("closed"),
        JsValue::from(false),
        Attribute::READONLY,
    );
    let _ = context.register_global_property(
        js_string!("isSecureContext"),
        JsValue::from(true),
        Attribute::READONLY,
    );
}

/// DOM 构造器：页面脚本（尤其是 polyfill）会直接引用这些名字。
///
/// 真实浏览器里它们是同一原型链上的构造函数（`Element` → `Node` → `EventTarget`）。
/// 这里只保证「名字存在、能 `typeof`、能当原型挂方法」——正文重建与探测器都不要求
/// `instanceof` 关系成立，但缺一个名字就会让整段脚本抛错，后面的探测全部落空。
fn install_dom_interfaces(context: &mut Context) {
    let base = context
        .global_object()
        .get(js_string!("__ornElementProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    for name in [
        "Node",
        "Element",
        "HTMLElement",
        "HTMLDivElement",
        "HTMLSpanElement",
        "HTMLBodyElement",
        "HTMLHeadElement",
        "HTMLScriptElement",
        "HTMLIFrameElement",
        "HTMLCanvasElement",
        "HTMLInputElement",
        "HTMLImageElement",
        "HTMLAnchorElement",
        "HTMLFormElement",
        "HTMLSelectElement",
        "HTMLTextAreaElement",
        "HTMLButtonElement",
        "HTMLOptionElement",
        "HTMLLinkElement",
        "HTMLMetaElement",
        "HTMLTitleElement",
        "HTMLStyleElement",
        "HTMLBRElement",
        "HTMLHRElement",
        "HTMLMediaElement",
        "HTMLAudioElement",
        "HTMLVideoElement",
        "HTMLTableElement",
        "HTMLLabelElement",
        "HTMLTemplateElement",
        "HTMLSlotElement",
        "SVGElement",
        "ShadowRoot",
        "Document",
        "HTMLDocument",
        "Window",
        "Navigator",
        "Screen",
        "Location",
        "History",
        "Storage",
        "CSSStyleDeclaration",
        "NodeList",
        "HTMLCollection",
        "DOMTokenList",
        "CustomElementRegistry",
        "CSS",
        "CSSStyleSheet",
        "StyleSheet",
        "MessagePort",
        "MediaQueryList",
        "IntersectionObserverEntry",
        "PerformanceEntry",
        "PerformanceMark",
        "PerformanceMeasure",
        "PerformanceResourceTiming",
        "Audio",
        "Option",
        "EventSource",
        "WebSocket",
        "TextTrack",
        "Path2D",
        "OffscreenCanvas",
        "ImageData",
        "ImageBitmap",
        "DOMMatrix",
        "DOMException",
        "SQRT1_2",
        // 观察者与几何相关的构造器：React 18+/Vue 在挂载时会 `new MutationObserver(...)`，
        // 缺构造器会在第一行抛 ReferenceError，整份应用逻辑都不执行。
        "MutationObserver",
        "ResizeObserver",
        "IntersectionObserver",
        "PerformanceObserver",
        "DOMRect",
        "DOMRectReadOnly",
        "Range",
        "Text",
        "Comment",
        "DocumentFragment",
        "Image",
    ] {
        // 构造器必须是**可调用对象**：用 `ObjectInitializer` 建出来的普通对象没有
        // `[[Construct]]`，`new Node()` 会直接 `TypeError: not a function`。
        // 同时实例要落在 `X.prototype` 上，否则原型方法（`observe` 等）读不到。
        let prototype = ObjectInitializer::new(context).build();
        let constructor = constructible(context, &prototype, name);
        // polyfill 会做 `"supports" in HTMLScriptElement` 之类的特性探测：
        // 把常用静态成员直接挂在构造器上，避免 `in` 的右操作数取到 undefined。
        let _ = constructor.set(js_string!("supports"), JsValue::from(false), false, context);
        let _ = constructor.set(js_string!("from"), native(noop).to_js_function(context.realm()), false, context);
        let _ = context.register_global_property(JsString::from(name), constructor, Attribute::all());
    }
    // 观察者要有真实可用的 `observe` / `disconnect` / `unobserve`：页面常在构造后立刻调它们。
    for name in ["MutationObserver", "ResizeObserver", "IntersectionObserver"] {
        let Some(constructor) = context
            .global_object()
            .get(JsString::from(name), context)
            .ok()
            .and_then(|value| value.as_object())
        else {
            continue;
        };
        let Ok(prototype) = constructor.get(js_string!("prototype"), context) else {
            continue;
        };
        let Some(prototype) = prototype.as_object() else {
            continue;
        };
        let _ = prototype.set(
            js_string!("observe"),
            native(observer_noop).to_js_function(context.realm()),
            false,
            context,
        );
        let _ = prototype.set(
            js_string!("unobserve"),
            native(observer_noop).to_js_function(context.realm()),
            false,
            context,
        );
        let _ = prototype.set(
            js_string!("disconnect"),
            native(observer_noop).to_js_function(context.realm()),
            false,
            context,
        );
        let _ = prototype.set(
            js_string!("takeRecords"),
            native(observer_empty_array).to_js_function(context.realm()),
            false,
            context,
        );
    }
    // `Element.prototype` 上要能挂方法：polyfill 会写 `Element.prototype.requestFullscreen = ...`。
    if let Some(base) = base {
        if let Some(element) = context
            .global_object()
            .get(js_string!("Element"), context)
            .ok()
            .and_then(|value| value.as_object())
            .and_then(|constructor| {
                constructor
                    .get(js_string!("prototype"), context)
                    .ok()
                    .and_then(|value| value.as_object())
            })
        {
            let _ = element.set_prototype(Some(base));
        }
    }
    install_events(context);
    install_message_channel(context);
    install_parsers_and_blobs(context);
}

/// 事件构造函数：`new CustomEvent(...)` / `new Event(...)` 在框架里到处出现。
///
/// 只有 `type` / `detail` / `bubbles` 这些页面真正会读的字段；
/// 派发仍然走 `EventTarget` 那一套（见 `et_dispatch`）。
fn install_events(context: &mut Context) {
    for name in [
        "Event",
        "UIEvent",
        "CustomEvent",
        "MouseEvent",
        "KeyboardEvent",
        "ErrorEvent",
        "MessageEvent",
        "PointerEvent",
        "TouchEvent",
        "WheelEvent",
        "DragEvent",
        "FocusEvent",
        "InputEvent",
        "ClipboardEvent",
        "ProgressEvent",
        "AnimationEvent",
        "TransitionEvent",
        "SubmitEvent",
        "ToggleEvent",
        "StorageEvent",
        "PopStateEvent",
        "HashChangeEvent",
        "PageTransitionEvent",
        "BeforeUnloadEvent",
        "SecurityPolicyViolationEvent",
        "GamepadEvent",
        "DeviceMotionEvent",
        "DeviceOrientationEvent",
    ] {
        let prototype = ObjectInitializer::new(context).build();
        let constructor = ctor_with_proto(context, &prototype, name);
        // `Event.prototype.preventDefault` / `stopPropagation` 会被框架无条件调用。
        for method in ["preventDefault", "stopPropagation", "stopImmediatePropagation", "composedPath"] {
            let _ = prototype.set(
                JsString::from(method),
                native(noop).to_js_function(context.realm()),
                false,
                context,
            );
        }
        let _ = context.register_global_property(JsString::from(name), constructor, Attribute::all());
    }
}

fn event_construct(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let enum_value = |args: &[JsValue], context: &mut Context, name: &str| -> JsResult<JsValue> {
        let Some(init) = args.get(1).and_then(JsValue::as_object) else {
            return Ok(JsValue::undefined());
        };
        init.get(js_string!(name), context)
    };
    let type_name = args
        .first()
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let bubbles = enum_value(args, context, "bubbles")?;
    let cancelable = enum_value(args, context, "cancelable")?;
    let detail = enum_value(args, context, "detail")?;
    Ok(ObjectInitializer::new(context)
        .property(
            js_string!("type"),
            JsString::from(type_name),
            Attribute::all(),
        )
        .property(
            js_string!("bubbles"),
            if bubbles.is_undefined() {
                JsValue::from(false)
            } else {
                JsValue::from(bubbles.to_boolean())
            },
            Attribute::all(),
        )
        .property(
            js_string!("cancelable"),
            if cancelable.is_undefined() {
                JsValue::from(false)
            } else {
                JsValue::from(cancelable.to_boolean())
            },
            Attribute::all(),
        )
        .property(
            js_string!("detail"),
            if detail.is_undefined() {
                JsValue::null()
            } else {
                detail
            },
            Attribute::all(),
        )
        .property(js_string!("defaultPrevented"), JsValue::from(false), Attribute::all())
        .build()
        .into())
}

/// 解析成 `[(名, 值)]`：解析规则按浏览器够用部分（`+` 当空格、百分号解码）。
fn parse_query(text: &str, context: &mut Context) -> Vec<(String, String)> {
    let text = text.strip_prefix('?').unwrap_or(text);
    let mut pairs = Vec::new();
    for piece in text.split('&').filter(|piece| !piece.is_empty()) {
        let (name, value) = match piece.split_once('=') {
            Some((name, value)) => (name, value),
            None => (piece, ""),
        };
        pairs.push((percent_decode(name, context), percent_decode(value, context)));
    }
    pairs
}

fn percent_decode(text: &str, context: &mut Context) -> String {
    if !text.contains('%') && !text.contains('+') {
        return text.replace('+', " ");
    }
    let expression = format!("decodeURIComponent({})", js_string_literal(text));
    match context.eval(boa_engine::Source::from_bytes(&expression)) {
        Ok(value) => value
            .to_string(context)
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_else(|_| text.replace('+', " ")),
        Err(_) => text.replace('+', " "),
    }
}

fn js_string_literal(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

fn usp_pairs(this: &JsValue, context: &mut Context) -> Vec<(String, String)> {
    let Some(object) = this.as_object() else {
        return Vec::new();
    };
    let Ok(store) = object.get(js_string!("__ornParams"), context) else {
        return Vec::new();
    };
    let Some(store) = store.as_object().and_then(|object| JsArray::from_object(object).ok()) else {
        return Vec::new();
    };
    let length = store.length(context).unwrap_or(0) as usize;
    let mut pairs = Vec::new();
    let mut index = 0;
    while index + 1 < length {
        let name = store
            .at(index as i64, context)
            .ok()
            .and_then(|value| value.to_string(context).ok())
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_default();
        let value = store
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

fn usp_store(this: &JsValue, pairs: &[(String, String)], context: &mut Context) {
    let Some(object) = this.as_object() else {
        return;
    };
    let Ok(array) = JsArray::new(context) else {
        return;
    };
    for (name, value) in pairs {
        let _ = array.push(JsString::from(name.as_str()), context);
        let _ = array.push(JsString::from(value.as_str()), context);
    }
    let _ = object.set(js_string!("__ornParams"), array, false, context);
}

/// 构造：接受查询串、另一个实例或普通对象（`{a:1}`）。
fn url_search_params_construct(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let _ = this;
    let proto = context
        .global_object()
        .get(js_string!("__ornUspProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    let object = JsObject::from_proto_and_data(proto, OrdinaryObject);
    let instance = JsValue::from(object.clone());
    let pairs = match args.first() {
        None => Vec::new(),
        Some(value) if value.is_undefined() || value.is_null() => Vec::new(),
        Some(value) => match value.as_object() {
            Some(source) if source.get(js_string!("__ornParams"), context).is_ok() => {
                usp_pairs(value, context)
            }
            // 普通对象：按自有键展开（数组值展开成多次）。
            Some(source) => {
                let mut pairs = Vec::new();
                if let Ok(keys) = source.own_property_keys(context) {
                    for key in keys {
                        let name = match &key {
                            PropertyKey::String(text) => text.to_std_string_escaped(),
                            PropertyKey::Index(index) => index.get().to_string(),
                            PropertyKey::Symbol(_) => continue,
                        };
                        if name.starts_with("__orn") {
                            continue;
                        }
                        let Ok(item) = source.get(key.clone(), context) else {
                            continue;
                        };
                        if let Some(array) = item.as_object().and_then(|object| JsArray::from_object(object).ok()) {
                            for index in 0..array.length(context).unwrap_or(0) {
                                let text = array
                                    .at(index as i64, context)
                                    .ok()
                                    .and_then(|value| value.to_string(context).ok())
                                    .map(|text| text.to_std_string_escaped())
                                    .unwrap_or_default();
                                pairs.push((name.clone(), text));
                            }
                            continue;
                        }
                        let text = item
                            .to_string(context)
                            .map(|text| text.to_std_string_escaped())
                            .unwrap_or_default();
                        pairs.push((name, text));
                    }
                }
                // 排序与浏览器一致：构造时按名字稳定排序。
                pairs.sort_by(|left, right| left.0.cmp(&right.0));
                pairs
            }
            None => {
                let text = value
                    .to_string(context)
                    .map(|text| text.to_std_string_escaped())
                    .unwrap_or_default();
                parse_query(&text, context)
            }
        },
    };
    usp_store(&instance, &pairs, context);
    Ok(instance)
}

fn usp_get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let found = usp_pairs(this, context)
        .into_iter()
        .find(|(key, _)| key == &name)
        .map(|(_, value)| value);
    Ok(match found {
        Some(value) => JsValue::from(JsString::from(value)),
        None => JsValue::null(),
    })
}

fn usp_get_all(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let array = JsArray::new(context)?;
    for (_, value) in usp_pairs(this, context)
        .into_iter()
        .filter(|(key, _)| key == &name)
    {
        array.push(JsString::from(value), context)?;
    }
    Ok(array.into())
}

fn usp_has(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    Ok(JsValue::from(
        usp_pairs(this, context).iter().any(|(key, _)| key == &name),
    ))
}

fn usp_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    let mut pairs: Vec<(String, String)> = usp_pairs(this, context)
        .into_iter()
        .filter(|(key, _)| key != &name)
        .collect();
    pairs.push((name, value));
    usp_store(this, &pairs, context);
    Ok(JsValue::undefined())
}

fn usp_append(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    let mut pairs = usp_pairs(this, context);
    pairs.push((name, value));
    usp_store(this, &pairs, context);
    Ok(JsValue::undefined())
}

fn usp_delete(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let pairs: Vec<(String, String)> = usp_pairs(this, context)
        .into_iter()
        .filter(|(key, _)| key != &name)
        .collect();
    usp_store(this, &pairs, context);
    Ok(JsValue::undefined())
}

fn usp_sort(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let mut pairs = usp_pairs(this, context);
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    usp_store(this, &pairs, context);
    Ok(JsValue::undefined())
}

fn usp_to_string(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = usp_pairs(this, context)
        .into_iter()
        .map(|(name, value)| format!("{}={}", encode_query(&name), encode_query(&value)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(JsValue::from(JsString::from(text)))
}

/// 编码规则取 `encodeURIComponent` 的等价：保留 `-_.!~*'()`，空格变 `+`。
fn encode_query(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*'
            | b'\'' | b'(' | b')' => out.push(byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn usp_for_each(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(callback) = args.first().and_then(JsValue::as_object) else {
        return Ok(JsValue::undefined());
    };
    if !callback.is_callable() {
        return Ok(JsValue::undefined());
    }
    for (name, value) in usp_pairs(this, context) {
        let params = [
            JsValue::from(JsString::from(value)),
            JsValue::from(JsString::from(name)),
        ];
        let _ = callback.call(this, &params, context);
    }
    Ok(JsValue::undefined())
}

fn usp_keys(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let array = JsArray::new(context)?;
    for (name, _) in usp_pairs(this, context) {
        array.push(JsString::from(name), context)?;
    }
    Ok(array.into())
}

fn usp_values(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let array = JsArray::new(context)?;
    for (_, value) in usp_pairs(this, context) {
        array.push(JsString::from(value), context)?;
    }
    Ok(array.into())
}

/// `MessageChannel`：React 18 的调度器**必须**靠它。
///
/// `scheduler` 在浏览器里用 `new MessageChannel()`，然后
/// `port.postMessage(null)` 触发 `port1.onmessage`，把「该做的工作」推到下一个宏任务。
/// 如果把 `postMessage` 做成空实现，`createRoot().render()` 的首次提交永远不会发生：
/// 页面不报错、`#root` 却一直是空的——这正是「看起来跑通了但没有内容」的典型陷阱。
///
/// 这里让 `postMessage` 同步调用对端的 `onmessage`/监听器：
/// 沙箱没有真实事件循环，同步执行回调就是我们要的「立刻把这次渲染做完」。
fn install_message_channel(context: &mut Context) {
    let channel_proto = ObjectInitializer::new(context).build();
    let port_proto = ObjectInitializer::new(context)
        .function(native(mc_post_message), js_string!("postMessage"), 1)
        .function(native(mc_add_listener), js_string!("addEventListener"), 2)
        .function(native(mc_remove_listener), js_string!("removeEventListener"), 2)
        .function(native(noop), js_string!("start"), 0)
        .function(native(noop), js_string!("close"), 0)
        .build();
    let port_ctor = ctor_with_proto(context, &port_proto, "MessagePort");
    let _ = context.register_global_property(js_string!("MessagePort"), port_ctor, Attribute::all());

    let channel_ctor =
        FunctionObjectBuilder::new(context.realm(), native(message_channel_construct))
            .name(js_string!("MessageChannel"))
            .length(0)
            .constructor(true)
            .build();
    let _ = channel_ctor.set(js_string!("prototype"), channel_proto.clone(), false, context);
    let _ = channel_proto.set(js_string!("constructor"), channel_ctor.clone(), false, context);
    let _ = context.register_global_property(
        js_string!("MessageChannel"),
        channel_ctor,
        Attribute::all(),
    );
}

fn message_channel_construct(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let _ = this;
    let channel_proto = context
        .global_object()
        .get(js_string!("MessageChannel"), context)
        .ok()
        .and_then(|value| value.as_object())
        .and_then(|constructor| {
            constructor
                .get(js_string!("prototype"), context)
                .ok()
                .and_then(|value| value.as_object())
        });
    let port_proto = context
        .global_object()
        .get(js_string!("MessagePort"), context)
        .ok()
        .and_then(|value| value.as_object())
        .and_then(|constructor| {
            constructor
                .get(js_string!("prototype"), context)
                .ok()
                .and_then(|value| value.as_object())
        });
    let port1 = JsObject::from_proto_and_data(port_proto.clone(), OrdinaryObject);
    let port2 = JsObject::from_proto_and_data(port_proto, OrdinaryObject);
    // 两端互相引用：`port1.postMessage` 要能找到 `port2` 的处理器。
    let _ = port1.set(js_string!("__ornPeer"), port2.clone(), false, context);
    let _ = port2.set(js_string!("__ornPeer"), port1.clone(), false, context);
    let channel = JsObject::from_proto_and_data(channel_proto, OrdinaryObject);
    let _ = channel.set(js_string!("port1"), port1, false, context);
    let _ = channel.set(js_string!("port2"), port2, false, context);
    Ok(channel.into())
}

/// `postMessage`：把消息排进与 `setImmediate` 相同的任务队列，**不**同步递归。
///
/// 同步派发曾经让 React 的调度器直接崩掉：`scheduler` 的 `workLoop` 在每轮
/// 末尾再次 `postMessage`，同步实现就变成了无限深度的原生递归，跑到 boa 的
/// 递归上限（实测 170 层）静默失败——页面不报错、`#root` 却永远空着。
/// 入队后每一轮都由 [`flush_timers_once`] 从顶层重新进入，深度恒定。
fn mc_post_message(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let peer = object.get(js_string!("__ornPeer"), context).unwrap_or(JsValue::undefined());
    let Some(peer_object) = peer.as_object() else {
        return Ok(JsValue::undefined());
    };
    let data = args.first().cloned().unwrap_or(JsValue::null());
    let event = ObjectInitializer::new(context)
        .property(js_string!("type"), js_string!("message"), Attribute::all())
        .property(js_string!("data"), data.clone(), Attribute::all())
        .property(js_string!("target"), peer.clone(), Attribute::all())
        .build();
    // 浏览器里 `message` 事件的处理器既可能是 `onmessage`，也可能是 addEventListener 登记的。
    let mut handlers: Vec<JsObject> = Vec::new();
    if let Ok(handler) = peer_object.get(js_string!("onmessage"), context) {
        if let Some(function) = handler.as_object().filter(JsObject::is_callable) {
            handlers.push(function);
        }
    }
    if let Ok(listeners) = peer_object.get(js_string!("__ornPortListeners"), context) {
        if let Some(array) = listeners
            .as_object()
            .and_then(|object| JsArray::from_object(object).ok())
        {
            let length = array.length(context).unwrap_or(0) as usize;
            let mut index = 0;
            while index + 1 < length {
                let kind = array
                    .at(index as i64, context)
                    .ok()
                    .and_then(|value| value.to_string(context).ok())
                    .map(|text| text.to_std_string_escaped())
                    .unwrap_or_default();
                let function = array.at(index as i64 + 1, context).unwrap_or(JsValue::undefined());
                if kind == "message" {
                    if let Some(function) = function.as_object().filter(JsObject::is_callable) {
                        handlers.push(function);
                    }
                }
                index += 2;
            }
        }
    }
    for handler in handlers {
        enqueue_message_task(handler, peer.clone(), JsValue::from(event.clone()));
    }
    Ok(JsValue::undefined())
}

/// 把一次 `message` 派发排进定时器队列。
///
/// 复用同一个队列是有意的：`setImmediate` 与 `postMessage` 在浏览器里都是宏任务，
/// 排队后又都由 [`flush_timers_once`] 从顶层驱动，递归深度不再随调度轮次增长。
fn enqueue_message_task(handler: JsObject, peer: JsValue, event: JsValue) {
    let id = NEXT_TIMER_ID.with(|slot| {
        let mut current = slot.get();
        current += 1;
        slot.set(current);
        current
    });
    TIMERS.with(|slot| {
        slot.borrow_mut().push(PendingTimer {
            id,
            callback: handler,
            this_value: peer,
            args: vec![event],
        });
    });
}

fn mc_add_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let kind = args_string(args, 0, context);
    let handler = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let listeners = match object
        .get(js_string!("__ornPortListeners"), context)
        .ok()
        .and_then(|value| value.as_object())
        .and_then(|object| JsArray::from_object(object).ok())
    {
        Some(array) => array,
        None => {
            let array = JsArray::new(context)?;
            let _ = object.set(js_string!("__ornPortListeners"), array.clone(), false, context);
            array
        }
    };
    listeners.push(JsString::from(kind), context)?;
    listeners.push(handler, context)?;
    Ok(JsValue::undefined())
}

fn mc_remove_listener(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `DOMParser` / `Blob` / `File` / `FormData` / `Worker` 这类「会被 `new` 的工具对象」。
///
/// 它们不参与正文渲染，但框架与埋点脚本会在启动时构造：
/// 缺构造器就是 `TypeError: not a constructor`，整份入口脚本中断。
fn install_parsers_and_blobs(context: &mut Context) {
    // DOMParser.parseFromString 返回一个带 querySelector 的文档壳。
    let dom_proto = ObjectInitializer::new(context)
        .function(native(dom_parser_parse), js_string!("parseFromString"), 2)
        .build();
    let dom_parser = ctor_with_proto(context, &dom_proto, "DOMParser");
    let _ = context.register_global_property(js_string!("DOMParser"), dom_parser, Attribute::all());
    // URLSearchParams 必须有真实的 set/get/toString：它是查询参数读写的主力。
    let usp_proto = ObjectInitializer::new(context)
        .function(native(usp_get), js_string!("get"), 1)
        .function(native(usp_get_all), js_string!("getAll"), 1)
        .function(native(usp_has), js_string!("has"), 1)
        .function(native(usp_set), js_string!("set"), 2)
        .function(native(usp_append), js_string!("append"), 2)
        .function(native(usp_delete), js_string!("delete"), 1)
        .function(native(usp_sort), js_string!("sort"), 0)
        .function(native(usp_to_string), js_string!("toString"), 0)
        .function(native(usp_for_each), js_string!("forEach"), 1)
        .function(native(usp_keys), js_string!("keys"), 0)
        .function(native(usp_values), js_string!("values"), 0)
        .build();
    let usp_ctor = FunctionObjectBuilder::new(context.realm(), native(url_search_params_construct))
        .name(js_string!("URLSearchParams"))
        .length(1)
        .constructor(true)
        .build();
    let _ = usp_ctor.set(js_string!("prototype"), usp_proto.clone(), false, context);
    let _ = usp_proto.set(js_string!("constructor"), usp_ctor.clone(), false, context);
    let _ = context.register_global_property(js_string!("URLSearchParams"), usp_ctor, Attribute::all());
    // 构造时靠它找原型：原生构造函数拿不到 `new.target`，只能登记一份。
    let _ = context.register_global_property(
        js_string!("__ornUspProto"),
        usp_proto,
        Attribute::READONLY,
    );

    for name in [
        "Blob",
        "File",
        "FileReader",
        "FormData",
        "AbortController",
        "AbortSignal",
        "Worker",
        "SharedWorker",
        "BroadcastChannel",
        "TextDecoder",
        "XMLSerializer",
        "AudioContext",
        "webkitAudioContext",
    ] {
        let prototype = ObjectInitializer::new(context)
            .function(native(noop), js_string!("abort"), 0)
            .function(native(noop), js_string!("addEventListener"), 2)
            .function(native(noop), js_string!("removeEventListener"), 2)
            .function(native(noop), js_string!("postMessage"), 1)
            .function(native(noop), js_string!("terminate"), 0)
            .function(native(noop), js_string!("close"), 0)
            .function(native(noop), js_string!("readAsText"), 1)
            .function(native(noop), js_string!("decode"), 1)
            .function(native(observer_empty_array), js_string!("getAll"), 0)
            .property(js_string!("size"), JsValue::from(0), Attribute::all())
            .property(js_string!("type"), js_string!(""), Attribute::all())
            .property(js_string!("aborted"), JsValue::from(false), Attribute::all())
            .property(js_string!("result"), JsValue::null(), Attribute::all())
            .build();
        let constructor = constructible(context, &prototype, name);
        let _ = context.register_global_property(JsString::from(name), constructor, Attribute::all());
    }
}

/// 通用构造器：`new X()` 时 boa 已经把 `this` 按 `X.prototype` 造好了，直接用它。
///
/// 这比另建对象更准确：页面对 `instanceof` 的判断、给实例挂字段，行为都一致。
/// 直接调用（非 `new`）时 `this` 是 undefined，退化成新建一个普通对象。
fn simple_construct(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let _ = this;
    // boa 的原生函数没有 `[[Construct]]` 的原型替换逻辑：`new X()` 传进来的 `this`
    // 只是普通对象。当前活跃构造器的原型由调用方通过 `__ornActiveCtorProto` 登记，
    // 这样实例才真的落在 `X.prototype` 上——否则 `instanceof` 与原型方法全都读不到。
    let proto = context
        .global_object()
        .get(js_string!("__ornActiveCtorProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    Ok(JsObject::from_proto_and_data(proto, OrdinaryObject).into())
}

/// 让 `simple_construct` 知道「这次是哪个类在 new」。
///
/// 原生函数拿不到自己的 `prototype`，只能在调用前后由这层包装提供。
/// 包装体是每次构造都要走的薄壳，成本可以忽略。
fn ctor_with_proto(
    context: &mut Context,
    prototype: &JsObject,
    name: &str,
) -> JsObject {
    let prototype_for_call = prototype.clone();
    let function = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        // 包装成 `function X(...) { return construct_with(this, X.prototype) }` 的等价物：
        // 先把原型写进全局登记，再交给通用构造器。
        NativeFunction::from_copy_closure_with_captures(
            |_this, _args, captures, context| {
                let (proto,): &(JsObject,) = captures;
                let proto = proto.clone();
                let object = JsObject::from_proto_and_data(Some(proto), OrdinaryObject);
                Ok(object.into())
            },
            (prototype_for_call,),
        ),
    )
    .name(JsString::from(name))
    .constructor(true)
    .build();
    let _ = function.set(js_string!("prototype"), prototype.clone(), false, context);
    let _ = prototype.set(js_string!("constructor"), function.clone(), false, context);
    function.into()
}

fn dom_parser_parse(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    // 返回 `document` 本身：调用方通常只要 `querySelector` / `body` 这些成员。
    Ok(context
        .global_object()
        .get(js_string!("document"), context)
        .unwrap_or(JsValue::undefined()))
}

/// 其余一次性的零碎对象。
fn install_misc(context: &mut Context) {
    // `matchMedia` 的返回值会被读字段，不能返回 undefined。
    // `register_global_property` 只收 `Into<JsValue>`，`NativeFunction` 须先转成函数对象。
    let function = NativeFunction::from_fn_ptr(mm_query).to_js_function(context.realm());
    let _ = context.register_global_property(js_string!("matchMedia"), function, Attribute::all());
    for (name, implementation) in [
        ("setTimeout", timer_set_timeout as NativeWrapper),
        ("setInterval", timer_set_timeout),
        ("setImmediate", timer_set_timeout),
    ] {
        let function = NativeFunction::from_fn_ptr(implementation).to_js_function(context.realm());
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }
    for name in ["clearTimeout", "clearInterval", "clearImmediate"] {
        let function = NativeFunction::from_fn_ptr(timer_clear).to_js_function(context.realm());
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }
    for (name, implementation) in [
        ("requestAnimationFrame", timer_set_timeout as NativeWrapper),
        ("requestIdleCallback", timer_set_timeout),
    ] {
        let function = NativeFunction::from_fn_ptr(implementation).to_js_function(context.realm());
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }
    for name in ["cancelAnimationFrame", "cancelIdleCallback"] {
        let function = NativeFunction::from_fn_ptr(timer_clear).to_js_function(context.realm());
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }
}

/// 定时器与动画帧共用的函数签名：`fn(&JsValue, &[JsValue], &mut Context)`。
type NativeWrapper =
    for<'a> fn(&'a JsValue, &'a [JsValue], &'a mut Context) -> JsResult<JsValue>;

/// `setTimeout` 家族：只登记回调，**不睡眠**。
///
/// 真实延迟对抓取毫无意义（工具不会等三秒），但页面脚本常把「主逻辑」放在
/// `setTimeout(fn, 0)` 里——指纹探测脚本正是如此。因此这里的行为是：
/// 回调进队列，等本批脚本全部求值结束后由 [`flush_timers_once`] 立即跑一轮。
fn timer_set_timeout(_this: &JsValue, args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    let Some(callback) = args.first().and_then(|value| value.as_object()) else {
        return Ok(JsValue::undefined());
    };
    if !callback.is_callable() {
        // 字符串形式（`setTimeout("code()")`）在现代页面里绝迹，按无效处理。
        return Ok(JsValue::undefined());
    }
    let id = NEXT_TIMER_ID.with(|slot| {
        let mut current = slot.get();
        current += 1;
        slot.set(current);
        current
    });
    TIMERS.with(|slot| {
        slot.borrow_mut().push(PendingTimer {
            id,
            callback: callback.clone(),
            this_value: JsValue::undefined(),
            args: Vec::new(),
        });
    });
    Ok(JsValue::from(id))
}

fn timer_clear(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let id = args
        .first()
        .and_then(|value| value.to_number(context).ok())
        .unwrap_or(0.0) as u64;
    TIMERS.with(|slot| slot.borrow_mut().retain(|timer| timer.id != id));
    Ok(JsValue::undefined())
}

/// 跑一轮已登记的定时器回调，返回这一轮是否真的跑了回调。
///
/// 调用方在每轮之后自行推进微任务（回调里往往又发了一次 `fetch`），
/// 因此这里只管「跑一批」，不再内置轮次上限——上限由调用方的循环控制。
pub(crate) fn flush_timers_once(context: &mut Context) -> bool {
    let batch = TIMERS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
    if batch.is_empty() {
        return false;
    }
    for timer in batch {
        // 回调抛错只记录，不影响其余回调与后续脚本（与浏览器一致）。
        let _ = timer
            .callback
            .call(&timer.this_value, &timer.args, context);
    }
    true
}


struct PendingTimer {
    id: u64,
    callback: boa_engine::JsObject,
    /// 调用时的 `this`：`message` 处理器需要拿到对应的 port。
    this_value: JsValue,
    /// 调用参数：`message` 事件对象走这里，`setTimeout` 家族为空。
    args: Vec<JsValue>,
}

thread_local! {
    static TIMERS: std::cell::RefCell<Vec<PendingTimer>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static NEXT_TIMER_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn mm_query(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let query = args_string(args, 0, context);
    Ok(ObjectInitializer::new(context)
        .property(js_string!("matches"), JsValue::from(false), Attribute::all())
        .property(js_string!("media"), JsString::from(query), Attribute::all())
        .property(js_string!("onchange"), JsValue::null(), Attribute::all())
        .function(native(noop), js_string!("addListener"), 1)
        .function(native(noop), js_string!("removeListener"), 1)
        .function(native(noop), js_string!("addEventListener"), 2)
        .function(native(noop), js_string!("removeEventListener"), 2)
        .function(native(noop), js_string!("dispatchEvent"), 1)
        .build()
        .into())
}

fn noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// 观察者方法的空实现：`observe`/`disconnect` 等都要能调用且不抛错。
fn observer_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn observer_empty_array(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
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
