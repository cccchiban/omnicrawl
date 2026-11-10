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
//!
//! 两次往返压成一次：查询向量与候选向量合并进同一批请求。远端端点单次往返就可能十几秒，
//! 单独为查询多发一次等于把检索延迟翻倍。
//!
//! 失败后进冷却（[`FAILURE_COOLDOWN`]）：窗口内直接返回 `None` 走本地规则排序，
//! 不再每次调用都重付一轮超时；单次请求时限另有 [`REQUEST_TIMEOUT_CAP_SECONDS`] 收口。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::embedding::{
    load_embedding_model_configuration, EmbeddingChannelConfig, RerankChannelConfig,
};
use omnicrawl_core::diagnostics;

use crate::embedding_wire::{cosine_similarity, parse_embeddings, request_body};
use crate::rerank_wire::{parse_rerank, request_body as rerank_request_body};

const USER_AGENT: &str = omnicrawl_version::USER_AGENT;
/// 单条输入进请求前的截断长度：向量只看语义，不需要整段正文。
const INPUT_MAX_CHARS: usize = 2000;
/// 单次向量请求的时长上限（秒）：配置值再大也按此收口。
///
/// 检索是每轮都会发生的高频调用，正常端点几百毫秒就返回；允许它占用一分钟，
/// 意味着端点抖动时整轮对话会被一次可选加速拖住。
const REQUEST_TIMEOUT_CAP_SECONDS: u64 = 20;

/// 向量召回连续失败后的冷却期：这段时间内直接跳过外发，改用本地规则排序。
///
/// 远端向量端点抖动时单次超时可能长达数十秒，而检索是每轮都会发生的高频调用；
/// 没有冷却窗口时每次调用都要重付一整轮超时成本。
pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);

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
    let timeout =
        std::time::Duration::from_secs(channel.timeout_seconds.min(REQUEST_TIMEOUT_CAP_SECONDS));
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

/// 一个可用的重排渠道（凭据已解析，配置已归一化）。
#[derive(Debug, Clone, PartialEq)]
pub struct RerankChannel {
    pub model: String,
    pub base_url: String,
    /// 已解析出的凭据；为空表示不可用。
    pub api_key: String,
    pub timeout_seconds: u64,
}

impl RerankChannel {
    fn from_config(channel: &RerankChannelConfig, environment: &ConfigEnvironment) -> Self {
        Self {
            model: channel.model.clone(),
            base_url: channel.base_url.clone(),
            api_key: channel.resolve_api_key(environment),
            timeout_seconds: channel.timeout_seconds.max(1) as u64,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/rerank", self.base_url.trim_end_matches('/'))
    }
}

/// 重排请求的传输层：真实实现走 HTTP；测试注入桩以便不起网络。
pub trait RerankClient: Send + Sync {
    /// 按相关度降序返回 `(候选下标, 相关度)`；`Err` 表示该批不可用。
    fn rerank(
        &self,
        channel: &RerankChannel,
        query: &str,
        documents: &[String],
    ) -> Result<Vec<(usize, f32)>, String>;
}

/// 真实实现：OpenAI 兼容的 `POST {base_url}/rerank`。
pub struct HttpRerankClient;

impl RerankClient for HttpRerankClient {
    fn rerank(
        &self,
        channel: &RerankChannel,
        query: &str,
        documents: &[String],
    ) -> Result<Vec<(usize, f32)>, String> {
        if channel.api_key.trim().is_empty() {
            return Err(
                "重排序渠道缺少凭据：请在「向量模型」设置里填写 API Key。".to_string(),
            );
        }
        let body = rerank_request_body(&channel.model, query, documents, documents.len());
        let body_text = serde_json::to_string(&body)
            .map_err(|error| format!("构造重排请求失败：{error}"))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
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
                "重排服务返回 HTTP {status}：{}",
                shorten(text.trim())
            ));
        }
        parse_rerank(&text)
    }
}

/// 向量召回运行期：一个宿主一份，供所有记忆作用域共用。
#[derive(Clone)]
pub struct EmbeddingOptions {
    /// 是否启用嵌入模型（`[memory_embedding] enabled`）。
    pub enabled: bool,
    /// 默认嵌入渠道；`None` 表示没有可用渠道（未配置、未启用或缺凭据）。
    pub channel: Option<EmbeddingChannel>,
    /// 是否启用重排序模型（`[memory_rerank] enabled`）。
    pub rerank_enabled: bool,
    /// 默认重排渠道；`None` 表示没有可用渠道。
    pub rerank_channel: Option<RerankChannel>,
    pub cache: Arc<EmbeddingCache>,
    /// 传输实现（测试注入桩）。
    pub client: Arc<dyn EmbeddingClient>,
    /// 重排传输实现（测试注入桩）。
    pub rerank_client: Arc<dyn RerankClient>,
    /// 最近一次失败的时刻：冷却期内不再外发（[`FAILURE_COOLDOWN`]）。
    pub last_failure: Arc<Mutex<Option<Instant>>>,
    /// 重排侧的失败时刻：与嵌入分开，一侧坏掉不该连累另一侧。
    pub last_rerank_failure: Arc<Mutex<Option<Instant>>>,
}

impl Default for EmbeddingOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: None,
            rerank_enabled: false,
            rerank_channel: None,
            cache: Arc::new(EmbeddingCache::default()),
            client: Arc::new(HttpEmbeddingClient),
            rerank_client: Arc::new(HttpRerankClient),
            last_failure: Arc::new(Mutex::new(None)),
            last_rerank_failure: Arc::new(Mutex::new(None)),
        }
    }
}

impl std::fmt::Debug for EmbeddingOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmbeddingOptions")
            .field("enabled", &self.enabled)
            .field("channel", &self.channel)
            .field("rerank_enabled", &self.rerank_enabled)
            .field("rerank_channel", &self.rerank_channel)
            .finish()
    }
}

impl EmbeddingOptions {
    /// 嵌入模型是否真正可用：开关打开、有渠道、有凭据。
    pub fn active(&self) -> bool {
        self.enabled
            && self
                .channel
                .as_ref()
                .map(|channel| !channel.api_key.trim().is_empty())
                .unwrap_or(false)
    }

    /// 重排序模型是否真正可用（口径同上）。
    pub fn rerank_active(&self) -> bool {
        self.rerank_enabled
            && self
                .rerank_channel
                .as_ref()
                .map(|channel| !channel.api_key.trim().is_empty())
                .unwrap_or(false)
    }

    /// 是否有任何一路可用（决定检索要不要取更宽的候选池）。
    pub fn any_active(&self) -> bool {
        self.active() || self.rerank_active()
    }

    /// 是否处在失败冷却期内：是则跳过外发，直接走本地规则排序。
    pub fn cooling_down(&self) -> bool {
        Self::within_cooldown(&self.last_failure)
    }

    /// 重排侧是否处在失败冷却期内。
    pub fn rerank_cooling_down(&self) -> bool {
        Self::within_cooldown(&self.last_rerank_failure)
    }

    fn within_cooldown(slot: &Mutex<Option<Instant>>) -> bool {
        slot.lock()
            .ok()
            .and_then(|at| *at)
            .map(|at| at.elapsed() < FAILURE_COOLDOWN)
            .unwrap_or(false)
    }

    /// 记一次失败，开启冷却窗口。
    pub fn record_failure(&self) {
        if let Ok(mut at) = self.last_failure.lock() {
            *at = Some(Instant::now());
        }
    }

    /// 记一次重排失败，开启重排侧的冷却窗口。
    pub fn record_rerank_failure(&self) {
        if let Ok(mut at) = self.last_rerank_failure.lock() {
            *at = Some(Instant::now());
        }
    }

    /// 一次召回成功后立即解除冷却，不必再等窗口走完。
    pub fn clear_failure(&self) {
        if let Ok(mut at) = self.last_failure.lock() {
            *at = None;
        }
    }

    /// 一次重排成功后立即解除重排侧的冷却。
    pub fn clear_rerank_failure(&self) {
        if let Ok(mut at) = self.last_rerank_failure.lock() {
            *at = None;
        }
    }
}

/// 按配置装配向量召回运行期（TUI 与 API 共用同一口径）。
///
/// 两个开关都关时连配置文件都不读：这是纯加速项，不该在启动路径上多花一次磁盘往返。
pub fn embedding_options_from_config(
    environment: &ConfigEnvironment,
    enabled: bool,
    rerank_enabled: bool,
) -> EmbeddingOptions {
    if !enabled && !rerank_enabled {
        return EmbeddingOptions::default();
    }
    let configuration = load_embedding_model_configuration(environment, None).ok();
    let channel = configuration
        .as_ref()
        .and_then(|configuration| configuration.active_channel().cloned())
        .map(|channel| EmbeddingChannel::from_config(&channel, environment))
        .filter(|channel| !channel.api_key.trim().is_empty());
    let rerank_channel = configuration
        .as_ref()
        .and_then(|configuration| configuration.active_rerank_channel().cloned())
        .map(|channel| RerankChannel::from_config(&channel, environment))
        .filter(|channel| !channel.api_key.trim().is_empty());
    EmbeddingOptions {
        enabled,
        channel,
        rerank_enabled,
        rerank_channel,
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
    if options.cooling_down() {
        return None;
    }
    let channel = request.channel;
    let fingerprint = channel.fingerprint();

    // 查询向量同样走缓存；查询词跨轮变化，命中率低，但只要命中就省一次外发。
    let query_key = format!("{fingerprint}|q:{}", clamp_input(request.query));
    let mut query_vector = request.cache.get(&query_key);

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

    // 查询与候选合并成一批：向量端点一次往返就要十几秒，单独为查询多发一次请求
    // 等于把检索延迟翻一倍。查询落在这一批的末位，避免挤掉候选的批内位置。
    if query_vector.is_none() && !pending.is_empty() {
        let text = clamp_input(request.query);
        let capacity = (channel.batch_size.saturating_sub(1)).max(1);
        let take = capacity.min(pending.len());
        let mut first: Vec<String> = pending
            .iter()
            .take(take)
            .map(|(_, _, text)| text.clone())
            .collect();
        first.push(text.clone());
        match request.client.embed(channel, &first) {
            Ok(vectors) if vectors.len() == first.len() => {
                let query_vec = Arc::new(vectors.last()?.clone());
                request.cache.put(query_key.clone(), Arc::clone(&query_vec));
                query_vector = Some(query_vec);
                for ((index, key, _), vector) in pending
                    .iter()
                    .take(take)
                    .zip(vectors.into_iter().take(take))
                {
                    let vector = Arc::new(vector);
                    request.cache.put(key.clone(), Arc::clone(&vector));
                    resolved[*index] = Some(vector);
                }
            }
            Ok(vectors) => {
                diagnostics::warn(format!(
                    "[host] 向量召回不可用，改用规则排序：返回 {} 条向量，期望 {} 条。",
                    vectors.len(),
                    first.len()
                ));
                options.record_failure();
                return None;
            }
            Err(error) => {
                diagnostics::warn(format!("[host] 向量召回不可用，改用规则排序：{error}"));
                options.record_failure();
                return None;
            }
        }
        pending.drain(..take);
    }

    for batch in pending.chunks(channel.batch_size) {
        let inputs: Vec<String> = batch.iter().map(|(_, _, text)| text.clone()).collect();
        let vectors = match request.client.embed(channel, &inputs) {
            Ok(vectors) => vectors,
            Err(error) => {
                diagnostics::warn(format!("[host] 向量召回不可用，改用规则排序：{error}"));
                options.record_failure();
                return None;
            }
        };
        if vectors.len() != batch.len() {
            diagnostics::warn(format!(
                "[host] 向量召回不可用，改用规则排序：返回 {} 条向量，期望 {} 条。",
                vectors.len(),
                batch.len()
            ));
            options.record_failure();
            return None;
        }
        for ((index, key, _), vector) in batch.iter().zip(vectors.into_iter()) {
            let vector = Arc::new(vector);
            request.cache.put(key.clone(), Arc::clone(&vector));
            resolved[*index] = Some(vector);
        }
    }
    options.clear_failure();

    // 查询向量迟迟没进缓存（候选全部命中缓存、查询未命中）时单独补一次。
    let query_vector = match query_vector {
        Some(vector) => vector,
        None => {
            let inputs = vec![clamp_input(request.query)];
            match request.client.embed(channel, &inputs) {
                Ok(vectors) => {
                    let vector = Arc::new(vectors.into_iter().next()?);
                    request.cache.put(query_key, Arc::clone(&vector));
                    vector
                }
                Err(error) => {
                    diagnostics::warn(format!("[host] 向量召回不可用，改用规则排序：{error}"));
                    options.record_failure();
                    return None;
                }
            }
        }
    };

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

/// 第二阶段：把候选交给重排序模型逐条打分，返回**候选下标及其相关度**（降序）。
///
/// 与嵌入召回同一套取舍：`None` 表示本次不可用，调用方保留现有顺序；失败进冷却，
/// 冷却期内不再外发。候选不足两条时不值得多发一次请求，直接返回 `None`。
pub fn rerank_scores(
    options: &EmbeddingOptions,
    query: &str,
    candidates: &[String],
) -> Option<Vec<(usize, f32)>> {
    if !options.rerank_active() || candidates.len() < 2 || options.rerank_cooling_down() {
        return None;
    }
    let channel = options.rerank_channel.as_ref()?;
    let documents: Vec<String> = candidates.iter().map(|text| clamp_input(text)).collect();
    match options
        .rerank_client
        .rerank(channel, &clamp_input(query), &documents)
    {
        Ok(scored) => {
            // 越界下标一律丢弃：调用方按这个下标重排，越界会错位。
            let scored: Vec<(usize, f32)> = scored
                .into_iter()
                .filter(|(index, _)| *index < documents.len())
                .collect();
            if scored.is_empty() {
                options.record_rerank_failure();
                return None;
            }
            options.clear_rerank_failure();
            Some(scored)
        }
        Err(error) => {
            diagnostics::warn(format!("[host] 重排序模型不可用，保留原顺序：{error}"));
            options.record_rerank_failure();
            None
        }
    }
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
            ("d", vec![1.0, 0.0]),
        ]);
        let cache = EmbeddingCache::default();
        let candidates = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
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
        assert!(calls.iter().all(|batch| batch.len() <= 2), "每批不超过渠道上限");
        // 查询搭上第一批（2 条里留 1 个位置），剩下的候选再分两批：共 3 次而非 1+2=3 次以上。
        assert_eq!(calls.len(), 3);
        assert!(
            calls[0].contains(&"q".to_string()),
            "查询应并入第一批，而不是单独一次请求：{:?}",
            *calls
        );
    }

    /// 查询与候选合并成一次往返：端点单次就要十几秒，多发一次等于延迟翻倍。
    #[test]
    fn query_rides_along_instead_of_separate_request() {
        let client = StubClient::new(&[
            ("q", vec![1.0, 0.0]),
            ("a", vec![0.9, 0.1]),
            ("b", vec![0.0, 1.0]),
        ]);
        let cache = EmbeddingCache::default();
        let candidates = vec!["a".to_string(), "b".to_string()];
        let channel = EmbeddingChannel {
            batch_size: 64,
            ..channel()
        };
        let options = EmbeddingOptions {
            enabled: true,
            channel: Some(channel.clone()),
            ..EmbeddingOptions::default()
        };
        let order = semantic_scores(
            &options,
            EmbeddingRequest {
                channel: &channel,
                cache: &cache,
                client: &client,
                query: "q",
                candidates: &candidates,
            },
        )
        .expect("应当算出顺序");
        assert_eq!(
            order.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "查询应与候选同批发出：{:?}", *calls);
        assert_eq!(calls[0].len(), 3, "一批里含查询 + 两条候选");
    }

    /// 候选全部命中缓存时，查询仍要单独补一次，不能因为没有待发候选而漏算。
    #[test]
    fn query_sent_alone_when_all_candidates_cached() {
        let client = StubClient::new(&[
            ("q", vec![1.0, 0.0]),
            ("a", vec![1.0, 0.0]),
            ("b", vec![0.0, 1.0]),
        ]);
        let cache = EmbeddingCache::default();
        let channel = EmbeddingChannel {
            batch_size: 64,
            ..channel()
        };
        let options = EmbeddingOptions {
            enabled: true,
            channel: Some(channel.clone()),
            ..EmbeddingOptions::default()
        };
        let candidates = vec!["a".to_string(), "b".to_string()];
        // 先预热候选缓存。
        let _ = request_with(&options, &channel, &cache, &client, "warm", &candidates);
        let before = client.calls.lock().unwrap().len();
        let order = request_with(&options, &channel, &cache, &client, "q", &candidates);
        assert!(order.is_some());
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), before + 1, "只有查询需要外发");
        assert_eq!(calls.last().unwrap().len(), 1);
    }

    fn request_with(
        options: &EmbeddingOptions,
        channel: &EmbeddingChannel,
        cache: &EmbeddingCache,
        client: &StubClient,
        query: &str,
        candidates: &[String],
    ) -> Option<Vec<(usize, f32)>> {
        semantic_scores(
            options,
            EmbeddingRequest {
                channel,
                cache,
                client,
                query,
                candidates,
            },
        )
    }

    /// 失败后进入冷却期：窗口内不再外发，避免每次检索都重付一整轮超时。
    #[test]
    fn failure_starts_cooldown_and_skips_requests() {
        let client = StubClient::failing();
        let cache = EmbeddingCache::default();
        let channel = EmbeddingChannel {
            batch_size: 64,
            ..channel()
        };
        let options = EmbeddingOptions {
            enabled: true,
            channel: Some(channel.clone()),
            ..EmbeddingOptions::default()
        };
        let candidates = vec!["a".to_string(), "b".to_string()];
        assert!(request_with(&options, &channel, &cache, &client, "q", &candidates).is_none());
        let after_first = client.calls.lock().unwrap().len();
        assert!(options.cooling_down(), "失败应开启冷却");

        // 冷却期内：直接返回 None，一次外发都不发。
        assert!(request_with(&options, &channel, &cache, &client, "q2", &candidates).is_none());
        assert_eq!(
            client.calls.lock().unwrap().len(),
            after_first,
            "冷却期内不应再发请求"
        );

        // 手动解除冷却后恢复外发。
        options.clear_failure();
        assert!(!options.cooling_down());
        assert!(request_with(&options, &channel, &cache, &client, "q3", &candidates).is_none());
        assert!(client.calls.lock().unwrap().len() > after_first);
    }

    /// 重排桩：按文档文本查表给出相关度，或整体失败。
    struct StubRerank {
        scores: HashMap<String, f32>,
        fail: bool,
        calls: Mutex<usize>,
    }

    impl StubRerank {
        fn new(scores: &[(&str, f32)]) -> Self {
            Self {
                scores: scores
                    .iter()
                    .map(|(text, score)| (text.to_string(), *score))
                    .collect(),
                fail: false,
                calls: Mutex::new(0),
            }
        }

        fn failing() -> Self {
            Self {
                scores: HashMap::new(),
                fail: true,
                calls: Mutex::new(0),
            }
        }
    }

    impl RerankClient for StubRerank {
        fn rerank(
            &self,
            _channel: &RerankChannel,
            _query: &str,
            documents: &[String],
        ) -> Result<Vec<(usize, f32)>, String> {
            *self.calls.lock().unwrap() += 1;
            if self.fail {
                return Err("桩：故意失败".to_string());
            }
            let mut scored: Vec<(usize, f32)> = documents
                .iter()
                .enumerate()
                .map(|(index, text)| (index, self.scores.get(text).copied().unwrap_or(0.0)))
                .collect();
            scored.sort_by(|left, right| {
                right
                    .1
                    .partial_cmp(&left.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            Ok(scored)
        }
    }

    fn rerank_channel() -> RerankChannel {
        RerankChannel {
            model: "r".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            api_key: "k".to_string(),
            timeout_seconds: 5,
        }
    }

    #[test]
    fn rerank_orders_candidates_by_score() {
        let client = Arc::new(StubRerank::new(&[("无关", 0.1), ("相近", 0.9)]));
        let options = EmbeddingOptions {
            rerank_enabled: true,
            rerank_channel: Some(rerank_channel()),
            rerank_client: client.clone(),
            ..EmbeddingOptions::default()
        };
        let candidates = vec!["无关".to_string(), "相近".to_string()];
        let scored = rerank_scores(&options, "查询", &candidates).expect("应当给出名次");
        let indices: Vec<usize> = scored.iter().map(|(index, _)| *index).collect();
        assert_eq!(indices, vec![1, 0], "高分候选应排在前面");
    }

    /// 失败后只让重排侧进冷却：嵌入侧不受牵连。
    #[test]
    fn rerank_failure_cools_down_only_the_rerank_side() {
        let client = Arc::new(StubRerank::failing());
        let options = EmbeddingOptions {
            rerank_enabled: true,
            rerank_channel: Some(rerank_channel()),
            rerank_client: client.clone(),
            ..EmbeddingOptions::default()
        };
        let candidates = vec!["a".to_string(), "b".to_string()];
        assert!(rerank_scores(&options, "q", &candidates).is_none());
        assert!(options.rerank_cooling_down(), "重排失败应开启重排侧冷却");
        assert!(!options.cooling_down(), "嵌入侧不该被牵连");

        // 冷却期内不再外发。
        assert!(rerank_scores(&options, "q", &candidates).is_none());
        assert_eq!(*client.calls.lock().unwrap(), 1, "冷却期内不应再发请求");

        options.clear_rerank_failure();
        assert!(!options.rerank_cooling_down());
        assert!(rerank_scores(&options, "q", &candidates).is_none());
        assert_eq!(*client.calls.lock().unwrap(), 2);
    }

    /// 开关没开或缺凭据时不发请求，也不误报失败。
    #[test]
    fn rerank_inactive_without_switch_or_key() {
        let client = Arc::new(StubRerank::new(&[("a", 1.0)]));
        let candidates = vec!["a".to_string(), "b".to_string()];
        let options = EmbeddingOptions {
            rerank_channel: Some(rerank_channel()),
            rerank_client: client.clone(),
            ..EmbeddingOptions::default()
        };
        assert!(rerank_scores(&options, "q", &candidates).is_none());
        assert!(!options.rerank_active());
        assert_eq!(*client.calls.lock().unwrap(), 0, "开关关着不该外发");

        let options = EmbeddingOptions {
            rerank_enabled: true,
            rerank_channel: Some(RerankChannel {
                api_key: String::new(),
                ..rerank_channel()
            }),
            rerank_client: client.clone(),
            ..EmbeddingOptions::default()
        };
        assert!(!options.rerank_active());
        assert!(rerank_scores(&options, "q", &candidates).is_none());
        assert_eq!(*client.calls.lock().unwrap(), 0, "缺凭据不该外发");
    }
}
