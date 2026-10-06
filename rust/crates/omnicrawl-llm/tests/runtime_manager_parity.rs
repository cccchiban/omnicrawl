//! 模型运行时管理器（runtime）的对照测试。
//!
//! 数据集是冻结的对照契约。

use std::sync::Arc;

use omnicrawl_llm::{
    ChatRequestInput, ModelCapabilities, ModelDescriptor, ModelError, ModelErrorCode, ModelRuntime,
    ModelRuntimeManager, ProviderProfile, RuntimeError, TurnSink,
};
use omnicrawl_protocol::ModelReply;
use serde_json::{json, Value as Json};

const FIXTURE: &str = include_str!("fixtures/llm_runtime_manager_parity.json");

/// 只用于占位的运行时：管理器只关心引用与释放。
struct FakeRuntime;

impl ModelRuntime for FakeRuntime {
    fn run_turn(
        &self,
        _input: &ChatRequestInput<'_>,
        _sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        Err(RuntimeError::stream_interrupted("fake"))
    }
}

fn make_profile(model_id: &str) -> ProviderProfile {
    ProviderProfile {
        id: format!("profile-{model_id}"),
        provider: "openai".to_string(),
        base_url: "https://api.example.com/v1".to_string(),
        api_key: "sk-x".to_string(),
        user_agent: String::new(),
        default_protocol: "openai_chat_completions".to_string(),
    }
}

fn make_descriptor(step: &Json) -> ModelDescriptor {
    ModelDescriptor {
        model_id: step["model_id"].as_str().unwrap().to_string(),
        protocol: "openai_chat_completions".to_string(),
        capabilities: Some(ModelCapabilities {
            context_window_tokens: step
                .get("capabilities_window")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            ..ModelCapabilities::default()
        }),
        context_window_tokens: step
            .get("context_window")
            .and_then(|value| value.as_i64())
            .unwrap_or(0),
        max_output_tokens: None,
    }
}

fn observe(manager: &ModelRuntimeManager) -> Json {
    json!({
        "generation": manager.generation(),
        "model_id": manager.current_model_id(),
        "context_window": manager.current_context_window(),
        "has_active_turn": manager.has_active_turn(),
        "has_snapshot": manager.active_snapshot().is_some(),
    })
}

#[test]
fn runtime_manager_matches_python() {
    let traces: Vec<Json> = serde_json::from_str(FIXTURE).expect("解析对照数据集");
    for trace in &traces {
        let name = trace["name"].as_str().unwrap();
        let manager = ModelRuntimeManager::new();
        let factory = |descriptor: &ModelDescriptor| -> Arc<dyn ModelRuntime> {
            let _ = descriptor;
            Arc::new(FakeRuntime)
        };
        for step in trace["steps"].as_array().unwrap() {
            let op = step["op"].as_str().unwrap();
            let mut record: Json = json!({"op": op});
            match op {
                "bootstrap" => {
                    manager.bootstrap_with(
                        &make_profile(step["model_id"].as_str().unwrap()),
                        &make_descriptor(step),
                        factory(&make_descriptor(step)),
                    );
                }
                "acquire" => {
                    if let Err(error) = manager.acquire_turn() {
                        record["error"] = json!(error.message);
                    }
                }
                "release" => {
                    if let Some(snapshot) = manager.active_snapshot() {
                        manager.release_turn(&snapshot);
                    }
                }
                "switch" => {
                    let persist_fails = step
                        .get("persist_fails")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let persist = move || -> Result<(), ModelError> {
                        if persist_fails {
                            return Err(ModelError::configuration("保存模型选择失败。"));
                        }
                        Ok(())
                    };
                    let allow_during_turn = step
                        .get("allow_during_turn")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let descriptor = make_descriptor(step);
                    let runtime_factory =
                        |_profile: &ProviderProfile,
                         _descriptor: &ModelDescriptor|
                         -> Result<Arc<dyn ModelRuntime>, String> {
                            Ok(Arc::new(FakeRuntime))
                        };
                    let outcome = manager.switch(
                        &make_profile(step["model_id"].as_str().unwrap()),
                        &descriptor,
                        Some(&persist),
                        allow_during_turn,
                        Some(&runtime_factory),
                    );
                    if let Err(error) = outcome {
                        record["error"] = json!(error.message);
                        record["error_code"] = json!(error.code.as_str());
                    }
                }
                "set_window" => {
                    let tokens = step["tokens"].as_i64().unwrap();
                    if let Err(error) = manager.set_context_window_tokens(tokens) {
                        record["error"] = json!(error.message);
                    }
                }
                "close" => manager.close(),
                "observe" => {}
                other => panic!("未知操作 {other}"),
            }
            record["state"] = observe(&manager);
            let expected = step;
            assert_eq!(normalise(&record), normalise(expected), "{name} / {op}");
        }
    }
}

/// 只比较结果面（输入参数不参与比对）。
fn normalise(value: &Json) -> Json {
    let mut object = serde_json::Map::new();
    for key in ["op", "error", "error_code", "state"] {
        if let Some(item) = value.get(key) {
            object.insert(key.to_string(), item.clone());
        }
    }
    Json::Object(object)
}

#[test]
fn runtime_manager_error_codes_match_python() {
    let manager = ModelRuntimeManager::new();
    let error = manager.acquire_turn().expect_err("未初始化应报错");
    assert_eq!(error.message, "模型运行时尚未初始化。");
    assert_eq!(error.code, ModelErrorCode::ConfigurationError);

    let error = manager
        .set_context_window_tokens(0)
        .expect_err("非正整数应报错");
    assert_eq!(error.message, "上下文长度必须是正整数 Token。");
}
