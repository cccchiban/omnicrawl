//! 由 `browser_env.rs` 按假装的全局对象拆出，语义未变。

use super::*;
use super::codec::{atob, btoa};

use super::location::{custom_elements_get, get_computed_style, get_selection};




/// `console` 在真实浏览器里一定存在，缺失本身就是破绽。
pub(crate) fn install_console(context: &mut Context) {
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


/// 任务调度与结构化克隆：`queueMicrotask` / `structuredClone` / `reportError`。
///
/// 这三者是「现代打包产物的启动必经之路」：调度器 polyfill 会直接
/// `queueMicrotask(fn)`，框架的深比较会用 `structuredClone`。
/// 缺失时不是「少一个探测点」，而是入口脚本第一句就 `TypeError: not a callable function`。
pub(crate) fn install_task_and_clone(context: &mut Context) {
    // `queueMicrotask`：同步执行。沙箱没有真实事件循环，但回调里往往是
    // 「渲染 + 再发一次请求」，同步跑完正是我们想要的结果。
    let microtask = NativeFunction::from_fn_ptr(run_callback_now).to_js_function(context.realm());
    let _ =
        context.register_global_property(js_string!("queueMicrotask"), microtask, Attribute::all());

    // `reportError`：埋点与错误边界会调用；不记录只吞掉。
    let report = NativeFunction::from_fn_ptr(noop).to_js_function(context.realm());
    let _ = context.register_global_property(js_string!("reportError"), report, Attribute::all());

    // `structuredClone`：深层复制。boa 无内置实现，用 JSON 往返近似——
    // 对「配置对象深拷贝」这类用途足够，函数与循环引用会退化为浅引用。
    let clone = NativeFunction::from_fn_ptr(structured_clone).to_js_function(context.realm());
    let _ =
        context.register_global_property(js_string!("structuredClone"), clone, Attribute::all());

    // `btoa` / `atob`：base64 编解码，埋点上报与图标内联常见。
    for (name, function) in [
        (
            "btoa",
            NativeFunction::from_fn_ptr(btoa).to_js_function(context.realm()),
        ),
        (
            "atob",
            NativeFunction::from_fn_ptr(atob).to_js_function(context.realm()),
        ),
    ] {
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }

    // `getComputedStyle`：返回带常用属性的样式对象。页面常读 `display`/`visibility`。
    let computed = NativeFunction::from_fn_ptr(get_computed_style).to_js_function(context.realm());
    let _ = context.register_global_property(
        js_string!("getComputedStyle"),
        computed,
        Attribute::all(),
    );

    // `getSelection`：编辑器与「选中文本」逻辑会调用，返回带常用方法的对象。
    let selection = NativeFunction::from_fn_ptr(get_selection).to_js_function(context.realm());
    let _ =
        context.register_global_property(js_string!("getSelection"), selection, Attribute::all());

    // `customElements`：Web Components 注册表。`define` 只登记，不参与渲染。
    let registry = ObjectInitializer::new(context)
        .function(native(noop), js_string!("define"), 2)
        .function(native(noop), js_string!("upgrade"), 1)
        .function(native(custom_elements_get), js_string!("get"), 1)
        .function(native(noop), js_string!("whenDefined"), 1)
        .build();
    let _ =
        context.register_global_property(js_string!("customElements"), registry, Attribute::all());
}

pub(crate) fn run_callback_now(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    if let Some(callback) = args.first().and_then(JsValue::as_object) {
        if callback.is_callable() {
            let _ = callback.call(&JsValue::undefined(), &[], context);
        }
    }
    Ok(JsValue::undefined())
}

pub(crate) fn structured_clone(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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
        .call(&JsValue::undefined(), &[encoded], context)
        .or(Ok(value.clone()))
}

/// 其余一次性的零碎对象。
pub(crate) fn install_misc(context: &mut Context) {
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
pub(crate) type NativeWrapper = for<'a> fn(&'a JsValue, &'a [JsValue], &'a mut Context) -> JsResult<JsValue>;

/// `setTimeout` 家族：只登记回调，**不睡眠**。
///
/// 真实延迟对抓取毫无意义（工具不会等三秒），但页面脚本常把「主逻辑」放在
/// `setTimeout(fn, 0)` 里——指纹探测脚本正是如此。因此这里的行为是：
/// 回调进队列，等本批脚本全部求值结束后由 [`flush_timers_once`] 立即跑一轮。
pub(crate) fn timer_set_timeout(
    _this: &JsValue,
    args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
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

pub(crate) fn timer_clear(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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
        let _ = timer.callback.call(&timer.this_value, &timer.args, context);
    }
    true
}

pub(crate) struct PendingTimer {
    pub(crate) id: u64,
    pub(crate) callback: boa_engine::JsObject,
    /// 调用时的 `this`：`message` 处理器需要拿到对应的 port。
    pub(crate) this_value: JsValue,
    /// 调用参数：`message` 事件对象走这里，`setTimeout` 家族为空。
    pub(crate) args: Vec<JsValue>,
}

thread_local! {
    pub(crate) static TIMERS: std::cell::RefCell<Vec<PendingTimer>> =
        const { std::cell::RefCell::new(Vec::new()) };
    pub(crate) static NEXT_TIMER_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn mm_query(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let query = args_string(args, 0, context);
    Ok(ObjectInitializer::new(context)
        .property(
            js_string!("matches"),
            JsValue::from(false),
            Attribute::all(),
        )
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






