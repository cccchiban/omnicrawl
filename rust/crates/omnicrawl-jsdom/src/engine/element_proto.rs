//! `Element.prototype` 的构建：把 DOM 元素的方法表一次性挂到原型上。
//!
//! 从 `engine.rs` 拆出：这里只有「方法名 → 原生函数」的登记，具体实现都在
//! 父模块的 `el_*` / `cl_*` / `style_*` 系列函数里。

use super::*;

pub(super) fn build_element_prototype(context: &mut Context) -> JsObject {
    // 同一函数要挂到多个名字上时复用同一个对象：先统一转成 JsFunction 再进场。
    let realm = context.realm().clone();
    let text_get = NativeFunction::from_fn_ptr(el_get_text_content).to_js_function(&realm);
    let text_set = NativeFunction::from_fn_ptr(el_set_text_content).to_js_function(&realm);
    let html_get = NativeFunction::from_fn_ptr(el_get_inner_html).to_js_function(&realm);
    let html_set = NativeFunction::from_fn_ptr(el_set_inner_html).to_js_function(&realm);
    let tag_get = NativeFunction::from_fn_ptr(el_get_tag_name).to_js_function(&realm);

    ObjectInitializer::new(context)
        .accessor(
            js_string!("textContent"),
            Some(text_get.clone()),
            Some(text_set.clone()),
            Attribute::all(),
        )
        .accessor(
            js_string!("innerText"),
            Some(text_get),
            Some(text_set),
            Attribute::all(),
        )
        .accessor(
            js_string!("innerHTML"),
            Some(html_get),
            Some(html_set),
            Attribute::all(),
        )
        .accessor(
            js_string!("tagName"),
            Some(tag_get.clone()),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("nodeName"),
            Some(tag_get),
            None,
            Attribute::all(),
        )
        // `nodeType` 必须是访问器：文本节点是 3、元素是 1，写死成 1 会让
        // React 把文本节点当元素处理（`createTextInstance` 之后立刻崩）。
        .accessor(
            js_string!("nodeType"),
            Some(NativeFunction::from_fn_ptr(el_node_type).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("nodeValue"),
            Some(NativeFunction::from_fn_ptr(el_node_value).to_js_function(&realm)),
            Some(NativeFunction::from_fn_ptr(el_set_node_value).to_js_function(&realm)),
            Attribute::all(),
        )
        // 树遍历四件套 + `childNodes`：React 的协调器（`getChildNodes`、卸载遍历）
        // 只认这些，只有 `appendChild` 是挂不上一棵树的。
        .accessor(
            js_string!("childNodes"),
            Some(NativeFunction::from_fn_ptr(el_child_nodes).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("children"),
            Some(NativeFunction::from_fn_ptr(el_child_elements).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("firstChild"),
            Some(NativeFunction::from_fn_ptr(el_first_child).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("lastChild"),
            Some(NativeFunction::from_fn_ptr(el_last_child).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("nextSibling"),
            Some(NativeFunction::from_fn_ptr(el_next_sibling).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("previousSibling"),
            Some(NativeFunction::from_fn_ptr(el_previous_sibling).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("parentNode"),
            Some(NativeFunction::from_fn_ptr(el_parent_node).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("parentElement"),
            Some(NativeFunction::from_fn_ptr(el_parent_node).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("firstElementChild"),
            Some(NativeFunction::from_fn_ptr(el_first_child).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        // `ownerDocument` 必须指向真实的 document 对象：React 的事件系统会读
        // `el.ownerDocument` 再往上挂 `selectionchange` 监听，`undefined` 会变成
        // `TypeError: cannot convert 'null' or 'undefined' to object`。
        .accessor(
            js_string!("ownerDocument"),
            Some(NativeFunction::from_fn_ptr(el_owner_document).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(el_get_attribute),
            js_string!("getAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_set_attribute),
            js_string!("setAttribute"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(el_has_attribute),
            js_string!("hasAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove_attribute),
            js_string!("removeAttribute"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_append_child),
            js_string!("appendChild"),
            1,
        )
        // React 的提交阶段只用 `insertBefore` 落位，`appendChild` 是它的 `null` 特例。
        .function(
            NativeFunction::from_fn_ptr(el_insert_before),
            js_string!("insertBefore"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove_child),
            js_string!("removeChild"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_replace_child),
            js_string!("replaceChild"),
            2,
        )
        // `cloneNode`：模板/Portal 库把 `<template>` 内容复制进真实树时用它。
        .function(
            NativeFunction::from_fn_ptr(el_clone_node),
            js_string!("cloneNode"),
            1,
        )
        // `replaceChildren`：React 的 effect 里常用它一次性换掉整批子节点。
        .function(
            NativeFunction::from_fn_ptr(el_replace_children),
            js_string!("replaceChildren"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove),
            js_string!("remove"),
            0,
        )
        // `contains` 是 React-Modal、Portal、tooltip 库卸载时的必经判断：
        // 缺了它错误边界接住异常，整棵子树退回成一句 "not a callable function"。
        .function(
            NativeFunction::from_fn_ptr(el_contains),
            js_string!("contains"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_matches),
            js_string!("matches"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_closest),
            js_string!("closest"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_query_selector),
            js_string!("querySelector"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_query_selector_all),
            js_string!("querySelectorAll"),
            1,
        )
        // 布局相关：没有渲染引擎，给稳定的占位值而不是 undefined。
        // 页面脚本普遍拿 `clientWidth` 做「元素是否可见」判断，undefined 会让整段逻辑抛错。
        .accessor(
            js_string!("clientWidth"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("clientHeight"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("offsetWidth"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .accessor(
            js_string!("offsetHeight"),
            Some(NativeFunction::from_fn_ptr(el_zero).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        // `getBoundingClientRect` 是布局库（header 偏移、懒加载、动画）的高频入口：
        // 缺了它是 `not a callable function`，整棵子树被错误边界替换。
        .function(
            NativeFunction::from_fn_ptr(el_bounding_rect),
            js_string!("getBoundingClientRect"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(el_zero),
            js_string!("getClientRects"),
            0,
        )
        // `classList` 在真实 DOM 里是 getter 而非方法：注册成方法会让
        // `element.classList.remove(...)` 变成「对函数取属性」，拿到 undefined。
        .accessor(
            js_string!("classList"),
            Some(NativeFunction::from_fn_ptr(el_class_list).to_js_function(&realm)),
            None,
            Attribute::all(),
        )
        .function(
            NativeFunction::from_fn_ptr(el_insert_adjacent),
            js_string!("insertAdjacentHTML"),
            2,
        )
        // 返回已 resolve 的 Promise：fullscreen polyfill 会 `!result && new Promise(...)`，
        // 拿到 undefined 时它转而监听 fullscreenchange 事件，那条路在没有事件循环时永远不结束。
        .function(
            NativeFunction::from_fn_ptr(el_resolve_promise),
            js_string!("requestFullscreen"),
            0,
        )
        .function(NativeFunction::from_fn_ptr(el_noop), js_string!("click"), 0)
        .function(NativeFunction::from_fn_ptr(el_noop), js_string!("focus"), 0)
        // 元素也是 EventTarget：页面脚本普遍在元素上挂监听（`e.addEventListener("click", ...)`）。
        // 沙箱没有事件循环，监听器不会因此被触发，但**方法必须存在**，
        // 否则脚本在挂监听这一步就抛错，后面的渲染代码全部不执行。
        .function(
            NativeFunction::from_fn_ptr(el_add_listener),
            js_string!("addEventListener"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(el_remove_listener),
            js_string!("removeEventListener"),
            2,
        )
        // `getElementsByTagName` 与 `getElementsByClassName`：老代码与 polyfill 常用，
        // 缺失会让「探测 → 遍历 → 写入」这条链在第一步就断掉。
        .function(
            NativeFunction::from_fn_ptr(el_get_elements_by_tag_name),
            js_string!("getElementsByTagName"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(el_get_elements_by_class_name),
            js_string!("getElementsByClassName"),
            1,
        )
        // Canvas 尺寸必须是访问器：`canvas.width = 200` 要改位图尺寸并清空内容，
        // 直接把宽度写成固定属性会得到「赋值无效」这个真实浏览器里不存在的形态。
        .accessor(
            js_string!("width"),
            Some(NativeFunction::from_fn_ptr(el_canvas_get_width).to_js_function(&realm)),
            Some(NativeFunction::from_fn_ptr(el_canvas_set_width).to_js_function(&realm)),
            Attribute::all(),
        )
        .accessor(
            js_string!("height"),
            Some(NativeFunction::from_fn_ptr(el_canvas_get_height).to_js_function(&realm)),
            Some(NativeFunction::from_fn_ptr(el_canvas_set_height).to_js_function(&realm)),
            Attribute::all(),
        )
        // `getContext` 是 Canvas 指纹的入口：缺了它，探测脚本在第一步就抛
        // `not a callable function`，整段能力判定拿不到结果。
        .function(
            NativeFunction::from_fn_ptr(el_get_context),
            js_string!("getContext"),
            1,
        )
        // `HTMLCanvasElement.toDataURL()`：指纹探测的第二个入口（读像素哈希）。
        .function(
            NativeFunction::from_fn_ptr(el_to_data_url),
            js_string!("toDataURL"),
            0,
        )
        .build()
}
