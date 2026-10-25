//! 由 `browser_env.rs` 按假装的全局对象拆出，语义未变。

use super::*;
use super::codec::{split_url};






pub(crate) fn get_computed_style(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // 没有布局引擎：给出「元素可见」这一最常见前提下的取值。
    // `display: none` 的判定依赖真实样式表，沙箱里做不到，这一点在文档中声明。
    Ok(ObjectInitializer::new(context)
        .property(js_string!("display"), js_string!("block"), Attribute::all())
        .property(
            js_string!("visibility"),
            js_string!("visible"),
            Attribute::all(),
        )
        .property(js_string!("opacity"), js_string!("1"), Attribute::all())
        .property(
            js_string!("position"),
            js_string!("static"),
            Attribute::all(),
        )
        .property(js_string!("height"), js_string!("0px"), Attribute::all())
        .property(js_string!("width"), js_string!("0px"), Attribute::all())
        .property(
            js_string!("overflow"),
            js_string!("visible"),
            Attribute::all(),
        )
        .function(native(noop), js_string!("getPropertyValue"), 1)
        .build()
        .into())
}

/// `window.getSelection()`：返回一个「什么都没选中」的选区对象。
pub(crate) fn get_selection(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("rangeCount"), JsValue::from(0), Attribute::all())
        .property(js_string!("type"), js_string!("None"), Attribute::all())
        .property(
            js_string!("isCollapsed"),
            JsValue::from(true),
            Attribute::all(),
        )
        .function(native(text_returning_empty), js_string!("toString"), 0)
        .function(native(noop), js_string!("removeAllRanges"), 0)
        .function(native(noop), js_string!("addRange"), 1)
        .function(native(noop), js_string!("collapse"), 0)
        .function(native(observer_empty_array), js_string!("getRangeAt"), 1)
        .build()
        .into())
}

pub(crate) fn text_returning_empty(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!("")))
}

pub(crate) fn custom_elements_get(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

pub(crate) fn install_location(context: &mut Context, profile: &Profile) {
    // 指向真实抓取地址：写死成 example.com 是页面一眼能看出的破绽。
    let (origin, protocol, host, pathname) = split_url(&profile.href);
    let location = ObjectInitializer::new(context)
        .property(
            js_string!("href"),
            JsString::from(profile.href.clone()),
            Attribute::all(),
        )
        .property(
            js_string!("origin"),
            JsString::from(origin),
            Attribute::all(),
        )
        .property(
            js_string!("protocol"),
            JsString::from(protocol),
            Attribute::all(),
        )
        .property(
            js_string!("host"),
            JsString::from(host.clone()),
            Attribute::all(),
        )
        .property(
            js_string!("hostname"),
            JsString::from(host),
            Attribute::all(),
        )
        .property(js_string!("port"), js_string!(""), Attribute::all())
        .property(
            js_string!("pathname"),
            JsString::from(pathname),
            Attribute::all(),
        )
        .property(js_string!("search"), js_string!(""), Attribute::all())
        .property(js_string!("hash"), js_string!(""), Attribute::all())
        .function(native(noop), js_string!("assign"), 1)
        .function(native(noop), js_string!("replace"), 1)
        .function(native(noop), js_string!("reload"), 0)
        .build();
    let _ = context.register_global_property(js_string!("location"), location, Attribute::all());
}

pub(crate) fn install_history(context: &mut Context) {
    let history = ObjectInitializer::new(context)
        .property(js_string!("length"), JsValue::from(1), Attribute::all())
        .property(js_string!("state"), JsValue::null(), Attribute::all())
        .property(
            js_string!("scrollRestoration"),
            js_string!("auto"),
            Attribute::all(),
        )
        .function(native(noop), js_string!("pushState"), 3)
        .function(native(noop), js_string!("replaceState"), 3)
        .function(native(noop), js_string!("back"), 0)
        .function(native(noop), js_string!("forward"), 0)
        .function(native(noop), js_string!("go"), 1)
        .build();
    let _ = context.register_global_property(js_string!("history"), history, Attribute::all());
}

pub(crate) fn install_performance(context: &mut Context) {
    let performance = ObjectInitializer::new(context)
        .function(native(perf_now), js_string!("now"), 0)
        .function(native(noop), js_string!("mark"), 1)
        .function(native(noop), js_string!("measure"), 1)
        .function(native(perf_timing), js_string!("getEntriesByType"), 1)
        .property(js_string!("timeOrigin"), JsValue::from(0), Attribute::all())
        .build();
    let _ =
        context.register_global_property(js_string!("performance"), performance, Attribute::all());
}

pub(crate) fn perf_now(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    // 固定值而非真实时钟：页面拿它做「是否被拖慢」的探测时结论稳定。
    Ok(JsValue::from(12.5))
}

pub(crate) fn perf_timing(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
}

/// `Date.prototype.toLocaleString` / `toLocaleDateString` / `toLocaleTimeString`。
///
/// boa 未启用 `intl` feature（需要 ICU），这三个方法会抛「Function Unimplemented」。
/// 页面脚本用它们拼时间字符串，不补就会在渲染时间那一栏时中断整段脚本。
pub(crate) fn install_date_formatting(context: &mut Context) {
    for name in ["toLocaleString", "toLocaleDateString", "toLocaleTimeString"] {
        let function =
            NativeFunction::from_fn_ptr(date_locale_string).to_js_function(context.realm());
        let _ = context.register_global_property(
            JsString::from(format!("__ornDate_{name}")),
            function,
            Attribute::READONLY,
        );
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
        for name in ["toLocaleString", "toLocaleDateString", "toLocaleTimeString"] {
            let function =
                NativeFunction::from_fn_ptr(date_locale_string).to_js_function(context.realm());
            let _ = prototype.set(JsString::from(name), function, false, context);
        }
    }
}

/// 固定格式的本地时间：`YYYY/MM/DD HH:MM:SS`（Windows 中文环境下的 Chrome 形态）。
pub(crate) fn date_locale_string(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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
pub(crate) fn document_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// 返回 rejected Promise：页面脚本的 `.catch` 分支会正常走到。
pub(crate) fn network_unavailable(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // `from_opaque` 把任意 JS 值包装成错误载荷：reject 原因仍是那个字符串。
    let message = JsValue::from(JsString::from(
        "Network access is not available in this sandbox",
    ));
    Ok(JsPromise::reject(boa_engine::JsError::from_opaque(message), context)?.into())
}

/// `document.URL` / `document.referrer` / `document.readyState` 等：常被一起读。
pub(crate) fn install_document_state(context: &mut Context, profile: &Profile) {
    let Some(document) = context
        .global_object()
        .get(js_string!("document"), context)
        .ok()
        .and_then(|value| value.as_object())
    else {
        return;
    };
    let (origin, _protocol, _host, _pathname) = split_url(&profile.href);
    let _ = document.set(
        js_string!("URL"),
        JsString::from(profile.href.clone()),
        false,
        context,
    );
    let _ = document.set(
        js_string!("documentURI"),
        JsString::from(profile.href.clone()),
        false,
        context,
    );
    let _ = document.set(js_string!("referrer"), js_string!(""), false, context);
    let _ = document.set(js_string!("origin"), JsString::from(origin), false, context);
    // 抓到的页面在真实浏览器里已经加载完毕，因此一定是 complete。
    let _ = document.set(
        js_string!("readyState"),
        js_string!("complete"),
        false,
        context,
    );
    let _ = document.set(js_string!("hidden"), JsValue::from(false), false, context);
    let _ = document.set(
        js_string!("visibilityState"),
        js_string!("visible"),
        false,
        context,
    );
    let _ = document.set(js_string!("cookie"), js_string!(""), false, context);
    let _ = document.set(
        js_string!("characterSet"),
        js_string!("UTF-8"),
        false,
        context,
    );
    let _ = document.set(
        js_string!("compatMode"),
        js_string!("CSS1Compat"),
        false,
        context,
    );

    // 事件构造与默认视图：fullscreen polyfill 等库会直接调用它们。
    for name in ["createEvent", "createEventObject"] {
        let function =
            NativeFunction::from_fn_ptr(document_create_event).to_js_function(context.realm());
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
    let _ = document.set(
        js_string!("fullscreenEnabled"),
        JsValue::from(true),
        false,
        context,
    );
    let _ = document.set(
        js_string!("fullscreenElement"),
        JsValue::null(),
        false,
        context,
    );
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
pub(crate) fn document_create_event(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("type"), js_string!(""), Attribute::all())
        .property(
            js_string!("bubbles"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(
            js_string!("cancelable"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(js_string!("target"), JsValue::null(), Attribute::all())
        .property(
            js_string!("currentTarget"),
            JsValue::null(),
            Attribute::all(),
        )
        .property(
            js_string!("defaultPrevented"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(js_string!("timeStamp"), JsValue::from(0), Attribute::all())
        .function(native(event_init), js_string!("initEvent"), 3)
        .function(native(noop), js_string!("preventDefault"), 0)
        .function(native(noop), js_string!("stopPropagation"), 0)
        .function(native(noop), js_string!("stopImmediatePropagation"), 0)
        .build()
        .into())
}

/// `event.initEvent(type, bubbles, cancelable)`：把参数写回自身。
pub(crate) fn event_init(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(event) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    let _ = event.set(
        js_string!("type"),
        JsString::from(type_name),
        false,
        context,
    );
    let bubbles = args.get(1).map(|value| value.to_boolean()).unwrap_or(false);
    let _ = event.set(
        js_string!("bubbles"),
        JsValue::from(bubbles),
        false,
        context,
    );
    let cancelable = args.get(2).map(|value| value.to_boolean()).unwrap_or(false);
    let _ = event.set(
        js_string!("cancelable"),
        JsValue::from(cancelable),
        false,
        context,
    );
    Ok(JsValue::undefined())
}

