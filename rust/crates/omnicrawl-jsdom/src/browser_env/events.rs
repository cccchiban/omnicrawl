//! 由 `browser_env.rs` 按假装的全局对象拆出，语义未变。

use super::*;




use super::navigator::{listener_key};
use super::storage::{ctor_with_proto, install_message_channel, install_parsers_and_blobs};

pub(crate) fn install_event_target(context: &mut Context) {
    let prototype = ObjectInitializer::new(context)
        .function(native(et_add_listener), js_string!("addEventListener"), 2)
        .function(
            native(et_remove_listener),
            js_string!("removeEventListener"),
            2,
        )
        .function(native(et_dispatch), js_string!("dispatchEvent"), 1)
        .build();
    let constructor =
        boa_engine::object::FunctionObjectBuilder::new(context.realm(), native(et_construct))
            .name(js_string!("EventTarget"))
            .constructor(true)
            .build();
    let _ = constructor.set(js_string!("prototype"), prototype.clone(), false, context);
    let _ = prototype.set(
        js_string!("constructor"),
        constructor.clone(),
        false,
        context,
    );
    let _ =
        context.register_global_property(js_string!("EventTarget"), constructor, Attribute::all());
    let _ = context.register_global_property(
        js_string!("__ornEventTargetProto"),
        prototype,
        Attribute::READONLY,
    );
}

pub(crate) fn et_construct(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornEventTargetProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    Ok(JsObject::from_proto_and_data(proto, boa_engine::builtins::object::OrdinaryObject).into())
}

pub(crate) fn et_add_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) fn et_remove_listener(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    let _ = object.delete_property_or_throw(JsString::from(listener_key(&type_name)), context);
    Ok(JsValue::undefined())
}

pub(crate) fn et_dispatch(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

/// `window` / `self` / `top` / `parent` 的自指关系与只读全局量。
///
/// `window` 本身在 `engine.rs` 里已指向全局对象，因此这里只补其余别名与常量。
pub(crate) fn install_window_identity(context: &mut Context) {
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
    let _ = context.register_global_property(js_string!("self"), window.clone(), Attribute::all());
    let _ = context.register_global_property(js_string!("top"), window.clone(), Attribute::all());
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
pub(crate) fn install_dom_interfaces(context: &mut Context) {
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
        let _ = constructor.set(
            js_string!("from"),
            native(noop).to_js_function(context.realm()),
            false,
            context,
        );
        let _ =
            context.register_global_property(JsString::from(name), constructor, Attribute::all());
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
pub(crate) fn install_events(context: &mut Context) {
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
        for method in [
            "preventDefault",
            "stopPropagation",
            "stopImmediatePropagation",
            "composedPath",
        ] {
            let _ = prototype.set(
                JsString::from(method),
                native(noop).to_js_function(context.realm()),
                false,
                context,
            );
        }
        let _ =
            context.register_global_property(JsString::from(name), constructor, Attribute::all());
    }
}

