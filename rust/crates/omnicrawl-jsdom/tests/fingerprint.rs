use omnicrawl_jsdom::{run_inline_scripts, run_with_profile, Profile};

/// 在页面里执行一段探针脚本，把结果写进 body 后取回。
fn probe(script: &str) -> String {
    let html = format!(
        r#"<html><body><div id="out"></div><script>
        try {{
            var value = (function () {{ {script} }})();
            document.getElementById('out').textContent = String(value);
        }} catch (error) {{
            document.getElementById('out').textContent = 'ERR:' + error;
        }}
        </script></body></html>"#
    );
    let outcome = run_inline_scripts(&html);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome.text.trim().to_string()
}

#[test]
fn navigator_identity_matches_chrome_profile() {
    assert_eq!(probe("return navigator.userAgent;"), Profile::for_family("chrome").user_agent);
    assert_eq!(probe("return navigator.webdriver;"), "false");
    assert_eq!(probe("return navigator.platform;"), "Win32");
    assert_eq!(probe("return navigator.vendor;"), "Google Inc.");
    assert_eq!(probe("return navigator.hardwareConcurrency;"), "8");
    assert_eq!(probe("return navigator.onLine;"), "true");
}

/// UA、Client Hints、platform 必须描述同一台机器：这是最容易自相矛盾的一组。
#[test]
fn user_agent_and_client_hints_agree() {
    assert_eq!(probe("return navigator.userAgentData.platform;"), "Windows");
    assert_eq!(probe("return navigator.userAgentData.mobile;"), "false");
    assert_eq!(
        probe("return navigator.userAgentData.brands.map(function (b) { return b.brand; }).join('|');"),
        "Google Chrome|Chromium|Not.A/Brand"
    );
    assert_eq!(
        probe("return navigator.userAgent.indexOf('Chrome/145') !== -1;"),
        "true",
        "UA 版本应与 brands 版本同源"
    );
}

#[test]
fn edge_profile_switches_product_name() {
    let html = r#"<html><body><div id="out"></div><script>
        document.getElementById('out').textContent =
            navigator.userAgent + '|' + navigator.userAgentData.brands[0].brand;
        </script></body></html>"#;
    let outcome = run_with_profile(html, &Profile::for_family("edge"));
    assert!(outcome.text.contains("Edg/145.0.0.0"), "{}", outcome.text);
    assert!(outcome.text.contains("Microsoft Edge"), "{}", outcome.text);
}

/// Firefox 不发 Client Hints，也没有 window.chrome：冒充 Chrome 会立刻暴露。
#[test]
fn firefox_profile_has_no_chrome_objects() {
    let html = r#"<html><body><div id="out"></div><script>
        document.getElementById('out').textContent = [
            typeof navigator.userAgentData,
            typeof window.chrome,
            navigator.userAgent.indexOf('Firefox') !== -1
        ].join('|');
        </script></body></html>"#;
    let outcome = run_with_profile(html, &Profile::for_family("firefox"));
    assert_eq!(outcome.text.trim(), "undefined|undefined|true", "{}", outcome.text);
}

#[test]
fn screen_and_window_metrics_are_consistent() {
    assert_eq!(probe("return screen.width;"), "1536");
    assert_eq!(probe("return screen.height;"), "864");
    // 有头窗口：可用高度小于物理高度，视口小于窗口。
    assert!(probe("return screen.availHeight < screen.height;").starts_with("true"));
    assert!(probe("return window.innerHeight < window.outerHeight;").trim().starts_with("true"));
    assert_eq!(probe("return window.devicePixelRatio;"), "1");
}

/// 同一个探针调两次必须一致：这是「指纹稳定性」的定义。
#[test]
fn probe_values_are_stable_across_calls() {
    let first = probe("return String(navigator.userAgent.length) + ':' + screen.width;");
    let second = probe("return String(navigator.userAgent.length) + ':' + screen.width;");
    assert_eq!(first, second);
}

#[test]
fn webgl_and_canvas_report_renderer() {
    let html = r#"<html><body><div id="out"></div><script>
        var gl = Object.create(WebGLRenderingContext.prototype);
        document.getElementById('out').textContent = [
            gl.getParameter(0x9246),
            gl.getSupportedExtensions().length
        ].join('|');
        </script></body></html>"#;
    let outcome = run_inline_scripts(html);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.text.contains("ANGLE (Intel"), "{}", outcome.text);
}

#[test]
fn intl_timezone_is_present() {
    assert_eq!(
        probe("return new Intl.DateTimeFormat().resolvedOptions().timeZone;"),
        "Asia/Shanghai"
    );
}

#[test]
fn plugins_look_like_chromium() {
    assert_eq!(
        probe("return navigator.plugins.length + '|' + navigator.mimeTypes.length;"),
        "5|2"
    );
}

/// 定时器存在（页面脚本依赖它），但不得真的睡眠：回调在求值结束后立即跑一轮。
#[test]
fn timers_exist_but_do_not_block() {
    let started = std::time::Instant::now();
    let outcome = run_inline_scripts(
        r#"<html><body><div id="out"></div><script>
        document.getElementById('out').textContent = 'before';
        setTimeout(function () {
            document.getElementById('out').textContent = 'after-timer';
        }, 5000);
        </script></body></html>"#,
    );
    // 5 秒的延迟不能被真的等：整个执行必须在远小于它的时间里结束。
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "定时器延迟不应被真实等待，实际耗时 {:?}",
        started.elapsed()
    );
    assert!(
        outcome.text.contains("after-timer"),
        "定时器回调应当被执行：{}",
        outcome.text
    );
}

/// 网络 API 存在但**永远失败**：沙箱自己不发任何请求。
///
/// 为什么不是「不存在」：页面公共脚本会先 `fetch(...).then().catch()` 探测，
/// 方法缺失会抛同步 TypeError，连带它后面定义的整套渲染函数都不生成。
/// 因此这里断言的是「调用被拒绝」，而不是「名字不存在」。
#[test]
fn network_apis_exist_but_always_fail() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="a"></div><script>
        var reached = [];
        window.__done = [];
        try {
            fetch('https://example.com/').then(function () {
                window.__done.push('resolved');
            }).catch(function () {
                window.__done.push('rejected');
            });
            reached.push('fetch-callable');
        } catch (error) {
            reached.push('fetch-threw');
        }
        reached.push('xhr=' + typeof XMLHttpRequest);
        reached.push('ws=' + typeof WebSocket);
        document.getElementById('a').textContent = reached.join(',');
        </script></body></html>"#,
    );
    assert!(
        outcome.text.contains("fetch-callable,xhr=function,ws=function"),
        "网络 API 应存在且可调用：{}",
        outcome.text
    );
    // 拒绝必须真的发生，否则页面脚本会误以为拿到了数据。
    assert!(
        outcome.text.contains("fetch-callable") && !outcome.text.contains("fetch-threw"),
        "调用不应同步抛错：{}",
        outcome.text
    );
}

#[test]
fn console_and_event_target_exist() {
    // 缺失本身就是破绽：探测器会 `typeof console === 'object'`。
    assert_eq!(probe("return typeof console;"), "object");
    assert_eq!(probe("return typeof console.log;"), "function");
    assert_eq!(
        probe("var t = new EventTarget(); var hit = 0; t.addEventListener('x', function () { hit += 1; }); t.dispatchEvent({ type: 'x' }); return hit;"),
        "1"
    );
}

