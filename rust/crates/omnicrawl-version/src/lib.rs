//! 产品版本与出站 User-Agent。
//!
//! AI 请求网关的 UA 统一是 `omnicrawl/<产品版本>`：主模型请求、审查 / 顾问 / 决策模型、
//! 向量召回与语音合成都报同一个身份，网关侧按这一个标识就能认出调用方。
//! 版本号在构建期取自 `packages/cli/package.json`（见 `build.rs`），与发布版本一致。

/// 产品版本（构建期注入）。
pub const VERSION: &str = env!("OMNICRAWL_VERSION");

/// 出站请求的 User-Agent：`omnicrawl/<版本>`。
pub const USER_AGENT: &str = concat!("omnicrawl/", env!("OMNICRAWL_VERSION"));

#[cfg(test)]
mod tests {
    use super::*;

    /// 注入的版本必须等于 npm 产品版本：读到不该回落成 Cargo 包版本，否则网关侧的
    /// 版本标识会悄悄停在 0.0.1。
    #[test]
    fn injected_version_matches_the_npm_package() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../packages/cli/package.json");
        let text = std::fs::read_to_string(&manifest).expect("读不到 npm 包清单");
        let value: serde_json::Value = serde_json::from_str(&text).expect("npm 包清单不是合法 JSON");
        assert_eq!(
            value.get("version").and_then(|value| value.as_str()),
            Some(VERSION),
            "产品版本应与 packages/cli/package.json 一致"
        );
        assert_eq!(USER_AGENT, format!("omnicrawl/{VERSION}"));
    }
}
