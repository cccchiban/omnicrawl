//! NER 兜底层（对应 `omnicrawl/llm/desensitization/ner.py`）。
//!
//! 语义兜底：识别「形态普通但语义敏感」的人名 / 地名 / 机构名，接在结构层、值类型规则层
//! 与熵兜底之后，只产出**实体区间**（占位符分配与还原沿用既有链路）。
//!
//! 模型前向由 [`NerBackend`] 注入：分块、中文隔离、批次打包、BIO 解码、实体过滤、
//! 块级 LRU 缓存与静默降级判定都在本模块，与权重格式和推理设备无关。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::desensitization::find_placeholders;

/// 支持脱敏的实体类型（与模型 BIO 标签一致）。
pub const NER_ENTITY_TYPES: [&str; 3] = ["PER", "ORG", "LOC"];

pub const DEVICE_AUTO: &str = "auto";
pub const DEVICE_CPU: &str = "cpu";
pub const DEVICE_CUDA: &str = "cuda";
pub const NER_DEVICES: [&str; 3] = [DEVICE_AUTO, DEVICE_CPU, DEVICE_CUDA];

/// 随包分发的默认 checkpoint 文件名。
pub const PACKAGED_MODEL_FILENAME: &str = "bilstm_crf_best.pt";
pub const DEFAULT_MAX_SEQ_LEN: usize = 200;
pub const DEFAULT_BATCH_TOKENS: usize = 12000;
/// 结果缓存容量，单位为**块**（0 表示关闭）。
pub const DEFAULT_CACHE_SIZE: usize = 2048;

/// 用于长文本切块的句末标点（与训练时语料切分口径一致）。
const SENT_END_CHARS: &str = "。！？；!?;…\n\r";
const ENV_MODEL_PATH: &str = "OMNICRAWL_NER_MODEL";

/// 中文姓名内部连接符：随中文片段一起进入模型，也允许出现在实体区间内。
const CHINESE_CONNECTORS: [char; 2] = ['·', '・'];
/// 隔离用的分隔符：与被替换字符等长，保证模型返回的偏移与原文一一对应。
const CHINESE_ISOLATION_SEPARATOR: char = ' ';

/// 一个实体区间（字符偏移的半开区间）。
pub type Entity = (usize, usize, String);

/// 模型前向的注入点：权重格式与设备由实现方决定。
pub trait NerBackend: Send + Sync {
    /// 标签表，顺序即标签 id。
    fn tags(&self) -> &[String];
    /// 字符 → 词表 id。
    fn char_id(&self, ch: char) -> u32;
    /// 未登录字符的 id。
    fn unk_id(&self) -> u32;
    /// 逐行预测标签 id（长度与输入一致，padding 由实现方处理）。
    fn predict(&self, batch: &[Vec<u32>]) -> Vec<Vec<usize>>;
}

/// 是否属于兜底层保留的字符：汉字与中文姓名连接符。
pub fn is_chinese_char(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch) || CHINESE_CONNECTORS.contains(&ch)
}

/// 是否含中日韩汉字（模型对纯拉丁片段的误报多来自邮箱 / 网址 / 编号）。
pub fn has_cjk(text: &str) -> bool {
    text.chars()
        .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
}

/// 中文片段隔离：非中文字符等长替换为分隔符，只让中文片段进入模型。
pub fn isolate_chinese(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if is_chinese_char(ch) {
                ch
            } else {
                CHINESE_ISOLATION_SEPARATOR
            }
        })
        .collect()
}

/// 实体区间是否整体落在中文片段内（至少含一个汉字，其余只能是连接符）。
pub fn is_chinese_span(value: &str) -> bool {
    if value.is_empty() || !has_cjk(value) {
        return false;
    }
    value.chars().all(is_chinese_char)
}

/// 解析 checkpoint 路径：显式配置 > 环境变量 > 随包默认（相对 `default_dir`）。
pub fn resolve_model_path(
    configured: Option<&str>,
    env_value: Option<&str>,
    default_dir: &Path,
) -> PathBuf {
    if let Some(configured) = configured {
        if !configured.is_empty() {
            return PathBuf::from(configured);
        }
    }
    if let Some(env_value) = env_value {
        let trimmed = env_value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    default_dir.join(PACKAGED_MODEL_FILENAME)
}

/// 读取 `OMNICRAWL_NER_MODEL` 的名字（供调用方从环境注入）。
pub fn model_path_env_name() -> &'static str {
    ENV_MODEL_PATH
}

/// 设备选择：内核只有 CPU 实现，`auto` / `cuda` 一律回到 CPU。
pub fn resolve_device(requested: Option<&str>) -> &'static str {
    let normalized = requested.unwrap_or(DEVICE_AUTO).trim().to_lowercase();
    if normalized == DEVICE_CPU {
        return DEVICE_CPU;
    }
    DEVICE_CPU
}

/// 先按句末标点切句；超长句再按 `max_len` 硬切（与训练时切分口径一致）。
pub fn split_units(text: &str, max_len: usize) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    let mut units: Vec<String> = Vec::new();
    let mut start = 0usize;
    for (index, ch) in characters.iter().enumerate() {
        if SENT_END_CHARS.contains(*ch) {
            units.push(characters[start..=index].iter().collect());
            start = index + 1;
        }
    }
    if start < characters.len() {
        units.push(characters[start..].iter().collect());
    }

    let mut out: Vec<String> = Vec::new();
    for unit in units {
        let length = unit.chars().count();
        if length == 0 {
            continue;
        }
        if length <= max_len {
            out.push(unit);
        } else {
            let chars: Vec<char> = unit.chars().collect();
            let mut offset = 0;
            while offset < chars.len() {
                let end = (offset + max_len).min(chars.len());
                out.push(chars[offset..end].iter().collect());
                offset = end;
            }
        }
    }
    out
}

/// 把句子单元贪心打包成不超过 `max_len` 的块，保持原文顺序。
pub fn iter_chunks(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut buffer = String::new();
    for unit in split_units(text, max_len) {
        let buffer_len = buffer.chars().count();
        let unit_len = unit.chars().count();
        if buffer_len > 0 && buffer_len + unit_len > max_len {
            chunks.push(std::mem::take(&mut buffer));
        }
        buffer.push_str(&unit);
    }
    if !buffer.is_empty() {
        chunks.push(buffer);
    }
    chunks
}

/// BIO 标签序列 → 实体区间（半开区间，按出现位置排序）。
///
/// `characters_len` 是原文本的字符数：末尾未闭合的实体以它为终点（与 Python 同口径）。
pub fn extract_entities(tags: &[String], characters_len: usize) -> Vec<Entity> {
    let mut entities: Vec<Entity> = Vec::new();
    let mut start: isize = -1;
    let mut current = String::new();
    for (index, tag) in tags.iter().enumerate() {
        if let Some(entity_type) = tag.strip_prefix("B-") {
            if !current.is_empty() {
                entities.push((start as usize, index, current.clone()));
            }
            start = index as isize;
            current = entity_type.to_string();
        } else if let Some(entity_type) = tag.strip_prefix("I-") {
            if current != entity_type {
                // 非法 I-x：当作新的实体起点（模型受约束后基本不会出现）。
                if !current.is_empty() {
                    entities.push((start as usize, index, current.clone()));
                }
                start = index as isize;
                current = entity_type.to_string();
            }
        } else {
            if !current.is_empty() {
                entities.push((start as usize, index, current.clone()));
            }
            start = -1;
            current = String::new();
        }
    }
    if !current.is_empty() {
        entities.push((start as usize, characters_len, current));
    }
    entities
}

/// 按 token 预算动态分批（减少 padding 浪费），不改动结果顺序。
pub fn iter_batches(lengths: &[usize], batch_tokens: usize) -> Vec<Vec<usize>> {
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut batch: Vec<usize> = Vec::new();
    let mut batch_max = 0usize;
    for (index, length) in lengths.iter().enumerate() {
        let mut new_max = batch_max.max(*length);
        if !batch.is_empty() && new_max * (batch.len() + 1) > batch_tokens {
            batches.push(std::mem::take(&mut batch));
            new_max = *length;
        }
        batch.push(index);
        batch_max = new_max;
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

/// 抽取器统计（与 Python 的 `cache_stats()` 同名同口径）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NerCacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub inferred_chunks: u64,
    pub ascii_skips: u64,
}

#[derive(Default)]
struct CacheState {
    keys: Vec<String>,
    values: HashMap<String, Vec<Entity>>,
}

/// 先入先出的块级结果缓存（命中即刷新到末尾）。
struct EntityCache {
    capacity: usize,
    state: Mutex<CacheState>,
}

impl EntityCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(CacheState::default()),
        }
    }

    fn get(&self, key: &str) -> Option<Vec<Entity>> {
        if self.capacity == 0 {
            return None;
        }
        let mut state = lock(&self.state);
        let value = state.values.get(key).cloned()?;
        state.keys.retain(|item| item != key);
        state.keys.push(key.to_string());
        Some(value)
    }

    fn put(&self, key: &str, value: Vec<Entity>) {
        if self.capacity == 0 {
            return;
        }
        let mut state = lock(&self.state);
        state.values.insert(key.to_string(), value);
        state.keys.retain(|item| item != key);
        state.keys.push(key.to_string());
        while state.keys.len() > self.capacity {
            let evicted = state.keys.remove(0);
            state.values.remove(&evicted);
        }
    }

    fn len(&self) -> usize {
        lock(&self.state).keys.len()
    }
}

/// 加载了权重的抽取器：分块 → 批量前向 → 实体区间。
pub struct NerExtractor {
    backend: Arc<dyn NerBackend>,
    max_seq_len: usize,
    batch_tokens: usize,
    cache: EntityCache,
    infer_lock: Mutex<()>,
    hits: Mutex<u64>,
    misses: Mutex<u64>,
    ascii_skips: Mutex<u64>,
    inferred_chunks: Mutex<u64>,
}

impl NerExtractor {
    pub fn new(
        backend: Arc<dyn NerBackend>,
        max_seq_len: usize,
        batch_tokens: usize,
        cache_size: usize,
    ) -> Self {
        Self {
            backend,
            max_seq_len: max_seq_len.max(1),
            batch_tokens: batch_tokens.max(1),
            cache: EntityCache::new(cache_size),
            infer_lock: Mutex::new(()),
            hits: Mutex::new(0),
            misses: Mutex::new(0),
            ascii_skips: Mutex::new(0),
            inferred_chunks: Mutex::new(0),
        }
    }

    /// 返回实体区间（字符偏移，按出现位置排序）。
    pub fn find_entities(&self, text: &str) -> Vec<Entity> {
        if text.is_empty() {
            return Vec::new();
        }
        if !has_cjk(text) {
            // 纯 ASCII 文本不可能产出本层需要的实体，直接短路。
            *lock(&self.ascii_skips) += 1;
            return Vec::new();
        }

        let mut entities: Vec<Entity> = Vec::new();
        let mut missing: Vec<(usize, String)> = Vec::new();
        let mut offset = 0usize;
        for chunk in iter_chunks(text, self.max_seq_len) {
            let chunk_len = chunk.chars().count();
            match self.cache_get(&chunk) {
                Some(cached) => {
                    entities.extend(cached.into_iter().map(|(start, end, entity_type)| {
                        (offset + start, offset + end, entity_type)
                    }));
                }
                None => {
                    if has_cjk(&chunk) {
                        missing.push((offset, chunk.clone()));
                    }
                    // 纯 ASCII 块同样不可能有实体：跳过且不进缓存。
                }
            }
            offset += chunk_len;
        }

        if !missing.is_empty() {
            let inferred = self.infer_chunks(
                &missing
                    .iter()
                    .map(|(_, chunk)| chunk.clone())
                    .collect::<Vec<String>>(),
            );
            for ((chunk_offset, chunk), chunk_entities) in missing.iter().zip(inferred) {
                self.cache_put(chunk, chunk_entities.clone());
                entities.extend(chunk_entities.into_iter().map(|(start, end, entity_type)| {
                    (chunk_offset + start, chunk_offset + end, entity_type)
                }));
            }
        }
        entities.sort_by_key(|item| item.0);
        entities
    }

    fn cache_get(&self, key: &str) -> Option<Vec<Entity>> {
        let value = self.cache.get(key);
        let counter = if value.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        if self.cache.capacity > 0 {
            *lock(counter) += 1;
        }
        value
    }

    fn cache_put(&self, key: &str, value: Vec<Entity>) {
        self.cache.put(key, value);
    }

    /// 对多个块做批量前向，返回每块的实体（偏移相对块首，顺序与输入一致）。
    fn infer_chunks(&self, chunks: &[String]) -> Vec<Vec<Entity>> {
        if chunks.is_empty() {
            return Vec::new();
        }
        *lock(&self.inferred_chunks) += chunks.len() as u64;
        let encoded: Vec<Vec<u32>> = chunks
            .iter()
            .map(|chunk| {
                isolate_chinese(chunk)
                    .chars()
                    .map(|ch| self.backend.char_id(ch))
                    .collect()
            })
            .collect();

        let mut results: Vec<Vec<Entity>> = vec![Vec::new(); chunks.len()];
        let tags = self.backend.tags().to_vec();
        let _guard = lock(&self.infer_lock);
        let lengths: Vec<usize> = encoded.iter().map(|ids| ids.len()).collect();
        for batch_indices in iter_batches(&lengths, self.batch_tokens) {
            let batch: Vec<Vec<u32>> = batch_indices
                .iter()
                .map(|index| encoded[*index].clone())
                .collect();
            let paths = self.backend.predict(&batch);
            for (local, job_index) in batch_indices.iter().enumerate() {
                let row = paths.get(local).cloned().unwrap_or_default();
                let row_tags: Vec<String> = row
                    .iter()
                    .map(|tag| tags.get(*tag).cloned().unwrap_or_else(|| "O".to_string()))
                    .collect();
                results[*job_index] =
                    extract_entities(&row_tags, chunks[*job_index].chars().count());
            }
        }
        results
    }

    pub fn cache_stats(&self) -> NerCacheStats {
        NerCacheStats {
            entries: self.cache.len(),
            hits: *lock(&self.hits),
            misses: *lock(&self.misses),
            inferred_chunks: *lock(&self.inferred_chunks),
            ascii_skips: *lock(&self.ascii_skips),
        }
    }
}

/// 在抽取结果之上做「过滤 + 占位符保护」的兜底层。
pub struct NerLayer {
    extractor: Arc<NerExtractor>,
    types: Vec<String>,
    min_entity_chars: usize,
}

impl NerLayer {
    pub fn new(
        extractor: Arc<NerExtractor>,
        entity_types: &[String],
        min_entity_chars: i64,
    ) -> Self {
        let selected: Vec<String> = entity_types
            .iter()
            .map(|item| item.trim().to_uppercase())
            .filter(|item| !item.is_empty())
            .collect();
        let types = if selected.is_empty() {
            NER_ENTITY_TYPES
                .iter()
                .map(|item| item.to_string())
                .collect()
        } else {
            selected
        };
        Self {
            extractor,
            types,
            min_entity_chars: min_entity_chars.max(1) as usize,
        }
    }

    /// 底层抽取结果（未过滤），供观测与对照使用。
    pub fn extractor_entities(&self, text: &str) -> Vec<Entity> {
        self.extractor.find_entities(text)
    }

    /// 抽取器的缓存计数。
    pub fn stats(&self) -> NerCacheStats {
        self.extractor.cache_stats()
    }

    /// 返回需要占位的实体区间（已过滤、已剔除占位符重叠）。
    pub fn find_spans(&self, text: &str) -> Vec<(usize, usize)> {
        if text.is_empty() {
            return Vec::new();
        }
        let entities = self.extractor.find_entities(text);
        if entities.is_empty() {
            return Vec::new();
        }
        let characters: Vec<char> = text.chars().collect();
        let total = characters.len();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut placeholders: Option<Vec<(usize, usize)>> = None;
        let mut pointer = 0usize;
        for (start, end, entity_type) in entities {
            if !self.types.contains(&entity_type) {
                continue;
            }
            if end <= start || end > total {
                continue;
            }
            let value: String = characters[start..end].iter().collect();
            if value.chars().count() < self.min_entity_chars || !is_chinese_span(&value) {
                continue;
            }
            if placeholders.is_none() {
                placeholders = Some(
                    find_placeholders(text)
                        .into_iter()
                        .map(|(start, end, _)| (start, end))
                        .collect(),
                );
            }
            let spans_of_placeholders = placeholders.as_ref().expect("已初始化占位符区间");
            while pointer < spans_of_placeholders.len() && spans_of_placeholders[pointer].1 <= start
            {
                pointer += 1;
            }
            if pointer < spans_of_placeholders.len() {
                let (holder_start, holder_end) = spans_of_placeholders[pointer];
                if holder_start < end && start < holder_end {
                    continue;
                }
            }
            spans.push((start, end));
        }
        spans
    }
}

/// 脱敏配置里 NER 相关字段的视图（配置层与 llm 层不互相依赖）。
#[derive(Debug, Clone)]
pub struct NerLayerOptions {
    pub enabled: bool,
    pub model_path: String,
    pub device: String,
    pub entity_types: Vec<String>,
    pub min_entity_chars: i64,
    pub cache_size: i64,
}

impl Default for NerLayerOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: String::new(),
            device: DEVICE_AUTO.to_string(),
            entity_types: NER_ENTITY_TYPES
                .iter()
                .map(|item| item.to_string())
                .collect(),
            min_entity_chars: 2,
            cache_size: DEFAULT_CACHE_SIZE as i64,
        }
    }
}

/// 共享抽取器池的键：路径 + 设备 + 缓存容量。
pub type NerPoolKey = (String, String, usize);

/// 按 (路径, 设备, 缓存容量) 复用抽取器：模型只在首次启用时加载一次。
#[derive(Default)]
pub struct NerExtractorPool {
    entries: Mutex<HashMap<NerPoolKey, Arc<NerExtractor>>>,
}

impl NerExtractorPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&self) {
        lock(&self.entries).clear();
    }

    pub fn get_or_build<F>(&self, key: NerPoolKey, build: F) -> Option<Arc<NerExtractor>>
    where
        F: FnOnce() -> Option<Arc<NerExtractor>>,
    {
        if let Some(existing) = lock(&self.entries).get(&key) {
            return Some(existing.clone());
        }
        let extractor = build()?;
        lock(&self.entries).insert(key, extractor.clone());
        Some(extractor)
    }
}

/// 按配置构建 NER 兜底层；未启用或后端不可用时返回 `None`（静默降级）。
pub fn build_ner_layer<F>(
    options: &NerLayerOptions,
    pool: &NerExtractorPool,
    model_dir: &Path,
    env_model_path: Option<&str>,
    load_backend: F,
) -> Option<NerLayer>
where
    F: FnOnce(&Path) -> Option<Arc<dyn NerBackend>>,
{
    if !options.enabled {
        return None;
    }
    let path = resolve_model_path(Some(options.model_path.as_str()), env_model_path, model_dir);
    let device = resolve_device(Some(options.device.as_str())).to_string();
    let cache_size = if options.cache_size < 0 {
        0
    } else {
        options.cache_size as usize
    };
    let key = (path.to_string_lossy().to_string(), device, cache_size);
    let extractor = pool.get_or_build(key, || {
        let backend = load_backend(&path)?;
        Some(Arc::new(NerExtractor::new(
            backend,
            DEFAULT_MAX_SEQ_LEN,
            DEFAULT_BATCH_TOKENS,
            cache_size,
        )))
    })?;
    Some(NerLayer::new(
        extractor,
        &options.entity_types,
        options.min_entity_chars,
    ))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 把实体区间映射成占位符替换（供 `engine` 在 `mask_text` 末尾调用）。
pub fn mask_spans<F>(text: &str, spans: &[(usize, usize)], mut placeholder_for: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    if spans.is_empty() {
        return text.to_string();
    }
    let characters: Vec<char> = text.chars().collect();
    let mut ordered: Vec<(usize, usize)> = spans.to_vec();
    ordered.sort_by_key(|item| item.0);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end) in ordered {
        if start < cursor || end > characters.len() || end <= start {
            continue;
        }
        out.extend(characters[cursor..start].iter());
        let value: String = characters[start..end].iter().collect();
        match placeholder_for(&value) {
            Some(placeholder) => out.push_str(&placeholder),
            None => out.push_str(&value),
        }
        cursor = end;
    }
    out.extend(characters[cursor..].iter());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_isolation_keeps_length() {
        let text = "张三 abc 李四";
        let isolated = isolate_chinese(text);
        assert_eq!(isolated.chars().count(), text.chars().count());
        assert_eq!(isolated, "张三     李四");
    }

    #[test]
    fn chunks_respect_sentence_boundaries() {
        let chunks = iter_chunks("第一句。第二句！超长句", 5);
        assert_eq!(chunks, vec!["第一句。", "第二句！", "超长句"]);
    }
}
