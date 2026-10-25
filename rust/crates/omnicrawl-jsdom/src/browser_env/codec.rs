//! 由 `browser_env.rs` 按假装的全局对象拆出，语义未变。

use super::*;


use super::location::{network_unavailable};


use super::storage::{text_encode};

pub(crate) fn btoa(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Ok(JsValue::from(JsString::from(base64_encode(
        text.as_bytes(),
    ))))
}

pub(crate) fn atob(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
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

pub(crate) fn base64_decode(text: &str) -> Vec<u8> {
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

/// `Intl` 在 boa 里是可选 feature（需要 ICU），未开启时整个对象不存在。
/// 这里给一个最小可信实现：`resolvedOptions` 必须返回带 `timeZone` 的对象，
/// 因为时区是浏览器指纹的固定探测项。
pub(crate) fn install_intl(context: &mut Context, profile: &Profile) {
    if context
        .global_object()
        .get(js_string!("Intl"), context)
        .is_ok_and(|value| !value.is_undefined())
    {
        return;
    }
    let date_time_format_proto = ObjectInitializer::new(context)
        .function(
            native(dtf_resolved_options),
            js_string!("resolvedOptions"),
            0,
        )
        .function(native(noop), js_string!("format"), 1)
        .build();
    let date_time_format =
        boa_engine::object::FunctionObjectBuilder::new(context.realm(), native(dtf_construct))
            .name(js_string!("DateTimeFormat"))
            .constructor(true)
            .build();
    let _ = date_time_format.set(
        js_string!("prototype"),
        date_time_format_proto.clone(),
        false,
        context,
    );
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
        .property(
            js_string!("DateTimeFormat"),
            date_time_format,
            Attribute::all(),
        )
        .function(native(noop), js_string!("NumberFormat"), 0)
        .function(native(noop), js_string!("Collator"), 0)
        .property(
            js_string!("locale"),
            js_string!(profile.locale),
            Attribute::all(),
        )
        .build();
    let _ = context.register_global_property(js_string!("Intl"), intl, Attribute::all());
}

/// `new Intl.DateTimeFormat()` 必须返回带原型的实例，否则 `.resolvedOptions` 读不到。
pub(crate) fn dtf_construct(_this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let proto = context
        .global_object()
        .get(js_string!("__ornDtfProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    Ok(JsObject::from_proto_and_data(proto, boa_engine::builtins::object::OrdinaryObject).into())
}

pub(crate) fn dtf_resolved_options(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(js_string!("locale"), js_string!("zh-CN"), Attribute::all())
        .property(
            js_string!("calendar"),
            js_string!("gregory"),
            Attribute::all(),
        )
        .property(
            js_string!("numberingSystem"),
            js_string!("latn"),
            Attribute::all(),
        )
        .property(
            js_string!("timeZone"),
            js_string!("Asia/Shanghai"),
            Attribute::all(),
        )
        .build()
        .into())
}

/// boa 没有 `URL`。缺失会被直接读出，这里给最小实现（解析用字符串处理，足够探针使用）。
///
/// 必须是可 `new` 的构造器：打包产物里 `new URL(import.meta.url)`、
/// `new URL(path, location.href)` 是 router 与资源路径解析的常规写法。
pub(crate) fn install_url(context: &mut Context) {
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
    let _ = prototype.set(
        js_string!("constructor"),
        constructor.clone(),
        false,
        context,
    );
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

pub(crate) fn url_object_url(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    // 不做真实对象 URL：返回一个形状合法的字符串，调用方只当作标识使用。
    Ok(JsValue::from(js_string!(
        "blob:https://localhost/00000000-0000-4000-8000-000000000000"
    )))
}

pub(crate) fn url_parse(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let raw = args
        .first()
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
        })
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
pub(crate) fn absolutize_url(base: &str, target: &str) -> String {
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

pub(crate) fn url_to_string(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(this
        .as_object()
        .and_then(|object| object.get(js_string!("href"), context).ok())
        .unwrap_or_else(|| JsValue::from(js_string!(""))))
}

/// 极简切分：`scheme://host/path`。探针只用得到这几段，不需要完整 URL 语义。
pub(crate) fn split_url(raw: &str) -> (String, String, String, String) {
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

pub(crate) fn install_text_codec(context: &mut Context) {
    let encoder = ObjectInitializer::new(context)
        .function(native(text_encode), js_string!("encode"), 1)
        .function(native(text_encode_into), js_string!("encodeInto"), 2)
        .property(
            js_string!("encoding"),
            js_string!("utf-8"),
            Attribute::all(),
        )
        .build();
    // 必须是可 `new` 的构造器：`new TextEncoder().encode(...)` 是最常见写法。
    let constructor = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        native(text_encoder_construct),
    )
    .name(js_string!("TextEncoder"))
    .constructor(true)
    .build();
    let _ = constructor.set(js_string!("prototype"), encoder.clone(), false, context);
    let _ = encoder.set(
        js_string!("constructor"),
        constructor.clone(),
        false,
        context,
    );
    let _ =
        context.register_global_property(js_string!("TextEncoder"), constructor, Attribute::all());
    let _ = context.register_global_property(
        js_string!("__ornTextEncoderProto"),
        encoder,
        Attribute::READONLY,
    );
}

pub(crate) fn text_encoder_construct(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let _ = this;
    let proto = context
        .global_object()
        .get(js_string!("__ornTextEncoderProto"), context)
        .ok()
        .and_then(|value| value.as_object());
    Ok(JsObject::from_proto_and_data(proto, OrdinaryObject).into())
}

/// `encodeInto`：把编码结果写进调用方给的 `Uint8Array`，返回 `{read, written}`。
pub(crate) fn text_encode_into(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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
        .property(
            js_string!("written"),
            JsValue::from(written),
            Attribute::all(),
        )
        .build()
        .into())
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
pub(crate) fn install_network_stubs(context: &mut Context) {
    for name in ["fetch", "XMLHttpRequest", "WebSocket", "EventSource"] {
        let function =
            NativeFunction::from_fn_ptr(network_unavailable).to_js_function(context.realm());
        let _ = context.register_global_property(JsString::from(name), function, Attribute::all());
    }
}

