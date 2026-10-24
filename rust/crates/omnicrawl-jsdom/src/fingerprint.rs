//! 浏览器画像：把「传输层已经在用的身份」翻译成 JS 侧可见的字段。
//!
//! 只为**一致性**服务：`navigator.userAgent`、`navigator.userAgentData`、`sec-ch-ua`
//! 三者必须描述同一台机器，否则探测器一句比较就能识破。因此画像按传输层档案族
//! （chrome / edge / firefox / safari）而不是按页面任意生成。
//!
//! 明确不做的事：不伪造真实渲染结果。`Canvas` 像素、`WebGL` 渲染器字符串是常量，
//! 一致性校验（同一像素在不同调用下必须一致）能通过，但与真机 GPU 输出不同——
//! 这一点在 crate 文档里如实声明，不假装等价于真实浏览器。

/// 一个窗口环境下探测器能读到的全部静态事实。
#[derive(Debug, Clone)]
pub struct Profile {
    /// `navigator.userAgent`。
    pub user_agent: String,
    /// `navigator.appVersion`（UA 去掉 `Mozilla/` 前缀）。
    pub app_version: String,
    /// `navigator.vendor`。
    pub vendor: &'static str,
    /// `navigator.platform`。
    pub platform: &'static str,
    /// `navigator.userAgentData.brands` 的重量级版本号。
    pub brand_version: &'static str,
    /// `navigator.userAgentData.brands` 的产品名（Chromium 系才有）。
    pub brand_name: &'static str,
    /// `navigator.userAgentData.platform`。
    pub ch_platform: &'static str,
    /// `navigator.userAgentData.platformVersion`（Windows 11 = 19.0.0 起）。
    pub platform_version: &'static str,
    /// UA 里的完整版本，如 `145.0.0.0`。
    pub full_version: &'static str,
    /// 是否暴露 `navigator.userAgentData`。
    pub client_hints: bool,
    /// 是否暴露 `window.chrome`。
    pub chrome_object: bool,
    /// 视口宽度。
    pub width: u32,
    /// 视口高度。
    pub height: u32,
    /// `navigator.hardwareConcurrency`。
    pub hardware_concurrency: u32,
    /// `navigator.deviceMemory`。
    pub device_memory: u32,
    /// `Intl.DateTimeFormat().resolvedOptions().timeZone`。
    pub time_zone: &'static str,
    /// `Intl.DateTimeFormat().resolvedOptions().locale`。
    pub locale: &'static str,
    /// 当前页面地址：`location.href` 与 `document.URL` 都读它。
    pub href: String,
}

impl Profile {
    /// 按传输层档案族取画像；未知族按 chrome 处理（与 `identity_for` 的兜底一致）。
    pub fn for_family(family: &str) -> Self {
        match family {
            "firefox" => Self {
                user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:151.0) \
Gecko/20100101 Firefox/151.0"
                    .to_string(),
                app_version: "5.0 (Windows)".to_string(),
                vendor: "",
                platform: "Win32",
                brand_version: "151",
                brand_name: "Firefox",
                ch_platform: "Windows",
                platform_version: "10.0",
                full_version: "151.0",
                // Firefox 不发也不暴露 Client Hints。
                client_hints: false,
                chrome_object: false,
                width: 1536,
                height: 864,
                hardware_concurrency: 8,
                device_memory: 8,
                time_zone: "Asia/Shanghai",
                locale: "zh-CN",
                href: "https://example.com/".to_string(),
            },
            "safari" => Self {
                user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15"
                    .to_string(),
                app_version: "5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
(KHTML, like Gecko) Version/26.0 Safari/605.1.15"
                    .to_string(),
                vendor: "Apple Computer, Inc.",
                platform: "MacIntel",
                brand_version: "26",
                brand_name: "Safari",
                ch_platform: "macOS",
                platform_version: "15.0.0",
                full_version: "26.0",
                client_hints: false,
                chrome_object: false,
                width: 1440,
                height: 900,
                hardware_concurrency: 8,
                device_memory: 8,
                time_zone: "Asia/Shanghai",
                locale: "zh-CN",
                href: "https://example.com/".to_string(),
            },
            // chrome 与 edge 同属 Chromium：共享 TLS 档案，只在产品名上分开。
            other => {
                let edge = other == "edge";
                Self {
                    user_agent: format!(
                        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36{edge_suffix}",
                        edge_suffix = if edge { " Edg/145.0.0.0" } else { "" },
                    ),
                    app_version: format!(
                        "5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36{edge_suffix}",
                        edge_suffix = if edge { " Edg/145.0.0.0" } else { "" }
                    ),
                    vendor: "Google Inc.",
                    platform: "Win32",
                    brand_version: "145",
                    brand_name: if edge {
                        "Microsoft Edge"
                    } else {
                        "Google Chrome"
                    },
                    ch_platform: "Windows",
                    platform_version: "19.0.0",
                    full_version: "145.0.0.0",
                    client_hints: true,
                    chrome_object: true,
                    width: 1536,
                    height: 864,
                    hardware_concurrency: 8,
                    device_memory: 8,
                    time_zone: "Asia/Shanghai",
                    locale: "zh-CN",
                    href: "https://example.com/".to_string(),
                }
            }
        }
    }

    /// `navigator.userAgentData.brands` 的顺序与真实 Chromium 一致：
    /// 自己的产品名在前，随后 Chromium，最后一个 GREASE 品牌。
    pub fn brands(&self) -> Vec<(String, String)> {
        if !self.client_hints {
            return Vec::new();
        }
        vec![
            (self.brand_name.to_string(), self.brand_version.to_string()),
            ("Chromium".to_string(), self.brand_version.to_string()),
            ("Not.A/Brand".to_string(), "24".to_string()),
        ]
    }

    /// 把 `location` 指向真实地址：页面脚本常拿它做「是否被搬到别处」的判断。
    pub fn with_href(mut self, href: &str) -> Self {
        if !href.is_empty() {
            self.href = href.to_string();
        }
        self
    }

    /// `navigator.languages`：与 `Accept-Language` 的自洽集合。
    pub fn languages(&self) -> Vec<&'static str> {
        vec!["zh-CN", "zh", "en"]
    }
}

#[cfg(test)]
mod tests {
    use super::Profile;

    #[test]
    fn chromium_families_share_version_but_differ_in_product() {
        let chrome = Profile::for_family("chrome");
        let edge = Profile::for_family("edge");
        assert!(chrome.user_agent.contains("Chrome/145.0.0.0"));
        assert!(!chrome.user_agent.contains("Edg/"));
        assert!(edge.user_agent.contains("Edg/145.0.0.0"));
        // appVersion 是 UA 去掉 `Mozilla/` 前缀：产品差异必须一并体现。
        assert_eq!(
            chrome.app_version,
            chrome.user_agent.trim_start_matches("Mozilla/")
        );
        assert_eq!(
            edge.app_version,
            edge.user_agent.trim_start_matches("Mozilla/")
        );
        assert_ne!(chrome.app_version, edge.app_version);
        assert_ne!(chrome.brand_name, edge.brand_name);
    }

    #[test]
    fn user_agent_and_client_hints_agree_on_version() {
        for family in ["chrome", "edge", "firefox", "safari"] {
            let profile = Profile::for_family(family);
            if profile.client_hints {
                assert!(
                    profile.user_agent.contains(profile.brand_version),
                    "{family} 的 UA 与 Client Hints 版本不一致"
                );
                let brands = profile.brands();
                assert_eq!(brands.len(), 3, "{family} 应有三个品牌");
                assert_eq!(brands[0].0, profile.brand_name);
                assert_eq!(brands[1].0, "Chromium");
            } else {
                assert!(profile.brands().is_empty(), "{family} 不应有 brands");
            }
        }
    }

    #[test]
    fn firefox_and_safari_do_not_claim_chrome() {
        for family in ["firefox", "safari"] {
            let profile = Profile::for_family(family);
            assert!(!profile.chrome_object, "{family} 不应有 window.chrome");
            assert!(
                !profile.user_agent.contains("Chrome/"),
                "{family} 不应自称 Chrome"
            );
        }
    }
}
