//! 表单桥：三套渠道表单（模型 / 决策 / 向量）的字段读写、校验与只读视图。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按职责拆出：字段表、掩码口径与校验
//! 规则只在对应页改动时才需要看，混在键位逻辑里会让每次改字段都碰同一个文件。

use super::*;

/// 向量渠道表单的只读视图（渲染层用）。
pub struct EmbeddingFormView<'a> {
    pub row: &'a EmbeddingRow,
    pub field: EmbeddingField,
    /// 正在编辑的文本字段缓冲（非空表示处于输入态）。
    pub input: Option<&'a Composer>,
    /// 与决策表单同形，但向量页没有枚举字段，恒为 `None`。
    pub dropdown: Option<&'a DecisionDropdown>,
    pub is_new: bool,
}

/// 向量表单字段的当前显示值。
pub fn embedding_field_value(row: &EmbeddingRow, field: EmbeddingField) -> String {
    match field {
        EmbeddingField::Name => row.name.clone(),
        EmbeddingField::BaseUrl => row.base_url.clone(),
        // 明文永不进界面：只给出「是否已配置 + 末 4 位」的掩码（与渠道页同款）。
        EmbeddingField::ApiKey => mask_api_key(&row.api_key),
        EmbeddingField::ModelId => row.model.clone(),
        EmbeddingField::Dimensions => {
            if row.dimensions > 0 {
                row.dimensions.to_string()
            } else {
                "由服务端决定".to_string()
            }
        }
        EmbeddingField::TimeoutSeconds => row.timeout_seconds.to_string(),
        EmbeddingField::Enabled => {
            if row.enabled {
                "开启".to_string()
            } else {
                "关闭".to_string()
            }
        }
    }
}

/// 向量表单某字段的输入缓冲初值（凭据从空开始）。
pub(super) fn embedding_input_composer(row: &EmbeddingRow, field: EmbeddingField) -> Composer {
    let mut composer = Composer::default();
    if field != EmbeddingField::ApiKey {
        composer.insert(&embedding_field_value(row, field));
    }
    composer
}

/// 把表单值写回向量渠道记录。
pub(super) fn set_embedding_field(row: &mut EmbeddingRow, field: EmbeddingField, value: String) {
    match field {
        EmbeddingField::Name => row.name = value,
        EmbeddingField::BaseUrl => row.base_url = value.trim().to_string(),
        // 留空 = 不改动（与渠道页、决策页同一套口径，避免误触把磁盘上的密钥抹掉）。
        EmbeddingField::ApiKey => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                row.api_key = trimmed.to_string();
            }
        }
        EmbeddingField::ModelId => row.model = value.trim().to_string(),
        // 「由服务端决定」这类非数字输入按 0 处理（0 即不下发 dimensions）。
        EmbeddingField::Dimensions => {
            row.dimensions = value.trim().parse::<i64>().unwrap_or(0).max(0);
        }
        EmbeddingField::TimeoutSeconds => {
            if let Ok(seconds) = value.trim().parse::<i64>() {
                row.timeout_seconds = seconds.max(1);
            }
        }
        EmbeddingField::Enabled => row.enabled = value == "开启",
    }
}

/// 保存前的本地校验：至少一条、key 唯一、地址与模型非空。
pub(super) fn validate_embedding_rows(rows: &[EmbeddingRow]) -> Result<(), String> {
    if rows.is_empty() {
        return Err("至少要保留一个向量渠道。".to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let key = row.key.trim();
        if key.is_empty() {
            return Err("向量渠道 key 不能为空。".to_string());
        }
        if seen.contains(&key) {
            return Err(format!("向量渠道 key 重复：{key}。"));
        }
        seen.push(key);
        if row.base_url.trim().is_empty() {
            return Err(format!("向量渠道 {key} 的 Base URL 不能为空。"));
        }
        if row.model.trim().is_empty() {
            return Err(format!("向量渠道 {key} 的模型 ID 不能为空。"));
        }
    }
    Ok(())
}

/// 决策渠道表单的只读视图（渲染层用）。
pub struct DecisionFormView<'a> {
    pub row: &'a DecisionRow,
    pub field: DecisionField,
    /// 正在编辑的文本字段缓冲（非空表示处于输入态）。
    pub input: Option<&'a Composer>,
    pub dropdown: Option<&'a DecisionDropdown>,
    pub is_new: bool,
}

/// 决策表单字段的当前显示值（`Enabled` 给「开启/关闭」）。
pub fn decision_field_value(row: &DecisionRow, field: DecisionField) -> String {
    match field {
        DecisionField::Name => row.name.clone(),
        DecisionField::Mode => row.mode.clone(),
        DecisionField::BaseUrl => row.base_url.clone(),
        // 明文永不进界面：只给出「是否已配置 + 末 4 位」的掩码（与渠道页同款）。
        DecisionField::ApiKey => mask_api_key(&row.api_key),
        DecisionField::ModelId => row.model.clone(),
        DecisionField::RetryCount => row.retry_count.to_string(),
        DecisionField::Enabled => {
            if row.enabled {
                "开启".to_string()
            } else {
                "关闭".to_string()
            }
        }
    }
}

/// 决策表单某字段的输入缓冲初值（与渠道表单同口径：凭据从空开始）。
pub(super) fn decision_input_composer(row: &DecisionRow, field: DecisionField) -> Composer {
    let mut composer = Composer::default();
    if field != DecisionField::ApiKey {
        composer.insert(&decision_field_value(row, field));
    }
    composer
}

/// 把表单值写回决策渠道记录。
pub(super) fn set_decision_field(row: &mut DecisionRow, field: DecisionField, value: String) {
    match field {
        DecisionField::Name => row.name = value,
        DecisionField::Mode => row.mode = value.trim().to_lowercase(),
        DecisionField::BaseUrl => row.base_url = value.trim().to_string(),
        // 留空 = 不改动（与渠道页、TTS 面板同一套口径，避免误触把磁盘上的密钥抹掉）。
        DecisionField::ApiKey => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                row.api_key = trimmed.to_string();
            }
        }
        DecisionField::ModelId => row.model = value.trim().to_string(),
        // 非数字输入原样保留（保存前的本地校验会给出可读提示），越界值由配置侧收口。
        DecisionField::RetryCount => {
            if let Ok(count) = value.trim().parse::<i64>() {
                row.retry_count = count;
            }
        }
        DecisionField::Enabled => row.enabled = value == "开启",
    }
}

/// 保存前的本地校验：与配置侧同口径，先挡住常见手误，避免白跑一次磁盘往返。
pub(super) fn validate_decision_rows(rows: &[DecisionRow]) -> Result<(), String> {
    if rows.is_empty() {
        return Err("至少要保留一个决策渠道。".to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let key = row.key.trim();
        if key.is_empty() {
            return Err("决策渠道 key 不能为空。".to_string());
        }
        if seen.contains(&key) {
            return Err(format!("决策渠道 key 重复：{key}。"));
        }
        seen.push(key);
        if row.name.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的名称不能为空。"));
        }
        if row.mode.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的请求方式不能为空。"));
        }
        if row.base_url.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的基地址不能为空。"));
        }
        if row.model.trim().is_empty() {
            return Err(format!("决策渠道 {key} 的模型 ID 不能为空。"));
        }
    }
    Ok(())
}

/// 挑默认决策渠道：原来的那条还在且启用就沿用，否则取第一条启用的。
pub(super) fn pick_default_decision_channel(rows: &[DecisionRow], current: &str) -> String {
    if let Some(row) = rows.iter().find(|row| row.key == current && row.enabled) {
        return row.key.clone();
    }
    rows.iter()
        .find(|row| row.enabled)
        .or_else(|| rows.first())
        .map(|row| row.key.clone())
        .unwrap_or_default()
}

pub(super) fn pick_default_embedding_channel(rows: &[EmbeddingRow], current: &str) -> String {
    if let Some(row) = rows.iter().find(|row| row.key == current && row.enabled) {
        return row.key.clone();
    }
    rows.iter()
        .find(|row| row.enabled)
        .or_else(|| rows.first())
        .map(|row| row.key.clone())
        .unwrap_or_default()
}

/// 渠道表单的只读视图（渲染层用）。
pub struct ChannelFormView<'a> {
    pub row: &'a ChannelRow,
    pub field: ChannelField,
    /// 正在编辑的文本字段缓冲（非空表示处于输入态）。
    pub input: Option<&'a Composer>,
    pub dropdown: Option<&'a ChannelDropdown>,
    pub is_new: bool,
}

/// 表单字段的当前显示值（`Enabled` 给「开启/关闭」）。
pub fn channel_field_value(row: &ChannelRow, field: ChannelField) -> String {
    match field {
        ChannelField::Name => row.name.clone(),
        ChannelField::Provider => row.provider.clone(),
        ChannelField::Protocol => row.protocol.clone(),
        ChannelField::BaseUrl => row.base_url.clone(),
        // 明文永不进界面：只给出「是否已配置 + 末 4 位」的掩码。
        ChannelField::ApiKey => mask_api_key(&row.api_key),
        ChannelField::ModelId => row.model_id.clone(),
        ChannelField::UserAgent => row.user_agent.clone(),
        ChannelField::Enabled => {
            if row.enabled {
                "开启".to_string()
            } else {
                "关闭".to_string()
            }
        }
    }
}

/// 渠道表单某字段的输入缓冲初值：API Key 从空开始（不回显明文，让「留空＝不改动」成为默认），
/// 其余字段以当前值为起点。
pub(super) fn channel_input_composer(row: &ChannelRow, field: ChannelField) -> Composer {
    let mut composer = Composer::default();
    if field != ChannelField::ApiKey {
        composer.insert(&channel_field_value(row, field));
    }
    composer
}

/// 把表单值写回渠道记录。
pub(super) fn set_channel_field(row: &mut ChannelRow, field: ChannelField, value: String) {
    match field {
        ChannelField::Name => row.name = value,
        ChannelField::Provider => row.provider = value,
        ChannelField::Protocol => row.protocol = value,
        ChannelField::BaseUrl => row.base_url = value,
        // 留空 = 不改动（与 TTS 面板同一套口径，避免误触把磁盘上的密钥抹掉）。
        ChannelField::ApiKey => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                row.api_key = trimmed.to_string();
            }
        }
        ChannelField::ModelId => row.model_id = value,
        ChannelField::UserAgent => row.user_agent = value,
        ChannelField::Enabled => row.enabled = value == "开启",
    }
}

/// 保存前的本地校验：与配置侧的口径一致，但给出更直接的中文提示。
///
/// 配置侧（`save_channel_configuration`）还会再校验一次并做原子回滚，这里只挡住
/// 最常见的手误，避免白跑一次磁盘往返。
pub(super) fn validate_channels(rows: &[ChannelRow]) -> Result<(), String> {
    if rows.is_empty() {
        return Err("至少要保留一个模型渠道。".to_string());
    }
    if rows.iter().all(|row| !row.enabled) {
        return Err("至少需要启用一个模型渠道。".to_string());
    }
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let key = row.key.trim();
        if key.is_empty() {
            return Err("渠道 key 不能为空。".to_string());
        }
        if seen.contains(&key) {
            return Err(format!("渠道 key 重复：{key}。"));
        }
        seen.push(key);
        if row.name.trim().is_empty() {
            return Err(format!("渠道 {key} 的名称不能为空。"));
        }
        if row.model_id.trim().is_empty() {
            return Err(format!("渠道 {key} 的模型 ID 不能为空。"));
        }
        if row.base_url.trim().is_empty() {
            return Err(format!("渠道 {key} 的基地址不能为空。"));
        }
    }
    Ok(())
}

/// 挑默认渠道：原来的那条还在且启用就沿用，否则取第一条启用的。
pub(super) fn pick_default_channel(rows: &[ChannelRow], current: &str) -> String {
    if let Some(row) = rows.iter().find(|row| row.key == current && row.enabled) {
        return row.key.clone();
    }
    rows.iter()
        .find(|row| row.enabled)
        .or_else(|| rows.first())
        .map(|row| row.key.clone())
        .unwrap_or_default()
}
