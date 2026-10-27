//! 向量召回的执行器：一次查询文本 → 一组候选文本，返回按语义相关度排好的顺序。
//!
//! 设计口径与检索重排（[`crate::tools::decision_search`]）一致：**失败一律 fail-open**，
//! 返回 `None` 让调用方继续用本地规则排序。这里多一层取舍——向量召回要么整池可用，
//! 要么整池放弃，绝不拿半份向量去算相似度。
//!
//! 三条兜底（按序检查，任一不满足就整次跳过，不报错）：
//! 1. 没有启用渠道 / 没有凭据 → 不发起任何请求；
//! 2. 单批请求失败（网络、鉴权、响应形状不对）→ 跳过该批；
//! 3. 任一批失败 → 整次召回作废（排序不能有缺口）。
//!
//! 向量按「模型 + 文本」缓存，命中即不再请求：记忆条目高度重复出现在多轮检索里，
//! 这一层缓存把外发次数压到与「新增或变更的文本」同阶。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::embedding::{
    load_embedding_model_configuration, EmbeddingChannelConfig,
};
use omnicrawl_core::diagnostics;

use crate::embedding_wire::{cosine_similarity, parse_embeddings, request_body};

const USER_AGENT: &str = omnicrawl_version::USER_AGENT;
/// 单条输入进请求前的截断长度：向量只看语义，不需要整段正文。
const INPUT_MAX_CHARS: usize = 2000;

/// 一个可用的向量渠道（凭据已解析，配置已归一化）。
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingChannel {
    pub model: String,
    pub base_url: String,
    /// 已解析出的凭据；为空表示不可用。
    pub api_key: String,
    pub dimensions: i64,
    pub batch_size: usize,
    pub timeout_seconds: u64,
}

impl EmbeddingChannel {
    fn from_config(channel: &EmbeddingChannelConfig, environment: &ConfigEnvironment) -> Self {
        Self {
            model: channel.model.clone(),
            base_url: channel.base_url.clone(),
            api_key: channel.resolve_api_key(environment),
            dimensions: channel.dimensions,
            batch_size: channel.batch_size.max(1) as usize,
            timeout_seconds: channel.timeout_seconds.max(1) as u64,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    /// 缓存键里的渠道指纹：换模型或换地址后旧向量不该再被复用。
    fn fingerprint(&self) -> String {
        format!("{}|{}|{}", self.base_url, self.model, self.dimensions)
    }
}

/// 向量缓存：`渠道指纹 + 文本` → 向量。纯内存，进程内复用。
#[derive(Default)]
pub struct EmbeddingCache {
    entries: Mutex<HashMap<String, Arc<Vec<f32>>>>,
}

impl EmbeddingCache {
    fn get(&self, key: &str) -> Option<Arc<Vec<f32>>> {
        self.entries.lock().ok()?.get(key).cloned()
    }

    fn put(&self, key: String, vector: Arc<Vec<f32>>) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(key, vector);
        }
    }

    /// 缓存里现有多少条（供测试与诊断）。
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 向量请求的传输层：真实实现走 HTTP；测试注入桩以便不起网络。
pub trait EmbeddingClient: Send + Sync {
    /// 按输入顺序返回向量；`Err` 表示该批不可用。
    fn embed(&self, channel: &EmbeddingChannel, inputs: &[String])
        -> Result<Vec<Vec<f32>>, String>;
}

/// 真实实现：OpenAI 兼容的 `POST {base_url}/embeddings`。
pub struct HttpEmbeddingClient;

impl EmbeddingClient for HttpEmbeddingClient {
    fn embed(
        &self,
        channel: &EmbeddingChannel,
        inputs: &[String],
    ) -> Result<Vec<Vec<f32>>, String> {
        if channel.api_key.trim().is_empty() {
            return Err(format!(
                "向量渠道缺少凭据：请在「向量模型」设置里填写 API Key 或设置对应环境变量。"
            ));
        }
        let body = request_body(&channel.model, inputs, channel.dimensions);
        let body_text =
            serde_json::to_string(&body).map_err(|error| format!("构造向量请求失败：{error}"))?;
        let text = post_embeddings(channel, &body_text)?;
        parse_embeddings(&text, inputs.len())
    }
}

fn post_embeddings(channel: &EmbeddingChannel, body_text: &str) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        // 状态码不转错误：上游 4xx/5xx 的正文要留给诊断。
        .http_status_as_error(false)
        .build()
        .into();
    let timeout = std::time::Duration::from_secs(channel.timeout_seconds);
    let response = agent
        .post(&channel.endpoint())
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", channel.api_key))
        .header("User-Agent", USER_AGENT)
        .send(body_text)
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    if status >= 400 {
        return Err(format!(
            "向量服务返回 HTTP {status}：{}",
            shorten(text.trim())
        ));
    }
    Ok(text)
}

/// 向量召回运行期：一个宿主一份，供所有记忆作用域共用。
#[derive(Clone)]
pub struct EmbeddingOptions {
    /// 是否启用向量召回（`[memory_embedding] enabled`）。
    pub enabled: bool,
    /// 默认向量渠道；`None` 表示没有可用渠道（未配置、未启用或缺凭据）。
    pub channel: Option<EmbeddingChannel>,
    pub cache: Arc<EmbeddingCache>,
    /// 传输实现（测试注入桩）。
    pub client: Arc<dyn EmbeddingClient>,
}

impl Default for EmbeddingOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: None,
            cache: Arc::new(EmbeddingCache::default()),
            client: Arc::new(HttpEmbeddingClient),
        }
    }
}

impl std::fmt::Debug for EmbeddingOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmbeddingOptions")
            .field("enabled", &self.enabled)
            .field("channel", &self.channel)
            .finish()
    }
}

impl EmbeddingOptions {
    /// 是否真正可用：开关打开、有渠道、有凭据。
    pub fn active(&self) -> bool {
        self.enabled
            && self
                .channel
                .as_ref()
                .map(|channel| !channel.api_key.trim().is_empty())
                .unwrap_or(false)
    }
}

/// 按配置装配向量召回运行期（TUI 与 API 共用同一口径）。
///
/// 未启用时连配置文件都不读：这是纯加速项，不该在启动路径上多花一次磁盘往返。
pub fn embedding_options_from_config(
    environment: &ConfigEnvironment,
    enabled: bool,
) -> EmbeddingOptions {
    if !enabled {
        return EmbeddingOptions::default();
    }
    let channel = load_embedding_model_configuration(environment, None)
        .ok()
        .and_then(|configuration| configuration.active_channel().cloned())
        .map(|channel| EmbeddingChannel::from_config(&channel, environment))
        .filter(|channel| !channel.api_key.trim().is_empty());
    EmbeddingOptions {
        enabled: true,
        channel,
        ..EmbeddingOptions::default()
    }
}

/// 一次向量召回的输入：查询 + 候选文本（下标即调用方的候选下标）。
pub struct EmbeddingRequest<'a> {
    pub channel: &'a EmbeddingChannel,
    pub cache: &'a EmbeddingCache,
    pub client: &'a (dyn EmbeddingClient + 'a),
    pub query: &'a str,
    pub candidates: &'a [String],
}

/// 倒数排名融合（RRF）：把「规则序」与「语义序」两路名次合成一个分数。
///
/// 单路打分彼此不可比（规则分是量纲自由的关键词加权，余弦相似度落在 -1..1），
/// 因此不直接相加，只比较名次：每个候选得 `Σ 1/(k + 名次)`，`k` 取 60（RRF 常用值，
/// 作用是压平原路内部的分数差距，让两路都只贡献「排在第几名」这一信息）。
/// 两路都靠前的候选自然胜出；任一路缺失时它退化成按该路名次排序。
pub fn fuse_rankings(rule_order: &[usize], semantic_order: &[usize]) -> Vec<usize> {
    const K: f64 = 60.0;
    let mut scores: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    for (rank, index) in rule_order.iter().enumerate() {
        *scores.entry(*index).or_insert(0.0) += 1.0 / (K + rank as f64 + 1.0);
    }
    for (rank, index) in semantic_order.iter().enumerate() {
        *scores.entry(*index).or_insert(0.0) += 1.0 / (K + rank as f64 + 1.0);
    }
    let mut fused: Vec<(usize, f64)> = scores.into_iter().collect();
    // 同分时按规则序里的名次排（规则序是兜底基准，语义只做加权），再退到下标保证稳定。
    fused.sort_by(|left, right| {
        let left_rule = rule_order.iter().position(|index| *index == left.0);
        let right_rule = rule_order.iter().position(|index| *index == right.0);
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left_rule.cmp(&right_rule))
            .then_with(|| left.0.cmp(&right.0))
    });
    fused.into_iter().map(|(index, _)| index).collect()
}

/// 给查询与候选都算向量，返回**候选下标及其余弦相似度**（按相似度降序）。
///
/// `None` 表示整次召回不可用，调用方应保留本地规则排序：
/// 没有候选、任一批请求失败、或某些候选取不到向量。
pub fn semantic_scores(
    options: &EmbeddingOptions,
    request: EmbeddingRequest<'_>,
) -> Option<Vec<(usize, f32)>> {
    if !options.active() || request.candidates.len() < 2 {
        return None;
    }
    let channel = request.channel;
    let fingerprint = channel.fingerprint();

    // 查询向量：单独一次请求（不参与批量化，缓存键带 `q:` 前缀以免与候选文本撞键）。
    let query_key = format!("{fingerprint}|q:{}", clamp_input(request.query));
    let query_vector = match request.cache.get(&query_key) {
        Some(vector) => vector,
        None => {
            let inputs = vec![clamp_input(request.query)];
            let vectors = request.client.embed(channel, &inputs).ok()?;
            let vector = Arc::new(vectors.into_iter().next()?);
            request.cache.put(query_key, Arc::clone(&vector));
            vector
        }
    };

    // 候选向量：先查缓存，只把未命中的按渠道批量上限分批送出去。
    let mut resolved: Vec<Option<Arc<Vec<f32>>>> = Vec::with_capacity(request.candidates.len());
    let mut pending: Vec<(usize, String, String)> = Vec::new();
    for (index, candidate) in request.candidates.iter().enumerate() {
        let text = clamp_input(candidate);
        let key = format!("{fingerprint}|c:{text}");
        match request.cache.get(&key) {
            Some(vector) => resolved.push(Some(vector)),
            None => {
                resolved.push(None);
                pending.push((index, key, text));
            }
        }
    }

    for batch in pending.chunks(channel.batch_size) {
        let inputs: Vec<String> = batch.iter().map(|(_, _, text)| text.clone()).collect();
        let vectors = match request.client.embed(channel, &inputs) {
            Ok(vectors) => vectors,
            Err(error) => {
                diagnostics::warn(format!("[host] 向量召回不可用，改用规则排序：{error}"));
                return None;
            }
        };
        if vectors.len() != batch.len() {
            diagnostics::warn(format!(
                "[host] 向量召回不可用，改用规则排序：返回 {} 条向量，期望 {} 条。",
                vectors.len(),
                batch.len()
            ));
            return None;
        }
        for ((index, key, _), vector) in batch.iter().zip(vectors.into_iter()) {
            let vector = Arc::new(vector);
            request.cache.put(key.clone(), Arc::clone(&vector));
            resolved[*index] = Some(vector);
        }
    }

    let mut scored: Vec<(usize, f32)> = Vec::with_capacity(resolved.len());
    for (index, slot) in resolved.iter().enumerate() {
        let vector = slot.as_ref()?;
        scored.push((index, cosine_similarity(&query_vector, vector)));
    }
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    Some(scored)
}

/// 输入收敛：空白压成单空格并截断，避免超长正文把请求撑爆。
fn clamp_input(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= INPUT_MAX_CHARS {
        return collapsed;
    }
    collapsed.chars().take(INPUT_MAX_CHARS).collect()
}

fn shorten(text: &str) -> String {
    let limit = 200;
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct StubClient {
        /// 每条输入返回的向量（按输入文本查表，缺省返回零向量）。
        vectors: HashMap<String, Vec<f32>>,
        fail: bool,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl StubClient {
        fn new(vectors: &[(&str, Vec<f32>)]) -> Self {
            Self {
                vectors: vectors
                    .iter()
                    .map(|(text, vector)| (text.to_string(), vector.clone()))
                    .collect(),
                fail: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing() -> Self {
            Self {
                vectors: HashMap::new(),
                fail: true,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl EmbeddingClient for StubClient {
        fn embed(
            &self,
            _channel: &EmbeddingChannel,
            inputs: &[String],
        ) -> Result<Vec<Vec<f32>>, String> {
            self.calls
                .lock()
                .unwrap()
                .push(inputs.iter().map(|text| text.to_string()).collect());
            if self.fail {
                return Err("桩：故意失败".to_string());
            }
            Ok(inputs
                .iter()
                .map(|text| {
                    self.vectors
                        .get(text)
                        .cloned()
                        .unwrap_or_else(|| vec![0.0, 0.0])
                })
                .collect())
        }
    }

    fn channel() -> EmbeddingChannel {
        EmbeddingChannel {
            model: "m".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            api_key: "k".to_string(),
            dimensions: 0,
            batch_size: 2,
            timeout_seconds: 5,
        }
    }

    #[test]
    fn inactive_without_key_or_switch() {
        let options = EmbeddingOptions::default();
        assert!(!options.active());

        let channel = EmbeddingChannel {
            api_key: String::new(),
            ..channel()
        };
        let options = EmbeddingOptions {
            enabled: true,
            channel: Some(channel),
            ..EmbeddingOptions::default()
        };
        assert!(!options.active());
    }

    #[test]
    fn orders_candidates_by_cosine_and_caches() {
        let client = StubClient::new(&[
            ("查询", vec![1.0, 0.0]),
            ("相近", vec![0.9, 0.1]),
            ("无关", vec![0.0, 1.0]),
        ]);
        let cache = EmbeddingCache::default();
        let candidates = vec!["无关".to_string(), "相近".to_string()];
        let options = EmbeddingOptions {
            enabled: true,
            channel: Some(channel()),
            cache: Arc::new(EmbeddingCache::default()),
            client: Arc::new(HttpEmbeddingClient),
        };
        let order = semantic_scores(
            &options,
            EmbeddingRequest {
                channel: &channel(),
                cache: &cache,
                client: &client,
                query: "查询",
                candidates: &candidates,
            },
        )
        .expect("应当算出顺序");
        let indices: Vec<usize> = order.iter().map(|(index, _)| *index).collect();
        assert_eq!(indices, vec![1, 0], "相近的候选应排在前面");
        assert!(order[0].1 > order[1].1, "相似度应递减");

        // 再跑一次：全部命中缓存，不再发起请求。
        let before = client.calls.lock().unwrap().len();
        let _ = semantic_scores(
            &options,
            EmbeddingRequest {
                channel: &channel(),
                cache: &cache,
                client: &client,
                query: "查询",
                candidates: &candidates,
            },
        );
        assert_eq!(client.calls.lock().unwrap().len(), before, "缓存应当命中");
    }

    #[test]
    fn failure_falls_back_to_none() {
        let client = StubClient::failing();
        let cache = EmbeddingCache::default();
        let candidates = vec!["a".to_string(), "b".to_string()];
        let options = EmbeddingOptions {
            enabled: true,
            channel: Some(channel()),
            ..EmbeddingOptions::default()
        };
        let order = semantic_scores(
            &options,
            EmbeddingRequest {
                channel: &channel(),
                cache: &cache,
                client: &client,
                query: "查询",
                candidates: &candidates,
            },
        );
        assert!(order.is_none(), "失败必须 fail-open 返回 None");
    }

    #[test]
    fn respects_batch_size() {
        let client = StubClient::new(&[
            ("q", vec![1.0, 0.0]),
            ("a", vec![1.0, 0.0]),
            ("b", vec![1.0, 0.0]),
            ("c", vec![1.0, 0.0]),
        ]);
        let cache = EmbeddingCache::default();
        let candidates = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let options = EmbeddingOptions {
            enabled: true,
            channel: Some(channel()),
            ..EmbeddingOptions::default()
        };
        let _ = semantic_scores(
            &options,
            EmbeddingRequest {
                channel: &channel(),
                cache: &cache,
                client: &client,
                query: "q",
                candidates: &candidates,
            },
        );
        let calls = client.calls.lock().unwrap();
        // 1 次查询 + ceil(3/2)=2 批候选。
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|batch| batch.len() <= 2));
    }
}
