//! 屏蔽计划缓存（对映 `omnicrawl/llm/desensitization/plan_cache.py`）。
//!
//! 历史每轮全量重发，同一段未变文本会被反复处理：结构层（`.env` / `key: value` /
//! JSON 片段）、值类型规则层、熵兜底、NER。规则扫描结果已有缓存，但每条消息仍要重建
//! 上下文并逐层重跑。本模块把一次屏蔽的**匹配计划**按文本存起来：
//!
//! - 计划只记录「各阶段待替换区间的起止 + 稳定序号 + 阶段标签」，**不含原文**；
//! - 命中时按区间从**当前文本**取值，再走标准 `placeholder_for` 重新登记到当前周期，
//!   因此还原 / 注销 / 并发周期隔离的语义与未命中路径完全一致；
//! - 命中后序号必须与计划一致（序号由进程级稳定索引按值指纹决定）：不一致说明规则或
//!   稳定索引变了，按未命中重算——宁慢勿错。
//!
//! 缓存按运行时实例持有（匹配器、规则集合、NER 层都是实例级），随运行时关闭清空；
//! 条目数与字节预算双上限，只保存区间与序号，不保存文本。

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// 默认容量：条目数 + 字节预算（与规则扫描缓存同量级，单条计划只是若干小整数）。
pub const DEFAULT_MAX_ENTRIES: usize = 1024;
pub const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// 阶段标签 → 计数口径（只在「本周期首次登记」时递增，与未命中路径一致）。
pub const STAGE_COUNTER_FIELDS: [(&str, &str); 3] = [
    ("rules", "rules_masked"),
    ("entropy", "entropy_masked"),
    ("ner", "ner_masked"),
];

/// 阶段标签对应的计数口径字段名；无标签或未知标签返回 `None`。
pub fn stage_counter_field(counter: &str) -> Option<&'static str> {
    STAGE_COUNTER_FIELDS
        .iter()
        .find(|(label, _)| *label == counter)
        .map(|(_, field)| *field)
}

/// 一个待替换区间：`(start, end)` 是所在阶段输入文本的坐标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSpan {
    pub start: usize,
    pub end: usize,
    pub seq: u64,
    pub counter: String,
}

/// 一次屏蔽的匹配计划：阶段按执行顺序排列，阶段内的区间都按**该阶段输入文本**的坐标记录。
///
/// 阶段内区间互不重叠（各层记录前已去重），但**记录顺序并不统一**：结构层正序记录；
/// 规则层 / 熵兜底层 / NER 层是逆序应用替换（靠后的区间先换），记录顺序也就是逆序。
/// 因此重放不能照搬记录顺序，必须显式按起点降序应用（见 `MaskContext::replay_plan`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaskPlan {
    pub stages: Vec<Vec<PlanSpan>>,
}

impl MaskPlan {
    /// 区间总数（观测与容量估算用）。
    pub fn span_count(&self) -> usize {
        self.stages.iter().map(Vec::len).sum()
    }
}

/// 按阶段记录待替换区间，供 [`super::engine::MaskContext`] 在屏蔽过程中调用。
///
/// 记录数与实际占位符分配数不一致（例如转义值与文本切片不一致、计划漏记）时
/// [`MaskPlanBuilder::build`] 返回 `None`，该文本不进缓存——漏缓存只损失速度，
/// 错缓存会改语义。
#[derive(Debug, Default)]
pub struct MaskPlanBuilder {
    stages: Vec<Vec<PlanSpan>>,
    current: Option<usize>,
    counter: String,
    records: u64,
    registrations: u64,
}

impl MaskPlanBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开启一个新阶段（对应一次赋值替换 / 一层规则）；阶段内坐标同源。
    pub fn begin_stage(&mut self, counter: &str) {
        self.stages.push(Vec::new());
        self.current = Some(self.stages.len() - 1);
        self.counter = counter.to_string();
    }

    /// 记录一个待替换区间；未显式开始阶段时按无标签阶段处理。
    pub fn record(&mut self, start: usize, end: usize, seq: u64) {
        if self.current.is_none() {
            self.begin_stage("");
        }
        if let Some(index) = self.current {
            self.stages[index].push(PlanSpan {
                start,
                end,
                seq,
                counter: self.counter.clone(),
            });
        }
        self.records += 1;
    }

    /// 记账一次占位符分配（含无法记录区间的调用，用于判定计划是否完整）。
    pub fn note_registration(&mut self) {
        self.registrations += 1;
    }

    /// 产出计划；记录与实际分配不一致时返回 `None`（不缓存该文本）。
    ///
    /// 空阶段（开启了阶段但一个区间都没记）在产出时丢掉，与 Python 侧
    /// `tuple(tuple(stage) for stage in self._stages if stage)` 同义。
    pub fn build(self) -> Option<MaskPlan> {
        if self.records != self.registrations {
            return None;
        }
        Some(MaskPlan {
            stages: self
                .stages
                .into_iter()
                .filter(|stage| !stage.is_empty())
                .collect(),
        })
    }
}

/// 文本指纹（SHA-256）：缓存键只用指纹，不保存文本本体。
pub fn text_key(text: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(text.as_bytes());
    let out = digest.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

/// 占用估算：指纹键 + 区间对象与阶段列表的固定开销。
fn plan_bytes(plan: &MaskPlan) -> usize {
    64 + plan.span_count() * 128 + plan.stages.len() * 64
}

/// 一次缓存观测（不含任何原文或计划内容）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanCacheStats {
    pub entries: usize,
    pub size_bytes: usize,
    pub lookups: u64,
    /// 命中数：命中但重放失败时按未命中计数（见 [`MaskPlanCache::note_invalid`]）。
    pub hits: i64,
    pub misses: i64,
    pub invalid: u64,
    pub evictions: u64,
}

#[derive(Default)]
struct CacheState {
    /// 最近使用的排在末尾；淘汰从队首取（与 Python 的 `OrderedDict` 同口径）。
    order: Vec<[u8; 32]>,
    entries: HashMap<[u8; 32], MaskPlan>,
    size_bytes: usize,
    lookups: u64,
    hits: i64,
    misses: i64,
    invalid: u64,
    evictions: u64,
}

/// 按文本指纹缓存屏蔽计划的有界 LRU（条目数 + 字节预算双上限）。
pub struct MaskPlanCache {
    max_entries: usize,
    max_bytes: usize,
    state: Mutex<CacheState>,
}

impl Default for MaskPlanCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, DEFAULT_MAX_BYTES)
    }
}

impl MaskPlanCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            max_entries,
            max_bytes,
            state: Mutex::new(CacheState::default()),
        }
    }

    /// 任一上限为 0 时整体停用，等价于未接线。
    pub fn enabled(&self) -> bool {
        self.max_entries > 0 && self.max_bytes > 0
    }

    /// 取计划；未命中返回 `None`（调用方走完整屏蔽）。
    pub fn get(&self, text: &str) -> Option<MaskPlan> {
        if !self.enabled() || text.is_empty() {
            return None;
        }
        let key = text_key(text);
        let mut state = lock(&self.state);
        state.lookups += 1;
        let Some(plan) = state.entries.get(&key).cloned() else {
            state.misses += 1;
            return None;
        };
        state.order.retain(|item| item != &key);
        state.order.push(key);
        state.hits += 1;
        Some(plan)
    }

    /// 存计划；超预算的条目直接放弃（不淘汰既有条目）。
    pub fn put(&self, text: &str, plan: MaskPlan) {
        if !self.enabled() || text.is_empty() {
            return;
        }
        let cost = plan_bytes(&plan);
        if cost > self.max_bytes {
            return;
        }
        let key = text_key(text);
        let mut state = lock(&self.state);
        if let Some(previous) = state.entries.remove(&key) {
            state.size_bytes -= plan_bytes(&previous);
            state.order.retain(|item| item != &key);
        }
        state.order.push(key);
        state.entries.insert(key, plan);
        state.size_bytes += cost;
        while !state.order.is_empty()
            && (state.size_bytes > self.max_bytes || state.entries.len() > self.max_entries)
        {
            let evicted_key = state.order.remove(0);
            if let Some(evicted) = state.entries.remove(&evicted_key) {
                state.size_bytes -= plan_bytes(&evicted);
                state.evictions += 1;
            }
        }
    }

    /// 命中但重放失败（序号 / 值与计划不符）：按未命中计数，便于观测规则漂移。
    pub fn note_invalid(&self) {
        let mut state = lock(&self.state);
        state.invalid += 1;
        state.hits -= 1;
        state.misses += 1;
    }

    /// 清空条目（运行时关闭时调用）；计数保留，便于收尾观测。
    pub fn clear(&self) {
        let mut state = lock(&self.state);
        state.order.clear();
        state.entries.clear();
        state.size_bytes = 0;
    }

    /// 返回计数与占用（不含任何原文或计划内容）。
    pub fn stats(&self) -> PlanCacheStats {
        let state = lock(&self.state);
        PlanCacheStats {
            entries: state.entries.len(),
            size_bytes: state.size_bytes,
            lookups: state.lookups,
            hits: state.hits,
            misses: state.misses,
            invalid: state.invalid,
            evictions: state.evictions,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个单阶段、单区间的计划（占用固定 256 字节）。
    fn single_span_plan(seq: u64) -> MaskPlan {
        let mut builder = MaskPlanBuilder::new();
        builder.begin_stage("rules");
        builder.record(0, 1, seq);
        builder.note_registration();
        builder.build().expect("计划完整")
    }

    /// 造一个两个阶段、共两个区间的计划（占用 448 字节）。
    fn two_stage_plan() -> MaskPlan {
        let mut builder = MaskPlanBuilder::new();
        builder.begin_stage("rules");
        builder.record(0, 1, 1);
        builder.note_registration();
        builder.begin_stage("entropy");
        builder.record(2, 3, 2);
        builder.note_registration();
        builder.build().expect("计划完整")
    }

    #[test]
    fn builder_rejects_incomplete_plans() {
        let mut builder = MaskPlanBuilder::new();
        builder.begin_stage("rules");
        builder.record(0, 3, 1);
        builder.note_registration();
        builder.begin_stage("entropy");
        // 第二个阶段只记了分配、没记区间：计划不可重放。
        builder.note_registration();
        assert!(builder.build().is_none(), "记录数与分配数不一致时不缓存");

        let exact = MaskPlanBuilder::new();
        let plan = exact.build().expect("一致即产出");
        assert_eq!(plan.stages.len(), 0, "空计划（零区间）照样产出");
        assert_eq!(plan.span_count(), 0);

        // 开了阶段但一个区间都没记：该阶段在产出时丢掉。
        let mut with_empty_stage = MaskPlanBuilder::new();
        with_empty_stage.begin_stage("");
        with_empty_stage.begin_stage("rules");
        with_empty_stage.record(0, 3, 1);
        with_empty_stage.note_registration();
        let plan = with_empty_stage.build().expect("计划完整");
        assert_eq!(plan.stages.len(), 1);
        assert_eq!(plan.stages[0][0].counter, "rules");
    }

    #[test]
    fn builder_without_stage_records_under_empty_label() {
        let mut builder = MaskPlanBuilder::new();
        builder.record(2, 5, 9);
        builder.note_registration();
        let plan = builder.build().expect("计划完整");
        assert_eq!(plan.stages.len(), 1);
        assert_eq!(plan.stages[0][0].start, 2);
        assert_eq!(plan.stages[0][0].end, 5);
        assert_eq!(plan.stages[0][0].seq, 9);
        assert_eq!(plan.stages[0][0].counter, "");
    }

    #[test]
    fn text_key_is_sha256_of_utf8() {
        // sha256("abc")
        assert_eq!(
            text_key("abc").to_vec(),
            hex_bytes("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        // 中文按 UTF-8 字节计，与 Python 的 encode("utf-8") 同源。
        assert_eq!(
            text_key("密钥").to_vec(),
            hex_bytes("f67bca8f42bcf07373a9ed1eb3dcd45d527a18e3c2d1803d2b76d74234cb1403")
        );
        assert_ne!(text_key("abc"), text_key("abd"));
    }

    fn hex_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len() / 2)
            .map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("十六进制"))
            .collect()
    }

    #[test]
    fn cache_hits_and_reports_stats() {
        let cache = MaskPlanCache::new(8, 1_000_000);
        let plan = single_span_plan(1);
        assert!(cache.get("abc").is_none());
        cache.put("abc", plan.clone());
        assert_eq!(cache.get("abc"), Some(plan));
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.size_bytes, 256);
        assert_eq!(stats.lookups, 2);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.invalid, 0);
        assert_eq!(stats.evictions, 0);
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let cache = MaskPlanCache::new(2, 1_000_000);
        for text in ["a", "b", "c"] {
            cache.put(text, single_span_plan(1));
        }
        assert!(cache.get("a").is_none(), "最久未用的先淘汰");
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        assert_eq!(cache.stats().evictions, 1);
    }

    #[test]
    fn cache_rejects_oversized_plan_without_evicting() {
        let cache = MaskPlanCache::new(8, 300);
        cache.put("keep", single_span_plan(1));
        cache.put("huge", two_stage_plan());
        let stats = cache.stats();
        assert_eq!(stats.entries, 1, "超预算的条目直接放弃");
        assert_eq!(stats.evictions, 0, "不淘汰既有条目");
        assert!(cache.get("keep").is_some());
    }

    #[test]
    fn cache_is_disabled_with_zero_budget() {
        let cache = MaskPlanCache::new(0, 1024);
        assert!(!cache.enabled());
        cache.put("abc", single_span_plan(1));
        assert!(cache.get("abc").is_none());
        assert_eq!(cache.stats().lookups, 0, "停用时不计 lookup");

        let bytes = MaskPlanCache::new(1024, 0);
        bytes.put("abc", single_span_plan(1));
        assert!(bytes.get("abc").is_none());
    }

    #[test]
    fn note_invalid_moves_hit_to_miss() {
        let cache = MaskPlanCache::new(8, 1_000_000);
        cache.put("abc", single_span_plan(1));
        assert!(cache.get("abc").is_some());
        cache.note_invalid();
        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.invalid, 1);
        assert_eq!(stats.entries, 1, "失效条目仍留在缓存里");
    }

    #[test]
    fn clear_drops_entries_but_keeps_counters() {
        let cache = MaskPlanCache::new(8, 1_000_000);
        cache.put("abc", single_span_plan(1));
        cache.get("abc");
        cache.clear();
        let stats = cache.stats();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.size_bytes, 0);
        assert_eq!(stats.hits, 1);
    }

    #[test]
    fn stage_counter_fields_match_python_table() {
        assert_eq!(stage_counter_field("rules"), Some("rules_masked"));
        assert_eq!(stage_counter_field("entropy"), Some("entropy_masked"));
        assert_eq!(stage_counter_field("ner"), Some("ner_masked"));
        assert_eq!(stage_counter_field(""), None);
        assert_eq!(stage_counter_field("struct"), None);
    }
}
