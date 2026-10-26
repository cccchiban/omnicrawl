//! `skill_search` 工具：按作用域与关键词只读检索 Skill 索引元数据。
//!
//! 语义基准是 `omnicrawl-extensions` 的 [`SkillManager`]（发现 → 索引 → 打分）：Skill 目录
//! 不再整份注入模型上下文，改由模型按需检索；正文仍由 `read` 打开 `SKILL.md` 获取。
//! 每次调用重扫一次索引，覆盖「本会话内刚装好 Skill」的场景。
//!
//! 带关键词的检索可选交给结构化决策模型重排（`decision_models.toml` 的
//! `[features] skill_search_rerank`，默认关闭）：本地打分只用来挑出更宽的一池候选，
//! 最终顺序按决策模型给出的置信度，条数仍按 `max_results`。没有关键词（按作用域列出全部）
//! 时没有「相关度」可判，不送审。

use std::path::Path;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::skills::load_skills_config;
use omnicrawl_controllers::json::python_dumps;
use omnicrawl_extensions::skill::{
    SkillManager, SkillSearchHit, DEFAULT_MATCH_THRESHOLD, SEARCH_SCOPES,
};
use serde_json::{json, Map, Value};

use super::arguments::{limited_int, optional_text};
use super::decision_search::{apply_order, reranked_order, RerankOptions, RERANK_POOL_LIMIT};
use super::error::{ToolError, ToolOutcome};

pub const DEFAULT_SEARCH_RESULTS: i64 = 10;
pub const MAX_SEARCH_RESULTS: i64 = 50;

/// 检索 Skill 索引元数据；`scope` 缺省 `all`，省略 `query` 时按作用域列出全部。
pub fn skill_search(
    root: &Path,
    env: &ConfigEnvironment,
    rerank: &RerankOptions,
    arguments: &Map<String, Value>,
) -> ToolOutcome {
    let raw_scope = optional_text(arguments, "scope").to_lowercase();
    let scope = if raw_scope.is_empty() {
        "all".to_string()
    } else {
        raw_scope
    };
    if !SEARCH_SCOPES.contains(&scope.as_str()) {
        return Err(ToolError::new(format!(
            "scope 必须是 {} 之一。",
            SEARCH_SCOPES.join("、")
        )));
    }
    let query = optional_text(arguments, "query");
    let max_results = limited_int(
        arguments,
        "max_results",
        DEFAULT_SEARCH_RESULTS,
        1,
        MAX_SEARCH_RESULTS,
    ) as usize;
    // 重排开启且有关键词时先取更宽的一池候选：本地打分只用于挑池子，最终顺序由决策模型决定。
    let reranking = rerank.active() && !query.trim().is_empty();
    let pool = if reranking {
        max_results.max(RERANK_POOL_LIMIT)
    } else {
        max_results
    };

    let mut manager = SkillManager::new();
    let extra_paths = load_skills_config(env, None)
        .map(|config| config.extra_paths)
        .unwrap_or_default();
    manager.discover(Some(root), &extra_paths);
    let mut hits = manager.search_meta(&query, &scope, pool, DEFAULT_MATCH_THRESHOLD);
    if reranking {
        let state = json!({
            "query": query,
            "scope": scope,
        });
        let texts: Vec<String> = hits.iter().map(rerank_candidate_text).collect();
        if let Some(order) = reranked_order(rerank, state, RERANK_INSTRUCTIONS, &query, &texts) {
            hits = apply_order(hits, &order);
        }
    }
    hits.truncate(max_results);
    Ok(python_dumps(
        &Value::Array(hits.iter().map(hit_value).collect()),
        2,
    ))
}

/// 送进重排的候选描述：名称、作用域与描述即可判断相关度，不必带正文。
fn rerank_candidate_text(hit: &SkillSearchHit) -> String {
    format!(
        "名称：{}\n作用域：{}\n描述：{}",
        hit.meta.name, hit.meta.scope, hit.meta.description
    )
}

/// 重排指令：只要求按相关度排序，不做其他判断。
const RERANK_INSTRUCTIONS: &str =
    "给定 state 里的检索查询与作用域，判断每个候选项与该查询的相关程度；\
state 是事实来源，不要根据候选项自身措辞推测查询；候选是一批待选 Skill，排在前面表示更该被使用。";

fn hit_value(hit: &SkillSearchHit) -> Value {
    json!({
        "name": hit.meta.name,
        "description": hit.meta.description,
        "scope": hit.meta.scope,
        "location": hit.meta.source_path.to_string_lossy(),
        "score": hit.score,
        "reason": hit.reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-skill-search-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".omnicrawl").join("skills")).expect("创建临时工作区");
        root
    }

    fn write_skill(root: &Path, name: &str, frontmatter: &str) {
        let dir = root.join(".omnicrawl").join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("建 skill 目录");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}\n---\n\n正文\n"),
        )
        .expect("写 SKILL.md");
    }

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn env(root: &Path) -> ConfigEnvironment {
        ConfigEnvironment::new(root, "windows")
    }

    fn names(output: &str) -> Vec<String> {
        serde_json::from_str::<Value>(output)
            .expect("结果是 JSON 数组")
            .as_array()
            .expect("数组")
            .iter()
            .map(|item| item["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// 不用重排的默认运行期。
    fn plain() -> RerankOptions {
        RerankOptions::default()
    }

    /// 重排桩：按给定顺序返回下标，并记录收到的候选文本（不起网络）。
    struct StubRerank {
        order: Vec<usize>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl super::super::decision_search::RerankClient for StubRerank {
        fn rank(
            &self,
            request: &super::super::decision_search::RerankRequest<'_>,
        ) -> Result<Vec<usize>, String> {
            *self.seen.lock().expect("记录未被毒化") = request.candidates.to_vec();
            Ok(self.order.clone())
        }
    }

    fn rerank_options(
        order: Vec<usize>,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> RerankOptions {
        RerankOptions {
            enabled: true,
            channel: Some(super::super::decision_search::RerankChannel {
                mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
                model: "jev-latest".to_string(),
                base_url: "http://127.0.0.1:1".to_string(),
                api_key: "jv_test".to_string(),
                retry_count: 0,
            }),
            client: std::sync::Arc::new(StubRerank { order, seen }),
            ..RerankOptions::default()
        }
    }

    #[test]
    fn query_hits_are_metadata_only() {
        let root = temp_root("query");
        write_skill(
            &root,
            "python-code-review",
            "name: python-code-review\ndescription: 审查 Python 代码的正确性与测试覆盖",
        );
        let output = skill_search(
            &root,
            &env(&root),
            &plain(),
            &arguments(json!({"query": "python code review"})),
        )
        .expect("检索成功");
        let hits: Value = serde_json::from_str(&output).expect("结果是 JSON 数组");
        let first = &hits[0];
        assert_eq!(first["name"], "python-code-review");
        assert_eq!(first["scope"], "project");
        assert!(
            first["location"]
                .as_str()
                .unwrap_or_default()
                .ends_with("SKILL.md"),
            "{first}"
        );
        assert!(first.get("body").is_none(), "只返回索引元数据：{first}");
    }

    #[test]
    fn scope_filters_and_missing_query_lists_all() {
        let root = temp_root("scope");
        write_skill(&root, "alpha", "name: alpha\ndescription: 甲级技能");
        write_skill(&root, "beta", "name: beta\ndescription: 乙级技能");

        // `project` 作用域只落在工作区目录里，结果可精确断言；`all` 还会带上本机个人级
        // 与企业级目录，因此只断言项目级的两条在其中。
        let project_only = skill_search(
            &root,
            &env(&root),
            &plain(),
            &arguments(json!({"scope": "project", "max_results": 50})),
        )
        .expect("列出成功");
        assert_eq!(names(&project_only), vec!["alpha", "beta"]);

        let all = skill_search(
            &root,
            &env(&root),
            &plain(),
            &arguments(json!({"scope": "all", "max_results": 50})),
        )
        .expect("列出成功");
        let all_names = names(&all);
        assert!(all_names.contains(&"alpha".to_string()), "{all}");
        assert!(all_names.contains(&"beta".to_string()), "{all}");

        let other_scope = skill_search(
            &root,
            &env(&root),
            &plain(),
            &arguments(json!({"scope": "path", "max_results": 50})),
        )
        .expect("列出成功");
        assert!(
            names(&other_scope).iter().all(|name| name != "alpha"),
            "作用域过滤后不应出现项目级 Skill：{other_scope}"
        );
    }

    #[test]
    fn invalid_scope_is_rejected_with_candidates() {
        let root = temp_root("bad-scope");
        let error = skill_search(
            &root,
            &env(&root),
            &plain(),
            &arguments(json!({"scope": "global"})),
        )
        .expect_err("非法作用域");
        assert_eq!(
            error.message,
            "scope 必须是 all、project、user、enterprise、path 之一。"
        );
    }

    #[test]
    fn rerank_reorders_hits_and_keeps_max_results() {
        let root = temp_root("rerank");
        for (name, description) in [
            ("skill-a", "rust 内核：工具表装配"),
            ("skill-b", "rust 内核：检索重排"),
            ("skill-c", "rust 内核：知识库索引"),
        ] {
            write_skill(
                &root,
                name,
                &format!("name: {name}\ndescription: {description}"),
            );
        }

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let options = rerank_options(vec![2, 1, 0], std::sync::Arc::clone(&seen));
        let output = skill_search(
            &root,
            &env(&root),
            &options,
            &arguments(json!({"scope": "project", "query": "rust 内核", "max_results": 2})),
        )
        .expect("检索成功");
        let hits: Value = serde_json::from_str(&output).expect("结果是 JSON 数组");
        let items = hits.as_array().expect("数组");
        assert_eq!(items.len(), 2, "返回条数仍按 max_results：{output}");
        assert_eq!(items[0]["name"], "skill-c", "按重排顺序返回：{output}");
        assert_eq!(items[1]["name"], "skill-b", "{output}");

        let recorded = seen.lock().expect("记录未被毒化").clone();
        assert_eq!(recorded.len(), 3, "候选池比 max_results 宽：{recorded:?}");
        assert!(
            recorded.iter().all(|text| text.contains("名称：")),
            "候选描述应带名称：{recorded:?}"
        );
    }

    /// 没有关键词时按作用域列全部，没有「相关度」可判，因此即使开关开着也不送审。
    #[test]
    fn listing_without_query_skips_the_rerank() {
        let root = temp_root("rerank-list");
        write_skill(&root, "alpha", "name: alpha\ndescription: 甲级技能");
        write_skill(&root, "beta", "name: beta\ndescription: 乙级技能");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let options = rerank_options(vec![1, 0], std::sync::Arc::clone(&seen));
        let output = skill_search(
            &root,
            &env(&root),
            &options,
            &arguments(json!({"scope": "project", "max_results": 50})),
        )
        .expect("列出成功");
        assert_eq!(names(&output), vec!["alpha", "beta"]);
        assert!(seen.lock().expect("记录未被毒化").is_empty());
    }

    /// 重排不可用（缺渠道）时退回本地排序，检索不该失败。
    #[test]
    fn rerank_failure_falls_back_to_local_order() {
        let root = temp_root("rerank-fail");
        write_skill(&root, "alpha", "name: alpha\ndescription: 甲级技能检索");
        let options = RerankOptions {
            enabled: true,
            channel: None,
            ..RerankOptions::default()
        };
        let output = skill_search(
            &root,
            &env(&root),
            &options,
            &arguments(json!({"scope": "project", "query": "甲级技能"})),
        )
        .expect("检索不该因为重排不可用而失败");
        assert_eq!(names(&output), vec!["alpha"]);
    }
}
