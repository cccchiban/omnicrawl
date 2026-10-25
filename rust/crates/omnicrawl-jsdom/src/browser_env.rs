//! 浏览器环境伪装：给沙箱补上探测器会读的全局对象。
//!
//! 为什么用 Rust 原生函数而不是 JS 补丁：boa 的 `Function.prototype.toString` 对
//! `NativeFunction` 直接输出 `function name() { [native code] }`，连 `name` 属性都不用伪造。
//! 用 JS 包一层反而要自己维护 `toString` 与属性描述符，任何一处漏掉就是可读的破绽。
//!
//! 边界（与 crate 其余部分一致，刻意保留）：
//! - **不主动联网**：不挂 `fetch` / `XMLHttpRequest` / `WebSocket` / `EventSource`。
//!   外链脚本由调用方注入的 [`crate::ScriptSource`] 取回，沙箱自己不持有网络能力。
//! - **不等待真实时间**：`setTimeout` 家族存在（页面脚本会依赖它），但回调在脚本求值结束后
//!   统一跑一轮，不按真实延迟睡眠，因此抓取不会被页面挂住。
//! - **不落盘**：`localStorage` / `sessionStorage` 是内存空实现，不跨请求复用。
//!
//! 所有返回值都是常量或纯计算：同一探针调两次必须得到同一结果，
//! 这是通过「指纹稳定性」校验的最低要求。

use boa_engine::builtins::object::OrdinaryObject;
use boa_engine::object::builtins::{JsArray, JsPromise};
use boa_engine::object::{FunctionObjectBuilder, ObjectInitializer};
use boa_engine::property::{Attribute, PropertyKey};
use boa_engine::{
    js_string, Context, JsError, JsObject, JsResult, JsString, JsValue, NativeFunction,
};

use crate::fingerprint::Profile;

/// 安装全部伪装。
pub fn install(context: &mut Context, profile: &Profile) -> JsResult<()> {
    // 每个 install_* 内部自带 `Ok(())` 兜底：单个对象缺失不该让整批伪装失败，
    // 否则一个探测点没补上会连带后面全部失效。
    install_console(context);
    install_event_target(context);
    install_navigator(context, profile)?;
    install_screen(context, profile);
    install_window_metrics(context, profile);
    install_location(context, profile);
    install_history(context);
    install_performance(context);
    install_chrome(context, profile);
    install_webgl(context);
    install_canvas(context);
    install_intl(context, profile);
    install_url(context);
    install_text_codec(context);
    install_crypto(context);
    install_storage(context);
    install_audio(context);
    install_speech(context);
    install_misc(context);
    install_task_and_clone(context);
    install_dom_interfaces(context);
    install_network_stubs(context);
    install_date_formatting(context);
    install_document_state(context, profile);
    install_window_identity(context);
    Ok(())
}

/// `EventTarget` 与它的三个方法：DOM 事件系统的最小可信形状。
///
/// 监听器存进 JS 数组而不是 Rust 侧哈希表：`WeakMap` 语义由 JS 自己维护，
/// 也避免 Rust 持有 `JsValue` 造成的生命周期纠缠。
/// 造一个可 `new` 的构造器，并把它与给定原型互指。
///
/// 页面会对这些类做 `new X()` 或 `instanceof`；用普通对象冒充会让
/// `new` 直接抛 `TypeError: not a callable function`，整段脚本中断。
fn constructible(context: &mut Context, prototype: &JsObject, name: &str) -> JsObject {
    ctor_with_proto(context, prototype, name)
}

fn noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// 观察者方法的空实现：`observe`/`disconnect` 等都要能调用且不抛错。
fn observer_noop(_this: &JsValue, _args: &[JsValue], _context: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn observer_empty_array(
    _this: &JsValue,
    _args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    Ok(JsArray::new(context)?.into())
}

fn native(function: boa_engine::native_function::NativeFunctionPointer) -> NativeFunction {
    NativeFunction::from_fn_ptr(function)
}

fn args_string(args: &[JsValue], index: usize, context: &mut Context) -> String {
    args.get(index)
        .cloned()
        .unwrap_or(JsValue::undefined())
        .to_string(context)
        .map(|text| text.to_std_string_escaped())
        .unwrap_or_default()
}


pub(crate) use console::flush_timers_once;
// Canvas 相关状态由引擎侧（`engine.rs` 的 `getContext`）与伪装侧（`media.rs`）共同访问：
// 位图按元素号存放，两边都要能读写同一份。
pub(crate) use media::{
    canvas_to_data_url, clear_canvas_state, remember_context, reset_surface, surface_for,
    DEFAULT_CANVAS_HEIGHT, DEFAULT_CANVAS_WIDTH,
};

use codec::{install_intl, install_network_stubs, install_text_codec, install_url};
use console::{install_console, install_misc, install_task_and_clone};
use events::{install_dom_interfaces, install_event_target, install_window_identity};
use location::{install_date_formatting, install_document_state, install_history, install_location, install_performance};
use media::{install_audio, install_canvas, install_speech, install_webgl};
use navigator::{install_chrome, install_navigator, install_screen, install_window_metrics};
use storage::{ctor_with_proto, define_slot_accessor, install_crypto, install_storage};
mod console;
mod events;
mod navigator;
mod location;
mod media;
mod codec;
mod storage;
