//! `controllers/turn/loop.py` 手动技能命令的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `skill_command` 段——探针用技能管理器桩真跑
//! `_apply_skill_command`。本套件用同一批输入重放 Rust 实现，比对命令解析、状态提示与匹配结果。

use omnicrawl_controllers::turn::skill_command::{
    manual_skill_reason, parse_skill_command, skill_default_prompt, skill_loaded_status,
    skill_not_found_message, skill_not_found_status, MANUAL_SKILL_SCORE,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["skill_command"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

#[test]
fn skill_command_matches_python() {
    let data = section();
    let cases = data["cases"].as_array().expect("cases");
    assert!(cases.len() >= 9, "对照用例过少：{}", cases.len());
    for case in cases {
        let label = case["label"].as_str().expect("label");
        let text = case["text"].as_str().expect("text");
        let available = strings(&case["available"]);
        let returned = case["returned"].as_str().expect("returned");
        let statuses = strings(&case["statuses"]);
        let active = case["active_skills"].as_array().expect("active_skills");

        let parsed = parse_skill_command(text);
        // 没有技能管理器时，即使文本是合法命令也原样放行。
        if !case["manager_present"].as_bool().expect("manager_present") || parsed.is_none() {
            assert_eq!(returned, text, "原样返回（{label}）");
            assert!(statuses.is_empty(), "不应有状态提示（{label}）");
            assert!(active.is_empty(), "不应加载技能（{label}）");
            continue;
        }

        let (name, rest) = parsed.expect("已确认是命令");
        if let Some(skill) = active.first() {
            assert_eq!(
                skill["name"].as_str(),
                Some(name.as_str()),
                "加载的技能（{label}）"
            );
            assert_eq!(
                skill["score"].as_f64(),
                Some(MANUAL_SKILL_SCORE),
                "匹配分数（{label}）"
            );
            assert_eq!(
                skill["reason"].as_str(),
                Some(manual_skill_reason(&name).as_str()),
                "匹配说明（{label}）"
            );
            assert_eq!(
                statuses,
                vec![skill_loaded_status(&name)],
                "加载提示（{label}）"
            );
            let expected = rest.unwrap_or_else(|| skill_default_prompt(&name));
            assert_eq!(returned, expected, "加载后的任务文本（{label}）");
        } else {
            assert!(
                available.iter().all(|item| item != &name),
                "未命中前提（{label}）"
            );
            assert_eq!(
                statuses,
                vec![skill_not_found_status(&name)],
                "未命中提示（{label}）"
            );
            assert_eq!(
                returned,
                skill_not_found_message(&name, &available),
                "未命中文案（{label}）"
            );
        }
    }
}
