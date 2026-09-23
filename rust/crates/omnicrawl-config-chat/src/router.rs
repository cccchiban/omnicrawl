//! 配置对话路由（对映 `omnicrawl/config_chat/router.py`）：一句自然语言 →
//! 「动作 + 配置路径 + 取值」列表。
//!
//! 三层与 Python 一一对应：
//!
//! 1. **片段切分**：按标点与连接词（然后 / 接着 / 顺便 / 另外 / 还有）把输入切成若干从句；
//! 2. **逐从句前向**：字符级双向 GRU 的两个头做 BIO 解码——`config_logits` 定配置片段、
//!    `value_logits` 定取值片段（并按 URL / 路径字符集向右吞并）；
//! 3. **别名检索**：片段内的 token 表示求均值并归一化，与预计算的别名向量点积，
//!    同一配置取最大相似度，最大者即命中的配置路径。
//!
//! 与 Python 的差异只有一处：切分用**手写扫描器**替代 `re.split`（分隔符集合固定、
//! 不依赖正则语义，见 [`split_clauses`]）。

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use crate::assets::{LabelsDocument, ALIASES_FILENAME, LABELS_FILENAME};
use crate::router_weights::{mean_pool, normalize, RouterWeights, WEIGHTS_FILENAME};
use crate::service::ConfigChatCommand;

/// 单个从句进入模型的最大字符数（对映 Python 的 `list(clause)[:96]`）。
pub const MAX_CLAUSE_CHARS: usize = 96;
/// 单个从句最多产出的命令数（对映 Python 的 `spans[:12]`）。
pub const MAX_SPANS: usize = 12;
/// 没有被任何别名覆盖的配置初始分（对映 Python 的 `torch.full(..., -2.0)`）。
const NO_MATCH_SCORE: f32 = -2.0;
/// 片段向量归一化的范数下限（对映 Python 的 `pooled.norm().clamp(min=1e-6)`）。
const POOL_NORM_MIN: f32 = 1e-6;

/// 取值片段允许向右吞并的字符集（与 Python 的字面量逐字一致）。
const VALUE_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-/:\\";

/// 切分从句的连接词（与 Python 正则的 `|` 备选逐字一致）。
const CLAUSE_WORDS: [&str; 5] = ["然后", "接着", "顺便", "另外", "还有"];

/// 配置对话模型不可用（对映 Python 的 `ConfigRouterUnavailable`：
/// Python 侧是缺可选依赖 torch，内核侧是缺随包权重文件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRouterError {
    /// 权重或资源缺失 / 不可读；文案直接进服务层错误。
    Unavailable(String),
    /// 资源存在但不可用（解析失败、张量缺失或形状不符）。
    Invalid(String),
}

impl ConfigRouterError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(message) | Self::Invalid(message) => message,
        }
    }
}

impl fmt::Display for ConfigRouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ConfigRouterError {}

/// 本地无上下文的配置对话路由器：权重 + 标签表 + 别名向量索引。
pub struct ConfigRouter {
    weights: RouterWeights,
    /// `kind == "section"` 的配置路径（开关动作要改写成 `<段>.enabled`）。
    sections: HashSet<String>,
    /// 别名向量（行主序：每个别名一行，已 L2 归一化）。
    alias_vectors: Vec<Vec<f32>>,
    /// 别名向量所属的配置下标。
    alias_owner: Vec<usize>,
}

impl ConfigRouter {
    /// 从内核资源目录读取 `config_router.bin` / `labels.json` / `aliases.json`。
    pub fn load(assets_dir: &Path) -> Result<Self, ConfigRouterError> {
        let weights_path = assets_dir.join(WEIGHTS_FILENAME);
        let weights = std::fs::read(&weights_path).map_err(|error| {
            ConfigRouterError::Unavailable(format!(
                "配置对话需要内核权重文件 {}：{error}",
                weights_path.display()
            ))
        })?;
        let labels =
            read_text(&assets_dir.join(LABELS_FILENAME)).map_err(ConfigRouterError::Invalid)?;
        let aliases =
            read_text(&assets_dir.join(ALIASES_FILENAME)).map_err(ConfigRouterError::Invalid)?;
        Self::from_sources(&weights, &labels, &aliases)
    }

    /// 从内存中的资源构造（权重二进制 + 两份 JSON 文本）。
    pub fn from_sources(
        weights: &[u8],
        labels: &str,
        aliases: &str,
    ) -> Result<Self, ConfigRouterError> {
        let weights = RouterWeights::from_bytes(weights).map_err(ConfigRouterError::Invalid)?;
        if weights.configs().is_empty() {
            return Err(ConfigRouterError::Invalid(
                "配置对话权重缺少配置表。".to_string(),
            ));
        }
        let labels = LabelsDocument::parse(labels).map_err(|error| {
            ConfigRouterError::Invalid(format!("解析配置对话标签表失败：{error}"))
        })?;
        let alias_map = crate::assets::parse_aliases(aliases).map_err(|error| {
            ConfigRouterError::Invalid(format!("解析配置对话别名表失败：{error}"))
        })?;

        // 别名索引：`alias_map.get(config, [config])`——没有别名的配置以路径本身入表；
        // 空列表则整条配置不入表（检索时保持 -2.0 初始分）。
        let mut texts: Vec<&str> = Vec::new();
        let mut owners: Vec<usize> = Vec::new();
        for (index, config) in weights.configs().iter().enumerate() {
            match alias_map.get(config) {
                Some(aliases) => {
                    for alias in aliases {
                        texts.push(alias);
                        owners.push(index);
                    }
                }
                None => {
                    texts.push(config);
                    owners.push(index);
                }
            }
        }

        let mut alias_vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for text in &texts {
            alias_vectors.push(
                weights
                    .encode_text(text)
                    .map_err(ConfigRouterError::Invalid)?,
            );
        }

        Ok(Self {
            sections: labels.sections().into_iter().collect(),
            alias_vectors,
            alias_owner: owners,
            weights,
        })
    }

    pub fn weights(&self) -> &RouterWeights {
        &self.weights
    }

    pub fn sections(&self) -> &HashSet<String> {
        &self.sections
    }

    /// 别名索引向量条数（对照测试与诊断用）。
    pub fn alias_count(&self) -> usize {
        self.alias_vectors.len()
    }

    /// 一句话 → 命令列表（顺序即从句顺序，从句内即片段顺序）。
    pub fn predict(&self, text: &str) -> Result<Vec<ConfigChatCommand>, ConfigRouterError> {
        let mut commands: Vec<ConfigChatCommand> = Vec::new();
        for clause in split_clauses(text) {
            let characters: Vec<char> = clause.chars().take(MAX_CLAUSE_CHARS).collect();
            if characters.is_empty() {
                continue;
            }
            let ids = self.weights.token_ids(&characters);
            let output = self
                .weights
                .forward(&ids)
                .map_err(ConfigRouterError::Invalid)?;

            let config_tags: Vec<usize> =
                output.config_logits.iter().map(|row| argmax(row)).collect();
            let value_tags: Vec<usize> =
                output.value_logits.iter().map(|row| argmax(row)).collect();
            let spans = spans_of(&config_tags);
            let values = value_spans(&characters, &value_tags);

            for (index, (start, end)) in spans.iter().take(MAX_SPANS).enumerate() {
                let pooled = mean_pool(&output.token_reps[*start..*end]);
                let pooled = normalize(&pooled, POOL_NORM_MIN);
                let (config_index, score) = self.best_config(&pooled);
                let mut config = self.weights.configs()[config_index].clone();
                let action = self.weights.actions()[argmax(&output.action_logits[*start])].clone();
                // 段上的开关动作落到实处：`memory` → `memory.enabled`。
                if matches!(action.as_str(), "ENABLE" | "DISABLE")
                    && self.sections.contains(&config)
                {
                    let enabled_path = format!("{config}.enabled");
                    if self.weights.configs().contains(&enabled_path) {
                        config = enabled_path;
                    }
                }
                let mut value = values.get(index).cloned().unwrap_or_default();
                if matches!(action.as_str(), "ENABLE" | "DISABLE") {
                    value = if action == "ENABLE" { "true" } else { "false" }.to_string();
                }
                commands.push(ConfigChatCommand {
                    action,
                    config,
                    value,
                    score: f64::from(score),
                });
            }
        }
        Ok(commands)
    }

    /// 别名检索：每个配置取「其别名与该片段的最大相似度」，再取全局最大者。
    ///
    /// 公开是为了让对照测试能单独核对检索本身（片段向量由 [`mean_pool`] + [`normalize`] 给出）。
    pub fn best_config(&self, pooled: &[f32]) -> (usize, f32) {
        let mut scores = vec![NO_MATCH_SCORE; self.weights.configs().len()];
        for (index, vector) in self.alias_vectors.iter().enumerate() {
            let similarity: f32 = vector
                .iter()
                .zip(pooled.iter())
                .map(|(left, right)| left * right)
                .sum();
            let owner = self.alias_owner[index];
            if similarity > scores[owner] {
                scores[owner] = similarity;
            }
        }
        let index = argmax(&scores);
        (index, scores[index])
    }
}

/// 按标点与连接词切分从句：保留非空片段（对映 `re.split(...)` + `strip()` + 过滤）。
///
/// Python 侧的模式是 `[，,；;。！？!?]|然后|接着|顺便|另外|还有`；连接词的首字都不在标点
/// 集合里，因此「标点优先、词次之」的手写扫描与正则的择一语义完全一致。
pub fn split_clauses(text: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let ch = rest.chars().next().expect("rest 非空时必有首字符");
        if is_clause_punct(ch) {
            parts.push(std::mem::take(&mut current));
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        if let Some(word) = CLAUSE_WORDS
            .iter()
            .find(|candidate| rest.starts_with(**candidate))
        {
            parts.push(std::mem::take(&mut current));
            rest = &rest[word.len()..];
            continue;
        }
        current.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// 从句分隔标点（与 Python 的字符类逐字一致：没有 ASCII 句点）。
fn is_clause_punct(ch: char) -> bool {
    matches!(ch, '，' | ',' | '；' | ';' | '。' | '！' | '!' | '？' | '?')
}

/// BIO 式片段解码（对映 Python 的 `_spans`）：`1` 开新片段，`2` 续接，其余收尾。
pub fn spans_of(tags: &[usize]) -> Vec<(usize, usize)> {
    let mut result: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, tag) in tags.iter().enumerate() {
        if *tag == 1 {
            if let Some(previous) = start {
                result.push((previous, index));
            }
            start = Some(index);
        } else if *tag != 2 {
            if let Some(previous) = start {
                result.push((previous, index));
                start = None;
            }
        }
    }
    if let Some(previous) = start {
        result.push((previous, tags.len()));
    }
    result
}

/// 取值片段：在 BIO 片段基础上按 [`VALUE_CHARS`] 向右吞并（对映 Python 的 `_value_spans`）。
pub fn value_spans(characters: &[char], tags: &[usize]) -> Vec<String> {
    spans_of(tags)
        .into_iter()
        .map(|(start, end)| {
            let mut end = end;
            while end < characters.len() && VALUE_CHARS.contains(characters[end]) {
                end += 1;
            }
            characters[start..end].iter().collect()
        })
        .collect()
}

/// 取最大值的下标；并列时取最小下标（与 `torch.argmax` 一致）。
fn argmax(values: &[f32]) -> usize {
    let mut best = 0usize;
    let mut best_value = f32::NEG_INFINITY;
    for (index, value) in values.iter().enumerate() {
        if *value > best_value {
            best = index;
            best_value = *value;
        }
    }
    best
}

fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|error| format!("读取配置对话资源失败：{}，{error}", path.display()))
}
