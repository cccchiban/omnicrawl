//! 运行时工厂的单元验收：默认 API 根、能力组装与协议校验。
//!
//! Python 侧同一层是各 Adapter 的 `create_runtime`（会构造 SDK 客户端），内核换成
//! `ChatEndpoint` 后没有可对照的对象，因此这里用「表驱动断言 + 报错文案」钉住行为。

use omnicrawl_llm::{
    build_runtime, conservative_capabilities, default_base_url, ModelCapabilities, ModelDescriptor,
    ProviderProfile,
};
use omnicrawl_protocol::Protocol;

fn profile(provider: &str) -> ProviderProfile {
    ProviderProfile {
        id: "p1".to_string(),
        provider: provider.to_string(),
        api_key: "key".to_string(),
        ..ProviderProfile::default()
    }
}

fn model(protocol: &str) -> ModelDescriptor {
    ModelDescriptor {
        model_id: "m1".to_string(),
        protocol: protocol.to_string(),
        ..ModelDescriptor::default()
    }
}

#[test]
fn provider_defaults_pick_protocol_and_base_url() {
    for (provider, protocol, base_url) in [
        (
            "openai",
            Protocol::OpenaiChatCompletions,
            "https://api.openai.com/v1",
        ),
        (
            "anthropic",
            Protocol::AnthropicMessages,
            "https://api.anthropic.com",
        ),
        (
            "gemini",
            Protocol::GeminiGenerateContent,
            "https://generativelanguage.googleapis.com",
        ),
    ] {
        let bundle = build_runtime(&profile(provider), &model("")).expect("应当构建成功");
        assert_eq!(bundle.protocol, protocol, "Provider {provider} 的默认协议");
        assert_eq!(
            bundle.base_url, base_url,
            "Provider {provider} 的默认 API 根"
        );
    }
}

#[test]
fn requested_protocol_wins_over_provider_default() {
    let mut target = profile("openai");
    target.default_protocol = "openai_chat_completions".to_string();
    let bundle = build_runtime(&target, &model("openai_responses")).expect("应当构建成功");
    assert_eq!(bundle.protocol, Protocol::OpenaiResponses);
    assert_eq!(bundle.base_url, default_base_url(Protocol::OpenaiResponses));
}

#[test]
fn configured_base_url_wins() {
    let mut target = profile("gemini");
    target.base_url = "https://gateway.internal/v1beta".to_string();
    let bundle = build_runtime(&target, &model("")).expect("应当构建成功");
    assert_eq!(bundle.base_url, "https://gateway.internal/v1beta");
}

#[test]
fn unknown_provider_and_protocol_mismatch_are_rejected() {
    let error = build_runtime(&profile("nope"), &model("")).expect_err("未知 Provider 必须失败");
    assert_eq!(error.message, "未知 Provider：nope");

    let error = build_runtime(&profile("gemini"), &model("anthropic_messages"))
        .expect_err("协议与 Provider 不匹配必须失败");
    assert!(
        error.message.contains("anthropic_messages"),
        "报错应点名协议：{}",
        error.message
    );
}

#[test]
fn capabilities_follow_conservative_then_model_layers() {
    // Gemini 的保守默认：不开推理、开视觉；模型声明显式关掉视觉并开推理。
    let declared = ModelCapabilities {
        vision: Some(false),
        reasoning: Some(true),
        ..ModelCapabilities::default()
    };
    let mut described = model("");
    described.capabilities = Some(declared);
    let bundle = build_runtime(&profile("gemini"), &described).expect("应当构建成功");

    let conservative = conservative_capabilities(Protocol::GeminiGenerateContent);
    assert_eq!(bundle.capabilities.vision, Some(false), "模型声明显式覆盖");
    assert_eq!(bundle.capabilities.reasoning, Some(true), "模型声明补上层");
    assert_eq!(
        bundle.capabilities.tools, conservative.tools,
        "未声明的字段保持保守默认"
    );
}

#[test]
fn context_window_only_overridden_when_positive() {
    let mut described = model("");
    described.context_window_tokens = 128_000;
    let bundle = build_runtime(&profile("openai"), &described).expect("应当构建成功");
    assert_eq!(bundle.capabilities.context_window_tokens, 128_000);

    described.context_window_tokens = 0;
    let bundle = build_runtime(&profile("openai"), &described).expect("应当构建成功");
    assert_eq!(
        bundle.capabilities.context_window_tokens,
        conservative_capabilities(Protocol::OpenaiChatCompletions).context_window_tokens,
        "0 不覆盖"
    );
}

#[test]
fn every_protocol_builds_a_runtime() {
    for (provider, protocol) in [
        ("openai", "openai_chat_completions"),
        ("openai", "openai_responses"),
        ("anthropic", "anthropic_messages"),
        ("gemini", "gemini_generate_content"),
    ] {
        let bundle = build_runtime(&profile(provider), &model(protocol))
            .unwrap_or_else(|error| panic!("{protocol} 应当构建成功：{}", error.message));
        // trait 对象本身不透明，这里只确认端点与协议就位（具体行为由各 provider 的回环测试覆盖）。
        assert_eq!(
            bundle.protocol,
            Protocol::parse(protocol).expect("合法协议")
        );
        assert!(!bundle.base_url.is_empty());
    }
}
