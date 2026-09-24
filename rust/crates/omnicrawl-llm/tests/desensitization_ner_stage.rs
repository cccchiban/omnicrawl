//! NER 兜底层接线的验收：`mask_text` 的最后一层、计划缓存的 `ner` 阶段计数、
//! 以及运行期按配置装载/静默降级。
//!
//! 前向本身（BiLSTM-CRF 逐位结果）由 `ner_parity.rs` 对着 Python/torch 验；这里关心的是
//! **接线**：区间怎么进占位符流程、怎么计数、怎么随计划缓存重放、配置关闭时是否真的不生效。
//! 因此用一个「按字符 id 出标签」的桩后端，不必加载 5 MB 权重。

use std::sync::Arc;

use omnicrawl_llm::desensitization::engine::{mask_text, MaskContext, SensitiveMatcher};
use omnicrawl_llm::desensitization::ner::{
    NerBackend, NerExtractor, NerLayer, DEFAULT_BATCH_TOKENS, DEFAULT_CACHE_SIZE,
    DEFAULT_MAX_SEQ_LEN,
};
use omnicrawl_llm::desensitization::oneshot::{OneShotMasker, OneshotOptions};
use omnicrawl_llm::desensitization::plan_cache::MaskPlanCache;
use omnicrawl_llm::desensitization::{find_placeholders, DesensitizationStats, SequenceRegistry};

/// 桩后端：标签表 O / B-PER / I-PER；把「张」「三」两个字符标成一个 PER 实体。
struct StubBackend;

const TAG_O: usize = 0;
const TAG_B_PER: usize = 1;
const TAG_I_PER: usize = 2;

impl NerBackend for StubBackend {
    fn tags(&self) -> &[String] {
        // `tags()` 返回借用的标签表，因此用一次性的静态表。
        static TAGS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        TAGS.get_or_init(|| {
            ["O", "B-PER", "I-PER"]
                .iter()
                .map(|item| item.to_string())
                .collect()
        })
    }

    /// 词表：0 = padding/未知，1 = 其他字符，2 = 「张」，3 = 「三」。
    fn char_id(&self, ch: char) -> u32 {
        match ch {
            '张' => 2,
            '三' => 3,
            _ => 1,
        }
    }

    fn unk_id(&self) -> u32 {
        0
    }

    /// 每个位置照字符 id 出标签：实体首位给 B-PER，其余连续位给 I-PER。
    fn predict(&self, batch: &[Vec<u32>]) -> Vec<Vec<usize>> {
        batch
            .iter()
            .map(|row| {
                let mut tags = vec![TAG_O; row.len()];
                let mut in_entity = false;
                for (index, id) in row.iter().enumerate() {
                    let is_entity_char = matches!(id, 2 | 3);
                    if is_entity_char {
                        tags[index] = if in_entity { TAG_I_PER } else { TAG_B_PER };
                    }
                    in_entity = is_entity_char;
                }
                tags
            })
            .collect()
    }
}

fn layer(entity_types: &[&str], min_entity_chars: i64) -> NerLayer {
    let extractor = Arc::new(NerExtractor::new(
        Arc::new(StubBackend),
        DEFAULT_MAX_SEQ_LEN,
        DEFAULT_BATCH_TOKENS,
        DEFAULT_CACHE_SIZE,
    ));
    let types: Vec<String> = entity_types.iter().map(|item| item.to_string()).collect();
    NerLayer::new(extractor, &types, min_entity_chars)
}

/// 一套「上下文 + 计数 + 周期」，供各用例复用。
struct Harness {
    matcher: SensitiveMatcher,
    cycle_registry: SequenceRegistry,
    stats: DesensitizationStats,
}

impl Harness {
    /// 注入自增计数器：`SequenceRegistry::new()` 用的是**进程级**共享稳定索引，序号会
    /// 跟着同一测试二进制里其它用例的脱敏串号，断言就没法确定。
    fn new() -> Self {
        let keys: Vec<String> = Vec::new();
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let source = Arc::clone(&counter);
        Self {
            matcher: SensitiveMatcher::new(&keys, &keys),
            cycle_registry: SequenceRegistry::with_sequence_source(move || {
                source.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
            }),
            stats: DesensitizationStats::default(),
        }
    }

    fn run(
        &mut self,
        layer: Option<&NerLayer>,
        cache: Option<Arc<MaskPlanCache>>,
        text: &str,
    ) -> String {
        let (mut cycle, _) = self.cycle_registry.begin_cycle(text);
        let mut context = MaskContext {
            matcher: &self.matcher,
            cycle: &mut cycle,
            stats: &mut self.stats,
            entropy_enabled: false,
            entropy_min_length: 0,
            entropy_min_bits: 0.0,
            entropy_pure_letters: false,
            entropy_pure_digits: false,
            pattern_rules: &[],
            gitleaks_rules: &[],
            ner: layer,
            plan_cache: cache,
            plan_builder: None,
        };
        mask_text(text, &mut context)
    }
}

#[test]
fn ner_layer_is_the_last_stage_and_uses_the_placeholder_cycle() {
    let layer = layer(&["PER"], 2);
    let mut harness = Harness::new();
    let text = "张三在测试";

    let masked = harness.run(Some(&layer), None, text);

    assert_ne!(masked, text, "实体应当被替换成占位符");
    assert!(!masked.contains("张三"), "实体本体不应留在文本里：{masked}");
    assert!(masked.contains("在测试"), "非实体部分原样保留：{masked}");
    assert_eq!(harness.stats.ner_masked, 1);
    assert_eq!(harness.stats.values_masked, 1);
    assert_eq!(harness.stats.rules_masked, 0, "这条路径不该动规则层计数");

    // 占位符形状与其余层一致（同一个 `placeholder_for` 分配）。
    let placeholders = find_placeholders(&masked);
    assert_eq!(placeholders.len(), 1, "应当只有一条占位符：{masked}");
    assert_eq!(placeholders[0].2, 1, "首个值拿 1 号");
}

/// 实体类型过滤与最小长度都照配置生效；两层过滤都不通过时文本原样返回。
#[test]
fn ner_layer_filters_by_entity_type_and_min_length() {
    let mut harness = Harness::new();
    // 类型表里没有 PER：整层没有命中。
    let without_per = layer(&["ORG"], 2);
    let text = "张三在测试";
    assert_eq!(harness.run(Some(&without_per), None, text), text);
    assert_eq!(harness.stats.ner_masked, 0);
    assert_eq!(harness.stats.values_masked, 0);

    // 最小长度 3 > 「张三」的 2 个字符：同样不替换。
    let too_short = layer(&["PER"], 3);
    assert_eq!(harness.run(Some(&too_short), None, text), text);
    assert_eq!(harness.stats.ner_masked, 0);
}

/// 接上计划缓存后：首次记录计划，第二次按计划重放，计数按阶段补回 `ner_masked`。
#[test]
fn ner_stage_participates_in_the_plan_cache() {
    let layer = layer(&["PER"], 2);
    let cache = Arc::new(MaskPlanCache::default());
    let mut harness = Harness::new();
    let text = "张三在测试";

    let first = harness.run(Some(&layer), Some(Arc::clone(&cache)), text);
    assert_eq!(harness.stats.ner_masked, 1);

    let reloaded = harness.run(Some(&layer), Some(Arc::clone(&cache)), text);
    assert_eq!(reloaded, first, "重放结果应与首次一致");
    assert_eq!(
        harness.stats.ner_masked, 2,
        "重放也要补回 NER 阶段计数（计划里记着阶段标签）"
    );
    let stats = cache.stats();
    assert_eq!(stats.hits, 1, "第二次应当命中计划缓存");
    assert_eq!(stats.misses, 1);
}

/// 未注入兜底层（未启用 / 权重不可用）时，屏蔽路径与从前完全一致。
#[test]
fn without_a_layer_the_stage_is_a_no_op() {
    let mut harness = Harness::new();
    let text = "张三在测试";
    assert_eq!(harness.run(None, None, text), text);
    assert_eq!(harness.stats.ner_masked, 0);
    assert_eq!(harness.stats.values_masked, 0);
}

/// 旁路一次性脱敏器（审查模型那条链路）与运行时装饰器共用同一层 NER 兜底。
///
/// Python `OneShotMasker.__init__` 同样自己 `build_ner_layer(config)`（`oneshot.py:52`），
/// 所以注入的层必须真的参与 `mask()` 的最后一层；否则审查请求的**出站**脱敏
/// 会比普通回合少一层语义保护。
#[test]
fn oneshot_masker_uses_an_injected_ner_layer() {
    let keys: Vec<String> = Vec::new();
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let source = Arc::clone(&counter);
    let registry = SequenceRegistry::with_sequence_source(move || {
        source.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    });

    let mut masker = OneShotMasker::with_registry(
        OneshotOptions::default(),
        Vec::new(),
        &keys,
        &keys,
        registry,
    );
    // 未注入时为 no-op（对应 Python 的 `_ner_layer = None`）。
    let text = "张三在测试";
    assert_eq!(masker.mask(text), text);
    assert_eq!(masker.stats().ner_masked, 0);
    masker.close();

    // 注入后：实体被替换，编号走本实例的注册表，非实体部分原样保留。
    let mut masker = OneShotMasker::with_registry(
        OneshotOptions::default(),
        Vec::new(),
        &keys,
        &keys,
        SequenceRegistry::with_sequence_source({
            let source = Arc::clone(&counter);
            move || source.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
        }),
    )
    .with_ner(Arc::new(layer(&["PER"], 2)));

    let masked = masker.mask(text);
    assert_ne!(masked, text, "实体应当被替换成占位符：{masked}");
    assert!(!masked.contains("张三"), "实体本体不应留在文本里：{masked}");
    assert!(masked.contains("在测试"), "非实体部分原样保留：{masked}");
    assert_eq!(masker.stats().ner_masked, 1);
    assert_eq!(
        find_placeholders(&masked).len(),
        1,
        "应当只有一条占位符：{masked}"
    );

    // 同一实例可整段还原（一次性语义：屏蔽 → 发送 → 还原 → 注销）。
    assert_eq!(masker.restore(&masked).expect("还原不应失败"), text);
    masker.close();
    assert_eq!(
        masker.restore(&masked).expect("注销后还原退化为原样"),
        masked,
        "注销后序号已失效，还原不再改动文本"
    );
}
