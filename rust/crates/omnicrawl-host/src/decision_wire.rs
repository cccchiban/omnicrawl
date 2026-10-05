//! 结构化决策请求的线格式：把 `state` + `questions` 组装成请求体，并从响应里取回 `answers`。
//!
//! 三种请求方式（`decision_models.toml` 的 `mode`）共享同一套 state / questions / answers
//! 语义，差异只在这一层：
//! * `jev`：请求体是 `{model, state, questions}`；响应顶层的 `answers` 就是答案。
//! * `onejev`：自部署的 OneJev 服务（`qev serve`），请求体与响应形状与 `jev` 相同，
//!   只有路径不同（`POST {base_url}/v1/systemone`）——它是同一协议的本地部署。
//! * `chat_completions`：请求体走 OpenAI 兼容的 `messages`——state + questions 作为一条 user
//!   消息的 JSON 文本，并带 `response_format` 要求 JSON 输出；答案从
//!   `choices[0].message.content` 里解析出同一形状的 `answers`。该方式的 `base_url` 已含
//!   `/v1`（与 `[image_gen]`、`[tts_api]` 的 OpenAI 兼容口径一致），因此只追加
//!   `/chat/completions`。
//!
//! 因此三个调用点（工具调用审查、检索重排、提问托管）只管按 mode 组装请求体与读回
//! `answers`，其余解析逻辑两种方式共用。

use omnicrawl_config::features::decision_model::{
    DECISION_MODE_CHAT_COMPLETIONS, DECISION_MODE_ONEJEV,
};
use serde_json::{json, Map, Value};

/// `chat_completions` 方式的输出约束：只要一个 JSON 对象，形状与 questions 对齐。
const CHAT_SYSTEM_PROMPT: &str = "你是结构化决策服务：按 user 消息里 JSON 的 questions 逐项判定，\
只输出一个 JSON 对象 {\"answers\": {...}}，键与 questions 的键一致，不要输出任何其他文字。";

/// 决策接口地址：按请求方式选路径（基地址末尾斜杠在此收口）。
///
/// `chat_completions` 的基地址按 OpenAI 兼容口径填到 `/v1`（同 `[image_gen]`、`[tts_api]`），
/// 因此只追加资源路径；`jev` 与 `onejev` 的基地址是站点根下的 API 前缀，各自补 `/v1/...`。
pub fn decide_url(mode: &str, base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if mode == DECISION_MODE_CHAT_COMPLETIONS {
        format!("{base}/chat/completions")
    } else if mode == DECISION_MODE_ONEJEV {
        // 自部署的 OneJev 服务：TypeSafe 兼容接口（与云端 jev 同形状、不同路径）。
        format!("{base}/v1/systemone")
    } else {
        format!("{base}/v1/decide")
    }
}

/// 该请求方式是否需要凭据：自部署的本地服务（`onejev`）不鉴权，其余一律要。
pub fn requires_api_key(mode: &str) -> bool {
    mode != DECISION_MODE_ONEJEV
}

/// 组装一次决策请求的请求体。
pub fn request_body(mode: &str, model: &str, state: &Value, questions: &Value) -> Value {
    if mode == DECISION_MODE_CHAT_COMPLETIONS {
        let payload = json!({"state": state, "questions": questions}).to_string();
        return json!({
            "model": model,
            "messages": [
                {"role": "system", "content": CHAT_SYSTEM_PROMPT},
                {"role": "user", "content": payload},
            ],
            "response_format": {"type": "json_object"},
        });
    }
    json!({"model": model, "state": state, "questions": questions})
}

/// 发一次决策请求：返回响应正文；连接失败与 4xx/5xx 都回 `Err`（正文带进错误便于诊断）。
///
/// 这是决策出站的唯一传输落点——工具调用审查、检索重排、提问托管与本地 REST 决策接口
/// 都经它发出，因此超时、鉴权头、`User-Agent` 与「状态码不转错误、正文留给诊断」的口径
/// 只有一份实现。请求体由调用方给（脱敏发生在构造请求体之后、发送之前）。
pub fn post_body(
    decide_url: &str,
    api_key: &str,
    body_text: &str,
    timeout_seconds: u64,
    user_agent: &str,
) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        // 状态码不转错误：上游 4xx/5xx 的正文要留给诊断。
        .http_status_as_error(false)
        .build()
        .into();
    let timeout = std::time::Duration::from_secs(timeout_seconds.max(1));
    let response = agent
        .post(decide_url)
        .config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .build()
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("User-Agent", user_agent)
        .send(body_text)
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    if status >= 400 {
        return Err(format!(
            "决策服务返回 HTTP {status}：{}",
            truncate(text.trim())
        ));
    }
    Ok(text)
}

/// 从响应体里取出 `answers` 对象；取不到时返回可直接展示的原因。
pub fn extract_answers(mode: &str, text: &str) -> Result<Value, String> {
    let payload = parse_json(text)?;
    if mode == DECISION_MODE_CHAT_COMPLETIONS {
        let content = payload
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "对话补全响应缺少 choices[0].message.content：{}",
                    truncate(text)
                )
            })?;
        let body = parse_json(strip_code_fence(content))?;
        // 允许模型少包一层：顶层就是 answers 时直接当答案用。
        return Ok(match body.get("answers") {
            Some(answers) => answers.clone(),
            None => body,
        });
    }
    payload
        .get("answers")
        .cloned()
        .ok_or_else(|| format!("决策服务响应缺少 answers：{}", truncate(text)))
}

fn parse_json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|error| format!("决策服务响应不是 JSON：{error}"))
}

/// 候选项在 `criteria` 里的默认键前缀；键即「候选下标」的稳定写法。
///
/// 决策模型只认键，因此调用方用「前缀 + 下标」构造候选项，读回时按同一规则反解——
/// 审查、检索重排、提问托管与决策 REST 服务共用这一套键。
pub const OPTION_KEY_PREFIX: &str = "o";

/// 按候选项下标构造请求里的键（`o0`、`o1`…）。
pub fn option_key(index: usize) -> String {
    format!("{OPTION_KEY_PREFIX}{index}")
}

/// 按给定前缀构造候选项键（重排用 `c0`、`c1`…，与托管区分开便于日志辨识）。
pub fn option_key_with(prefix: &str, index: usize) -> String {
    format!("{prefix}{index}")
}

/// 把 `criteria` 的键按前缀反解成候选下标；未知键返回 `None`。
pub fn option_index_with(prefix: &str, key: &str) -> Option<usize> {
    key.strip_prefix(prefix)?.parse::<usize>().ok()
}

/// 从 `answers.<question_id>` 读回胜出候选的下标（读 `choice`，缺失时退化成置信度最高项）。
///
/// 是所有 choice 类提问的共用读法：答案被约束在给定候选项里，因此调用方只需要一个下标，
/// 不必解析自由文本。越界或未知键一律忽略。
pub fn parse_choice_index(
    mode: &str,
    text: &str,
    question_id: &str,
    count: usize,
    prefix: &str,
) -> Result<usize, String> {
    let answers = extract_answers(mode, text)?;
    let answer = answers.get(question_id);
    if let Some(index) = answer
        .and_then(|value| value.get("choice"))
        .and_then(Value::as_str)
        .and_then(|key| option_index_with(prefix, key))
        .filter(|index| *index < count)
    {
        return Ok(index);
    }
    let best = answer
        .and_then(|value| value.get("probabilities"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((option_index_with(prefix, key)?, value.as_f64()?)))
        .filter(|(index, _)| *index < count)
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index);
    best.ok_or_else(|| format!("决策模型未返回可用选项：{}", truncate(text.trim())))
}

/// 从 `answers.<question_id>` 读回候选项的完整排序（按置信度降序；没有逐项置信度时用胜出项排首位）。
///
/// 返回的是输入候选的下标序列，长度恒为 `count`：读不到的候选按原顺序接在后面，
/// 因此调用方可以直接按它重排。
pub fn parse_ranking_order(
    mode: &str,
    text: &str,
    question_id: &str,
    count: usize,
    prefix: &str,
) -> Result<Vec<usize>, String> {
    let answers = extract_answers(mode, text)?;
    let answer = answers.get(question_id);
    let mut ranked: Vec<(f64, usize)> = Vec::new();
    if let Some(probabilities) = answer
        .and_then(|value| value.get("probabilities"))
        .and_then(Value::as_object)
    {
        for (key, value) in probabilities {
            let Some(index) = option_index_with(prefix, key) else {
                continue;
            };
            if index >= count {
                continue;
            }
            let Some(probability) = value.as_f64() else {
                continue;
            };
            ranked.push((probability, index));
        }
    }
    if ranked.is_empty() {
        // 没有逐项置信度时退化用胜出项：它排第一，其余保持原顺序。
        let winner = answer
            .and_then(|value| value.get("choice"))
            .and_then(Value::as_str)
            .and_then(|key| option_index_with(prefix, key))
            .filter(|index| *index < count)
            .ok_or_else(|| {
                format!(
                    "决策模型未返回可用的候选项排序：{}",
                    truncate(text.trim())
                )
            })?;
        ranked.push((1.0, winner));
    }
    ranked.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(&right.1)));

    let mut order: Vec<usize> = Vec::with_capacity(count);
    for (_, index) in ranked {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    for index in 0..count {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    Ok(order)
}

/// 组一个 choice 提问：`instructions` 是判定说明，`criteria` 是「候选项键 → 判定标准」。
pub fn choice_question(instructions: &str, criteria: Map<String, Value>) -> Value {
    json!({
        "type": "choice",
        "instructions": instructions,
        "criteria": criteria,
    })
}

/// 进请求前的文本收敛：空白压成单空格，超长截断。
///
/// 决策只是判个大小，不需要整段正文；调用方各自的上限不同（候选项 400、上下文 1200），
/// 因此上限由调用方给，收敛规则收在这里。
pub fn collapse_text(text: &str, limit: usize) -> String {
    truncate_to(&text.split_whitespace().collect::<Vec<_>>().join(" "), limit)
}

/// 剥掉模型偶尔带上的 Markdown 代码围栏（```json ... ```）。
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // 去掉语言标记那一行（若是 ```json 形式）。
    let rest = match rest.split_once('\n') {
        Some((_language, body)) => body,
        None => rest,
    };
    rest.trim_end().trim_end_matches("```").trim()
}

fn truncate(text: &str) -> String {
    truncate_to(text.trim(), 200)
}

/// 按字符截断到 `limit`，超出时以省略号收尾。
fn truncate_to(text: &str, limit: usize) -> String {
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= limit {
        return characters.iter().collect();
    }
    let head: String = characters[..limit].iter().collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::decision_model::DECISION_MODE_JEV;

    const JEV: &str = DECISION_MODE_JEV;

    #[test]
    fn decide_url_follows_the_request_mode() {
        assert_eq!(
            decide_url(DECISION_MODE_JEV, "https://jevtypesafeai.com/api/"),
            "https://jevtypesafeai.com/api/v1/decide"
        );
        assert_eq!(
            decide_url(DECISION_MODE_CHAT_COMPLETIONS, "https://api.openlux.ai/v1/"),
            "https://api.openlux.ai/v1/chat/completions"
        );
        assert_eq!(
            decide_url(DECISION_MODE_ONEJEV, "http://127.0.0.1:8766/"),
            "http://127.0.0.1:8766/v1/systemone"
        );
    }

    #[test]
    fn onejev_uses_the_native_body_and_top_level_answers() {
        // 自部署与云端原生方式共用请求体形状：只有路径不同。
        let body = request_body(
            DECISION_MODE_ONEJEV,
            "OneJev-0.8B",
            &json!({"task": "支付账单"}),
            &json!({"next": {"type": "choice"}}),
        );
        assert_eq!(body["model"], "OneJev-0.8B");
        assert_eq!(body["state"]["task"], "支付账单");
        assert_eq!(body["questions"]["next"]["type"], "choice");
        assert!(body.get("messages").is_none(), "自部署不带 messages");

        let response = json!({
            "model": "OneJev-0.8B",
            "answers": {"next": {"type": "choice", "choice": "click", "confidence": 0.9}}
        })
        .to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_ONEJEV, &response).expect("自部署响应可解析")["next"]
                ["choice"],
            "click"
        );
        assert!(
            extract_answers(DECISION_MODE_ONEJEV, "{}").is_err(),
            "缺 answers 一律拒绝"
        );
    }

    #[test]
    fn jev_body_keeps_the_native_shape() {
        let body = request_body(
            DECISION_MODE_JEV,
            "jev-latest",
            &json!({"ticket": "空白页"}),
            &json!({"urgency": {"type": "choice"}}),
        );
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["state"]["ticket"], "空白页");
        assert_eq!(body["questions"]["urgency"]["type"], "choice");
        assert!(body.get("messages").is_none(), "原生方式不带 messages");
    }

    #[test]
    fn chat_completions_body_wraps_state_and_questions_into_messages() {
        let body = request_body(
            DECISION_MODE_CHAT_COMPLETIONS,
            "jev-1.13.0",
            &json!({"ticket": "空白页"}),
            &json!({"urgency": {"type": "choice"}}),
        );
        assert_eq!(body["model"], "jev-1.13.0");
        assert_eq!(body["response_format"]["type"], "json_object");
        let messages = body["messages"].as_array().expect("messages 是数组");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        // user 消息里就是 state + questions 的原样 JSON 文本。
        let payload: Value =
            serde_json::from_str(messages[1]["content"].as_str().expect("content 是文本"))
                .expect("content 是 JSON");
        assert_eq!(payload["state"]["ticket"], "空白页");
        assert_eq!(payload["questions"]["urgency"]["type"], "choice");
    }

    #[test]
    fn answers_come_from_each_mode_own_place() {
        let native = json!({"answers": {"q": {"choice": "a"}}}).to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_JEV, &native).expect("原生方式可解析")["q"]["choice"],
            "a"
        );

        let chat = json!({
            "choices": [{"message": {"content": "{\"answers\": {\"q\": {\"choice\": \"b\"}}}"}}]
        })
        .to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_CHAT_COMPLETIONS, &chat).expect("对话补全可解析")["q"]
                ["choice"],
            "b"
        );
    }

    #[test]
    fn chat_completions_tolerates_fences_and_missing_answers_wrapper() {
        let fenced = json!({
            "choices": [{"message": {"content": "```json\n{\"q\": {\"choice\": \"c\"}}\n```"}}]
        })
        .to_string();
        assert_eq!(
            extract_answers(DECISION_MODE_CHAT_COMPLETIONS, &fenced).expect("剥围栏后可解析")["q"]
                ["choice"],
            "c"
        );
    }

    #[test]
    fn unusable_responses_are_rejected() {
        assert!(extract_answers(DECISION_MODE_JEV, "not json").is_err());
        assert!(extract_answers(DECISION_MODE_JEV, "{}").is_err());
        assert!(extract_answers(DECISION_MODE_CHAT_COMPLETIONS, "{}").is_err());
        let bad_content = json!({"choices": [{"message": {"content": "not json"}}]}).to_string();
        assert!(extract_answers(DECISION_MODE_CHAT_COMPLETIONS, &bad_content).is_err());
    }

    #[test]
    fn choice_index_reads_the_key_as_an_index() {
        let text = json!({
            "answers": {"pick": {"type": "choice", "choice": "o2", "confidence": 0.8}}
        })
        .to_string();
        assert_eq!(parse_choice_index(JEV, &text, "pick", 3, "o").expect("可解析"), 2);

        // 前缀不匹配的键不算数（重排的 `c0` 与托管的 `o0` 各自独立）。
        assert!(parse_choice_index(JEV, &text, "pick", 3, "c").is_err());

        // 没有 choice 时取概率最高的一项。
        let probabilities = json!({
            "answers": {"pick": {
                "type": "choice",
                "probabilities": {"o0": 0.1, "o1": 0.7, "o2": 0.2}
            }}
        })
        .to_string();
        assert_eq!(
            parse_choice_index(JEV, &probabilities, "pick", 3, "o").expect("可解析"),
            1
        );
    }

    #[test]
    fn unusable_choice_responses_are_rejected() {
        assert!(parse_choice_index(JEV, "not json", "pick", 2, "o").is_err());
        let empty = json!({"answers": {"pick": {"type": "choice"}}}).to_string();
        assert!(parse_choice_index(JEV, &empty, "pick", 2, "o").is_err());
        // 越界或未知键一律忽略；全都取不到时按不可用处理。
        let out_of_range =
            json!({"answers": {"pick": {"choice": "o9", "probabilities": {"o9": 0.9}}}}).to_string();
        assert!(parse_choice_index(JEV, &out_of_range, "pick", 2, "o").is_err());
    }

    #[test]
    fn ranking_order_covers_every_candidate() {
        let text = json!({
            "answers": {"rank": {"type": "choice",
                "probabilities": {"c0": 0.1, "c2": 0.9, "c1": 0.4}}}
        })
        .to_string();
        // 置信度降序：c2（0.9）→ c1（0.4）→ c0（0.1）。
        assert_eq!(
            parse_ranking_order(JEV, &text, "rank", 3, "c").expect("可解析"),
            vec![2, 1, 0]
        );

        // 没有逐项置信度：胜出项排第一，其余按原顺序补齐。
        let winner = json!({"answers": {"rank": {"choice": "c2"}}}).to_string();
        assert_eq!(
            parse_ranking_order(JEV, &winner, "rank", 3, "c").expect("可解析"),
            vec![2, 0, 1]
        );

        // 读不到的候选也要补齐，长度恒为 count。
        let partial = json!({"answers": {"rank": {"probabilities": {"c1": 0.5}}}}).to_string();
        assert_eq!(
            parse_ranking_order(JEV, &partial, "rank", 3, "c").expect("可解析"),
            vec![1, 0, 2]
        );
        assert!(parse_ranking_order(JEV, "{}", "rank", 3, "c").is_err());
    }
}
