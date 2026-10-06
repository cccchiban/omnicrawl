//! 对照：Rust 的 prompt cache 身份指纹 vs Python 真实现。
//!
//! 数据集是冻结的对照契约，期望值取自
//! `omnicrawl/agent/context/prompt_context.py::build_prompt_cache_identity` 与
//! `omnicrawl/llm/providers/openai_common.py::build_prompt_cache_key`。
//!
//! 两层断言各有分工：
//!   * 逐字段比对七个身份哈希，定位「哪一项算错了」；
//!   * 用内核的 `build_prompt_cache_key` 复核最终 key，覆盖整条链
//!     （七个字段 → 排序紧凑 JSON → sha256 → 截断 32 位）——任何一环漂了它先响。

use std::collections::BTreeMap;
use std::path::PathBuf;

use omnicrawl_extensions::skill::{Skill, SkillMatchResult, SkillMeta};
use omnicrawl_host::prompt_cache::build_prompt_cache_identity;
use omnicrawl_llm::build_prompt_cache_key;
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/prompt_cache_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn skill_from(raw: &Value) -> Skill {
    Skill {
        meta: SkillMeta {
            name: text(raw, "name"),
            description: text(raw, "description"),
            source_path: PathBuf::from(text(raw, "source_path")),
            base_dir: PathBuf::from(text(raw, "base_dir")),
            scope: text(raw, "scope"),
            disable_model_invocation: raw["disable_model_invocation"].as_bool().unwrap_or(false),
        },
        body: text(raw, "body"),
    }
}

#[test]
fn prompt_cache_identity_matches_python() {
    let data = fixture();
    let model = data["model"].as_str().expect("数据集应带模型名");
    let cases = data["cases"].as_array().expect("用例列表");
    assert!(!cases.is_empty(), "数据集不应为空");

    for case in cases {
        let name = case["name"].as_str().unwrap_or_default();
        let inputs = &case["inputs"];
        let skills: Vec<Skill> = inputs["skills"]
            .as_array()
            .expect("skills 是数组")
            .iter()
            .map(skill_from)
            .collect();
        // 活动 Skill 在数据集里以 skills 下标表示（避免同一份 Skill 写两遍）。
        let active: Vec<SkillMatchResult> = inputs["active_skills"]
            .as_array()
            .expect("active_skills 是数组")
            .iter()
            .map(|index| {
                let index = index.as_u64().expect("下标是整数") as usize;
                SkillMatchResult {
                    skill: skills[index].clone(),
                    score: 0.9,
                    reason: "关键词命中".to_string(),
                }
            })
            .collect();
        let chat_tools: Vec<Value> = inputs["chat_tools"].as_array().expect("工具列表").clone();
        let metas: Vec<SkillMeta> = skills.iter().map(|skill| skill.meta.clone()).collect();

        let identity = build_prompt_cache_identity(
            &text(inputs, "system_prompt"),
            &PathBuf::from(text(inputs, "workspace_root")),
            &text(inputs, "project_instructions"),
            &metas,
            &active,
            &chat_tools,
        );

        // 期望里带上 `model`（Python `as_payload` 的产物），它由内核补进 key，
        // 不属于身份映射本身，因此比对上要剔除。
        let expected = &case["expected"]["identity"];
        let mut expected_map: BTreeMap<String, String> = BTreeMap::new();
        for (key, value) in expected.as_object().expect("身份是对象") {
            if key == "model" {
                continue;
            }
            expected_map.insert(key.clone(), value.as_str().unwrap_or_default().to_string());
        }
        assert_eq!(
            identity.to_identity_map(),
            expected_map,
            "用例 {name} 的七项身份"
        );

        let key = build_prompt_cache_key(&identity.to_identity_map(), model);
        assert_eq!(
            key,
            case["expected"]["prompt_cache_key"]
                .as_str()
                .unwrap_or_default(),
            "用例 {name} 的 prompt_cache_key"
        );
    }
}
