//! 工具调用结果的按需淘汰：把「刚变老」的那一批交决策模型裁决，无用的整组移出上下文。
//!
//! 可选项、默认关闭（`decision_models.toml` 的 `[features] tool_call_prune`）。开启后，内核在
//! 执行完新一轮工具批次、又推进到下一批时，把**上一批**（刚变老的那一批，即「老登」）送审：
//! 一次请求里每个调用组一个 choice 提问（`keep` / `drop`），读回被判 `drop` 的组下标。
//!
//! 最新一批永不被送审（它还在被使用），因此淘汰点始终贴近上下文尾部——前面已发过的前缀
//! 逐字不变，前缀缓存不失效。
//!
//! **失败一律 fail-open**：开关没开、没有可用决策渠道、缺凭据、请求失败、响应不可解析都
//! 保留原文（与检索重排、提问托管同一语义）：淘汰是省上下文的手段，值不当为此丢信息或让回合失败。
//! 出网脱敏复用同一份 `[desensitization]` 旁路（[`crate::review::masking_from_config`]）。
//!
//! 判定面（送审形状、下标映射、整组剔除）在 `omnicrawl-controllers`；本模块只做出站与解析。

use std::sync::Arc;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::decision_model::{
    load_decision_model_configuration, load_decision_switches, DecisionChannelConfig,
};
use omnicrawl_controllers::turn::tool_prune::{
    evicted_call_ids, prune_question_id, prune_questions, prune_state, PruneCandidate, PRUNE_DROP,
    PRUNE_KEEP,
};
use omnicrawl_core::diagnostics;
use serde_json::Value;

use crate::review::{masking_from_config, ReviewMasking};

/// 一次裁决请求的超时（决策服务是 70–500ms 量级，余量留给网络）。
const PRUNE_TIMEOUT_SECONDS: u64 = 20;
const USER_AGENT: &str = "omnicrawl-tool-prune/0.0.1";

/// 淘汰用的决策渠道：与审查、检索重排、提问托管同源（`decision_models.toml` 的默认决策渠道）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneChannel {
    /// 请求方式（`jev` / `chat_completions` / `onejev`）。
    pub mode: String,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
}

impl PruneChannel {
    fn from_config(channel: &DecisionChannelConfig) -> Self {
        Self {
            mode: channel.mode.clone(),
            model: channel.model.clone(),
            base_url: channel.base_url.clone(),
            api_key: channel.api_key.clone(),
            api_key_env: channel.api_key_env.clone(),
        }
    }

    /// 决策接口地址（对应 `DecisionChannelConfig::decide_url`）。
    pub fn decide_url(&self) -> String {
        crate::decision_wire::decide_url(&self.mode, &self.base_url)
    }

    /// 生效的内联凭据：只认配置里的明文密钥。
    pub fn resolve_api_key(&self) -> String {
        self.api_key.trim().to_string()
    }
}

/// 一次裁决请求的全部输入。
pub struct PruneRequest<'a> {
    pub channel: &'a PruneChannel,
    /// 出网脱敏旁路；`None` 表示脱敏未启用。
    pub masking: Option<&'a ReviewMasking>,
    /// 本轮任务文本（判定「还有没有用」的背景）。
    pub task: &'a str,
    /// 待裁决的调用组；下标即提问 ID 里的下标。
    pub groups: &'a [PruneCandidate],
}

/// 裁决执行器：真实实现走决策服务；测试注入桩以便不起网络。
pub trait PruneClient: Send + Sync {
    /// 返回应予淘汰的组下标；`Err` 表示本次裁决不可用（调用方 fail-open 保留原文）。
    fn evict(&self, request: &PruneRequest<'_>) -> Result<Vec<usize>, String>;
}

/// 淘汰运行期。
#[derive(Clone)]
pub struct PruneOptions {
    /// 开关状态。
    pub enabled: bool,
    /// 默认决策渠道；`None` 表示没有可用渠道。
    pub channel: Option<PruneChannel>,
    pub masking: Option<Arc<ReviewMasking>>,
    /// 裁决执行器（测试注入桩）。
    pub client: Arc<dyn PruneClient>,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: None,
            masking: None,
            client: Arc::new(DecisionPruneClient),
        }
    }
}

impl std::fmt::Debug for PruneOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 脱敏旁路与执行器都是 trait 对象，只报「有没有」。
        formatter
            .debug_struct("PruneOptions")
            .field("enabled", &self.enabled)
            .field("channel", &self.channel)
            .field("masking", &self.masking.is_some())
            .finish()
    }
}

impl PruneOptions {
    /// 是否真正可用（开关打开且已选中决策渠道）。
    pub fn active(&self) -> bool {
        self.enabled && self.channel.is_some()
    }
}

/// 按配置装配淘汰运行期（TUI 与 API 共用同一口径）。
pub fn prune_options_from_config(environment: &ConfigEnvironment) -> PruneOptions {
    if !load_decision_switches(environment, None)
        .get(omnicrawl_config::features::decision_model::DECISION_SWITCH_TOOL_PRUNE)
        .copied()
        .unwrap_or(false)
    {
        // 关掉时连配置都不读：淘汰是个纯加速项，不该在启动路径上多花一次磁盘往返。
        return PruneOptions::default();
    }
    let channel = load_decision_model_configuration(environment, None)
        .ok()
        .and_then(|configuration| configuration.active_channel().cloned())
        .map(|channel| PruneChannel::from_config(&channel));
    PruneOptions {
        enabled: true,
        channel,
        masking: masking_from_config(environment).map(Arc::new),
        ..PruneOptions::default()
    }
}

/// 裁决一批调用组，返回**应予淘汰的调用 ID**；`None` 表示本次裁决不可用。
///
/// 可用的最低条件是开关打开、有渠道、且至少两个调用组：只剩一组时淘汰它省不下什么，
/// 却要多一次往返（与检索重排「候选不足两项不送审」同一取舍）。
pub fn evicted_call_ids_for(
    options: &PruneOptions,
    task: &str,
    groups: &[PruneCandidate],
) -> Option<Vec<String>> {
    if !options.active() || groups.is_empty() {
        return None;
    }
    let channel = options.channel.as_ref()?;
    let request = PruneRequest {
        channel,
        masking: options.masking.as_deref(),
        task,
        groups,
    };
    match options.client.evict(&request) {
        Ok(picked) => {
            let ids = evicted_call_ids(groups, &picked);
            if ids.is_empty() {
                // 一组都没判无用：不产生事件，上下文保持原样。
                None
            } else {
                Some(ids)
            }
        }
        Err(error) => {
            diagnostics::warn(format!("[host] 工具调用淘汰不可用，保留原文：{error}"));
            None
        }
    }
}

/// 真实实现：一次请求带 N 个 choice 提问，读回被判 `drop` 的组下标。
pub struct DecisionPruneClient;

impl PruneClient for DecisionPruneClient {
    fn evict(&self, request: &PruneRequest<'_>) -> Result<Vec<usize>, String> {
        let channel = request.channel;
        let api_key = channel.resolve_api_key();
        if api_key.is_empty() && crate::decision_wire::requires_api_key(&channel.mode) {
            return Err(format!(
                "缺少 API Key：请设置环境变量 {}。",
                channel.api_key_env
            ));
        }
        let state = prune_state(request.task, request.groups);
        let questions = prune_questions(request.groups);
        let body = crate::decision_wire::request_body(&channel.mode, &channel.model, &state, &questions);
        let body_text =
            serde_json::to_string(&body).map_err(|error| format!("构造淘汰请求失败：{error}"))?;
        let text = crate::review::decision_post_masked(
            &crate::review::DecisionReviewOptions {
                mode: channel.mode.clone(),
                model: channel.model.clone(),
                base_url: channel.base_url.clone(),
                api_key: channel.api_key.clone(),
                api_key_env: channel.api_key_env.clone(),
            },
            request.masking,
            // 脱敏失败即放弃本次淘汰（不外发原文），与「失败一律 fail-open」一致。
            false,
            PRUNE_TIMEOUT_SECONDS,
            &body_text,
        )?;
        parse_dropped_indexes(&text, &channel.mode, request.groups.len())
    }
}

/// 解析每个提问的裁定：读回 `drop` 的组下标；读不到答案的组按保留处理（fail-open 到原文）。
fn parse_dropped_indexes(text: &str, mode: &str, count: usize) -> Result<Vec<usize>, String> {
    let answers = crate::decision_wire::extract_answers(mode, text)?;
    let mut dropped: Vec<usize> = Vec::new();
    for index in 0..count {
        let answer = answers.get(prune_question_id(index));
        let choice = answer
            .and_then(|value| value.get("choice"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if choice == PRUNE_DROP {
            dropped.push(index);
        }
    }
    // 一个可识别答案都没有：整份响应不可用，交给调用方 fail-open。
    let answered = (0..count).any(|index| {
        answers
            .get(prune_question_id(index))
            .and_then(|value| value.get("choice"))
            .and_then(Value::as_str)
            .map(|choice| matches!(choice.trim(), PRUNE_KEEP | PRUNE_DROP))
            .unwrap_or(false)
    });
    if !answered {
        return Err(format!(
            "决策模型未返回可识别的淘汰裁定：{}",
            crate::decision_wire::collapse_text(text, 200)
        ));
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::decision_model::{
        DECISION_MODE_CHAT_COMPLETIONS, DECISION_MODE_JEV,
    };
    use serde_json::json;

    const JEV: &str = DECISION_MODE_JEV;

    fn candidate(call_id: &str, tool: &str) -> PruneCandidate {
        PruneCandidate::bounded(call_id, tool, "{}", true, "输出")
    }

    fn channel(base_url: &str) -> PruneChannel {
        PruneChannel {
            mode: JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: base_url.to_string(),
            api_key: "jv_test".to_string(),
            api_key_env: "JEV_API_KEY".to_string(),
        }
    }

    #[test]
    fn inactive_options_skip_the_request() {
        let groups = vec![candidate("c0", "bash"), candidate("c1", "grep")];
        assert!(evicted_call_ids_for(&PruneOptions::default(), "任务", &groups).is_none());
        // 开关开着但没有可用渠道同样跳过。
        let enabled_without_channel = PruneOptions {
            enabled: true,
            ..PruneOptions::default()
        };
        assert!(evicted_call_ids_for(&enabled_without_channel, "任务", &groups).is_none());
        // 没有待裁决的组也跳过。
        let options = PruneOptions {
            enabled: true,
            channel: Some(channel("http://127.0.0.1:1")),
            ..PruneOptions::default()
        };
        assert!(evicted_call_ids_for(&options, "任务", &[]).is_none());
    }

    #[test]
    fn dropped_indexes_map_to_call_ids() {
        let text = json!({
            "answers": {
                "g0": {"type": "choice", "choice": "keep"},
                "g1": {"type": "choice", "choice": "drop"},
                "g2": {"type": "choice", "probabilities": {"drop": 0.9, "keep": 0.1}},
            }
        })
        .to_string();
        // 只看字面 choice：概率分布不进淘汰判定（淘汰是不可逆的，宁可保守保留）。
        assert_eq!(
            parse_dropped_indexes(&text, JEV, 3).expect("可解析"),
            vec![1]
        );
    }

    #[test]
    fn unusable_responses_are_rejected_so_the_caller_keeps_the_originals() {
        assert!(parse_dropped_indexes("not json", JEV, 2).is_err());
        // 一条可识别答案都没有：整份响应按不可用处理。
        let empty = json!({"answers": {"g0": {"type": "choice"}}}).to_string();
        assert!(parse_dropped_indexes(&empty, JEV, 2).is_err());
        // 部分答案可识别时其余按保留处理（fail-open 到原文）。
        let partial = json!({
            "answers": {"g1": {"type": "choice", "choice": "keep"}}
        })
        .to_string();
        assert_eq!(
            parse_dropped_indexes(&partial, JEV, 2).expect("至少一条可识别"),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn all_keep_yields_no_eviction() {
        let text = json!({
            "answers": {
                "g0": {"type": "choice", "choice": "keep"},
                "g1": {"type": "choice", "choice": "keep"},
            }
        })
        .to_string();
        assert!(parse_dropped_indexes(&text, JEV, 2).expect("可解析").is_empty());
    }

    #[test]
    fn missing_credentials_skip_the_prune() {
        let options = PruneOptions {
            enabled: true,
            channel: Some(PruneChannel {
                api_key: String::new(),
                ..channel("http://127.0.0.1:1")
            }),
            ..PruneOptions::default()
        };
        assert!(evicted_call_ids_for(
            &options,
            "任务",
            &[candidate("c0", "bash"), candidate("c1", "grep")]
        )
        .is_none());
    }

    /// 对话补全方式：答案包在 `choices[0].message.content` 里，解析共用同一段代码。
    #[test]
    fn chat_completions_answers_read_from_message_content() {
        let answers = json!({"answers": {"g1": {"type": "choice", "choice": "drop"}}}).to_string();
        let text = json!({"choices": [{"message": {"content": answers}}]}).to_string();
        assert_eq!(
            parse_dropped_indexes(&text, DECISION_MODE_CHAT_COMPLETIONS, 2).expect("可解析"),
            vec![1]
        );
    }
}
