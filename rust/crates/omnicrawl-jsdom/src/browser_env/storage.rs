//! 由 `browser_env.rs` 按假装的全局对象拆出，语义未变。

use super::*;

use super::console::{NEXT_TIMER_ID, PendingTimer, TIMERS};





/// `crypto.getRandomValues` / `crypto.randomUUID`。
///
/// boa 0.21 完全没有 `crypto`，而现代打包产物普遍用它生成请求 id、埋点标识与 UUID
/// （Cloudflare 的 beacon 脚本就是 `crypto.randomUUID()`）。缺了它不是「少一个探测点」，
/// 而是整段入口脚本第一行就抛 `ReferenceError`，后面所有渲染逻辑都不会执行。
///
/// 取值来自真实的系统随机源（`std::random` 之外的简单自增混合即可满足「随机且不重复」），
/// 这里刻意不用固定常量：页面常检查两次调用结果是否相同。
pub(crate) fn install_crypto(context: &mut Context) {
    let crypto = ObjectInitializer::new(context)
        .function(
            native(crypto_get_random_values),
            js_string!("getRandomValues"),
            1,
        )
        .function(native(crypto_random_uuid), js_string!("randomUUID"), 0)
        .build();
    let _ = context.register_global_property(js_string!("crypto"), crypto, Attribute::all());
}

/// 填充 `Uint8Array` / `Uint8ClampedArray` 的每个元素，返回同一实例（浏览器语义）。
pub(crate) fn crypto_get_random_values(
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
        let _ = object.set(
            index as u64,
            JsValue::from((state & 0xff) as u32),
            false,
            context,
        );
    }
    Ok(target)
}

pub(crate) fn crypto_random_uuid(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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
pub(crate) fn next_entropy() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos ^ (count.wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1
}

pub(crate) fn text_encode(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|value| {
            value
                .to_string(context)
                .map(|text| text.to_std_string_escaped())
        })
        .transpose()?
        .unwrap_or_default();
    let bytes = text.into_bytes();
    let array = boa_engine::object::builtins::JsUint8Array::from_iter(bytes, context)?;
    Ok(array.into())
}

pub(crate) fn install_storage(context: &mut Context) {
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

pub(crate) fn storage_get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) fn storage_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) fn storage_remove(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let key = args_string(args, 0, context);
    if let Some(store) = object.get(js_string!("__ornStore"), context)?.as_object() {
        let _ = store.delete_property_or_throw(JsString::from(key), context);
    }
    Ok(JsValue::undefined())
}

pub(crate) fn storage_clear(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let empty = ObjectInitializer::new(context).build();
    if let Some(object) = this.as_object() {
        let _ = object.set(js_string!("__ornStore"), empty, false, context);
    }
    Ok(JsValue::undefined())
}

pub(crate) fn storage_key(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::null())
}

/// 解析成 `[(名, 值)]`：解析规则按浏览器够用部分（`+` 当空格、百分号解码）。
pub(crate) fn parse_query(text: &str, context: &mut Context) -> Vec<(String, String)> {
    let text = text.strip_prefix('?').unwrap_or(text);
    let mut pairs = Vec::new();
    for piece in text.split('&').filter(|piece| !piece.is_empty()) {
        let (name, value) = match piece.split_once('=') {
            Some((name, value)) => (name, value),
            None => (piece, ""),
        };
        pairs.push((
            percent_decode(name, context),
            percent_decode(value, context),
        ));
    }
    pairs
}

pub(crate) fn percent_decode(text: &str, context: &mut Context) -> String {
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

pub(crate) fn js_string_literal(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

pub(crate) fn usp_pairs(this: &JsValue, context: &mut Context) -> Vec<(String, String)> {
    let Some(object) = this.as_object() else {
        return Vec::new();
    };
    let Ok(store) = object.get(js_string!("__ornParams"), context) else {
        return Vec::new();
    };
    let Some(store) = store
        .as_object()
        .and_then(|object| JsArray::from_object(object).ok())
    else {
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

pub(crate) fn usp_store(this: &JsValue, pairs: &[(String, String)], context: &mut Context) {
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
pub(crate) fn url_search_params_construct(
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
                        if let Some(array) = item
                            .as_object()
                            .and_then(|object| JsArray::from_object(object).ok())
                        {
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

pub(crate) fn usp_get(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) fn usp_get_all(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) fn usp_has(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    Ok(JsValue::from(
        usp_pairs(this, context).iter().any(|(key, _)| key == &name),
    ))
}

pub(crate) fn usp_set(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) fn usp_append(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let value = args_string(args, 1, context);
    let mut pairs = usp_pairs(this, context);
    pairs.push((name, value));
    usp_store(this, &pairs, context);
    Ok(JsValue::undefined())
}

pub(crate) fn usp_delete(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let name = args_string(args, 0, context);
    let pairs: Vec<(String, String)> = usp_pairs(this, context)
        .into_iter()
        .filter(|(key, _)| key != &name)
        .collect();
    usp_store(this, &pairs, context);
    Ok(JsValue::undefined())
}

pub(crate) fn usp_sort(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let mut pairs = usp_pairs(this, context);
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    usp_store(this, &pairs, context);
    Ok(JsValue::undefined())
}

pub(crate) fn usp_to_string(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = usp_pairs(this, context)
        .into_iter()
        .map(|(name, value)| format!("{}={}", encode_query(&name), encode_query(&value)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(JsValue::from(JsString::from(text)))
}

/// 编码规则取 `encodeURIComponent` 的等价：保留 `-_.!~*'()`，空格变 `+`。
pub(crate) fn encode_query(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn usp_for_each(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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

pub(crate) fn usp_keys(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let array = JsArray::new(context)?;
    for (name, _) in usp_pairs(this, context) {
        array.push(JsString::from(name), context)?;
    }
    Ok(array.into())
}

pub(crate) fn usp_values(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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
pub(crate) fn install_message_channel(context: &mut Context) {
    let channel_proto = ObjectInitializer::new(context).build();
    let port_proto = ObjectInitializer::new(context)
        .function(native(mc_post_message), js_string!("postMessage"), 1)
        .function(native(mc_add_listener), js_string!("addEventListener"), 2)
        .function(
            native(mc_remove_listener),
            js_string!("removeEventListener"),
            2,
        )
        .function(native(noop), js_string!("start"), 0)
        .function(native(noop), js_string!("close"), 0)
        .build();
    let port_ctor = ctor_with_proto(context, &port_proto, "MessagePort");
    let _ =
        context.register_global_property(js_string!("MessagePort"), port_ctor, Attribute::all());

    let channel_ctor =
        FunctionObjectBuilder::new(context.realm(), native(message_channel_construct))
            .name(js_string!("MessageChannel"))
            .length(0)
            .constructor(true)
            .build();
    let _ = channel_ctor.set(
        js_string!("prototype"),
        channel_proto.clone(),
        false,
        context,
    );
    let _ = channel_proto.set(
        js_string!("constructor"),
        channel_ctor.clone(),
        false,
        context,
    );
    let _ = context.register_global_property(
        js_string!("MessageChannel"),
        channel_ctor,
        Attribute::all(),
    );
}

pub(crate) fn message_channel_construct(
    this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
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
pub(crate) fn mc_post_message(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let peer = object
        .get(js_string!("__ornPeer"), context)
        .unwrap_or(JsValue::undefined());
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
                let function = array
                    .at(index as i64 + 1, context)
                    .unwrap_or(JsValue::undefined());
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
pub(crate) fn enqueue_message_task(handler: JsObject, peer: JsValue, event: JsValue) {
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

pub(crate) fn mc_add_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
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
            let _ = object.set(
                js_string!("__ornPortListeners"),
                array.clone(),
                false,
                context,
            );
            array
        }
    };
    listeners.push(JsString::from(kind), context)?;
    listeners.push(handler, context)?;
    Ok(JsValue::undefined())
}

pub(crate) fn mc_remove_listener(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `DOMParser` / `Blob` / `File` / `FormData` / `Worker` 这类「会被 `new` 的工具对象」。
///
/// 它们不参与正文渲染，但框架与埋点脚本会在启动时构造：
/// 缺构造器就是 `TypeError: not a constructor`，整份入口脚本中断。
pub(crate) fn install_parsers_and_blobs(context: &mut Context) {
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
    let _ =
        context.register_global_property(js_string!("URLSearchParams"), usp_ctor, Attribute::all());
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
            .property(
                js_string!("aborted"),
                JsValue::from(false),
                Attribute::all(),
            )
            .property(js_string!("result"), JsValue::null(), Attribute::all())
            .build();
        let constructor = constructible(context, &prototype, name);
        let _ =
            context.register_global_property(JsString::from(name), constructor, Attribute::all());
    }
}

/// 在对象上定义一个只读访问器：读 `__ornSlot:<name>`，赋值被静默忽略。
///
/// 用 `from_copy_closure_with_captures` 捕获属性名，getter 就能按名取槽，
/// 不必为每个字段各写一个 `fn`。`NativeFunction` 走 boa 的原生函数路径，
/// `Function.prototype.toString` 仍然输出 `[native code]`。
pub(crate) fn define_slot_accessor(context: &mut Context, object: &JsObject, name: &str) {
    let getter = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            |this, _args, captures, context| {
                let (name,): &(JsString,) = captures;
                let slot = format!("__ornSlot:{}", name.to_std_string_escaped());
                let Some(object) = this.as_object() else {
                    return Ok(JsValue::undefined());
                };
                Ok(object.get(JsString::from(slot), context).unwrap_or(JsValue::undefined()))
            },
            (JsString::from(name),),
        ),
    )
    .name(JsString::from(format!("get {name}")))
    .length(0)
    .build();
    let descriptor = boa_engine::property::PropertyDescriptor::builder()
        .get(getter)
        .enumerable(true)
        .configurable(true)
        .build();
    let _ = object.define_property_or_throw(JsString::from(name), descriptor, context);
}

/// 造一个可 `new` 的构造器：包装体把 `X.prototype` 作为新实例的原型。
///
/// 原生函数拿不到自己的 `prototype`，只能由这层包装在构造时补上；
/// 包装体是每次构造都要走的薄壳，成本可以忽略。
pub(crate) fn ctor_with_proto(context: &mut Context, prototype: &JsObject, name: &str) -> JsObject {
    let prototype_for_call = prototype.clone();
    let function = boa_engine::object::FunctionObjectBuilder::new(
        context.realm(),
        // 等价于 `function X() { return Object.create(X.prototype) }`。
        NativeFunction::from_copy_closure_with_captures(
            |_this, _args, captures, _context| {
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

pub(crate) fn dom_parser_parse(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // 返回 `document` 本身：调用方通常只要 `querySelector` / `body` 这些成员。
    Ok(context
        .global_object()
        .get(js_string!("document"), context)
        .unwrap_or(JsValue::undefined()))
}

