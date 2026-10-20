//! 真实站点端到端：确认方案 B 下外链脚本真的被执行、指纹字段真的有值。
//!
//! 这类测试要联网，默认 `#[ignore]`；用
//! `cargo test -p omnicrawl-host --test fetched_scripts_live -- --ignored --nocapture` 运行。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use omnicrawl_jsdom::{run_with_scripts, Profile, ScriptSource};

use omnicrawl_host::tools::web_transport::{WebRequest, WebTransport, UreqWebTransport};

/// 极简加载器：用 ureq 通路取脚本（不涉及 omnicrawl-tls 的指纹细节，够验证执行链）。
struct LiveLoader {
    agent: UreqWebTransport,
    cache: Mutex<HashMap<String, String>>,
    loaded: Mutex<Vec<String>>,
}

impl ScriptSource for LiveLoader {
    fn load(&self, url: &str) -> Result<String, String> {
        if let Some(hit) = self.cache.lock().unwrap().get(url) {
            return Ok(hit.clone());
        }
        let request = WebRequest::new(
            url.to_string(),
            vec![(
                "user-agent".to_string(),
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"
                    .to_string(),
            )],
            Duration::from_secs(20),
        );
        let response = self.agent.send(&request).map_err(|error| error.message)?;
        if response.status >= 400 {
            return Err(format!("HTTP {}", response.status));
        }
        let text = response.text();
        self.cache.lock().unwrap().insert(url.to_string(), text.clone());
        self.loaded.lock().unwrap().push(url.to_string());
        Ok(text)
    }
}

#[test]
#[ignore = "需要联网，手动运行"]
fn browserleaks_javascript_fields_are_filled() {
    let agent = UreqWebTransport::new();
    let page_url = "https://browserleaks.com/javascript";
    let request = WebRequest::new(
        page_url.to_string(),
        vec![(
            "user-agent".to_string(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"
                .to_string(),
        )],
        Duration::from_secs(20),
    );
    let response = agent.send(&request).expect("取页面失败");
    let html = response.text();

    let loader = LiveLoader {
        agent,
        cache: Mutex::new(HashMap::new()),
        loaded: Mutex::new(Vec::new()),
    };
    let profile = Profile::for_family("chrome").with_href(page_url);
    let outcome = run_with_scripts(&html, &profile, Some(&loader));

    println!("执行的脚本数：{}", outcome.scripts);
    println!("加载的外链：{:?}", loader.loaded.lock().unwrap());
    println!("错误：{:?}", outcome.errors);
    // `js-inline`/`js-enabled` 这类单元格由外链脚本写入，是「脚本真的跑了」的直接证据。
    let text = &outcome.text;
    for probe in [
        "Chrome/145",
        "Win32",
        "Asia/Shanghai",
        "ANGLE (Intel",
        "userAgent",
        "1536",
        "16:9",
        "zh-CN",
        "Intel",
        "WebKit",
        "PDF Viewer",
        "4g",
    ] {
        println!("包含 {:?}: {}", probe, text.contains(probe));
    }
    // 打印屏幕段与 navigator 段的实际文本，确认字段真被写入。
    if let Some(index) = text.find("Screen Resolution") {
        println!("SCREEN: {}", &text[index..(index + 400).min(text.len())]);
    }
    assert!(
        outcome.scripts >= 2,
        "至少应执行内联脚本与外链脚本：{}",
        outcome.scripts
    );
    assert!(
        !loader.loaded.lock().unwrap().is_empty(),
        "外链脚本应当被真正取回"
    );
}
