//! 受控 JS 沙箱：为网页抓取与搜索重建「由脚本拼装出来」的正文。
//!
//! 目标范围：
//! - 页面已经把正文放在 HTML 里，只是被脚本挪动、解密、拼接时，本 crate 能把结果取回来；
//! - 页面正文需要脚本**再取一次数据**（SPA 外壳 + 接口）时，调用方可以通过
//!   [`HttpSource`] 把网络能力借给 `fetch` / `XMLHttpRequest`，脚本照常跑完再收正文；
//! - 同时把 JS 侧可见的环境伪装成有头浏览器（[`Profile`] 决定取值），
//!   让 `navigator` / `screen` / `Intl` / `WebGL` 这类探测点读到自洽的值。
//!
//! 沙箱本身仍然不持有网络能力：外链脚本由 [`ScriptSource`] 取回，
//! 脚本发起的 `fetch`/XHR 由 [`HttpSource`] 取回，访问控制全在调用方。
//! [`run_with_scripts`] 按文档顺序执行内联与外链脚本，[`run_with_profile`] 只跑内联脚本。
//!
//! 仍然不做的事：不模拟验证码、不绕过访问控制、不按真实时间等待。
//! `Canvas` / `WebGL` / 音频的输出是常量：形状与真机一致，取值不同——
//! 能通过「值是否稳定」的校验，但通不过与真机 GPU 结果逐位比对的校验。
//!
//! 入口有三个：[`run_inline_scripts`] 用默认 Chrome 画像跑内联脚本，
//! [`run_with_profile`] 让调用方传入与传输层档案一致的族，
//! [`run_with_scripts`] 额外执行外链脚本（需要 [`ScriptSource`]）；
//! 需要脚本内发请求时用 [`run_with_sources`]，同时给两种通道。
//!
//! ```no_run
//! let outcome = omnicrawl_jsdom::run_inline_scripts(
//!     "<html><body><div id=app></div><script>\
//!      document.getElementById('app').innerHTML = '<p>正文</p>';</script></body></html>",
//! );
//! assert!(outcome.text.contains("正文"));
//! ```

mod browser_env;
mod dom;
mod engine;
mod fetch_api;
mod fingerprint;

pub use engine::{
    run_inline_scripts, run_with_profile, run_with_scripts, run_with_sources, touches_dom,
    RunOutcome, ScriptKind, ScriptRef, ScriptSource, LOOP_ITERATION_LIMIT, MAX_DOM_NODES,
    MAX_INLINE_SCRIPTS, MAX_SCRIPT_BYTES,
};
pub use fetch_api::{HttpRequest, HttpResponse, HttpSource, MAX_RESPONSE_BYTES};
pub use fingerprint::Profile;
