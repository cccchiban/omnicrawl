//! 联网搜索页：默认引擎 + 六把凭据的独立一级设置页。
//!
//! 它原先是工具页里的一行入口（`Enter` 才进表单）；既然设置项本身与工具开关无关，
//! 就与其余表单页同构：左栏一项、右栏一页，草稿从配置投影起步。

use super::*;

/// 联网搜索页的草稿：默认引擎 + 各凭据的掩码（未配置留空）。
///
/// 与 `WEB_SEARCH_FIELDS` 的字段顺序一一对应。凭据本身从不进界面：已配置的给掩码
/// （保存时折算成「不改动」），未配置的留空。
pub(super) fn web_search_form_values(row: &WebSearchRow) -> Vec<FieldValue> {
    vec![
        FieldValue::Text(row.engine.clone()),
        FieldValue::Text(secret_draft_for(row.tavily_configured)),
        FieldValue::Text(secret_draft_for(row.exa_configured)),
        FieldValue::Text(secret_draft_for(row.firecrawl_configured)),
        FieldValue::Text(secret_draft_for(row.anysearch_configured)),
        FieldValue::Text(secret_draft_for(row.tinyfish_configured)),
        FieldValue::Text(secret_draft_for(row.github_configured)),
    ]
}

impl SettingsState {
    /// 联网搜索页的保存：先把掩码折算成空串（空 = 不改动磁盘原值），再交给宿主。
    ///
    /// 宿主那侧只认「非空即新值」，所以这一步必须在界面层做完，不能把掩码原样送出去。
    pub(super) fn save_web_search_form(&mut self) -> Option<SettingsEvent> {
        let form = self.form()?;
        let values: Vec<FieldValue> = (0..form.specs().len())
            .map(|index| {
                let raw = form.value(index).map(|value| value.text()).unwrap_or("");
                FieldValue::Text(secret_draft(raw))
            })
            .collect();
        Some(SettingsEvent::Apply(SettingsChange::Form {
            kind: FormKind::WebSearch,
            values,
        }))
    }
}
