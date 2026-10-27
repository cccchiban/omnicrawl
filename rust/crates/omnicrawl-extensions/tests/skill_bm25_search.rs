//! BM25 检索的补充覆盖：正文进索引、名称序截断、中文二字切分与长度归一。
//!
//! 用例只碰临时目录，不写任何项目文件。

use std::path::{Path, PathBuf};

use omnicrawl_extensions::skill::SkillManager;

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "omnicrawl-skill-bm25-{tag}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("临时目录创建失败");
    root
}

/// 在工作区写一个 Skill；`body` 是 frontmatter 之后的正文。
fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(".omnicrawl").join("skills").join(name);
    std::fs::create_dir_all(&dir).expect("skill 目录创建失败");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .expect("SKILL.md 写入失败");
}

fn manager(root: &Path) -> SkillManager {
    let mut manager = SkillManager::new();
    manager.discover_with_enterprise(Some(root), &[], None);
    manager
}

fn names(manager: &SkillManager, query: &str) -> Vec<String> {
    manager
        .search_meta(query, "project", 10)
        .into_iter()
        .map(|hit| hit.meta.name)
        .collect()
}

/// 正文里的词也能检索到——只在描述里检索会漏掉「描述写得泛」的 Skill。
#[test]
fn the_body_participates_in_the_index() {
    let root = temp_dir("body");
    write_skill(&root, "alpha", "通用助手", "这个 Skill 负责 kubernetes 部署");
    write_skill(&root, "beta", "通用助手", "这个 Skill 负责写周报");

    assert_eq!(names(&manager(&root), "kubernetes"), vec!["alpha"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// 中文按二字片段切分：「代码审查」与「审查代码」能互相命中。
#[test]
fn chinese_bigrams_match_regardless_of_word_order() {
    let root = temp_dir("cjk");
    write_skill(&root, "review", "代码审查", "正文");
    write_skill(&root, "unrelated", "图片压缩", "正文");

    let found = names(&manager(&root), "审查代码");
    assert_eq!(found, vec!["review"], "查询：{found:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 命中条目按名称排序，固定最多十条。
#[test]
fn hits_are_name_sorted_and_capped() {
    let root = temp_dir("cap");
    for index in 0..12 {
        let name = format!("skill-{index:02}");
        write_skill(&root, &name, "检索排序用技能", "正文");
    }
    let listed = names(&manager(&root), "检索排序用技能");
    assert_eq!(listed.len(), 10, "{listed:?}");
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "按名称排序：{listed:?}");
    assert_eq!(listed[0], "skill-00", "从最小的名称开始截断：{listed:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// 没有交集时返回空集，不返回「低分噪声」。
#[test]
fn a_query_without_common_tokens_matches_nothing() {
    let root = temp_dir("empty");
    write_skill(&root, "alpha", "甲级技能", "正文");
    assert!(names(&manager(&root), "毫不相干的其他词").is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// 长度归一：正文更短、命中同样词的条目得分不低于靠长度堆词的条目。
#[test]
fn a_shorter_document_is_not_outscored_by_padding() {
    let root = temp_dir("normalize");
    write_skill(&root, "short", "检索", "检索");
    write_skill(&root, "long", "检索", &"填充词 ".repeat(200));
    let hits = manager(&root).search_meta("检索", "project", 10);
    let short = hits
        .iter()
        .find(|hit| hit.meta.name == "short")
        .expect("short 应命中");
    let long = hits
        .iter()
        .find(|hit| hit.meta.name == "long")
        .expect("long 应命中");
    assert!(
        short.score >= long.score,
        "短正文不该被长正文压下去：short={} long={}",
        short.score,
        long.score
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 未给关键词时按名称列出，不打分。
#[test]
fn listing_without_query_is_name_sorted() {
    let root = temp_dir("list");
    write_skill(&root, "zeta", "戊", "正文");
    write_skill(&root, "alpha", "甲", "正文");
    let hits = manager(&root).search_meta("", "project", 10);
    let listed: Vec<String> = hits.iter().map(|hit| hit.meta.name.clone()).collect();
    assert_eq!(listed, vec!["alpha", "zeta"]);
    assert!(hits.iter().all(|hit| hit.score == 0.0));
    let _ = std::fs::remove_dir_all(&root);
}
