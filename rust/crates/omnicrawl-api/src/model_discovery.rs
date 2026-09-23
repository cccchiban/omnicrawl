//! 模型目录的两个网络端口（`omnicrawl-config` 的 `model_catalog` 注入点）。
//!
//! 目录聚合、发现缓存、失败文案都在 config 侧；这里只做需要真实网络的两件事：
//!
//! * [`discover_profile`]：一次 Profile 的模型列表发现。语义基准是 Python 各 Adapter 的
//!   `discover_models`，内核侧已有等价实现（`omnicrawl-llm` 按三家线上形态直接发 GET），
//!   这里只把 config 的 Profile 视图换成内核侧的形状，不再写第二套。
//! * [`fetch_model_list`]：拉取一个 `/models` 端点（兼容旧扁平模型列表用）。端点、请求头与
//!   超时由 config 侧构造（`ModelListRequest`），这里只负责发出去并把结果分成
//!   「正文 / 带状态码的失败 / 连接失败」三类。

use std::io::Read;
use std::time::Duration;

use omnicrawl_config::models::model_catalog::{
    ModelListFailure, ModelListOutcome, ModelListRequest, MAX_MODEL_LIST_BYTES,
};
use omnicrawl_config::models::ProviderProfile as ConfigProfile;
use omnicrawl_llm::ProviderProfile as KernelProfile;

/// 一次 Profile 发现：换成内核侧 Profile 后交给现成实现。
pub fn discover_profile(
    profile: &ConfigProfile,
    protocol: omnicrawl_protocol::Protocol,
    timeout_seconds: f64,
) -> omnicrawl_llm::DiscoveryResult {
    let kernel_profile = KernelProfile {
        id: profile.id.clone(),
        provider: profile.provider.clone(),
        base_url: profile.base_url.clone(),
        api_key: profile.api_key.clone(),
        user_agent: profile.user_agent.clone(),
        default_protocol: profile.default_protocol.clone(),
    };
    omnicrawl_llm::discover_models(&kernel_profile, protocol, timeout_seconds)
}

/// 拉取一次 `/models`：正文最多 `MAX_MODEL_LIST_BYTES + 1` 字节（与 Python 同口径）。
pub fn fetch_model_list(request: &ModelListRequest) -> ModelListOutcome {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        // 状态码不转错误：上游 4xx/5xx 的正文要留给诊断，错误分类自己给。
        .http_status_as_error(false)
        .build()
        .into();
    let timeout = Duration::from_secs_f64(request.timeout_seconds.max(1.0));
    let mut builder = agent
        .get(request.endpoint.as_str())
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build();
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }

    match builder.call() {
        Ok(response) => {
            let status = response.status().as_u16();
            if status >= 400 {
                return ModelListOutcome::Failure(ModelListFailure::Http {
                    status: Some(status as i64),
                });
            }
            let mut body: Vec<u8> = Vec::new();
            let mut reader = response
                .into_body()
                .into_reader()
                .take(MAX_MODEL_LIST_BYTES as u64 + 1);
            match reader.read_to_end(&mut body) {
                Ok(_) => ModelListOutcome::Body(body),
                Err(error) => ModelListOutcome::Failure(ModelListFailure::Io {
                    message: error.to_string(),
                }),
            }
        }
        Err(error) => ModelListOutcome::Failure(classify_failure(error)),
    }
}

/// 传输层失败分类：超时、DNS 与建连失败都归「连接失败」，其余归「读取失败」。
///
/// 与 Python 的三种异常分支同义（`HTTPError` 已在上面的状态码分支里处理）。
fn classify_failure(error: ureq::Error) -> ModelListFailure {
    let message = error.to_string();
    match error {
        ureq::Error::Io(inner) => ModelListFailure::Io {
            message: inner.to_string(),
        },
        _ => ModelListFailure::Connect { message },
    }
}
