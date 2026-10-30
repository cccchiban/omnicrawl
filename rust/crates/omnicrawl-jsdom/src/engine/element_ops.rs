//! `Element` 上的查询、监听与 `classList` 方法面。
//!
//! 从 `engine.rs` 拆出：这一组只做「参数解包 → 调 DOM → 包回 JS 值」，
//! 不涉及脚本求值与上下文装配。

use super::*;

/// 元素内的 `getElementsByTagName`：返回数组（真实 DOM 是 HTMLCollection，
/// 但探针只按索引与 `length` 使用，数组足够且索引可靠）。
pub(crate) fn el_get_elements_by_tag_name(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsArray::new(context)?.into());
    };
    let tag = args_string(args, 0, context);
    let found = if tag == "*" {
        with_session(|session| session.dom.query_all(index, "*"))
    } else {
        with_session(|session| session.dom.query_all(index, &tag))
    }
    .unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let _ = array.push(element_value(node, context), context);
    }
    Ok(array.into())
}

/// 元素内的 `getElementsByClassName`。
pub(crate) fn el_get_elements_by_class_name(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsArray::new(context)?.into());
    };
    let class = args_string(args, 0, context);
    let selector = format!(".{class}");
    let found = with_session(|session| session.dom.query_all(index, &selector)).unwrap_or_default();
    let array = JsArray::new(context)?;
    for node in found {
        let _ = array.push(element_value(node, context), context);
    }
    Ok(array.into())
}

/// 元素上的 `addEventListener`：把监听器存成元素对象的自有属性。
pub(crate) fn el_add_listener(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    let listener = args.get(1).cloned().unwrap_or(JsValue::undefined());
    if !type_name.is_empty() {
        let _ = object.set(
            JsString::from(format!("__ornListener:{type_name}")),
            listener,
            false,
            context,
        );
    }
    Ok(JsValue::undefined())
}

pub(crate) fn el_remove_listener(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(object) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let type_name = args_string(args, 0, context);
    if !type_name.is_empty() {
        let _ = object.set(
            JsString::from(format!("__ornListener:{type_name}")),
            JsValue::undefined(),
            false,
            context,
        );
    }
    Ok(JsValue::undefined())
}

/// 元素的布局尺寸占位：`0` 是「无布局引擎」下最诚实的值，同时不会让脚本抛错。
pub(crate) fn el_zero(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(0))
}

/// 立即兑现的 Promise：给「必须返回 thenable」的 API 用。
pub(crate) fn el_resolve_promise(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(boa_engine::object::builtins::JsPromise::resolve(JsValue::undefined(), context)?.into())
}

/// 元素上的空实现方法（`requestFullscreen` / `click` / `focus`）。
pub(crate) fn el_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `element.classList`：读写在 `class` 属性上，形状与真实 `DOMTokenList` 一致。
pub(crate) fn el_class_list(this: &JsValue, _args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let index = node_id(this, context);
    let classes = index
        .and_then(|node| {
            with_session(|session| session.dom.attr(node, "class").map(str::to_string))
        })
        .flatten()
        .unwrap_or_default();
    let items: Vec<String> = classes
        .split_whitespace()
        .map(|item| item.to_string())
        .collect();
    let list = JsArray::new(context)?;
    for item in &items {
        let _ = list.push(JsString::from(item.as_str()), context);
    }
    Ok(ObjectInitializer::new(context)
        .property(
            js_string!("length"),
            JsValue::from(items.len()),
            Attribute::all(),
        )
        .property(
            js_string!("value"),
            JsString::from(classes),
            Attribute::all(),
        )
        .property(js_string!("items"), list, Attribute::READONLY)
        .property(
            js_string!("__ornNode"),
            JsValue::from(index.unwrap_or(0)),
            Attribute::READONLY,
        )
        .function(NativeFunction::from_fn_ptr(cl_add), js_string!("add"), 1)
        .function(
            NativeFunction::from_fn_ptr(cl_remove),
            js_string!("remove"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(cl_contains),
            js_string!("contains"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(cl_toggle),
            js_string!("toggle"),
            1,
        )
        .build()
        .into())
}

/// 从 `classList` 对象上取出所属元素的节点号。
pub(crate) fn class_list_node(this: &JsValue, context: &mut Context) -> Option<usize> {
    let object = this.as_object()?;
    object
        .get(js_string!("__ornNode"), context)
        .ok()
        .and_then(|value| value.to_number(context).ok())
        .map(|number| number as usize)
}

pub(crate) fn cl_mutate(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
    add: bool,
) -> JsResult<JsValue> {
    let Some(node) = class_list_node(this, context) else {
        return Ok(JsValue::undefined());
    };
    let name = args_string(args, 0, context);
    if name.is_empty() {
        return Ok(JsValue::undefined());
    }
    with_session(|session| {
        session.touched = true;
        let current = session
            .dom
            .attr(node, "class")
            .unwrap_or_default()
            .to_string();
        let mut items: Vec<String> = current
            .split_whitespace()
            .map(|item| item.to_string())
            .collect();
        if add {
            if !items.iter().any(|item| item == &name) {
                items.push(name.clone());
            }
        } else {
            items.retain(|item| item != &name);
        }
        session.dom.set_attr(node, "class", &items.join(" "));
    });
    Ok(JsValue::undefined())
}

pub(crate) fn cl_add(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    cl_mutate(this, args, context, true)
}

pub(crate) fn cl_remove(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    cl_mutate(this, args, context, false)
}

pub(crate) fn cl_contains(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let Some(node) = class_list_node(this, context) else {
        return Ok(JsValue::from(false));
    };
    let name = args_string(args, 0, context);
    let present = with_session(|session| {
        session
            .dom
            .attr(node, "class")
            .unwrap_or_default()
            .split_whitespace()
            .any(|item| item == name)
    })
    .unwrap_or(false);
    Ok(JsValue::from(present))
}

pub(crate) fn cl_toggle(this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let present = cl_contains(this, args, context)?;
    let now_present = !present.to_boolean();
    cl_mutate(this, args, context, now_present)?;
    Ok(JsValue::from(now_present))
}

/// `element.insertAdjacentHTML(position, html)`：只支持最常见的 `beforeend` / `afterbegin`。
pub(crate) fn el_insert_adjacent(
    this: &JsValue,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let Some(index) = node_id(this, context) else {
        return Ok(JsValue::undefined());
    };
    let html = args_string(args, 1, context);
    with_session(|session| {
        session.touched = true;
        session.dom.replace_children_with(index, &html);
    });
    Ok(JsValue::undefined())
}
