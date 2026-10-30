//! `HTMLCanvasElement` 的方法面：取上下文、读写宽高、导出 data URL。
//!
//! 从 `engine.rs` 拆出。位图与指纹相关的真实实现仍在 `crate::browser_env`，
//! 这里只做元素属性与上下文缓存的接线。

use super::*;

fn el_to_data_url(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    crate::browser_env::canvas_to_data_url(this, args, context)
}

/// `canvas.getContext(kind)`：按需建上下文，并把位图与元素绑定。
///
/// 同一元素反复取同一类型必须返回同一个对象（Canvas 规范），否则
/// 「先画再取上下文读像素」的探测序列会读到空画布。
fn el_get_context(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = node_id(this, context) else {
        return Ok(JsValue::null());
    };
    let kind = args_string(args, 0, context).to_ascii_lowercase();
    // 未知类型按规范返回 null 而不是抛错。
    let (prototype_key, surface_kind) = match kind.as_str() {
        "2d" => ("__ornCanvasProto", "2d"),
        "webgl" | "experimental-webgl" => ("__ornWebGLProto", "webgl"),
        "webgl2" => ("__ornWebGL2Proto", "webgl2"),
        _ => return Ok(JsValue::null()),
    };
    let width = canvas_element_dimension(this, context, "Width", crate::browser_env::DEFAULT_CANVAS_WIDTH);
    let height = canvas_element_dimension(this, context, "Height", crate::browser_env::DEFAULT_CANVAS_HEIGHT);
    // 位图先建好：即使脚本不调用任何绘制方法，`getImageData` 也要读到正确尺寸。
    crate::browser_env::surface_for(node, width, height);
    crate::browser_env::remember_context(node, surface_kind);

    // 同类型重复调用返回同一对象：状态（路径、样式、位图）必须延续。
    if let Some(object) = this.as_object() {
        let cache_key = format!("__ornContext:{surface_kind}");
        if let Ok(existing) = object.get(JsString::from(cache_key.clone()), context) {
            if existing.is_object() {
                return Ok(existing);
            }
        }
    }

    let proto = context
        .global_object()
        .get(js_string!(prototype_key), context)
        .ok()
        .and_then(|value| value.as_object())
        .unwrap_or_else(|| {
            JsObject::from_proto_and_data(None, OrdinaryObject)
        });
    let created = JsObject::from_proto_and_data(Some(proto), OrdinaryObject);
    // 位图靠元素号定位：上下文自己只存引用信息。
    let _ = created.set(js_string!("__ornCanvasOwner"), node, false, context);
    let _ = created.set(js_string!("__ornCanvasWidth"), width, false, context);
    let _ = created.set(js_string!("__ornCanvasHeight"), height, false, context);
    let _ = created.set(js_string!("canvas"), this.clone(), false, context);
    // 尺寸同时写到元素上：`element.toDataURL()` 走元素分支，读不到尺寸会按默认 300×150
    // 重建位图，把刚画好的像素清空（这一处曾让 toDataURL 恒等于空画布）。
    if let Some(element) = this.as_object() {
        let _ = element.set(js_string!("__ornCanvasWidth"), width, false, context);
        let _ = element.set(js_string!("__ornCanvasHeight"), height, false, context);
    }
    if surface_kind == "2d" {
        // 默认样式与真实 Canvas 一致：填充黑、描边黑、线宽 1、字体 10px sans-serif。
        let _ = created.set(js_string!("__ornFillStyle"), js_string!("#000000"), false, context);
        let _ = created.set(js_string!("__ornStrokeStyle"), js_string!("#000000"), false, context);
        let _ = created.set(js_string!("__ornLineWidth"), JsValue::from(1), false, context);
        let _ = created.set(js_string!("__ornFont"), js_string!("10px sans-serif"), false, context);
        let _ = created.set(js_string!("__ornPath"), JsArray::new(context)?, false, context);
        let _ = created.set(js_string!("__ornStack"), JsArray::new(context)?, false, context);
    }
    if let Some(object) = this.as_object() {
        let cache_key = format!("__ornContext:{surface_kind}");
        let _ = object.set(JsString::from(cache_key.clone()), created.clone(), false, context);
    }
    Ok(created.into())
}

fn canvas_element_dimension(
    this: &JsValue,
    context: &mut Context,
    key: &str,
    fallback: u32,
) -> u32 {
    let Some(object) = this.as_object() else {
        return fallback;
    };
    let storage_key = format!("__ornCanvas{key}");
    object
        .get(JsString::from(storage_key.clone()), context)
        .ok()
        .and_then(|value| value.to_number(context).ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u32)
        .unwrap_or(fallback)
}

fn set_canvas_dimension(this: &JsValue, args: &[JsValue], context: &mut Context, key: &str) {
    let Some(object) = this.as_object() else {
        return;
    };
    let value = args
        .first()
        .and_then(|item| item.to_number(context).ok())
        .filter(|number| number.is_finite() && *number >= 0.0)
        .unwrap_or(0.0);
    let size = value as u32;
    let storage_key = format!("__ornCanvas{key}");
    let _ = object.set(JsString::from(storage_key.clone()), JsValue::from(size), false, context);
    // 改尺寸要同步位图并清空内容：真实 canvas 在赋值时丢弃已有像素，
    // 即使赋的是同一个值也一样（规范要求重置绘制状态并清空）。
    if let Some(node) = node_id(this, context) {
        let width = canvas_element_dimension(this, context, "Width", crate::browser_env::DEFAULT_CANVAS_WIDTH);
        let height = canvas_element_dimension(this, context, "Height", crate::browser_env::DEFAULT_CANVAS_HEIGHT);
        crate::browser_env::reset_surface(node, width, height);
    }
}

fn el_canvas_get_width(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(canvas_element_dimension(
        this,
        context,
        "width",
        crate::browser_env::DEFAULT_CANVAS_WIDTH,
    )))
}

fn el_canvas_set_width(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    set_canvas_dimension(this, args, context, "Width");
    Ok(JsValue::undefined())
}

fn el_canvas_get_height(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(canvas_element_dimension(
        this,
        context,
        "height",
        crate::browser_env::DEFAULT_CANVAS_HEIGHT,
    )))
}

fn el_canvas_set_height(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    set_canvas_dimension(this, args, context, "Height");
    Ok(JsValue::undefined())
}
