//! 联网搜索的单路排序：搜索原始排序加权 + 决策模型逐站点综合置信度 + 时间远近。
//!
//! 三路分数各自归一化后加权求和，得到这一路渠道内每个候选的名次：
//! * **搜索原始排序**：引擎自己的名次（第 1 名最高）。它永远可用，也是开关关闭时的唯一依据。
//! * **决策模型综合置信度**：由三级判定相乘——是否相关 → 相关程度 → 是否符合提问目的。
//!   是否相关接近 0 时综合置信度也接近 0（不相关的结果不该靠「沾边」拿到分）。
//! * **时间远近**：发布时间越近分越高；引擎没给日期时这一路缺席，不奖不罚。
//!
//! 决策模型的判定**一个候选一个请求、并发发出**：单次请求只带一个站点与三个小问题，
//! 比把整路候选塞进一个请求快得多，也避免了候选越多越容易超时。
//!
//! 失败一律 fail-open：开关没开、没有可用决策渠道、请求失败或响应不可解析都退回
//! 「搜索原始排序加权」，搜索不会因为决策服务不可用而失败。

use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_core::diagnostics;
use serde_json::{json, Map, Value};

use super::decision_search::{rerank_options_from_config, RerankChannel};
use crate::review::ReviewMasking;

/// 单路排序后每路渠道保留的条数。
pub const RERANK_KEEP_PER_CHANNEL: usize = 5;
/// 单路参与排序的候选上限：接口型引擎与混合搜索也按这个数向渠道要候选。
pub const RERANK_CHANNEL_POOL: usize = 10;
/// 单个站点判定的超时：一次只判一个候选、三个小问题，30s 留出余量。
const RERANK_TIMEOUT_SECONDS: u64 = 30;

/// 三路权重。
///
/// 决策模型的综合置信度是主导项（`0.60`）：它按提问目的判断，比引擎名次更贴近「要不要看这条」。
/// 搜索原始排序是基序（`0.25`）：决策模型关着或判不出来时，名次就由它决定；两路都判不出来
/// 的时候，它也负责把同分的候选维持成引擎原顺序。
/// 时间远近占 `0.15`：足够把明显更新的一条顶到同等相关度前面，又不足以让旧而更贴题的结果沉底。
const WEIGHT_SOURCE: f64 = 0.25;
const WEIGHT_DECISION: f64 = 0.60;
const WEIGHT_RECENCY: f64 = 0.15;

/// 时间这一路的半衰期：距今多少天，分数衰减到一半。
///
/// 半年是一般检索场景的常用量级：新闻类「昨天」与「上个月」差别明显，而技术文档的
/// 「两年」与「三年」不会被判成天差地别。
const RECENCY_HALF_LIFE_DAYS: f64 = 180.0;

/// 单个站点进判定请求前截断到的字符数（判相关度不需要整段正文）。
const SITE_MAX_CHARS: usize = 600;

/// 「是否相关」的问题标识。
const QUESTION_RELEVANT: &str = "relevant";
/// 「相关程度」的问题标识。
const QUESTION_DEGREE: &str = "degree";
/// 「是否符合提问目的」的问题标识。
const QUESTION_PURPOSE: &str = "purpose";
/// 候选项键前缀（`o0`、`o1`…）。
const OPTION_PREFIX: &str = "o";

/// 相关程度的四档取值：由高到低。
const DEGREE_LEVELS: [f64; 4] = [1.0, 0.75, 0.5, 0.25];
/// 提问目的的三档取值：直接可用 / 需再核实 / 目的不同。
const PURPOSE_LEVELS: [f64; 3] = [1.0, 0.5, 0.0];

const INSTRUCTIONS_RELEVANT: &str = "state.site 是一条网页搜索结果。判断它是否与 state.query \
表达的检索需求相关：网页主题与查询指向同一件事就算相关，只是出现相同字词但讲的不是一回事算不相关。";
const INSTRUCTIONS_DEGREE: &str = "state.site 是一条网页搜索结果。判断它与 state.query 的相关程度：\
越高档表示越能直接满足查询所需的信息。";
const INSTRUCTIONS_PURPOSE: &str = "state.site 是一条网页搜索结果。判断它是否符合提问目的：\
能直接用来回答提问、还是需要再点开核实、还是与提问目的不同。";

/// 单路排序的最终依据（便于测试与诊断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankLane {
    /// 只有搜索原始排序与时间参与（决策模型关闭或整体不可用）。
    Weighted,
    /// 加上决策模型的综合置信度。
    Decision,
}

/// 单路排序的结果：路内下标顺序与最终采用的口径。
pub struct ChannelRanking {
    pub order: Vec<usize>,
    pub lane: RankLane,
}

/// 一个站点的三级判定结果，取值都在 `0..=1`。
#[derive(Debug, Clone, Copy, Default)]
pub struct SiteConfidence {
    /// 是否与查询相关。
    pub relevance: f64,
    /// 相关程度。
    pub degree: f64,
    /// 是否符合提问目的。
    pub purpose: f64,
}

impl SiteConfidence {
    /// 综合置信度：三级相乘，不相关的结果不会因为相关程度高而拿到分。
    pub fn composite(self) -> f64 {
        self.relevance * self.degree * self.purpose
    }
}

/// 一次站点判定的全部输入。
pub struct JudgeRequest<'a> {
    pub channel: &'a RerankChannel,
    /// 出网脱敏旁路；`None` 表示脱敏未启用。
    pub masking: Option<&'a ReviewMasking>,
    /// 判定上下文：查询、语言与这一个站点的描述。
    pub state: &'a Value,
    pub timeout_seconds: u64,
}

/// 站点判定执行器：真实实现走决策服务；测试注入桩以便不起网络。
pub trait SiteJudge: Send + Sync {
    /// 判定一个站点；`Err` 表示这一个站点判定不可用。
    fn judge(&self, request: &JudgeRequest<'_>) -> Result<SiteConfidence, String>;
}

/// 联网搜索排序的运行期。
#[derive(Clone)]
pub struct WebRankOptions {
    /// `decision_models.toml` 的 `web_search_rerank` 开关。
    pub enabled: bool,
    /// 默认决策渠道；`None` 表示没有可用渠道。
    pub channel: Option<RerankChannel>,
    pub masking: Option<Arc<ReviewMasking>>,
    /// 判定执行器（测试注入桩）。
    pub judge: Arc<dyn SiteJudge>,
}

impl Default for WebRankOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: None,
            masking: None,
            judge: Arc::new(DecisionSiteJudge),
        }
    }
}

impl std::fmt::Debug for WebRankOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 脱敏旁路与执行器都是 trait 对象，只报「有没有」。
        formatter
            .debug_struct("WebRankOptions")
            .field("enabled", &self.enabled)
            .field("channel", &self.channel)
            .field("masking", &self.masking.is_some())
            .finish()
    }
}

impl WebRankOptions {
    /// 决策模型是否真正可用（开关打开且已选中决策渠道）。
    pub fn active(&self) -> bool {
        self.enabled && self.channel.is_some()
    }
}

/// 按配置装配联网搜索的排序运行期（TUI 与 API 共用同一口径）。
pub fn web_rank_options_from_config(environment: &ConfigEnvironment) -> WebRankOptions {
    let rerank = rerank_options_from_config(environment, DECISION_SWITCH_WEB_SEARCH);
    WebRankOptions {
        enabled: rerank.enabled,
        channel: rerank.channel,
        masking: rerank.masking,
        judge: Arc::new(DecisionSiteJudge),
    }
}

/// `decision_models.toml` 的联网搜索排序开关键。
const DECISION_SWITCH_WEB_SEARCH: &str =
    omnicrawl_config::features::decision_model::DECISION_SWITCH_WEB_SEARCH;

/// 单路排序：三路加权，返回**路内下标**的新顺序。
///
/// `published` 与候选一一对应，`None` 表示这一条没有日期（这一路对它缺席）。
/// `now` 由调用方注入，便于对照测试；时间这一路只用它与 `published` 的差。
///
/// `None` 只表示候选本来就不比保留数多（没有可筛选的余地），此时调用方保留引擎原顺序。
/// 其余情况总会给出名次：决策模型关着或不可用时就是搜索原始排序加权时间。
pub fn channel_ranking(
    options: &WebRankOptions,
    state: &Value,
    candidates: &[String],
    published: &[Option<i64>],
    now: i64,
) -> Option<ChannelRanking> {
    if candidates.len() <= RERANK_KEEP_PER_CHANNEL {
        return None;
    }
    let source = source_scores(candidates.len());
    let recency = recency_scores(published, candidates.len(), now);

    if options.active() {
        match site_confidences(options, state, candidates) {
            Some(confidences) => {
                let scores = combine(&source, Some(&confidences), &recency);
                return Some(ChannelRanking {
                    order: order_by_score(&scores),
                    lane: RankLane::Decision,
                });
            }
            None => diagnostics::warn(
                "[host] 联网搜索重排不可用，改用搜索原始排序加权：决策服务未给出可用判定",
            ),
        }
    }

    // 决策模型关着或不可用：名次由搜索原始排序与时间决定。
    let scores = combine(&source, None, &recency);
    Some(ChannelRanking {
        order: order_by_score(&scores),
        lane: RankLane::Weighted,
    })
}

/// 搜索原始排序的名次分：第 1 名 1.0，最后一名 0.0，中间按名次线性递减。
///
/// 用名次而不是「1/名次」：引擎自己的名次是强先验，但线性递减才让另外两路有足够的
/// 空间把一个靠后的候选顶上来；陡降会让原始名次事实上决定一切。
fn source_scores(count: usize) -> Vec<f64> {
    (0..count)
        .map(|rank| position_score(rank, count))
        .collect()
}

/// 名次分：第 `rank` 名（0 起）在 `total` 条里得多少分；只有一条时恒为 1.0。
fn position_score(rank: usize, total: usize) -> f64 {
    if total <= 1 {
        return 1.0;
    }
    1.0 - rank as f64 / (total - 1) as f64
}

/// 时间这一路的分：越新越接近 1.0，按半衰期指数衰减。
///
/// 没有日期的候选拿全路已有时段的**均值**：既不让缺日期把一条埋到最后，也不给它
/// 凭空加成（一条都没日期时整路缺席，因此这一路对所有候选同时为零，不改变相对名次）。
fn recency_scores(published: &[Option<i64>], count: usize, now: i64) -> Vec<f64> {
    let half_life = RECENCY_HALF_LIFE_DAYS * 86_400.0;
    let raw: Vec<Option<f64>> = (0..count)
        .map(|index| {
            published.get(index).copied().flatten().map(|stamp| {
                // 未来时间按「刚发布」处理：引擎给的日期可能有偏差，不该因此拿到额外分。
                let age = (now - stamp).max(0) as f64;
                0.5_f64.powf(age / half_life)
            })
        })
        .collect();
    let known: Vec<f64> = raw.iter().flatten().copied().collect();
    if known.is_empty() {
        // 一条都没有日期：这一路整路缺席。
        return vec![0.0; count];
    }
    let mean = known.iter().sum::<f64>() / known.len() as f64;
    raw.into_iter().map(|value| value.unwrap_or(mean)).collect()
}

/// 三路分数加权求和。`confidences` 缺席时不参与（开关关闭或判定失败）。
fn combine(source: &[f64], confidences: Option<&[f64]>, recency: &[f64]) -> Vec<f64> {
    let mut scores = vec![0.0; source.len()];
    for (index, score) in scores.iter_mut().enumerate() {
        *score += WEIGHT_SOURCE * source[index];
        *score += WEIGHT_RECENCY * recency.get(index).copied().unwrap_or(0.0);
        if let Some(confidences) = confidences {
            *score += WEIGHT_DECISION * confidences.get(index).copied().unwrap_or(0.0);
        }
    }
    scores
}

/// 按得分降序出路内下标；同分保持引擎原顺序（稳定）。
fn order_by_score(scores: &[f64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|left, right| {
        scores[*right]
            .total_cmp(&scores[*left])
            .then(left.cmp(right))
    });
    order
}

/// 并发判定每个候选，返回**已归一化**的综合置信度（最高为 1.0）。
///
/// 全部判定失败时返回 `None`（本次不接决策这一级）。部分失败时失败项取已判定项的均值：
/// 既不让一个瞬时失败把某个站点埋到最后，也不给它凭空加分。
fn site_confidences(
    options: &WebRankOptions,
    state: &Value,
    candidates: &[String],
) -> Option<Vec<f64>> {
    let channel = options.channel.as_ref()?;
    let judged: Vec<Result<SiteConfidence, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .iter()
            .map(|text| {
                let site_state = site_state(state, text);
                scope.spawn(move || {
                    options.judge.judge(&JudgeRequest {
                        channel,
                        masking: options.masking.as_deref(),
                        state: &site_state,
                        timeout_seconds: RERANK_TIMEOUT_SECONDS,
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("判定线程异常终止。".to_string()))
            })
            .collect()
    });

    let mut composites: Vec<Option<f64>> = Vec::with_capacity(judged.len());
    for outcome in judged {
        match outcome {
            Ok(confidence) => composites.push(Some(confidence.composite())),
            Err(error) => {
                diagnostics::warn(format!(
                    "[host] 联网搜索有站点判定失败，按均值参与排序：{error}"
                ));
                composites.push(None);
            }
        }
    }
    let judged_values: Vec<f64> = composites.iter().flatten().copied().collect();
    if judged_values.is_empty() {
        return None;
    }
    let mean = judged_values.iter().sum::<f64>() / judged_values.len() as f64;
    let top = judged_values.iter().copied().fold(0.0, f64::max);
    Some(
        composites
            .into_iter()
            .map(|value| {
                let value = value.unwrap_or(mean);
                if top > 0.0 {
                    value / top
                } else {
                    0.0
                }
            })
            .collect(),
    )
}

/// 单个站点的判定上下文：原 state 加上这一个站点的描述。
fn site_state(state: &Value, text: &str) -> Value {
    let mut fields: Map<String, Value> = match state.as_object() {
        Some(map) => map.clone(),
        None => Map::new(),
    };
    fields.insert(
        "site".to_string(),
        Value::String(collapse_text(text, SITE_MAX_CHARS)),
    );
    Value::Object(fields)
}

/// 三个判定问题：是否相关 → 相关程度 → 是否符合提问目的。
fn questions() -> Value {
    json!({
        QUESTION_RELEVANT: {
            "type": "choice",
            "instructions": INSTRUCTIONS_RELEVANT,
            "criteria": {
                option_key(0): "相关：网页主题与查询指向同一件事",
                option_key(1): "不相关：只是出现相同字词，讲的不是一回事",
            },
        },
        QUESTION_DEGREE: {
            "type": "choice",
            "instructions": INSTRUCTIONS_DEGREE,
            "criteria": {
                option_key(0): "高度相关：直接满足查询所需信息",
                option_key(1): "较相关：提供查询所需的主要信息",
                option_key(2): "一般相关：只涉及查询的一部分",
                option_key(3): "弱相关：仅沾边",
            },
        },
        QUESTION_PURPOSE: {
            "type": "choice",
            "instructions": INSTRUCTIONS_PURPOSE,
            "criteria": {
                option_key(0): "符合：能直接用来回答提问",
                option_key(1): "部分符合：需要再点开核实",
                option_key(2): "不符合：与提问目的不同",
            },
        },
    })
}

fn option_key(index: usize) -> String {
    crate::decision_wire::option_key_with(OPTION_PREFIX, index)
}

/// 真实实现：一次请求判一个站点，读回三级答案折算成综合置信度。
///
/// 公开是因为本地 REST 决策接口（`omnicrawl-decision`）可以直接复用这一条出站路径。
pub struct DecisionSiteJudge;

impl SiteJudge for DecisionSiteJudge {
    fn judge(&self, request: &JudgeRequest<'_>) -> Result<SiteConfidence, String> {
        let channel = request.channel;
        let api_key = channel.resolve_api_key();
        if api_key.is_empty() && crate::decision_wire::requires_api_key(&channel.mode) {
            return Err("缺少 API Key：请填写该决策渠道的 api_key。".to_string());
        }
        let body = crate::decision_wire::request_body(
            &channel.mode,
            &channel.model,
            request.state,
            &questions(),
        );
        let body_text =
            serde_json::to_string(&body).map_err(|error| format!("构造重排请求失败：{error}"))?;
        // 脱敏旁路：屏蔽失败即放弃本次判定（不外发原文），与「失败一律 fail-open」一致。
        let body_text = match request.masking {
            Some(masking) => {
                let mut masker =
                    (masking.factory)().map_err(|error| format!("脱敏不可用：{error}"))?;
                let masked = masker.mask(&body_text);
                masker.close();
                masked
            }
            None => body_text,
        };
        let text = crate::decision_wire::post_decision(
            &channel.mode,
            &channel.base_url,
            &channel.model,
            &api_key,
            &body_text,
            request.timeout_seconds,
            omnicrawl_version::USER_AGENT,
            channel.retry_count,
        )?;
        let answers = crate::decision_wire::extract_answers(&channel.mode, &text)?;
        let relevance = expected_level(&answers, QUESTION_RELEVANT, &[1.0, 0.0])?;
        let degree = expected_level(&answers, QUESTION_DEGREE, &DEGREE_LEVELS)?;
        let purpose = expected_level(&answers, QUESTION_PURPOSE, &PURPOSE_LEVELS)?;
        Ok(SiteConfidence {
            relevance,
            degree,
            purpose,
        })
    }
}

/// 一个问题折算成一档取值：有逐项置信度时取期望（`Σ 概率 × 档位`），只有胜出项时取该档位。
///
/// 取期望而不是只看胜出项：相关程度的四档是连续的，模型给的分布本身就表达了程度。
fn expected_level(answers: &Value, question: &str, levels: &[f64]) -> Result<f64, String> {
    let answer = answers
        .get(question)
        .ok_or_else(|| format!("决策模型未返回 {question} 的判定"))?;
    let mut expected = 0.0;
    let mut seen = false;
    if let Some(probabilities) = answer.get("probabilities").and_then(Value::as_object) {
        for (key, value) in probabilities {
            let Some(index) = crate::decision_wire::option_index_with(OPTION_PREFIX, key) else {
                continue;
            };
            let (Some(level), Some(probability)) = (levels.get(index), value.as_f64()) else {
                continue;
            };
            expected += level * probability;
            seen = true;
        }
    }
    if seen {
        return Ok(expected.clamp(0.0, 1.0));
    }
    answer
        .get("choice")
        .and_then(Value::as_str)
        .and_then(|key| crate::decision_wire::option_index_with(OPTION_PREFIX, key))
        .and_then(|index| levels.get(index).copied())
        .ok_or_else(|| format!("决策模型未返回可用的 {question} 判定"))
}

/// 进判定请求前的文本收敛：空白压成单空格，超长截断。
fn collapse_text(text: &str, limit: usize) -> String {
    crate::decision_wire::collapse_text(text, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 条候选：文本里有可辨认的序号，桩据此认站点。
    fn candidates() -> Vec<String> {
        (0..10).map(|index| format!("候选 {index}")).collect()
    }

    /// 把判定结果写死的桩：`scores[i]` 是第 i 个候选的综合置信度，`failing` 里的报错。
    struct ScriptedJudge {
        scores: Vec<f64>,
        failing: Vec<usize>,
    }

    impl SiteJudge for ScriptedJudge {
        fn judge(&self, request: &JudgeRequest<'_>) -> Result<SiteConfidence, String> {
            let position = request
                .state
                .get("site")
                .and_then(Value::as_str)
                .and_then(|text| text.split("候选 ").nth(1))
                .and_then(|rest| rest.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if self.failing.contains(&position) {
                return Err("判定不可用".to_string());
            }
            let score = self.scores.get(position).copied().unwrap_or(0.0);
            Ok(SiteConfidence {
                relevance: 1.0,
                degree: score,
                purpose: 1.0,
            })
        }
    }

    fn active_rank(judge: Arc<dyn SiteJudge>) -> WebRankOptions {
        WebRankOptions {
            enabled: true,
            channel: Some(RerankChannel {
                mode: "jev".to_string(),
                model: "jev-latest".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                api_key: "jv_test".to_string(),
                retry_count: 0,
            }),
            masking: None,
            judge,
        }
    }

    /// 无日期的候选：时间这一路整路缺席，名次回到原始排序 + 决策两路的旧行为。
    fn no_dates(count: usize) -> Vec<Option<i64>> {
        vec![None; count]
    }

    #[test]
    fn source_scores_are_linear_over_the_rank() {
        assert_eq!(source_scores(1), vec![1.0]);
        assert_eq!(source_scores(3), vec![1.0, 0.5, 0.0]);
    }

    #[test]
    fn too_few_candidates_are_not_ranked() {
        let options = WebRankOptions::default();
        let few = vec!["甲".to_string(), "乙".to_string()];
        assert!(channel_ranking(&options, &json!({}), &few, &no_dates(2), 0).is_none());
    }

    /// 决策模型关着：名次就是搜索原始排序。
    #[test]
    fn switch_off_falls_back_to_the_source_order() {
        let ranking = channel_ranking(
            &WebRankOptions::default(),
            &json!({}),
            &candidates(),
            &no_dates(10),
            0,
        )
        .expect("原始名次总是可用");
        assert_eq!(ranking.lane, RankLane::Weighted);
        assert_eq!(ranking.order, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn composite_multiplies_the_three_levels() {
        let confidence = SiteConfidence {
            relevance: 1.0,
            degree: 0.5,
            purpose: 0.5,
        };
        assert!((confidence.composite() - 0.25).abs() < 1e-9);
        // 不相关直接归零：相关程度再高也拿不到分。
        let unrelated = SiteConfidence {
            relevance: 0.0,
            degree: 1.0,
            purpose: 1.0,
        };
        assert_eq!(unrelated.composite(), 0.0);
    }

    #[test]
    fn expected_level_reads_probabilities_as_expectation() {
        let answers = json!({
            QUESTION_RELEVANT: {"probabilities": {"o0": 0.8, "o1": 0.2}},
        });
        let value = expected_level(&answers, QUESTION_RELEVANT, &[1.0, 0.0]).expect("可解析");
        assert!((value - 0.8).abs() < 1e-9);
        // 只有胜出项时取该档位。
        let choice_only = json!({QUESTION_RELEVANT: {"choice": "o1"}});
        assert_eq!(
            expected_level(&choice_only, QUESTION_RELEVANT, &[1.0, 0.0]).expect("可解析"),
            0.0
        );
    }

    #[test]
    fn expected_level_rejects_missing_or_unknown_answers() {
        assert!(expected_level(&json!({}), QUESTION_RELEVANT, &[1.0, 0.0]).is_err());
        let out_of_range = json!({QUESTION_RELEVANT: {"choice": "o9"}});
        assert!(expected_level(&out_of_range, QUESTION_RELEVANT, &[1.0, 0.0]).is_err());
    }

    #[test]
    fn decision_lane_wins_over_the_source_order() {
        // 第 9 个候选判为完全符合，其余为 0：它必须被顶到最前。
        let options = active_rank(Arc::new(ScriptedJudge {
            scores: vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            failing: Vec::new(),
        }));
        let ranking = channel_ranking(
            &options,
            &json!({"query": "rust"}),
            &candidates(),
            &no_dates(10),
            0,
        )
        .expect("决策模型参与时应当排出来");
        assert_eq!(ranking.lane, RankLane::Decision);
        assert_eq!(ranking.order[0], 9, "判定为完全符合的候选被顶到最前");
    }

    /// 判定全部失败：退回搜索原始排序，搜索本身不失败。
    #[test]
    fn all_judged_failures_fall_back_to_the_weighted_lane() {
        let options = active_rank(Arc::new(ScriptedJudge {
            scores: vec![1.0; 10],
            failing: (0..10).collect(),
        }));
        let ranking = channel_ranking(&options, &json!({}), &candidates(), &no_dates(10), 0)
            .expect("原始名次总是可用");
        assert_eq!(ranking.lane, RankLane::Weighted);
        assert_eq!(ranking.order, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn site_state_carries_the_collapsed_site_text() {
        let state = site_state(&json!({"query": "q", "language": "zh"}), "  标题\n\n链接  ");
        assert_eq!(state.get("query").and_then(Value::as_str), Some("q"));
        assert_eq!(
            state.get("site").and_then(Value::as_str),
            Some("标题 链接")
        );
    }

    const DAY: i64 = 86_400;

    /// 时间这一路：越新分越高，半衰期处减半，未来时间按刚发布处理。
    #[test]
    fn recency_decays_by_the_half_life() {
        let now = 1_000 * DAY;
        let scores = recency_scores(
            &[Some(now), Some(now - 180 * DAY), Some(now - 360 * DAY)],
            3,
            now,
        );
        assert!((scores[0] - 1.0).abs() < 1e-9, "刚发布拿满分");
        assert!((scores[1] - 0.5).abs() < 1e-9, "一个半衰期后减半");
        assert!((scores[2] - 0.25).abs() < 1e-9, "两个半衰期后四分之一");
        // 未来时间不给额外加分。
        let future = recency_scores(&[Some(now + 30 * DAY)], 1, now);
        assert!((future[0] - 1.0).abs() < 1e-9);
    }

    /// 一条日期都没有：这一路整路缺席，不改变相对名次。
    #[test]
    fn recency_is_absent_without_any_date() {
        assert_eq!(recency_scores(&[None, None, None], 3, 0), vec![0.0; 3]);
    }

    /// 缺日期的候选拿已有时段的均值：既不埋到最后，也不凭空加成。
    #[test]
    fn missing_dates_take_the_mean_of_known_ones() {
        let now = 1_000 * DAY;
        let scores = recency_scores(&[Some(now), None, Some(now - 180 * DAY)], 3, now);
        assert!((scores[1] - 0.75).abs() < 1e-9, "均值参与");
    }

    /// 决策模型关着时，更新时间近的一条能越过原始名次更靠前的旧结果。
    #[test]
    fn newer_result_overtakes_an_older_one() {
        let now = 1_000 * DAY;
        let published = vec![Some(now - 720 * DAY), Some(now)];
        let candidates = vec!["旧结果".to_string(), "新结果".to_string()];
        // 只有两条时没有筛选余地，这里手动按 pool 造满（保持索引对应）。
        let mut many = candidates.clone();
        let mut stamps = published.clone();
        for index in 2..RERANK_KEEP_PER_CHANNEL + 1 {
            many.push(format!("填充 {index}"));
            // 填充项给极旧的日期：让位给真正被测的两条。
            stamps.push(Some(now - 5_000 * DAY));
        }
        let ranking = channel_ranking(&WebRankOptions::default(), &json!({}), &many, &stamps, now)
            .expect("原始名次总是可用");
        assert_eq!(ranking.lane, RankLane::Weighted);
        assert_eq!(ranking.order[0], 1, "新结果被时间这一路顶到最前");
    }

    /// 时间不足以盖过决策模型的判断：更贴题的一条仍然赢。
    #[test]
    fn decision_still_outweighs_recency() {
        let now = 1_000 * DAY;
        // 第 0 条判为完全符合但很旧；第 1 条刚发布但判定为 0。
        let options = active_rank(Arc::new(ScriptedJudge {
            scores: vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            failing: Vec::new(),
        }));
        let mut stamps = vec![Some(now - 2_000 * DAY), Some(now)];
        stamps.extend(std::iter::repeat(Some(now - 2_000 * DAY)).take(8));
        let ranking = channel_ranking(&options, &json!({"query": "rust"}), &candidates(), &stamps, now)
            .expect("决策模型参与时应当排出来");
        assert_eq!(ranking.lane, RankLane::Decision);
        assert_eq!(ranking.order[0], 0, "更贴题的旧结果仍然排第一");
    }
}
