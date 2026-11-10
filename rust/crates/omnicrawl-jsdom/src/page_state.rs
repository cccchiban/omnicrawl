//! 一次执行期间页面脚本产生的「页面动作」：`document.cookie` 写入与脚本发起的跳转。
//!
//! 沙箱没有网络也没有导航，能做的只是把这两类动作记下来交给调用方：抓取工具据此把
//! cookie 落进真实的罐、并按脚本给出的地址再发一次请求（挑战页的「写 cookie → 延时
//! 重跳」就是这么把正文取回来的）。
//!
//! 语义边界：
//! - cookie 只按 `name=value` 记账，`domain`/`path`/`expires` 由调用方的罐负责——
//!   这里要回答的只是「脚本认为罐里现在有什么」。
//! - 跳转只记第一条：浏览器里第一条导航就已经离开本页，后续 `replace` 不可能执行。

use std::cell::RefCell;

thread_local! {
    static ACTIONS: RefCell<PageActions> = RefCell::new(PageActions::default());
}

#[derive(Default)]
struct PageActions {
    /// 脚本请求的跳转目标（原样记录，补全由调用方做）。
    navigation: Option<String>,
    /// 罐里的 `name → value`，同名覆盖。
    cookies: Vec<(String, String)>,
    /// 每次 `document.cookie = ...` 的原始赋值串，按发生顺序。
    writes: Vec<String>,
}

/// 每次执行开始前清空：上一份文档的 cookie 与跳转不该影响这一份。
pub(crate) fn reset() {
    ACTIONS.with(|slot| *slot.borrow_mut() = PageActions::default());
}

pub(crate) fn record_navigation(target: &str) {
    let target = target.trim();
    if target.is_empty() {
        return;
    }
    ACTIONS.with(|slot| {
        let mut state = slot.borrow_mut();
        if state.navigation.is_none() {
            state.navigation = Some(target.to_string());
        }
    });
}

pub(crate) fn record_cookie(assignment: &str) {
    // 只取第一段 `name=value`，其余是属性。
    let Some((name, value)) = assignment
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
    else {
        return;
    };
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    let value = value.trim().to_string();
    ACTIONS.with(|slot| {
        let mut state = slot.borrow_mut();
        state.writes.push(assignment.trim().to_string());
        match state.cookies.iter_mut().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = value,
            None => state.cookies.push((name.to_string(), value)),
        }
    });
}

/// 罐内容的 `Cookie` 头形态：脚本读 `document.cookie` 时拿到的就是它。
pub(crate) fn cookie_header() -> String {
    ACTIONS.with(|slot| {
        slot.borrow()
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

pub(crate) fn cookie_writes() -> Vec<String> {
    ACTIONS.with(|slot| slot.borrow().writes.clone())
}

pub(crate) fn navigation() -> Option<String> {
    ACTIONS.with(|slot| slot.borrow().navigation.clone())
}
