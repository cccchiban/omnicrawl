//! 配置对话的跨语言 parity：数据集是冻结的对照契约。
//!
//! 五组对照：
//!
//! - **资源快照**：三份内核数据文件的 sha256 / 大小与数据集一致，且权重能按声明形状装载；
//! - **片段切分**：手写扫描器 vs Python `re.split`；
//! - **路由**：逐从句的取词、两个 BIO 头的逐位标签、动作 id、片段区间、检索下标、最终命令
//!   （`score` 是浮点，按 1e-4 容差比对——torch 与本实现的 GRU 在低位会有差异）；
//! - **校验折算**：`_prepare_command` / `_coerce_value` 的报错文案与折算值；
//! - **服务写回**：`apply_text` 的改动列表与两份 TOML 的最终文本逐字节一致。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::toml::Value;
use omnicrawl_config_chat::router_weights::{mean_pool, normalize};
use omnicrawl_config_chat::{
    spans_of, split_clauses, ConfigChatCommand, ConfigChatService, ConfigRouter,
};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/config_chat_parity.json");
const DATA_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data");
const SCORE_TOLERANCE: f64 = 1e-4;
const POOL_NORM_MIN: f32 = 1e-6;

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn fixture() -> Json {
    serde_json::from_str(FIXTURE).expect("解析配置对话对照数据集")
}

fn data_dir() -> PathBuf {
    Path::new(DATA_DIR).to_path_buf()
}

fn data_file(name: &str) -> PathBuf {
    data_dir().join(name)
}

/// 数据集里记的是相对 crate 的路径（`data/config_router.bin`），取文件名后落到内核资源目录。
fn recorded_file(relative: &str) -> PathBuf {
    let name = Path::new(relative)
        .file_name()
        .expect("数据集里的数据文件名")
        .to_string_lossy()
        .to_string();
    data_dir().join(name)
}

fn sha256(path: &Path) -> String {
    let bytes = fs::read(path).expect("读取内核数据文件");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("{:x}", hasher.finalize())
}

/// 建一个空临时目录；用进程号 + 自增序号保证并发测试互不干扰。
fn temp_dir(tag: &str) -> PathBuf {
    let index = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "omnicrawl-config-chat-{tag}-{}-{index}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("创建临时目录");
    path
}

fn argmax(values: &[f32]) -> usize {
    let mut best = 0usize;
    let mut best_value = f32::NEG_INFINITY;
    for (index, value) in values.iter().enumerate() {
        if *value > best_value {
            best = index;
            best_value = *value;
        }
    }
    best
}

fn number_list(value: &Json) -> Vec<usize> {
    value
        .as_array()
        .expect("数值数组")
        .iter()
        .map(|item| item.as_u64().expect("正整数") as usize)
        .collect()
}

fn json_to_toml(value: &Json) -> Value {
    match value {
        Json::Bool(flag) => Value::Boolean(*flag),
        Json::String(text) => Value::String(text.clone()),
        Json::Number(number) => match number.as_i64() {
            Some(integer) => Value::Integer(integer),
            None => Value::Float(number.as_f64().unwrap_or_default()),
        },
        other => panic!("对照数据集里的取值类型超出预期：{other}"),
    }
}

#[test]
fn snapshot_files_match_fixture() {
    let fixture = fixture();
    let weights = &fixture["weights"];
    let binary = recorded_file(weights["path"].as_str().expect("权重路径"));
    assert_eq!(
        weights["sha256"].as_str().unwrap(),
        sha256(&binary),
        "内核权重与数据集不同源"
    );
    assert_eq!(
        weights["size"].as_u64().unwrap() as usize,
        fs::metadata(&binary).unwrap().len() as usize
    );
    assert_eq!(
        weights["labels_sha256"].as_str().unwrap(),
        sha256(&data_file("labels.json"))
    );
    assert_eq!(
        weights["aliases_sha256"].as_str().unwrap(),
        sha256(&data_file("aliases.json"))
    );

    let loaded = load_router();
    let config = loaded.weights().config();
    assert_eq!(
        config.vocab_size,
        weights["vocab_size"].as_u64().unwrap() as usize
    );
    assert_eq!(
        config.num_actions,
        weights["actions"].as_array().unwrap().len()
    );
    assert_eq!(config.num_actions, loaded.weights().actions().len());
    assert_eq!(
        loaded.weights().configs().len(),
        weights["configs"].as_u64().unwrap() as usize
    );
    assert_eq!(
        loaded.alias_count(),
        weights["aliases"].as_u64().unwrap() as usize
    );
    let expected_actions: Vec<String> = weights["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect();
    assert_eq!(loaded.weights().actions(), expected_actions.as_slice());
}

#[test]
fn clauses_match_python() {
    let fixture = fixture();
    for case in fixture["router"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let expected: Vec<String> = case["clauses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        assert_eq!(split_clauses(text), expected, "片段切分不一致：{text:?}");
    }
}

/// 全套件共享的路由器：别名索引是 907 条前向（release 约 50 秒），必须只算一次。
///
/// 全套件共享的路由器：别名索引是 907 条前向（release 约 50 秒、debug 下数分钟），
/// 必须只算一次；`OnceLock` 让并行跑的用例等同一份结果，而不是各算各的。
fn load_router() -> &'static ConfigRouter {
    static ROUTER: OnceLock<ConfigRouter> = OnceLock::new();
    ROUTER.get_or_init(|| ConfigRouter::load(&data_dir()).expect("装配配置对话路由器"))
}

#[test]
fn router_matches_python() {
    let fixture = fixture();
    let router = load_router();
    for case in fixture["router"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let details = case["detail"].as_array().unwrap();
        for detail in details {
            let clause = detail["text"].as_str().unwrap();
            let characters: Vec<char> = clause.chars().take(96).collect();
            let ids = router.weights().token_ids(&characters);
            let expected_ids: Vec<usize> = number_list(&detail["token_ids"]);
            assert_eq!(
                ids,
                expected_ids
                    .into_iter()
                    .map(|id| id as u32)
                    .collect::<Vec<_>>(),
                "取词不一致：{clause:?}"
            );

            let output = router.weights().forward(&ids).expect("前向");
            let config_tags: Vec<usize> =
                output.config_logits.iter().map(|row| argmax(row)).collect();
            let value_tags: Vec<usize> =
                output.value_logits.iter().map(|row| argmax(row)).collect();
            let action_ids: Vec<usize> =
                output.action_logits.iter().map(|row| argmax(row)).collect();
            assert_eq!(
                config_tags,
                number_list(&detail["config_tags"]),
                "配置头标签不一致：{clause:?}"
            );
            assert_eq!(
                value_tags,
                number_list(&detail["value_tags"]),
                "取值头标签不一致：{clause:?}"
            );
            assert_eq!(
                action_ids,
                number_list(&detail["action_ids"]),
                "动作头标签不一致：{clause:?}"
            );

            let spans: Vec<(usize, usize)> = detail["spans"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    let pair = item.as_array().unwrap();
                    (
                        pair[0].as_u64().unwrap() as usize,
                        pair[1].as_u64().unwrap() as usize,
                    )
                })
                .collect();
            assert_eq!(spans_of(&config_tags), spans, "片段区间不一致：{clause:?}");

            let expected_indices = number_list(&detail["config_indices"]);
            let expected_spans: Vec<(usize, usize)> = spans.clone();
            for (index, (start, end)) in expected_spans.iter().take(12).enumerate() {
                let pooled = mean_pool(&output.token_reps[*start..*end]);
                let pooled = normalize(&pooled, POOL_NORM_MIN);
                let (config_index, _) = router.best_config(&pooled);
                assert_eq!(
                    config_index, expected_indices[index],
                    "检索下标不一致：{clause:?} 片段 {start}..{end}"
                );
            }
        }

        let commands = load_router().predict(text).expect("预测");
        let expected = case["commands"].as_array().unwrap();
        assert_eq!(commands.len(), expected.len(), "命令条数不一致：{text:?}");
        for (actual, wanted) in commands.iter().zip(expected.iter()) {
            assert_eq!(
                actual.action,
                wanted["action"].as_str().unwrap(),
                "{text:?}"
            );
            assert_eq!(
                actual.config,
                wanted["config"].as_str().unwrap(),
                "{text:?}"
            );
            assert_eq!(actual.value, wanted["value"].as_str().unwrap(), "{text:?}");
            let expected_score = wanted["score"].as_f64().unwrap();
            assert!(
                (actual.score - expected_score).abs() <= SCORE_TOLERANCE,
                "相似度偏差过大：{text:?} {} vs {expected_score}",
                actual.score
            );
        }
    }
}

#[test]
fn prepare_matches_python() {
    let fixture = fixture();
    let dir = temp_dir("prepare");
    let env = ConfigEnvironment::new(dir.clone(), std::env::consts::OS);
    let service = ConfigChatService::new(&env, data_dir(), None);
    for case in fixture["prepare"].as_array().unwrap() {
        let command = ConfigChatCommand {
            action: case["action"].as_str().unwrap().to_string(),
            config: case["config"].as_str().unwrap().to_string(),
            value: case["value"].as_str().unwrap().to_string(),
            score: 0.0,
        };
        let actual = service.prepare_command(&command);
        match case.get("error").and_then(|item| item.as_str()) {
            Some(expected) => {
                let error = actual.expect_err("应当被拒绝");
                assert_eq!(error.message(), expected, "报错文案不一致：{command:?}");
            }
            None => {
                let value = actual.expect("应当通过校验");
                assert_eq!(
                    value,
                    json_to_toml(&case["coerced"]),
                    "折算值不一致：{command:?}"
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn service_matches_python() {
    let fixture = fixture();
    for case in fixture["service"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let dir = temp_dir("service");
        // 配置路径不再有环境变量覆盖：初始配置与断言都落在隔离 home 的 `.OmniCrawl/`。
        let home = dir.join("home");
        let user_dir = home.join(".OmniCrawl");
        fs::create_dir_all(&user_dir).expect("建隔离配置目录");
        let config_path = user_dir.join("config.toml");
        let subagents_path = user_dir.join("subagents.toml");
        if let Some(initial) = case["initial"].as_str() {
            fs::write(&config_path, initial).expect("写入初始配置");
        }
        let env = ConfigEnvironment::new(home, std::env::consts::OS);
        let mut service = ConfigChatService::new(&env, data_dir(), None);

        match case.get("error").and_then(|item| item.as_str()) {
            Some(expected) => {
                let error = service.apply_text(text).expect_err("应当被拒绝");
                assert_eq!(error.message(), expected, "报错文案不一致：{text:?}");
            }
            None => {
                let changes = service.apply_text(text).expect("写回");
                let expected = case["changes"].as_array().unwrap();
                assert_eq!(changes.len(), expected.len(), "改动条数不一致：{text:?}");
                for (actual, wanted) in changes.iter().zip(expected.iter()) {
                    assert_eq!(actual.path, wanted["path"].as_str().unwrap(), "{text:?}");
                    assert_eq!(
                        actual.value,
                        json_to_toml(&wanted["value"]),
                        "写回值不一致：{text:?}"
                    );
                }
                let commands = service.predict(text).expect("预测");
                let expected_commands = case["commands"].as_array().unwrap();
                assert_eq!(commands.len(), expected_commands.len(), "{text:?}");
                for (actual, wanted) in commands.iter().zip(expected_commands.iter()) {
                    assert_eq!(
                        actual.action,
                        wanted["action"].as_str().unwrap(),
                        "{text:?}"
                    );
                    assert_eq!(
                        actual.config,
                        wanted["config"].as_str().unwrap(),
                        "{text:?}"
                    );
                    assert_eq!(actual.value, wanted["value"].as_str().unwrap(), "{text:?}");
                }
            }
        }

        assert_eq!(
            read_optional(&config_path),
            case["config_file"].as_str().map(|item| item.to_string()),
            "config.toml 内容不一致：{text:?}"
        );
        assert_eq!(
            read_optional(&subagents_path),
            case["subagents_file"].as_str().map(|item| item.to_string()),
            "subagents.toml 内容不一致：{text:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

fn read_optional(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}
