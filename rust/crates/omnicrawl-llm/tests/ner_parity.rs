//! NER 兜底层的对照测试（纯逻辑 + 真实权重推理）。
//!
//! 数据集是冻结的对照契约，
//! 权重是 Python checkpoint 的逐位转换结果。

use std::path::Path;
use std::sync::Arc;

use omnicrawl_llm::desensitization::ner::{
    has_cjk, is_chinese_span, isolate_chinese, iter_batches, iter_chunks, resolve_device,
    resolve_model_path, NerBackend, NerExtractor, NerLayer, DEFAULT_BATCH_TOKENS,
    DEFAULT_CACHE_SIZE, DEFAULT_MAX_SEQ_LEN, NER_ENTITY_TYPES,
};
use omnicrawl_llm::desensitization::ner_weights::NerWeights;
use serde_json::{json, Value as Json};

const FIXTURE: &str = include_str!("fixtures/ner_parity.json");
const WEIGHTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/ner_bilstm_crf.bin");

fn fixture() -> Json {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn entity_types() -> Vec<String> {
    NER_ENTITY_TYPES
        .iter()
        .map(|item| item.to_string())
        .collect()
}

fn build_layer() -> NerLayer {
    let weights = NerWeights::load(Path::new(WEIGHTS)).expect("加载 NER 权重");
    let extractor = Arc::new(NerExtractor::new(
        Arc::new(weights),
        DEFAULT_MAX_SEQ_LEN,
        DEFAULT_BATCH_TOKENS,
        DEFAULT_CACHE_SIZE,
    ));
    NerLayer::new(extractor, &entity_types(), 2)
}

fn entity_json(entity: &(usize, usize, String)) -> Json {
    json!([entity.0, entity.1, entity.2])
}

#[test]
fn ner_misc_matches_python() {
    let data = fixture();
    let misc = &data["misc"];

    for case in misc["chinese_isolation"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(
            isolate_chinese(text),
            case["isolated"].as_str().unwrap(),
            "隔离 {text}"
        );
    }

    for case in misc["chinese_spans"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(
            is_chinese_span(text),
            case["is_chinese_span"].as_bool().unwrap(),
            "区间判定 {text}"
        );
        assert_eq!(
            has_cjk(text),
            case["has_cjk"].as_bool().unwrap(),
            "汉字判定 {text}"
        );
    }

    let default_dir = Path::new("D:/bundled/ner");
    for case in misc["model_paths"].as_array().unwrap() {
        let configured = case["configured"].as_str();
        let env_value = case["env"].as_str();
        let path = resolve_model_path(configured, env_value, default_dir);
        let suffix = case["expected_suffix"].as_str().unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(suffix),
            "模型路径 {configured:?} / {env_value:?}"
        );
    }

    for case in misc["devices"].as_array().unwrap() {
        let requested = case["requested"].as_str();
        assert_eq!(
            resolve_device(requested),
            case["expected"].as_str().unwrap(),
            "设备 {requested:?}"
        );
    }

    let tags: Vec<String> = misc["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect();
    let weights = NerWeights::load(Path::new(WEIGHTS)).expect("加载 NER 权重");
    assert_eq!(weights.tags(), tags.as_slice(), "标签表");
    assert_eq!(
        weights.config().vocab_size,
        misc["config"]["vocab_size"].as_u64().unwrap() as usize
    );
    assert_eq!(
        weights.config().num_tags,
        misc["config"]["num_tags"].as_u64().unwrap() as usize
    );
    assert_eq!(
        weights.config().hidden_dim,
        misc["config"]["hidden_dim"].as_u64().unwrap() as usize
    );
    assert_eq!(
        weights.config().embed_dim,
        misc["config"]["embed_dim"].as_u64().unwrap() as usize
    );
    assert!(weights.unk_id() > 0, "未登录 id 已就位");
}

#[test]
fn ner_entities_matches_python() {
    let data = fixture();
    for case in data["entities"].as_array().unwrap() {
        let tags: Vec<String> = case["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        let actual: Vec<Json> = omnicrawl_llm::desensitization::ner::extract_entities(
            &tags,
            case["characters_len"].as_u64().unwrap() as usize,
        )
        .iter()
        .map(entity_json)
        .collect();
        assert_eq!(Json::Array(actual), case["expected"], "标签 {tags:?}");
    }
}

#[test]
fn ner_chunks_match_python() {
    let data = fixture();
    for case in data["chunks"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let max_len = case["max_len"].as_u64().unwrap() as usize;
        let actual: Vec<Json> = iter_chunks(text, max_len)
            .into_iter()
            .map(Json::String)
            .collect();
        assert_eq!(Json::Array(actual), case["expected"], "{text} / {max_len}");
    }
}

#[test]
fn ner_batches_match_python() {
    let data = fixture();
    for case in data["batches"].as_array().unwrap() {
        let lengths: Vec<usize> = case["lengths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_u64().unwrap() as usize)
            .collect();
        let budget = case["budget"].as_u64().unwrap() as usize;
        let actual: Vec<Json> = iter_batches(&lengths, budget)
            .into_iter()
            .map(|batch| Json::Array(batch.into_iter().map(|index| json!(index as u64)).collect()))
            .collect();
        assert_eq!(
            Json::Array(actual),
            case["expected"],
            "{lengths:?} / {budget}"
        );
    }
}

#[test]
fn ner_spans_match_python() {
    let layer = build_layer();
    let data = fixture();
    for case in data["spans"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(
            isolate_chinese(text),
            case["isolated"].as_str().unwrap(),
            "隔离 {text}"
        );
        let chunks: Vec<Json> = iter_chunks(text, DEFAULT_MAX_SEQ_LEN)
            .into_iter()
            .map(Json::String)
            .collect();
        assert_eq!(Json::Array(chunks), case["chunks"], "分块 {text}");

        let entities: Vec<Json> = layer
            .extractor_entities(text)
            .iter()
            .map(entity_json)
            .collect();
        assert_eq!(Json::Array(entities), case["entities"], "实体 {text}");

        let spans: Vec<Json> = layer
            .find_spans(text)
            .into_iter()
            .map(|(start, end)| json!([start, end]))
            .collect();
        assert_eq!(Json::Array(spans), case["layer_spans"], "区间 {text}");
    }
}

#[test]
fn ner_cache_is_reused_across_calls() {
    let layer = build_layer();
    let text = "张三在北京大学读书，李四在上海工作。";
    let first = layer.find_spans(text);
    let second = layer.find_spans(text);
    assert_eq!(first, second, "重复调用结果一致");
    let stats = layer.stats();
    assert!(stats.inferred_chunks >= 1, "首次调用进过模型");
    assert!(stats.hits >= 1, "第二次调用命中块缓存");
}
