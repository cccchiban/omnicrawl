//! `fetch` / `XMLHttpRequest` 的沙箱行为：取数由调用方注入的通道完成。
//!
//! 这些用例只验证「翻译与回调驱动」是否正确：通道是本地桩，不发任何真实请求。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use omnicrawl_jsdom::{
    run_inline_scripts, run_with_sources, HttpRequest, HttpResponse, HttpSource, Profile,
};

/// 按路径返回固定 JSON 的桩通道；记录收到的请求供断言。
struct StubHttp {
    calls: Mutex<Vec<HttpRequest>>,
    counter: AtomicUsize,
}

impl StubHttp {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            counter: AtomicUsize::new(0),
        }
    }
}

impl HttpSource for StubHttp {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        self.calls.lock().unwrap().push(request.clone());
        self.counter.fetch_add(1, Ordering::Relaxed);
        let path = request.url.split("://").nth(1).unwrap_or_default();
        let path = path.splitn(2, '/').nth(1).unwrap_or_default();
        match path {
            "api/feed" => Ok(json(
                200,
                r#"{"size":2,"data":[{"id":1,"title":"第一篇文章"},{"id":2,"title":"第二篇"}]}"#,
            )),
            "api/missing" => Ok(json(404, r#"{"error":"not found"}"#)),
            "api/boom" => Err("连接被拒绝".to_string()),
            _ => Ok(json(200, "{}")),
        }
    }
}

fn json(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        status_text: if status == 200 {
            "OK".to_string()
        } else {
            "Not Found".to_string()
        },
        headers: vec![(
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )],
        body: body.as_bytes().to_vec(),
        redirected: false,
        url: String::new(),
    }
}

fn page(script: &str) -> String {
    format!("<html><body><div id=\"root\"></div><script>{script}</script></body></html>")
}

fn profile() -> Profile {
    Profile::for_family("chrome").with_href("https://blog.example.com/post/1")
}

/// SPA 的典型形态：`fetch(...).then(json => 渲染)`。
///
/// 这条用例是本次改造的核心目标：Promise 回调必须真的被执行，否则 DOM 永远是空的。
#[test]
fn fetch_then_renders_into_dom() {
    let html = page(
        r#"
        fetch('/api/feed')
          .then(function (response) { return response.json(); })
          .then(function (body) {
            document.getElementById('root').innerHTML =
              body.data.map(function (item) { return '<p>' + item.title + '</p>'; }).join('');
          });
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.text.contains("第一篇文章"),
        "正文应包含接口数据：{}",
        outcome.text
    );
    assert!(outcome.touched, "脚本应当操作过 DOM");
    // 相对地址必须按页面地址补全。
    let calls = http.calls.lock().unwrap();
    assert_eq!(
        calls[0].url, "https://blog.example.com/api/feed",
        "相对路径应按页面地址补全"
    );
}

/// `async`/`await` 形式同样要跑通：现代 bundle 普遍这么写。
#[test]
fn async_await_fetch_is_driven() {
    let html = page(
        r#"
        (async function () {
          const response = await fetch('https://blog.example.com/api/feed');
          const body = await response.json();
          document.getElementById('root').textContent = '共 ' + body.size + ' 篇';
        })();
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.text.contains("共 2 篇"), "{}", outcome.text);
}

/// 非 2xx 是「成功的响应」而不是 reject：`response.ok` 为 false，页面照常读 body。
#[test]
fn error_status_is_a_response_not_a_rejection() {
    let html = page(
        r#"
        fetch('/api/missing').then(function (response) {
          document.getElementById('root').textContent =
            'ok=' + response.ok + ' status=' + response.status;
        });
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.text.contains("ok=false status=404"),
        "{}",
        outcome.text
    );
}

/// 通道返回 Err 时是 rejected Promise：页面的 `.catch` 分支要能跑到。
#[test]
fn transport_failure_rejects_the_promise() {
    let html = page(
        r#"
        fetch('/api/boom')
          .then(function () { document.getElementById('root').textContent = '不该走到这里'; })
          .catch(function (error) {
            document.getElementById('root').textContent = '已进入 catch：' + error;
          });
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.text.contains("已进入 catch"), "{}", outcome.text);
    // 失败原因要浮到工具输出：页面常把 reject 静默吞掉。
    assert!(
        outcome
            .errors
            .iter()
            .any(|error| error.contains("fetch 失败")),
        "{:?}",
        outcome.errors
    );
}

/// `Response` 的文本视图与 `Headers` 读取。
#[test]
fn response_text_and_headers_are_readable() {
    let html = page(
        r#"
        fetch('/api/feed').then(function (response) {
          var text = '';
          response.text().then(function (body) {
            text = body.indexOf('第一篇文章') >= 0 ? 'body-ok' : 'body-missing';
            document.getElementById('root').textContent =
              text + '|' + response.headers.get('content-type');
          });
        });
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.text.contains("body-ok"), "{}", outcome.text);
    assert!(
        outcome.text.contains("application/json"),
        "头应当可读：{}",
        outcome.text
    );
}

/// XHR：同步派发 `onload`，页面在回调里渲染。
#[test]
fn xml_http_request_dispatches_load_synchronously() {
    let html = page(
        r#"
        var xhr = new XMLHttpRequest();
        xhr.open('GET', '/api/feed');
        xhr.onload = function () {
          var body = JSON.parse(xhr.responseText);
          document.getElementById('root').textContent = 'xhr 拿到 ' + body.size + ' 篇';
        };
        xhr.send();
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.text.contains("xhr 拿到 2 篇"), "{}", outcome.text);
}

/// `addEventListener('load')` 与 `readyState === DONE` 两种写法都要成立。
#[test]
fn xml_http_request_supports_listeners_and_ready_state() {
    let html = page(
        r#"
        var xhr = new XMLHttpRequest();
        xhr.open('GET', '/api/feed');
        xhr.addEventListener('load', function () {
          document.getElementById('root').textContent =
            'state=' + xhr.readyState + ' done=' + (xhr.readyState === XMLHttpRequest.DONE);
        });
        xhr.send();
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.text.contains("state=4 done=true"),
        "{}",
        outcome.text
    );
}

/// XHR 失败时派发 `onerror`，不抛异常中断脚本。
#[test]
fn xml_http_request_failure_fires_error_event() {
    let html = page(
        r#"
        var xhr = new XMLHttpRequest();
        xhr.open('GET', '/api/boom');
        xhr.onerror = function () {
          document.getElementById('root').textContent = 'onerror 已触发';
        };
        xhr.send();
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(outcome.text.contains("onerror 已触发"), "{}", outcome.text);
}

/// 没有接通道时保持旧行为：`fetch` 存在但 reject，页面 `.catch` 照常执行。
#[test]
fn without_source_fetch_still_rejects() {
    let html = page(
        r#"
        fetch('/api/feed')
          .then(function () { document.getElementById('root').textContent = '不该有数据'; })
          .catch(function () { document.getElementById('root').textContent = '没有通道'; });
        "#,
    );
    let outcome = run_inline_scripts(&html);
    assert!(outcome.text.contains("没有通道"), "{}", outcome.text);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
}

/// 请求数上限：防止页面把接口打到爆。
#[test]
fn request_count_is_capped() {
    let html = page(
        r#"
        var done = 0;
        for (var i = 0; i < 60; i++) {
          fetch('/api/feed').then(function () { done++; });
        }
        window.__done = function () { return done; };
        "#,
    );
    let http = StubHttp::new();
    let outcome = run_with_sources(&html, &profile(), None, Some(&http));
    assert!(
        http.counter.load(Ordering::Relaxed) <= 24,
        "实际发出 {} 次请求，超过上限",
        http.counter.load(Ordering::Relaxed)
    );
    assert!(
        outcome.errors.iter().any(|error| error.contains("请求数")),
        "超限应被记录：{:?}",
        outcome.errors
    );
}
