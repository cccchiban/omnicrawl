use omnicrawl_jsdom::{run_inline_scripts, run_with_scripts, Profile, ScriptSource};

/// 按 URL 返回固定脚本正文的桩加载器；未登记 URL 返回错误。
struct StubLoader {
    bodies: Vec<(&'static str, &'static str)>,
}

impl ScriptSource for StubLoader {
    fn load(&self, url: &str) -> Result<String, String> {
        self.bodies
            .iter()
            .find(|(key, _)| *key == url)
            .map(|(_, body)| body.to_string())
            .ok_or_else(|| "桩加载器没有登记该地址".to_string())
    }
}

fn page(scripts: &str) -> String {
    format!("<html><body><div id=\"out\"></div>{scripts}</body></html>")
}

#[test]
fn external_script_runs_and_writes_dom() {
    let html = page(r#"<script src="https://example.com/app.js"></script>"#);
    let loader = StubLoader {
        bodies: vec![(
            "https://example.com/app.js",
            "document.getElementById('out').textContent = '来自外链';",
        )],
    };
    let outcome = run_with_scripts(&html, &Profile::for_family("chrome"), Some(&loader));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.text.contains("来自外链"), "{}", outcome.text);
    assert_eq!(outcome.scripts, 1);
}

/// 相对路径必须按页面地址解析成绝对地址再去取。
#[test]
fn relative_src_is_resolved_against_page_url() {
    let html = page(r#"<script src="js/app.js"></script>"#);
    let loader = StubLoader {
        bodies: vec![(
            "https://example.com/lib/js/app.js",
            "document.getElementById('out').textContent = 'rel';",
        )],
    };
    let profile = Profile::for_family("chrome").with_href("https://example.com/lib/page.html");
    let outcome = run_with_scripts(&html, &profile, Some(&loader));
    assert!(outcome.text.contains("rel"), "{}", outcome.text);
}

/// 文档顺序必须保留：后面的脚本依赖前面脚本定义的全局。
#[test]
fn scripts_run_in_document_order() {
    let html = page(
        r#"<script src="https://example.com/a.js"></script>
           <script>document.getElementById('out').textContent = window.__order;</script>"#,
    );
    let loader = StubLoader {
        bodies: vec![("https://example.com/a.js", "window.__order = 'A先跑';")],
    };
    let outcome = run_with_scripts(&html, &Profile::for_family("chrome"), Some(&loader));
    assert!(
        outcome.text.contains("A先跑"),
        "外链应先于其后的内联脚本执行：{}",
        outcome.text
    );
}

/// `defer` 脚本排在所有普通脚本之后，即使它出现在文档更前面。
#[test]
fn deferred_script_runs_last() {
    let html = page(
        r#"<script defer src="https://example.com/d.js"></script>
           <script>window.__seen = window.__defer ? 'too-early' : 'not-yet';</script>
           <script>document.getElementById('out').textContent = window.__seen;</script>"#,
    );
    let loader = StubLoader {
        bodies: vec![("https://example.com/d.js", "window.__defer = true;")],
    };
    let outcome = run_with_scripts(&html, &Profile::for_family("chrome"), Some(&loader));
    assert!(outcome.text.contains("not-yet"), "{}", outcome.text);
}

/// 加载失败只影响该脚本：与其无关的内联脚本照旧执行。
#[test]
fn loader_failure_does_not_block_other_scripts() {
    let html = page(
        r#"<script src="https://example.com/missing.js"></script>
           <script>document.getElementById('out').textContent = '内联照跑';</script>"#,
    );
    let loader = StubLoader { bodies: vec![] };
    let outcome = run_with_scripts(&html, &Profile::for_family("chrome"), Some(&loader));
    assert!(outcome.text.contains("内联照跑"), "{}", outcome.text);
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
    assert!(
        outcome.errors[0].contains("取数失败"),
        "{:?}",
        outcome.errors
    );
}

/// 不给加载器时外链一律跳过：旧入口的命令保持完全不变。
#[test]
fn without_loader_external_scripts_are_skipped() {
    let outcome = run_inline_scripts(&page(
        r#"<script src="https://example.com/app.js"></script>
           <script>document.getElementById('out').textContent = 'only-inline';</script>"#,
    ));
    assert!(outcome.text.contains("only-inline"), "{}", outcome.text);
    assert_eq!(outcome.scripts, 1);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
}

/// 只有 http(s) 的 `src` 才会被当成外链：`data:` / `javascript:` 不该去取。
#[test]
fn non_http_src_is_ignored() {
    let html = page(
        r#"<script src="data:text/javascript,window.__x=1"></script>
           <script>document.getElementById('out').textContent = 'ok';</script>"#,
    );
    let loader = StubLoader { bodies: vec![] };
    let outcome = run_with_scripts(&html, &Profile::for_family("chrome"), Some(&loader));
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.scripts, 1);
}

/// 模块脚本里带 `import` 时必须报脚本错误，而不是让解释器在链接阶段 panic。
#[test]
fn module_script_with_import_reports_error_instead_of_panicking() {
    let html = page(r#"<script type="module">import "./dep.js";window.__m = 1;</script>"#);
    let outcome = run_with_scripts(&html, &Profile::for_family("chrome"), None);
    assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
}

/// 没有 `import` 的模块脚本照常执行：顶层声明不污染全局，但仍能写 DOM。
#[test]
fn module_script_without_import_runs() {
    let html = page(
        r#"<script type="module">const text = '模块正文';document.getElementById('out').textContent = text;</script>"#,
    );
    let outcome = run_inline_scripts(&html);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.text.contains("模块正文"), "{}", outcome.text);
}
