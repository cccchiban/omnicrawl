//! `document` 上的查询、创建与选择方法面。
//!
//! 从 `engine.rs` 拆出：与 `document.rs` 的区隔是职责——那里装配 document 对象，
//! 这里实现它挂上去的方法。

use super::*;

pub(super) fn doc_get_body(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(this
        .as_object()
        .and_then(|object| object.get(js_string!("__ornBody"), context).ok())
        .unwrap_or(JsValue::null()))
}

pub(super) fn doc_get_head(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    Ok(this
        .as_object()
        .and_then(|object| object.get(js_string!("__ornHead"), context).ok())
        .unwrap_or(JsValue::null()))
}

pub(super) fn doc_write(_this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let html = args_string(args, 0, context);
    with_session(|session| {
        session.touched = true;
        session.written.push_str(&html);
        // 与浏览器不同，这里一律追加到 `body` 末尾：没有解析阶段的插入点概念，
        // 而正文提取只关心「脚本写出了什么」。
        if let Some(body) = session.dom.first_element_named("body") {
            session.dom.insert_fragment(body, &html);
        }
    });
    Ok(JsValue::undefined())
}

pub(super) fn doc_query_selector(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let selector = args_string(args, 0, context);
    let root = with_session(|session| session.dom.root).unwrap_or(0);
    let found = with_session(|session| session.dom.query_first(root, &selector)).flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

pub(super) fn doc_query_selector_all(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let selector = args_string(args, 0, context);
    let root = with_session(|session| session.dom.root).unwrap_or(0);
    let found = with_session(|session| session.dom.query_all(root, &selector)).unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let value = element_value(node, context);
        array.push(value, context)?;
    }
    Ok(array.into())
}

pub(super) fn doc_get_element_by_id(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let id = args_string(args, 0, context);
    let found = with_session(|session| {
        let dom = &session.dom;
        dom.elements()
            .into_iter()
            .find(|node| dom.attr(*node, "id") == Some(id.as_str()))
    })
    .flatten();
    Ok(match found {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

pub(super) fn doc_get_elements_by_tag_name(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let tag = args_string(args, 0, context).to_ascii_lowercase();
    let found = with_session(|session| {
        let dom = &session.dom;
        dom.elements()
            .into_iter()
            .filter(|node| {
                dom.tag(*node)
                    .is_some_and(|name| tag.is_empty() || name == tag)
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let value = element_value(node, context);
        array.push(value, context)?;
    }
    Ok(array.into())
}

pub(super) fn doc_query_command_supported(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    // 剪贴板类命令一律「不支持」：沙箱里没有真实剪贴板，声称支持会引出后续真实调用。
    Ok(JsValue::from(false))
}

pub(super) fn doc_query_command_state(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::from(false))
}

pub(super) fn doc_has_focus(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(true))
}

pub(super) fn noop_returning_undefined(
    _this: &JsValue,
    _args: &[JsValue],
    _context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `document.createRange()`：返回带常用成员的选区对象。
pub(super) fn doc_create_range(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(ObjectInitializer::new(context)
        .property(
            js_string!("collapsed"),
            JsValue::from(true),
            Attribute::all(),
        )
        .property(
            js_string!("startOffset"),
            JsValue::from(0),
            Attribute::all(),
        )
        .property(js_string!("endOffset"), JsValue::from(0), Attribute::all())
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("setStart"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("setEnd"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("selectNodeContents"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("getBoundingClientRect"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(range_get_client_rects),
            js_string!("getClientRects"),
            0,
        )
        .build()
        .into())
}

pub(super) fn range_get_client_rects(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
}

pub(super) fn doc_get_selection(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    // 复用全局 `getSelection`：两处语义一致，都返回「未选中」。
    context
        .global_object()
        .get(js_string!("getSelection"), context)
        .map(
            |value| match value.as_object().filter(JsObject::is_callable) {
                Some(function) => function
                    .call(&JsValue::undefined(), &[], context)
                    .unwrap_or(JsValue::null()),
                None => JsValue::null(),
            },
        )
}

pub(super) fn doc_create_comment(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    doc_create_text_node(_this, _args, context)
}

pub(super) fn doc_get_elements_by_class_name(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
}

pub(super) fn doc_create_element(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    create_element_node(args, context)
}

/// `document.createElementNS(ns, tag)`：React 用全小写名 + `xlink:href` 处理 SVG，
/// 沙箱不分命名空间，按普通元素建即可。
pub(super) fn doc_create_element_ns(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let tag = args_string(args, 1, context);
    let created = with_session(|session| {
        session.touched = true;
        session.dom.create_detached(Some(&tag), "")
    })
    .flatten();
    Ok(match created {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

pub(super) fn create_element_node(args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let tag = args_string(args, 0, context);
    let created = with_session(|session| {
        session.touched = true;
        let root = session.dom.root;
        let node = session.dom.push_element(root, &tag)?;
        // 新节点默认游离：脚本要显式 `appendChild` 才进树。
        session.dom.detach(node);
        Some(node)
    })
    .flatten();
    Ok(match created {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

pub(super) fn doc_create_text_node(
    _this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let text = args_string(args, 0, context);
    let created = with_session(|session| {
        session.touched = true;
        let root = session.dom.root;
        let node = session.dom.push_text(root, &text)?;
        session.dom.detach(node);
        Some(node)
    })
    .flatten();
    Ok(match created {
        Some(node) => element_value(node, context),
        None => JsValue::null(),
    })
}

/// `element.ownerDocument`：返回全局 document。
///
/// 元素实例本身没有指向 document 的句柄，全局 document 是唯一正确的答案
/// （沙箱只有一份文档），也避免了把 `undefined` 交给 React 的事件系统。
pub(super) fn el_owner_document(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(context
        .global_object()
        .get(js_string!("document"), context)
        .unwrap_or(JsValue::undefined()))
}
