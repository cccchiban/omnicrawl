//! 抓取引擎页：选定 `fetcher` 用哪条通路取正文。
//!
//! 与联网搜索页同构（引擎候选 + 保存），区别是这里没有凭据：抓取接口与联网搜索打的是
//! 同一批服务，Key 仍填在「联网搜索」页，两页各存一份只会让它们不一致。
//!
//! 三个选项的含义：
//! - `local`：本地直连，不依赖任何凭据，行为与改造前完全一致（默认）；
//! - 某个引擎名：把抓取交给该服务的抓取接口，适合直连取不到内容的站点；
//! - `balanced`：在有凭据的引擎之间轮转，把额度摊开（AnySearch 的匿名额度也算可用）。
//!
//! 「接口失败时回落本地」默认开着：抓取是按次计费的，一次额度耗尽不该让整次抓取失败。

use super::*;

/// 抓取页的初值：当前引擎与回落开关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetcherRow {
    pub engine: String,
    /// 远程失败时是否回落本地直连。
    pub fallback_local: bool,
}

impl Default for FetcherRow {
    fn default() -> Self {
        Self {
            engine: omnicrawl_config::features::fetcher::DEFAULT_FETCH_ENGINE.to_string(),
            fallback_local: true,
        }
    }
}

/// 抓取页的草稿，与 `FETCHER_FIELDS` 的字段顺序一一对应。
pub(super) fn fetcher_form_values(row: &FetcherRow) -> Vec<FieldValue> {
    vec![
        FieldValue::Text(row.engine.clone()),
        FieldValue::Flag(row.fallback_local),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_local_with_fallback() {
        let row = FetcherRow::default();
        assert_eq!(row.engine, "local");
        assert!(row.fallback_local, "缺省应当有回落");
        assert_eq!(
            fetcher_form_values(&row),
            vec![
                FieldValue::Text("local".to_string()),
                FieldValue::Flag(true),
            ]
        );
    }
}
