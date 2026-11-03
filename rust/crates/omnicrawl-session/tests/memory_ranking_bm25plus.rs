//! 记忆检索打分（BM25+）的行为约束。
//!
//! 对照测试只保证「分数与夹具一致」，这里保证「夹具值变了之后语义还成立」：
//! 相关度必须压过目录与新鲜度偏置、长文档不会因为长度被压没、无关条目不会靠
//! 使用次数冒到前面。

use chrono::{DateTime, FixedOffset};
use omnicrawl_session::{score_search_entry, MemoryIndexEntry};
use serde_json::json;

const NOW: &str = "2026-11-03T12:00:00+08:00";

fn now() -> DateTime<FixedOffset> {
    omnicrawl_session::parse_memory_datetime(NOW).expect("固定时刻可解析")
}

fn entry(id: &str, storage_directory: &str, summary: &str, touch_count: u64) -> MemoryIndexEntry {
    MemoryIndexEntry::from_dict(&json!({
        "id": id,
        "path": format!("{storage_directory}/{id}.md"),
        "storage_directory": storage_directory,
        "timestamp": NOW,
        "touch_count": touch_count,
        "related_directories": [storage_directory],
        "summary": summary,
    }))
    .expect("索引条目可解析")
}

fn score(entry: &MemoryIndexEntry, query: &str, directories: &[&str]) -> f64 {
    let directories: Vec<String> = directories.iter().map(|item| item.to_string()).collect();
    score_search_entry(entry, query, &directories, now())
}

/// BM25+ 的存在理由：长文档里命中一次，得分必须仍明显高于完全没命中的短文档。
#[test]
fn a_long_document_that_matches_still_beats_an_unrelated_short_one() {
    let padding = "填充".repeat(400);
    let long = entry("20261103-120001", "code-knowledge/general", &format!("检索排序{padding}"), 0);
    let short = entry("20261103-120002", "code-knowledge/general", "沟通偏好", 9);

    let long_score = score(&long, "检索排序", &[]);
    let short_score = score(&short, "检索排序", &[]);
    assert!(
        long_score > short_score * 10.0,
        "命中的长文档 {long_score} 应远高于无关的短文档 {short_score}"
    );
}

/// BM25+ 下界的用处：长文档命中时的贡献不能被长度归一压到接近 0。
///
/// 构造「词汇丰富因而很长」的单条文档与一批极短条目，比较命中项本身的贡献
/// （用同一查询的 Δ 分数衡量：δ 让命中文档至少多出一份与长度无关的底分）。
#[test]
fn length_normalization_cannot_erase_a_hit_in_a_long_document() {
    // 词汇丰富：每个中文片段都不重复，长度接近 800 个 token。
    let rich: String = (0..400).map(|offset| char::from_u32(0x4e00 + offset).expect("汉字")).collect();
    let long = entry(
        "20261103-130009",
        "code-knowledge/general",
        &format!("排序{rich}"),
        0,
    );
    let filler: Vec<MemoryIndexEntry> = (0..6)
        .map(|index| {
            entry(
                &format!("20261103-13000{index}"),
                "code-knowledge/general",
                "短摘要",
                0,
            )
        })
        .collect();

    let mut entries = filler;
    entries.push(long.clone());
    let query = omnicrawl_session::SearchQuery::new("排序");
    let scorer = omnicrawl_session::MemoryScorer::new(&entries).with_query(&query);
    let now = now();

    let long_score = scorer.score_index(entries.len() - 1, &long, &query, &[], now);
    // 命中文档的分数必须与「完全没命中」拉开量级，而不是只差一份微弱的偏置。
    assert!(
        long_score > 20.0,
        "长度归一后命中项仍须保留可观贡献，实际 {long_score}"
    );
}

/// 重复词只做饱和加分，不该把得分推到无界。
#[test]
fn repeated_terms_saturate() {
    let once = entry("20261103-120003", "code-knowledge/general", "排序", 0);
    let many = entry(
        "20261103-120004",
        "code-knowledge/general",
        "排序排序排序排序排序排序排序排序排序排序",
        0,
    );
    let once_score = score(&once, "排序", &[]);
    let many_score = score(&many, "排序", &[]);
    assert!(many_score > once_score, "多出现应当更相关：{many_score} vs {once_score}");
    assert!(
        many_score < once_score * 4.0,
        "词频必须饱和，不能线性增长：{many_score} vs {once_score}"
    );
}

/// 只提到「有查询词」并不等于相关：目录命中也不能盖过正文相关度。
#[test]
fn text_relevance_outranks_directory_hits() {
    let text_hit = entry("20261103-120005", "code-knowledge/general", "记忆检索排序实现", 0);
    let directory_hit = entry("20261103-120006", "task-history/general", "任务完成情况", 0);

    let text_score = score(&text_hit, "检索排序", &[]);
    let directory_score = score(&directory_hit, "检索排序", &["task-history/general"]);
    assert!(
        text_score > directory_score,
        "正文命中 {text_score} 应高于目录命中 {directory_score}"
    );
}

/// 没有任何文本相关度时，使用次数不该把无关条目抬成命中。
#[test]
fn unrelated_entries_do_not_surface_on_usage_alone() {
    let unrelated = entry("20261103-120007", "task-history/general", "无关内容", 10);
    assert!(
        score(&unrelated, "检索排序", &[]) < 1.0,
        "无关条目只应拿到微小的偏置分"
    );
}

/// 中文二字片段要能对上顺序不同的说法（与 BM25 分词一致）。
#[test]
fn chinese_bigrams_ignore_word_order() {
    let entry_a = entry("20261103-120008", "code-knowledge/general", "代码审查要点", 0);
    let entry_b = entry("20261103-120009", "code-knowledge/general", "图片压缩", 0);
    assert!(
        score(&entry_a, "审查代码", &[]) > score(&entry_b, "审查代码", &[]),
        "换序的二字片段仍应命中"
    );
}

/// 整个查询原文出现在候选文本里时应当比只命中零散片段更相关。
#[test]
fn a_whole_phrase_scores_higher_than_scattered_tokens() {
    let phrase = entry("20261103-120010", "code-knowledge/general", "记忆检索", 0);
    let scattered = entry("20261103-120011", "code-knowledge/general", "记忆 与 检索", 0);
    assert!(
        score(&phrase, "记忆检索", &[]) > score(&scattered, "记忆检索", &[]),
        "整段命中应略高于零散命中"
    );
}
