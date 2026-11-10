//! 联网搜索页：默认引擎 + 各接口型引擎凭据的独立一级设置页。
//!
//! 它原先是工具页里的一行入口（`Enter` 才进表单）；既然设置项本身与工具开关无关，
//! 就与其余表单页同构：左栏一项、右栏一页，草稿从配置投影起步。

use super::*;

/// 联网搜索页的草稿：默认引擎 + 各凭据的掩码（未配置留空）。
///
/// 与 `WEB_SEARCH_FIELDS` 的字段顺序一一对应。凭据本身从不进界面：已配置的给掩码
/// （掩码本身就是「不改动」的标记），未配置的留空。
pub(super) fn web_search_form_values(row: &WebSearchRow) -> Vec<FieldValue> {
    vec![
        FieldValue::Text(row.engine.clone()),
        FieldValue::Text(secret_draft_for(row.tavily_configured)),
        FieldValue::Text(secret_draft_for(row.exa_configured)),
        FieldValue::Text(secret_draft_for(row.firecrawl_configured)),
        FieldValue::Text(secret_draft_for(row.anysearch_configured)),
        FieldValue::Text(secret_draft_for(row.tinyfish_configured)),
        FieldValue::Text(secret_draft_for(row.github_configured)),
        FieldValue::Text(row.deepseek_search_url.clone()),
        FieldValue::Text(secret_draft_for(row.deepseek_configured)),
        FieldValue::Text(row.grok_search_url.clone()),
        FieldValue::Text(secret_draft_for(row.grok_configured)),
        FieldValue::Text(row.grok_model.clone()),
        FieldValue::Text(secret_draft_for(row.keenable_configured)),
    ]
}

impl SettingsState {
    /// 联网搜索页的保存：草稿原样交给宿主。
    ///
    /// 凭据字段的掩码必须保留着送出去：宿主据此判断「不动磁盘原值」，
    /// 而空串表示「把这把凭据删掉」（见 `App::apply_web_search_form`）。
    pub(super) fn save_web_search_form(&mut self) -> Option<SettingsEvent> {
        let form = self.form()?;
        let values: Vec<FieldValue> = (0..form.specs().len())
            .map(|index| {
                let raw = form.value(index).map(|value| value.text()).unwrap_or("");
                FieldValue::Text(raw.to_string())
            })
            .collect();
        Some(SettingsEvent::Apply(SettingsChange::Form {
            kind: FormKind::WebSearch,
            values,
        }))
    }
}
