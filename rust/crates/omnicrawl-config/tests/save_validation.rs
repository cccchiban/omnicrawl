//! 设置写盘前的校验契约：写入方不得把读取器必然拒绝的值落到磁盘。
//!
//! 背景：TUI 与 API 的设置面板都直接调 `save_*`，配置合法性只由读取器
//! （`load_*`）保证。写盘函数若不校验，就能写出「下次读取整段失效」的内容
//! ——典型例子是 MCP server 名含大写字符，导致整份 MCP 配置回退默认值。
//!
//! 本套件固定这条不变量：非法输入在写盘阶段被拒，且磁盘保持原样。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::features::advisor::{save_advisor_config, AdvisorConfig};
use omnicrawl_config::features::agent_workspace::{
    save_agent_workspace_config, AgentWorkspaceConfig,
};
use omnicrawl_config::features::desensitization::{
    save_desensitization_config, DesensitizationConfig,
};
use omnicrawl_config::features::run_guard::{
    save_run_guard_config, ContinueConfig, ReasoningGuardConfig, RunGuardConfig,
};
use omnicrawl_config::features::tool_output_compression::{
    save_tool_output_compression_config, ToolOutputCompressionConfig,
};
use omnicrawl_config::features::tts::{save_tts_configuration, TtsConfiguration};
use omnicrawl_config::models::llm::ActiveModelRef;
use omnicrawl_config::models::vision::{save_vision_configuration, VisionConfiguration};

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-save-guard-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn env_for(root: &Path) -> R::ConfigEnvironment {
    R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
}

/// 准备一份带其他段的配置，返回 `(env, config_path, 初始文本)`。
fn prepare(tag: &str) -> (R::ConfigEnvironment, PathBuf, String) {
    let root = temp_root(tag);
    let user = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&user).expect("建立用户配置目录");
    let config_path = user.join("config.toml");
    let initial = "[llm]\nmodel = \"keep-me\"\n".to_string();
    std::fs::write(&config_path, &initial).expect("写初始配置");
    let env = env_for(&root);
    (env, config_path, initial)
}

fn text_of(path: &Path) -> String {
    std::fs::read_to_string(path).expect("读回配置")
}

/// 连续执行的校验用同一份用例目录：`prepare` 每次建新目录，便于各测独立。
fn assert_rejected<F>(tag: &str, expected_fragment: &str, save: F)
where
    F: FnOnce(&R::ConfigEnvironment) -> Result<PathBuf, omnicrawl_config::ConfigError>,
{
    let (env, config_path, initial) = prepare(tag);
    let error = save(&env).expect_err("非法配置应当被拒绝");
    assert!(
        error.message().contains(expected_fragment),
        "错误文案应包含 {expected_fragment}，实际：{}",
        error.message()
    );
    assert_eq!(text_of(&config_path), initial, "被拒的写入不得改动磁盘");
    let _ = std::fs::remove_dir_all(env.home());
}

/// 持续运转：越界的窗口字符数在写盘前被拒。
#[test]
fn run_guard_rejects_out_of_range_values() {
    assert_rejected("run-guard", "run_guard.window_chars", |env| {
        save_run_guard_config(
            env,
            &RunGuardConfig {
                guard: ReasoningGuardConfig {
                    window_chars: 10,
                    ..ReasoningGuardConfig::default()
                },
                ..RunGuardConfig::default()
            },
            None,
        )
    });
}

/// 持续运转：续跑次数上限的区间同样在写盘前检查。
#[test]
fn run_guard_rejects_out_of_range_followups() {
    assert_rejected(
        "run-guard-continue",
        "run_guard.max_auto_followups",
        |env| {
            save_run_guard_config(
                env,
                &RunGuardConfig {
                    continuation: ContinueConfig {
                        max_auto_followups: 99,
                        ..ContinueConfig::default()
                    },
                    ..RunGuardConfig::default()
                },
                None,
            )
        },
    );
}

/// 持续运转：合法的边界值仍然写得进去。
#[test]
fn run_guard_still_accepts_valid_values() {
    let (env, config_path, _) = prepare("run-guard-ok");
    save_run_guard_config(
        &env,
        &RunGuardConfig {
            guard: ReasoningGuardConfig {
                window_chars: 64,
                ..ReasoningGuardConfig::default()
            },
            ..RunGuardConfig::default()
        },
        None,
    )
    .expect("边界值应当合法");
    let text = text_of(&config_path);
    assert!(text.contains("window_chars = 64"), "{text}");
    let _ = std::fs::remove_dir_all(env.home());
}

/// 脱敏：熵阈值越界在写盘前被拒。
#[test]
fn desensitization_rejects_out_of_range_entropy() {
    assert_rejected("desensitization", "entropy_min_bits", |env| {
        save_desensitization_config(
            env,
            &DesensitizationConfig {
                entropy_min_bits: 9.0,
                ..DesensitizationConfig::default()
            },
            None,
        )
    });
}

/// 脱敏：未知 NER 设备在写盘前被拒。
#[test]
fn desensitization_rejects_unknown_ner_device() {
    assert_rejected("desensitization-device", "ner_device", |env| {
        save_desensitization_config(
            env,
            &DesensitizationConfig {
                ner_device: "tpu".to_string(),
                ..DesensitizationConfig::default()
            },
            None,
        )
    });
}

/// 脱敏：写盘前归一化——键名去空、实体类型转大写后落盘。
#[test]
fn desensitization_normalizes_before_writing() {
    let (env, config_path, _) = prepare("desensitization-normalize");
    save_desensitization_config(
        &env,
        &DesensitizationConfig {
            extra_sensitive_keys: vec![" secret ".to_string(), String::new()],
            ner_entity_types: vec!["per".to_string(), " loc ".to_string()],
            ..DesensitizationConfig::default()
        },
        None,
    )
    .expect("归一化后应当可写");
    let text = text_of(&config_path);
    assert!(text.contains("\"secret\""), "{text}");
    assert!(text.contains("\"PER\""), "{text}");
    assert!(text.contains("\"LOC\""), "{text}");
    let _ = std::fs::remove_dir_all(env.home());
}

/// 顾问：未知 effort 档位在写盘前被拒。
#[test]
fn advisor_rejects_unknown_effort() {
    assert_rejected("advisor", "advisor.effort", |env| {
        save_advisor_config(
            env,
            &AdvisorConfig {
                enabled: true,
                model_key: "gpt-5".to_string(),
                effort: "turbo".to_string(),
                disabled_for_models: Vec::new(),
            },
            None,
        )
    });
}

/// 隔离工作区：未知模式在写盘前被拒。
#[test]
fn agent_workspace_rejects_unknown_mode() {
    assert_rejected("agent-workspace-mode", "agent_workspace.mode", |env| {
        save_agent_workspace_config(
            env,
            &AgentWorkspaceConfig {
                mode: "container".to_string(),
                ..AgentWorkspaceConfig::default()
            },
            None,
        )
    });
}

/// 隔离工作区：未知清理策略在写盘前被拒。
#[test]
fn agent_workspace_rejects_unknown_cleanup() {
    assert_rejected("agent-workspace-cleanup", "cleanup_on_exit", |env| {
        save_agent_workspace_config(
            env,
            &AgentWorkspaceConfig {
                cleanup_on_exit: "always".to_string(),
                ..AgentWorkspaceConfig::default()
            },
            None,
        )
    });
}

/// 工具输出压缩：未知思考档位在写盘前被拒。
#[test]
fn compression_rejects_unknown_reasoning_effort() {
    assert_rejected(
        "compression",
        "tool_output_compression.reasoning_effort",
        |env| {
            save_tool_output_compression_config(
                env,
                &ToolOutputCompressionConfig {
                    reasoning_effort: "turbo".to_string(),
                    ..ToolOutputCompressionConfig::default()
                },
                None,
            )
        },
    );
}

/// 工具输出压缩：非正数预算在写盘前被拒。
#[test]
fn compression_rejects_non_positive_budget() {
    assert_rejected(
        "compression-budget",
        "tool_output_compression.min_chars",
        |env| {
            save_tool_output_compression_config(
                env,
                &ToolOutputCompressionConfig {
                    min_chars: 0,
                    ..ToolOutputCompressionConfig::default()
                },
                None,
            )
        },
    );
}

/// TTS：空输出目录在写盘前被拒。
#[test]
fn tts_rejects_blank_output_dir() {
    assert_rejected("tts", "tts.output_dir", |env| {
        save_tts_configuration(
            env,
            &TtsConfiguration {
                output_dir: "   ".to_string(),
                ..TtsConfiguration::default()
            },
            None,
        )
    });
}

/// 视觉：重复的模型引用在写盘前被拒。
#[test]
fn vision_rejects_duplicate_model_refs() {
    assert_rejected("vision", "vision.models[1]", |env| {
        let reference = ActiveModelRef {
            source: "custom".to_string(),
            key: "channel-a".to_string(),
            profile: "profile-a".to_string(),
            model_id: "vision-1".to_string(),
            protocol: String::new(),
        };
        save_vision_configuration(
            env,
            &VisionConfiguration {
                enabled: true,
                models: vec![reference.clone(), reference],
            },
            None,
        )
    });
}
