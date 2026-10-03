//! 结构化决策请求的线格式：把 `state` + `questions` 组装成请求体，并从响应里取回 `answers`。
//!
//! 三种请求方式（`decision_models.toml` 的 `mode`）共享同一套 state / questions / answers
//! 语义，差异只在这一层：
//! * `jev`：请求体是 `{model, state, questions}`；响应顶层的 `answers` 就是答案。
//! * `onejev`：自部署的 OneJev 服务（`qev serve`），请求体与响应形状与 `jev` 相同，
//!   只有路径不同（`POST {base_url}/v1/systemone`）——它是同一协议的本地部署。
//! * `chat_completions`：请求体走 OpenAI 兼容的 `messages`——state + questions 作为一条 user
//!   消息的 JSON 文本，并带 `response_format` 要求 JSON 输出；答案从
//!   `choices[0].message.content` 里解析出同一形状的 `answers`。
//!
//! 因此三个调用点（工具调用审查、检索重排、提问托管）只管按 mode 组装请求体与读回
//! `answers`，其余解析逻辑两种方式共用。

use omnicrawl_config::features::decision_model::{
    DECISION_MODE_CHAT_COMPLETIONS, DECISION_MODE_ONEJEV,
};
use serde_json::{json, Value};

/// `chat_completions` 方式的输出约束：只要一个 JSON 对象，形状与 questions 对齐。
const CHAT_SYSTEM_PROMPT: &str = "你是结构化决策服务：按 user 消息里 JSON 的 questions 逐项判定，\
只输出一个 JSON 对象 {\"answers\": {...}}，键与 questions 的键一致，不要输出任何其他文字。";

/// 决策接口地址：按请求方式选路径（基地址末尾斜杠在此收口）。
pub fn decide_url(mode: &str, base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if mode == DECISION_MODE_CHAT_COMPLETIONS {
        format!("{base}/v1/chat/completions")
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
    const LIMIT: usize = 200;
    let characters: Vec<char> = text.trim().chars().collect();
    if characters.len() <= LIMIT {
        return characters.iter().collect();
    }
    let head: String = characters[..LIMIT].iter().collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::features::decision_model::DECISION_MODE_JEV;

    #[test]
    fn decide_url_follows_the_request_mode() {
        assert_eq!(
            decide_url(DECISION_MODE_JEV, "https://jevtypesafeai.com/api/"),
            "https://jevtypesafeai.com/api/v1/decide"
        );
        assert_eq!(
            decide_url(DECISION_MODE_CHAT_COMPLETIONS, "https://api.openlux.ai"),
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
}
