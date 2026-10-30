//! `document` 全局对象的构建：文档级访问器与查询方法。
//!
//! 从 `engine.rs` 拆出：这是「把快照 DOM 挂成 JS 对象」的装配段，
//! 与脚本收集、求值、微任务驱动没有关系。

use super::*;

pub(super) fn build_document(context: &mut Context) -> JsResult<JsValue> {
    let body_id = with_session(|session| {
        session
            .dom
            .first_element_named("body")
            .unwrap_or(session.dom.root)
    })
    .unwrap_or(0);
    let head_id =
        with_session(|session| session.dom.first_element_named("head").unwrap_or(0)).unwrap_or(0);
    let root_id = with_session(|session| session.dom.root).unwrap_or(0);

    let body = element_value(body_id, context);
    let head = element_value(head_id, context);
    let document_element = element_value(root_id, context);

    // 先把全部原生函数转成对象再进场：构造期独占 `context`，中途不能再借。
    let get_body = NativeFunction::from_fn_ptr(doc_get_body).to_js_function(context.realm());
    let get_head = NativeFunction::from_fn_ptr(doc_get_head).to_js_function(context.realm());

    let document = ObjectInitializer::new(context)
        .function(
            NativeFunction::from_fn_ptr(doc_write),
            js_string!("write"),
            1,
        )
        // `document` 自身也是节点：`nodeType` 为 9 是 React 与选择器库的常识判断。
        .property(js_string!("nodeType"), JsValue::from(9), Attribute::all())
        .property(
            js_string!("nodeName"),
            js_string!("#document"),
            Attribute::all(),
        )
        .property(js_string!("nodeValue"), JsValue::null(), Attribute::all())
        .function(
            NativeFunction::from_fn_ptr(doc_query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_element_by_id),
            js_string!("getElementById"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_elements_by_tag_name),
            js_string!("getElementsByTagName"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_element),
            js_string!("createElement"),
            1,
        )
        // React 对 SVG 走 `createElementNS`：缺它时图标类组件直接抛错，
        // 错误边界之外没有兜底，整棵子树都不渲染。
        .function(
            NativeFunction::from_fn_ptr(doc_create_element_ns),
            js_string!("createElementNS"),
            2,
        )
        // `queryCommandSupported` / `queryCommandEnabled`：Monaco 这类编辑器启动时会探测剪贴板能力，
        // 缺了就是 `not a callable function`，整份 bundle 在顶层中断。
        .function(
            NativeFunction::from_fn_ptr(doc_query_command_supported),
            js_string!("queryCommandSupported"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_command_supported),
            js_string!("queryCommandEnabled"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_query_command_state),
            js_string!("queryCommandState"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(noop_returning_undefined),
            js_string!("execCommand"),
            3,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_has_focus),
            js_string!("hasFocus"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_range),
            js_string!("createRange"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_selection),
            js_string!("getSelection"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_comment),
            js_string!("createComment"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_get_elements_by_class_name),
            js_string!("getElementsByClassName"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(doc_create_text_node),
            js_string!("createTextNode"),
            1,
        )
        .accessor(js_string!("body"), Some(get_body), None, Attribute::all())
        .accessor(js_string!("head"), Some(get_head), None, Attribute::all())
        .property(
            js_string!("documentElement"),
            document_element,
            Attribute::all(),
        )
        .property(js_string!("__ornBody"), body, Attribute::READONLY)
        .property(js_string!("__ornHead"), head, Attribute::READONLY)
        .build();
    Ok(document.into())
}
