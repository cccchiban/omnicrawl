//! Canvas 2D / WebGL 的 JS 侧入口：把绘制调用落到 [`crate::canvas::Surface`] 上。
//!
//! 状态（位图、当前路径、样式）挂在 context 对象自身的隐藏字段上，而不是 Rust 侧全局表：
//! 同一元素反复 `getContext('2d')` 必须拿到同一个上下文（Canvas 规范要求），
//! 挂在对象上天然满足，也免去「节点号 → 状态」的失效问题。

use super::codec::base64_encode;
use super::*;
use crate::canvas::{parse_font_shorthand, resolve_font, Gradient, GradientKind, Surface};

/// 默认画布尺寸：HTML 规范里没有 `width`/`height` 属性时是 300×150。
pub(crate) const DEFAULT_CANVAS_WIDTH: u32 = 300;
pub(crate) const DEFAULT_CANVAS_HEIGHT: u32 = 150;

pub(crate) fn install_webgl(context: &mut Context) {
    let prototype = ObjectInitializer::new(context)
        .function(native(webgl_get_parameter), js_string!("getParameter"), 1)
        .function(
            native(webgl_extensions),
            js_string!("getSupportedExtensions"),
            0,
        )
        .function(native(webgl_extensions), js_string!("getExtension"), 1)
        .function(
            native(webgl_shader_precision),
            js_string!("getShaderPrecisionFormat"),
            2,
        )
        .function(
            native(webgl_context_attributes),
            js_string!("getContextAttributes"),
            0,
        )
        .function(native(webgl_get_error), js_string!("getError"), 0)
        .function(native(webgl_noop), js_string!("clearColor"), 4)
        .function(native(webgl_noop), js_string!("clear"), 1)
        .function(native(webgl_noop), js_string!("viewport"), 4)
        .function(native(webgl_noop), js_string!("enable"), 1)
        .function(native(webgl_noop), js_string!("disable"), 1)
        .function(native(webgl_noop), js_string!("drawArrays"), 3)
        .function(native(webgl_noop), js_string!("flush"), 0)
        .function(native(webgl_noop), js_string!("finish"), 0)
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

    // WebGL2 与 WebGL1 共用同一套常量：真机上 2.0 是超集，取同一份足以通过取值校验。
    let prototype2 = ObjectInitializer::new(context)
        .function(native(webgl_get_parameter), js_string!("getParameter"), 1)
        .function(
            native(webgl_extensions),
            js_string!("getSupportedExtensions"),
            0,
        )
        .function(native(webgl_extensions), js_string!("getExtension"), 1)
        .function(
            native(webgl_shader_precision),
            js_string!("getShaderPrecisionFormat"),
            2,
        )
        .function(
            native(webgl_context_attributes),
            js_string!("getContextAttributes"),
            0,
        )
        .function(native(webgl_get_error), js_string!("getError"), 0)
        .function(native(webgl_noop), js_string!("clearColor"), 4)
        .function(native(webgl_noop), js_string!("clear"), 1)
        .function(native(webgl_noop), js_string!("viewport"), 4)
        .function(native(webgl_noop), js_string!("flush"), 0)
        .function(native(webgl_noop), js_string!("finish"), 0)
        .build();
    let constructor2 = constructible(context, &prototype2, "WebGL2RenderingContext");
    let _ = context.register_global_property(
        js_string!("WebGL2RenderingContext"),
        constructor2,
        Attribute::all(),
    );
    let _ = context.register_global_property(
        js_string!("__ornWebGL2Proto"),
        prototype2,
        Attribute::READONLY,
    );
}

fn webgl_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

pub(crate) fn webgl_get_error(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    // 0 = NO_ERROR：探测脚本会把非零值当成「上下文丢失」。
    Ok(JsValue::from(0))
}

/// `getShaderPrecisionFormat(shaderType, precisionType)`：真机返回 `{rangeMin,rangeMax,precision}`。
///
/// 这是 WebGL 指纹里最常被读的字段之一：缺了它是 `not a callable function`，
/// 整段能力探测中断。取值按 ANGLE/D3D11 的真实常量给。
pub(crate) fn webgl_shader_precision(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let shape = ObjectInitializer::new(context)
        .property(js_string!("rangeMin"), JsValue::from(127), Attribute::all())
        .property(js_string!("rangeMax"), JsValue::from(127), Attribute::all())
        .property(js_string!("precision"), JsValue::from(23), Attribute::all())
        .build();
    Ok(shape.into())
}

pub(crate) fn webgl_context_attributes(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let attributes = ObjectInitializer::new(context)
        .property(js_string!("alpha"), JsValue::from(true), Attribute::all())
        .property(
            js_string!("antialias"),
            JsValue::from(true),
            Attribute::all(),
        )
        .property(
            js_string!("depth"),
            JsValue::from(true),
            Attribute::all(),
        )
        .property(
            js_string!("desynchronized"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(
            js_string!("failIfMajorPerformanceCaveat"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(
            js_string!("powerPreference"),
            js_string!("default"),
            Attribute::all(),
        )
        .property(
            js_string!("premultipliedAlpha"),
            JsValue::from(true),
            Attribute::all(),
        )
        .property(
            js_string!("preserveDrawingBuffer"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(js_string!("stencil"), JsValue::from(false), Attribute::all())
        .build();
    Ok(attributes.into())
}

pub(crate) fn webgl_get_parameter(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let parameter = args
        .first()
        .and_then(|value| value.to_number(context).ok())
        .unwrap_or(0.0) as i64;
    let value = match parameter {
        0x1F00 => JsValue::from(js_string!("WebKit")),
        0x1F01 => JsValue::from(js_string!("WebKit WebGL")),
        0x1F02 => JsValue::from(js_string!("WebGL 1.0 (OpenGL ES 2.0 Chromium)")),
        0x1F03 => JsValue::from(js_string!("WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)")),
        0x8B8C => JsValue::from(js_string!(
            "WebGL GLSL ES 1.0 (OpenGL ES GLSL ES 1.0 Chromium)"
        )),
        // 带括号的厂商串来自 ANGLE：Chromium 系在 Windows 上的真实形态。
        0x9245 => JsValue::from(js_string!("Google Inc. (Intel)")),
        0x9246 => JsValue::from(js_string!(
            "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)"
        )),
        // 0x9247/0x9248：WebGL2 的 UNMASKED_VENDOR/RENDERER，取值与 1.0 同源。
        0x9247 => JsValue::from(js_string!("Google Inc. (Intel)")),
        0x9248 => JsValue::from(js_string!(
            "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11)"
        )),
        0x0D33 => JsValue::from(16384),
        0x84E8 => JsValue::from(16384),
        0x851C => JsValue::from(16384),
        0x8D57 => JsValue::from(4),
        0x8073 => JsValue::from(8),
        0x8DFD => JsValue::from(30),
        0x8872 => JsValue::from(16),
        0x0D3A => {
            let dims = JsArray::new(context)?;
            dims.push(JsValue::from(16384), context)?;
            dims.push(JsValue::from(16384), context)?;
            dims.into()
        }
        0x86A3 => {
            // COMPRESSED_TEXTURE_FORMATS：压缩纹理格式列表，指纹里常被当成设备特征。
            let formats = JsArray::new(context)?;
            for format in [
                0x83F0u32, 0x83F1, 0x83F2, 0x83F3, 0x8C4C, 0x8C4D, 0x8C4E, 0x8C4F, 0x9270, 0x9271,
                0x9272, 0x9273, 0x9274, 0x9275, 0x9276, 0x9277, 0x9278, 0x9279,
            ] {
                formats.push(JsValue::from(format), context)?;
            }
            formats.into()
        }
        _ => JsValue::null(),
    };
    Ok(value)
}

pub(crate) fn webgl_extensions(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // `getExtension(name)` 传入名字时回一个对象；`getSupportedExtensions()` 无参时回数组。
    if let Some(name) = args
        .first()
        .and_then(|value| value.to_string(context).ok())
        .map(|text| text.to_std_string_escaped())
    {
        // 只承认确实支持的扩展：探测脚本会用它判断能力，乱给会与真机行为不符。
        let supported = [
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
        if !supported.contains(&name.as_str()) {
            return Ok(JsValue::null());
        }
        let _ = this;
        // 扩展对象只需是个可调用方法的普通对象：真实扩展的成员函数用不到。
        return Ok(ObjectInitializer::new(context).build().into());
    }
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

/// 在原型上定义样式访问器：读写都落到 `slot` 上。
///
/// 写进去的值可以是字符串（纯色）或渐变对象，绘制时按类型分别处理。
fn define_style_accessor(context: &mut Context, prototype: &JsObject, name: &str, slot: &str) {
    let getter = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            |this, _args, captures, context| {
                let (slot,): &(JsString,) = captures;
                let Some(object) = this.as_object() else {
                    return Ok(JsValue::undefined());
                };
                Ok(object
                    .get(slot.clone(), context)
                    .unwrap_or(JsValue::undefined()))
            },
            (JsString::from(slot),),
        ),
    )
    .name(JsString::from(format!("get {name}")))
    .length(0)
    .build();
    let setter = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            |this, args, captures, context| {
                let (slot,): &(JsString,) = captures;
                let Some(object) = this.as_object() else {
                    return Ok(JsValue::undefined());
                };
                let value = args.first().cloned().unwrap_or(JsValue::undefined());
                let _ = object.set(slot.clone(), value, false, context);
                Ok(JsValue::undefined())
            },
            (JsString::from(slot),),
        ),
    )
    .name(JsString::from(format!("set {name}")))
    .length(1)
    .build();
    let descriptor = boa_engine::property::PropertyDescriptor::builder()
        .get(getter)
        .set(setter)
        .enumerable(true)
        .configurable(true)
        .build();
    let _ = prototype.define_property_or_throw(JsString::from(name), descriptor, context);
}

pub(crate) fn install_canvas(context: &mut Context) {
    let prototype = ObjectInitializer::new(context)
        .function(native(canvas_get_image_data), js_string!("getImageData"), 4)
        .function(native(canvas_to_data_url), js_string!("toDataURL"), 0)
        .function(native(canvas_fill_rect), js_string!("fillRect"), 4)
        .function(native(canvas_clear_rect), js_string!("clearRect"), 4)
        .function(native(canvas_stroke_rect), js_string!("strokeRect"), 4)
        .function(native(canvas_fill_text), js_string!("fillText"), 3)
        .function(native(canvas_measure_text), js_string!("measureText"), 1)
        .function(native(canvas_begin_path), js_string!("beginPath"), 0)
        .function(native(canvas_move_to), js_string!("moveTo"), 2)
        .function(native(canvas_line_to), js_string!("lineTo"), 2)
        .function(native(canvas_arc), js_string!("arc"), 5)
        .function(native(canvas_rect), js_string!("rect"), 4)
        .function(native(canvas_close_path), js_string!("closePath"), 0)
        .function(native(canvas_fill), js_string!("fill"), 0)
        .function(native(canvas_stroke), js_string!("stroke"), 0)
        .function(native(canvas_save), js_string!("save"), 0)
        .function(native(canvas_restore), js_string!("restore"), 0)
        .function(native(canvas_translate), js_string!("translate"), 2)
        .function(native(canvas_create_linear_gradient), js_string!("createLinearGradient"), 4)
        .function(native(canvas_create_radial_gradient), js_string!("createRadialGradient"), 6)
        .build();
    let constructor = constructible(context, &prototype, "CanvasRenderingContext2D");
    // 样式字段在真机上是访问器：赋值写进内部槽，读取走 getter。
    // 直接用数据属性会让 `g.fillStyle='#f00'` 写进 `fillStyle`，而绘制时读的是内部槽。
    for (name, slot) in [
        ("fillStyle", "__ornFillStyle"),
        ("strokeStyle", "__ornStrokeStyle"),
        ("lineWidth", "__ornLineWidth"),
        ("font", "__ornFont"),
    ] {
        define_style_accessor(context, &prototype, name, slot);
    }
    let _ = context.register_global_property(
        js_string!("CanvasRenderingContext2D"),
        constructor,
        Attribute::all(),
    );
    let _ = context.register_global_property(
        js_string!("__ornCanvasProto"),
        prototype,
        Attribute::READONLY,
    );
}

// 每个 canvas 元素的位图：键是元素号，值随上下文一起存活。
thread_local! {
    /// canvas 元素 → 位图。用 `Rc<RefCell<_>>` 而不是全局 Vec：
    /// 同一元素的 2d 上下文与 `toDataURL` 必须看到同一张位图。
    static CANVAS_SURFACES: std::cell::RefCell<
        std::collections::HashMap<usize, std::rc::Rc<std::cell::RefCell<Surface>>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    /// canvas 元素 → 当前上下文类型（`2d` / `webgl` / `webgl2`）。
    static CANVAS_CONTEXTS: std::cell::RefCell<std::collections::HashMap<usize, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub(crate) fn clear_canvas_state() {
    CANVAS_SURFACES.with(|slot| slot.borrow_mut().clear());
    CANVAS_CONTEXTS.with(|slot| slot.borrow_mut().clear());
}

/// 取（或建）某元素的位图。尺寸变化会重建并清空，与真实 canvas 一致。
pub(crate) fn surface_for(
    node: usize,
    width: u32,
    height: u32,
) -> std::rc::Rc<std::cell::RefCell<Surface>> {
    CANVAS_SURFACES.with(|slot| {
        let mut surfaces = slot.borrow_mut();
        let surface = surfaces
            .entry(node)
            .or_insert_with(|| std::rc::Rc::new(std::cell::RefCell::new(Surface::new(width, height))));
        if surface.borrow().width != width || surface.borrow().height != height {
            *surface.borrow_mut() = Surface::new(width, height);
        }
        surface.clone()
    })
}

pub(crate) fn remember_context(node: usize, kind: &str) {
    let _ = CANVAS_CONTEXTS.with(|slot| {
        slot.borrow_mut().insert(node, kind.to_string());
    });
}

/// 无条件重建位图：`canvas.width = n` 即使赋同值也要清空，与规范一致。
pub(crate) fn reset_surface(node: usize, width: u32, height: u32) {
    CANVAS_SURFACES.with(|slot| {
        slot.borrow_mut().insert(
            node,
            std::rc::Rc::new(std::cell::RefCell::new(Surface::new(width, height))),
        );
    });
}

/// 从画布位图读 RGBA。
///
/// `this` 既可能是 2D 上下文，也可能是 canvas 元素自身（`toDataURL` 两种都能调）：
/// 元素上查不到 `__ornCanvasOwner` 时退回用元素自己的节点号。
fn with_surface<T>(
    this: &JsValue,
    context: &mut Context,
    callback: impl FnOnce(&mut Surface) -> T,
) -> Option<T> {
    let object = this.as_object()?;
    // 注意：元素上不存在 `__ornCanvasOwner` 时 `to_number` 会把 undefined 转成 NaN，
    // 不过滤就会误走上下文分支并把 NaN 当节点号（曾让 toDataURL 恒读到空位图）。
    let owner = object
        .get(js_string!("__ornCanvasOwner"), context)
        .ok()
        .and_then(|value| value.to_number(context).ok())
        .filter(|number| number.is_finite());
    // 元素分支：节点号挂在元素的 `__ornId` 上，尺寸取自 `__ornCanvas*` 槽。
    let (node, width, height) = match owner {
        Some(owner) => {
            // 上下文上缓存的尺寸可能已过期（脚本随后改了 `canvas.width`）：
            // 优先读元素自身的实时尺寸，读不到才退回上下文缓存的初值。
            let element = object
                .get(js_string!("canvas"), context)
                .ok()
                .and_then(|value| value.as_object());
            match element {
                Some(element) => {
                    // 逐次读：闭包形式的默认值会二次借用 context。
                    let element_width =
                        canvas_dimension(&element, context, "__ornCanvasWidth", u32::MAX);
                    let element_height =
                        canvas_dimension(&element, context, "__ornCanvasHeight", u32::MAX);
                    let width = if element_width == u32::MAX {
                        canvas_dimension(&object, context, "__ornCanvasWidth", DEFAULT_CANVAS_WIDTH)
                    } else {
                        element_width
                    };
                    let height = if element_height == u32::MAX {
                        canvas_dimension(&object, context, "__ornCanvasHeight", DEFAULT_CANVAS_HEIGHT)
                    } else {
                        element_height
                    };
                    (owner as usize, width, height)
                }
                None => {
                    let width = canvas_dimension(
                        &object,
                        context,
                        "__ornCanvasWidth",
                        DEFAULT_CANVAS_WIDTH,
                    );
                    let height = canvas_dimension(
                        &object,
                        context,
                        "__ornCanvasHeight",
                        DEFAULT_CANVAS_HEIGHT,
                    );
                    (owner as usize, width, height)
                }
            }
        }
        None => {
            let node = object
                .get(js_string!("__ornId"), context)
                .ok()
                .and_then(|value| value.to_number(context).ok())? as usize;
            // 元素上的尺寸槽与上下文同名（`__ornCanvasWidth` 已由 getContext 写入）。
            let width = canvas_dimension(&object, context, "__ornCanvasWidth", DEFAULT_CANVAS_WIDTH);
            let height =
                canvas_dimension(&object, context, "__ornCanvasHeight", DEFAULT_CANVAS_HEIGHT);
            (node, width, height)
        }
    };
    let surface = surface_for(node, width, height);
    let mut borrow = surface.borrow_mut();
    Some(callback(&mut borrow))
}

fn canvas_dimension(object: &JsObject, context: &mut Context, key: &str, fallback: u32) -> u32 {
    object
        .get(JsString::from(key), context)
        .ok()
        .and_then(|value| value.to_number(context).ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u32)
        .unwrap_or(fallback)
}

/// 当前填充样式：纯色字符串或已注册的渐变对象。
fn current_style(this: &JsValue, context: &mut Context, key: &str, fallback: [u8; 4]) -> StyleValue {
    let Some(object) = this.as_object() else {
        return StyleValue::Color(fallback);
    };
    let Ok(value) = object.get(JsString::from(key), context) else {
        return StyleValue::Color(fallback);
    };
    if let Some(style) = value.as_object() {
        let stops = read_gradient_stops(&style, context);
        let kind = read_gradient_kind(&style, context);
        if let (Some(kind), Some(stops)) = (kind, stops) {
            return StyleValue::Gradient(Gradient { kind, stops });
        }
        return StyleValue::Color(fallback);
    }
    let Ok(text) = value.to_string(context) else {
        return StyleValue::Color(fallback);
    };
    StyleValue::Color(parse_css_color(&text.to_std_string_escaped()).unwrap_or(fallback))
}

enum StyleValue {
    Color([u8; 4]),
    Gradient(Gradient),
}

fn read_gradient_kind(object: &JsObject, context: &mut Context) -> Option<GradientKind> {
    let kind = object
        .get(js_string!("__ornKind"), context)
        .ok()?
        .to_string(context)
        .ok()?
        .to_std_string_escaped();
    let mut number = |key: &str| -> f64 {
        object
            .get(JsString::from(key), context)
            .ok()
            .and_then(|value| value.to_number(context).ok())
            .unwrap_or(0.0)
    };
    match kind.as_str() {
        "linear" => Some(GradientKind::Linear {
            x0: number("__ornX0"),
            y0: number("__ornY0"),
            x1: number("__ornX1"),
            y1: number("__ornY1"),
        }),
        "radial" => Some(GradientKind::Radial {
            x0: number("__ornX0"),
            y0: number("__ornY0"),
            r0: number("__ornR0"),
            x1: number("__ornX1"),
            y1: number("__ornY1"),
            r1: number("__ornR1"),
        }),
        _ => None,
    }
}

fn read_gradient_stops(object: &JsObject, context: &mut Context) -> Option<Vec<(f64, [u8; 4])>> {
    let stops = object
        .get(js_string!("__ornStops"), context)
        .ok()?
        .as_object()?;
    let length = stops
        .get(js_string!("length"), context)
        .ok()
        .and_then(|value| value.to_number(context).ok())
        .unwrap_or(0.0) as usize;
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let entry = stops.get(index, context).ok()?.as_object()?;
        let offset = entry
            .get(js_string!("offset"), context)
            .ok()
            .and_then(|value| value.to_number(context).ok())
            .unwrap_or(0.0);
        let color = entry
            .get(js_string!("color"), context)
            .ok()
            .and_then(|value| value.to_string(context).ok())
            .map(|text| text.to_std_string_escaped())
            .unwrap_or_default();
        out.push((offset, parse_css_color(&color).unwrap_or([0, 0, 0, 255])));
    }
    out.sort_by(|left, right| left.0.partial_cmp(&right.0).unwrap_or(std::cmp::Ordering::Equal));
    Some(out)
}

/// CSS 颜色解析：覆盖 `#rgb`/`#rrggbb`/`#rrggbbaa`、`rgb()`/`rgba()` 与常见关键字。
pub(crate) fn parse_css_color(text: &str) -> Option<[u8; 4]> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let value = |index: usize| {
                    u8::from_str_radix(&hex[index..index + 1].repeat(2), 16).ok()
                };
                Some([value(0)?, value(1)?, value(2)?, 255])
            }
            4 => {
                let value = |index: usize| {
                    u8::from_str_radix(&hex[index..index + 1].repeat(2), 16).ok()
                };
                Some([value(0)?, value(1)?, value(2)?, value(3)?])
            }
            6 => Some([
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
                255,
            ]),
            8 => Some([
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
                u8::from_str_radix(&hex[6..8], 16).ok()?,
            ]),
            _ => None,
        };
    }
    if lower.starts_with("rgb") {
        let open = lower.find('(')?;
        let close = lower.rfind(')')?;
        let body = &lower[open + 1..close];
        let parts: Vec<&str> = body
            .split(|character| character == ',' || character == '/' || character == ' ')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() < 3 {
            return None;
        }
        let channel = |text: &str| -> Option<u8> {
            if let Some(percent) = text.strip_suffix('%') {
                percent.parse::<f64>().ok().map(|value| {
                    (value / 100.0 * 255.0).round().clamp(0.0, 255.0) as u8
                })
            } else {
                text.parse::<f64>()
                    .ok()
                    .map(|value| value.round().clamp(0.0, 255.0) as u8)
            }
        };
        let alpha = if parts.len() >= 4 {
            if let Some(percent) = parts[3].strip_suffix('%') {
                percent
                    .parse::<f64>()
                    .ok()
                    .map(|value| (value / 100.0 * 255.0).round().clamp(0.0, 255.0) as u8)
                    .unwrap_or(255)
            } else {
                parts[3]
                    .parse::<f64>()
                    .ok()
                    .map(|value| (value * 255.0).round().clamp(0.0, 255.0) as u8)
                    .unwrap_or(255)
            }
        } else {
            255
        };
        return Some([channel(parts[0])?, channel(parts[1])?, channel(parts[2])?, alpha]);
    }
    // 关键字：探测器常用纯黑与纯白做基线绘制。
    match lower.as_str() {
        "black" => Some([0, 0, 0, 255]),
        "white" => Some([255, 255, 255, 255]),
        "red" => Some([255, 0, 0, 255]),
        "green" => Some([0, 128, 0, 255]),
        "blue" => Some([0, 0, 255, 255]),
        "yellow" => Some([255, 255, 0, 255]),
        "cyan" | "aqua" => Some([0, 255, 255, 255]),
        "magenta" | "fuchsia" => Some([255, 0, 255, 255]),
        "gray" | "grey" => Some([128, 128, 128, 255]),
        "transparent" => Some([0, 0, 0, 0]),
        _ => None,
    }
}

pub(crate) fn canvas_fill_rect(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x = number_arg(args, 0, context);
    let y = number_arg(args, 1, context);
    let width = number_arg(args, 2, context);
    let height = number_arg(args, 3, context);
    let style = current_style(this, context, "__ornFillStyle", [0, 0, 0, 255]);
    with_surface(this, context, |surface| match style {
        StyleValue::Color(color) => surface.fill_rect(x, y, width, height, color),
        StyleValue::Gradient(gradient) => {
            surface.fill_rect_gradient(x, y, width, height, &gradient)
        }
    });
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_clear_rect(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x = number_arg(args, 0, context);
    let y = number_arg(args, 1, context);
    let width = number_arg(args, 2, context);
    let height = number_arg(args, 3, context);
    with_surface(this, context, |surface| {
        let left = x.min(x + width).max(0.0) as u32;
        let right = (x.max(x + width).max(0.0) as u32).min(surface.width);
        let top = y.min(y + height).max(0.0) as u32;
        let bottom = (y.max(y + height).max(0.0) as u32).min(surface.height);
        for py in top..bottom {
            for px in left..right {
                let index = ((py as usize) * (surface.width as usize) + px as usize) * 4;
                if index + 3 < surface.data.len() {
                    surface.data[index] = 0;
                    surface.data[index + 1] = 0;
                    surface.data[index + 2] = 0;
                    surface.data[index + 3] = 0;
                }
            }
        }
    });
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_stroke_rect(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x = number_arg(args, 0, context);
    let y = number_arg(args, 1, context);
    let width = number_arg(args, 2, context);
    let height = number_arg(args, 3, context);
    let style = current_style(this, context, "__ornStrokeStyle", [0, 0, 0, 255]);
    let line_width = object_number(this, context, "__ornLineWidth").unwrap_or(1.0).max(1.0);
    let points = [
        (x, y),
        (x + width, y),
        (x + width, y + height),
        (x, y + height),
    ];
    with_surface(this, context, |surface| {
        if let StyleValue::Color(color) = style {
            surface.stroke_polygon(&points, color, line_width, true);
        }
    });
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_fill_text(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let text = args_string(args, 0, context);
    let x = number_arg(args, 1, context);
    let y = number_arg(args, 2, context);
    let style = current_style(this, context, "__ornFillStyle", [0, 0, 0, 255]);
    let font_shorthand = object_string(this, context, "__ornFont")
        .unwrap_or_else(|| "10px sans-serif".to_string());
    let (size, bold, family) = parse_font_shorthand(&font_shorthand);
    let font = resolve_font(&family, bold);
    with_surface(this, context, |surface| {
        if let StyleValue::Color(color) = style {
            surface.fill_text(&text, x, y, size, color, font.as_deref());
        }
    });
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_measure_text(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let text = args_string(args, 0, context);
    let font_shorthand = object_string(this, context, "__ornFont")
        .unwrap_or_else(|| "10px sans-serif".to_string());
    let (size, bold, family) = parse_font_shorthand(&font_shorthand);
    let font = resolve_font(&family, bold);
    // 宽度按字形的真实 advance 求和：布局库会用它算居中和换行。
    let width = font
        .as_deref()
        .map(|face| face.measure(&text, size))
        .unwrap_or(size * text.chars().count() as f64 * 0.5);
    let metrics = ObjectInitializer::new(context)
        .property(js_string!("width"), JsValue::from(width), Attribute::all())
        .property(
            js_string!("actualBoundingBoxLeft"),
            JsValue::from(0),
            Attribute::all(),
        )
        .property(
            js_string!("actualBoundingBoxRight"),
            JsValue::from(width),
            Attribute::all(),
        )
        .property(
            js_string!("actualBoundingBoxAscent"),
            JsValue::from(size * 0.8),
            Attribute::all(),
        )
        .property(
            js_string!("actualBoundingBoxDescent"),
            JsValue::from(size * 0.2),
            Attribute::all(),
        )
        .property(
            js_string!("fontBoundingBoxAscent"),
            JsValue::from(size * 0.9),
            Attribute::all(),
        )
        .property(
            js_string!("fontBoundingBoxDescent"),
            JsValue::from(size * 0.22),
            Attribute::all(),
        )
        .build();
    Ok(metrics.into())
}

fn number_arg(args: &[JsValue], index: usize, context: &mut Context) -> f64 {
    args.get(index)
        .and_then(|value| value.to_number(context).ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

fn object_number(this: &JsValue, context: &mut Context, key: &str) -> Option<f64> {
    this.as_object()?
        .get(JsString::from(key), context)
        .ok()
        .and_then(|value| value.to_number(context).ok())
}

fn object_string(this: &JsValue, context: &mut Context, key: &str) -> Option<String> {
    this.as_object()?
        .get(JsString::from(key), context)
        .ok()?
        .to_string(context)
        .ok()
        .map(|text| text.to_std_string_escaped())
}

pub(crate) fn canvas_begin_path(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    if let Some(object) = this.as_object() {
        let _ = object.set(js_string!("__ornPath"), JsArray::new(context)?, false, context);
    }
    Ok(JsValue::undefined())
}

fn push_path_point(this: &JsValue, x: f64, y: f64, context: &mut Context) {
    let Some(object) = this.as_object() else {
        return;
    };
    let Ok(path) = object.get(js_string!("__ornPath"), context) else {
        return;
    };
    let Some(array) = path.as_object() else {
        return;
    };
    let point = JsArray::new(context).map(|point| {
        let _ = point.push(JsValue::from(x), context);
        let _ = point.push(JsValue::from(y), context);
        point
    });
    if let Ok(point) = point {
        let _ = JsArray::from_object(array.clone()).and_then(|a| a.push(point, context));
    }
}

pub(crate) fn canvas_move_to(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x = number_arg(args, 0, context);
    let y = number_arg(args, 1, context);
    push_path_point(this, x, y, context);
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_line_to(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x = number_arg(args, 0, context);
    let y = number_arg(args, 1, context);
    push_path_point(this, x, y, context);
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_rect(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x = number_arg(args, 0, context);
    let y = number_arg(args, 1, context);
    let width = number_arg(args, 2, context);
    let height = number_arg(args, 3, context);
    for (px, py) in [
        (x, y),
        (x + width, y),
        (x + width, y + height),
        (x, y + height),
        (x, y),
    ] {
        push_path_point(this, px, py, context);
    }
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_arc(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let center_x = number_arg(args, 0, context);
    let center_y = number_arg(args, 1, context);
    let radius = number_arg(args, 2, context);
    let start = number_arg(args, 3, context);
    let end = number_arg(args, 4, context);
    let mut points = Vec::new();
    crate::canvas::push_arc(&mut points, center_x, center_y, radius, start, end);
    for (x, y) in points {
        push_path_point(this, x, y, context);
    }
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_close_path(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn read_path(this: &JsValue, context: &mut Context) -> Vec<(f64, f64)> {
    let Some(object) = this.as_object() else {
        return Vec::new();
    };
    let Ok(path) = object.get(js_string!("__ornPath"), context) else {
        return Vec::new();
    };
    let Some(array) = path.as_object() else {
        return Vec::new();
    };
    let Ok(length) = array.get(js_string!("length"), context) else {
        return Vec::new();
    };
    let count = length.to_number(context).unwrap_or(0.0) as usize;
    let mut points = Vec::with_capacity(count);
    for index in 0..count {
        let Ok(point) = array.get(index, context) else {
            continue;
        };
        let Some(point) = point.as_object() else {
            continue;
        };
        let x = point
            .get(js_string!("0"), context)
            .ok()
            .and_then(|value| value.to_number(context).ok())
            .unwrap_or(0.0);
        let y = point
            .get(js_string!("1"), context)
            .ok()
            .and_then(|value| value.to_number(context).ok())
            .unwrap_or(0.0);
        points.push((x, y));
    }
    points
}

pub(crate) fn canvas_fill(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let points = read_path(this, context);
    let style = current_style(this, context, "__ornFillStyle", [0, 0, 0, 255]);
    with_surface(this, context, |surface| {
        if let StyleValue::Color(color) = style {
            if points.len() >= 3 {
                surface.fill_polygon(&points, color);
            }
        }
    });
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_stroke(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let points = read_path(this, context);
    let style = current_style(this, context, "__ornStrokeStyle", [0, 0, 0, 255]);
    let line_width = object_number(this, context, "__ornLineWidth")
        .unwrap_or(1.0)
        .max(1.0);
    with_surface(this, context, |surface| {
        if let StyleValue::Color(color) = style {
            if points.len() >= 2 {
                surface.stroke_polygon(&points, color, line_width, false);
            }
        }
    });
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_save(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // 保存当前样式：真正的状态栈只在样式被改过时才有意义。
    if let Some(object) = this.as_object() {
        // 先把四个值取出来：`ObjectInitializer` 会独占 `&mut Context`。
        let fill = object
            .get(js_string!("__ornFillStyle"), context)
            .unwrap_or(JsValue::undefined());
        let stroke = object
            .get(js_string!("__ornStrokeStyle"), context)
            .unwrap_or(JsValue::undefined());
        let line_width = object
            .get(js_string!("__ornLineWidth"), context)
            .unwrap_or(JsValue::undefined());
        let font = object
            .get(js_string!("__ornFont"), context)
            .unwrap_or(JsValue::undefined());
        let snapshot = ObjectInitializer::new(context)
            .property(js_string!("fillStyle"), fill, Attribute::all())
            .property(js_string!("strokeStyle"), stroke, Attribute::all())
            .property(js_string!("lineWidth"), line_width, Attribute::all())
            .property(js_string!("font"), font, Attribute::all())
            .build();
        if let Ok(stack) = object.get(js_string!("__ornStack"), context) {
            if let Some(stack) = stack.as_object() {
                let _ = JsArray::from_object(stack.clone()).and_then(|a| a.push(snapshot, context));
            }
        }
    }
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_restore(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Ok(stack) = object.get(js_string!("__ornStack"), context) else {
        return Ok(JsValue::undefined());
    };
    let Some(stack) = stack.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Ok(length) = stack
        .get(js_string!("length"), context)
        .and_then(|value| value.to_number(context))
    else {
        return Ok(JsValue::undefined());
    };
    if length < 1.0 {
        return Ok(JsValue::undefined());
    }
    if let Ok(entry) = stack.get((length - 1.0) as usize, context) {
        if let Some(entry) = entry.as_object() {
            for (source, target) in [
                ("fillStyle", "__ornFillStyle"),
                ("strokeStyle", "__ornStrokeStyle"),
                ("lineWidth", "__ornLineWidth"),
                ("font", "__ornFont"),
            ] {
                if let Ok(value) = entry.get(JsString::from(source), context) {
                    let _ = object.set(JsString::from(target), value, false, context);
                }
            }
        }
    }
    let _ = stack.set(
        js_string!("length"),
        JsValue::from((length - 1.0) as u32),
        false,
        context,
    );
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_translate(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    // 不做变换矩阵：正文提取不依赖坐标平移，静默接受即可（缺方法会让脚本抛错）。
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_create_linear_gradient(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x0 = number_arg(args, 0, context);
    let y0 = number_arg(args, 1, context);
    let x1 = number_arg(args, 2, context);
    let y1 = number_arg(args, 3, context);
    build_gradient(
        context,
        "linear",
        &[("__ornX0", x0), ("__ornY0", y0), ("__ornX1", x1), ("__ornY1", y1)],
    )
}

pub(crate) fn canvas_create_radial_gradient(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let x0 = number_arg(args, 0, context);
    let y0 = number_arg(args, 1, context);
    let r0 = number_arg(args, 2, context);
    let x1 = number_arg(args, 3, context);
    let y1 = number_arg(args, 4, context);
    let r1 = number_arg(args, 5, context);
    build_gradient(
        context,
        "radial",
        &[
            ("__ornX0", x0),
            ("__ornY0", y0),
            ("__ornR0", r0),
            ("__ornX1", x1),
            ("__ornY1", y1),
            ("__ornR1", r1),
        ],
    )
}

fn build_gradient(
    context: &mut Context,
    kind: &str,
    numbers: &[(&str, f64)],
) -> JsResult<JsValue> {
    let stops = JsArray::new(context)?;
    let kind_value = JsString::from(kind);
    let gradient = ObjectInitializer::new(context)
        .property(js_string!("__ornKind"), kind_value, Attribute::all())
        .property(js_string!("__ornStops"), stops, Attribute::all())
        .build();
    // 坐标逐个写到对象上：`ObjectInitializer` 只能一次性链式构建，
    // 而坐标个数随渐变种类变化（线性 4 个、径向 6 个）。
    for (key, value) in numbers {
        let _ = gradient.set(
            JsString::from(*key),
            JsValue::from(*value),
            false,
            context,
        );
    }
    // `addColorStop` 追加到色标数组上。
    let add_stop = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_fn_ptr(gradient_add_stop),
    )
    .name(js_string!("addColorStop"))
    .length(2)
    .build();
    let _ = gradient.set(js_string!("addColorStop"), add_stop, false, context);
    Ok(gradient.into())
}

pub(crate) fn gradient_add_stop(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let offset = number_arg(args, 0, context).clamp(0.0, 1.0);
    let color = args_string(args, 1, context);
    let Ok(stops) = object.get(js_string!("__ornStops"), context) else {
        return Ok(JsValue::undefined());
    };
    let Some(stops) = stops.as_object() else {
        return Ok(JsValue::undefined());
    };
    let entry = ObjectInitializer::new(context)
        .property(js_string!("offset"), JsValue::from(offset), Attribute::all())
        .property(
            js_string!("color"),
            JsString::from(color),
            Attribute::all(),
        )
        .build();
    let _ = JsArray::from_object(stops.clone()).and_then(|a| a.push(entry, context));
    Ok(JsValue::undefined())
}

pub(crate) fn canvas_get_image_data(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let offset_x = number_arg(args, 0, context) as i64;
    let offset_y = number_arg(args, 1, context) as i64;
    let width = number_arg(args, 2, context).max(0.0) as u32;
    let height = number_arg(args, 3, context).max(0.0) as u32;
    // 越界区域按透明补齐：与规范一致（返回 ImageData 而不是抛错）。
    let pixels = with_surface(this, context, |surface| {
        let mut data = vec![0u8; (width as usize) * (height as usize) * 4];
        for row in 0..height {
            let source_y = offset_y + row as i64;
            if source_y < 0 || source_y >= surface.height as i64 {
                continue;
            }
            for column in 0..width {
                let source_x = offset_x + column as i64;
                if source_x < 0 || source_x >= surface.width as i64 {
                    continue;
                }
                let source = ((source_y as usize) * (surface.width as usize) + source_x as usize) * 4;
                let target = ((row as usize) * (width as usize) + column as usize) * 4;
                data[target..target + 4].copy_from_slice(&surface.data[source..source + 4]);
            }
        }
        data
    })
    .unwrap_or_else(|| vec![0u8; (width as usize) * (height as usize) * 4]);

    let array = boa_engine::object::builtins::JsUint8ClampedArray::from_iter(pixels, context)?;
    Ok(ObjectInitializer::new(context)
        .property(js_string!("data"), array, Attribute::all())
        .property(js_string!("width"), JsValue::from(width), Attribute::all())
        .property(js_string!("height"), JsValue::from(height), Attribute::all())
        .build()
        .into())
}

pub(crate) fn canvas_to_data_url(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // 真实 PNG：像素可被解码复核，`toDataURL().length` 也不再是常数。
    let encoded = with_surface(this, context, |surface| surface.to_png()).and_then(Result::ok);
    let url = match encoded {
        Some(bytes) => format!("data:image/png;base64,{}", base64_encode(&bytes)),
        None => "data:image/png;base64,".to_string(),
    };
    Ok(JsValue::from(JsString::from(url)))
}

/// Web Audio 指纹来自 `OfflineAudioContext` 渲染出的浮点输出。
/// 没有 DSP：给稳定的噪声底，形状正确、值恒定。
pub(crate) fn install_audio(context: &mut Context) {
    let analyser = ObjectInitializer::new(context)
        .function(
            native(analyser_float_data),
            js_string!("getFloatFrequencyData"),
            1,
        )
        .function(
            native(analyser_byte_data),
            js_string!("getByteFrequencyData"),
            1,
        )
        .build();
    let constructor = constructible(context, &analyser, "AnalyserNode");
    let _ =
        context.register_global_property(js_string!("AnalyserNode"), constructor, Attribute::all());

    // `OfflineAudioContext` 是音频指纹的主入口（渲染固定频率再取浮点样本），
    // 缺了它探测脚本直接拿 undefined。
    let offline = ObjectInitializer::new(context)
        .property(js_string!("sampleRate"), JsValue::from(44100), Attribute::all())
        .function(native(audio_noop), js_string!("createOscillator"), 0)
        .function(native(audio_noop), js_string!("createDynamicsCompressor"), 0)
        .function(native(audio_noop), js_string!("createGain"), 0)
        .function(native(audio_noop), js_string!("startRendering"), 0)
        .function(native(audio_noop), js_string!("destination"), 0)
        .build();
    let offline_ctor = constructible(context, &offline, "OfflineAudioContext");
    let _ = context.register_global_property(
        js_string!("OfflineAudioContext"),
        offline_ctor,
        Attribute::all(),
    );
    // `AudioContext` 实例要能 `createAnalyser` 并读到 `sampleRate`。
    let audio = ObjectInitializer::new(context)
        .property(js_string!("sampleRate"), JsValue::from(44100), Attribute::all())
        .property(
            js_string!("currentTime"),
            JsValue::from(0),
            Attribute::all(),
        )
        .property(js_string!("state"), js_string!("running"), Attribute::all())
        .function(native(audio_create_analyser), js_string!("createAnalyser"), 0)
        .function(native(audio_noop), js_string!("createOscillator"), 0)
        .function(native(audio_noop), js_string!("createGain"), 0)
        .function(native(audio_noop), js_string!("resume"), 0)
        .function(native(audio_noop), js_string!("close"), 0)
        .build();
    let audio_ctor = constructible(context, &audio, "AudioContext");
    let _ = context.register_global_property(
        js_string!("AudioContext"),
        audio_ctor,
        Attribute::all(),
    );
}

fn audio_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `AudioContext.createAnalyser()` 要回一个有分析方法的节点，否则音频指纹链在第二步断掉。
fn audio_create_analyser(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let analyser = ObjectInitializer::new(context)
        .property(js_string!("fftSize"), JsValue::from(2048), Attribute::all())
        .property(
            js_string!("frequencyBinCount"),
            JsValue::from(1024),
            Attribute::all(),
        )
        .property(
            js_string!("smoothingTimeConstant"),
            JsValue::from(0.8),
            Attribute::all(),
        )
        .function(native(analyser_float_data), js_string!("getFloatFrequencyData"), 1)
        .function(native(analyser_byte_data), js_string!("getByteFrequencyData"), 1)
        .function(native(audio_noop), js_string!("connect"), 1)
        .build();
    Ok(analyser.into())
}

pub(crate) fn analyser_float_data(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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

pub(crate) fn analyser_byte_data(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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
pub(crate) fn install_speech(context: &mut Context) {
    let speech = ObjectInitializer::new(context)
        .function(native(speech_voices), js_string!("getVoices"), 0)
        .function(native(speech_noop), js_string!("speak"), 1)
        .function(native(speech_noop), js_string!("cancel"), 0)
        .function(native(speech_noop), js_string!("pause"), 0)
        .function(native(speech_noop), js_string!("resume"), 0)
        .property(
            js_string!("speaking"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(
            js_string!("pending"),
            JsValue::from(false),
            Attribute::all(),
        )
        .property(js_string!("paused"), JsValue::from(false), Attribute::all())
        .build();
    let _ =
        context.register_global_property(js_string!("speechSynthesis"), speech, Attribute::all());
    // `SpeechSynthesisUtterance` 是 `speak()` 的必填参数类型，缺了构造会抛错。
    let utterance = ObjectInitializer::new(context)
        .property(js_string!("text"), js_string!(""), Attribute::all())
        .property(js_string!("lang"), js_string!(""), Attribute::all())
        .build();
    let utterance_ctor = constructible(context, &utterance, "SpeechSynthesisUtterance");
    let _ = context.register_global_property(
        js_string!("SpeechSynthesisUtterance"),
        utterance_ctor,
        Attribute::all(),
    );
}

fn speech_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

pub(crate) fn speech_voices(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let voices = [
        ("Microsoft Huihui - Chinese (Simplified, PRC)", "zh-CN"),
        ("Microsoft David - English (United States)", "en-US"),
    ];
    let array = JsArray::new(context)?;
    for (name, lang) in voices {
        let voice = ObjectInitializer::new(context)
            .property(js_string!("name"), js_string!(name), Attribute::all())
            .property(js_string!("lang"), js_string!(lang), Attribute::all())
            .property(
                js_string!("localService"),
                JsValue::from(true),
                Attribute::all(),
            )
            .property(
                js_string!("default"),
                JsValue::from(name.starts_with("Microsoft Huihui")),
                Attribute::all(),
            )
            .property(js_string!("voiceURI"), js_string!(name), Attribute::all())
            .build();
        array.push(voice, context)?;
    }
    Ok(array.into())
}
