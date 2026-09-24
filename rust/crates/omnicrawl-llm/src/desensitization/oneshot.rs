//! 旁路调用的一次性文本屏蔽 / 还原（设计稿 §4.4：审批自动审查等非 Runtime 链路）。
//!
//! 语义基准是 Python `omnicrawl/llm/desensitization/oneshot.py`：不经统一运行时的模型调用同样需要
//! 「出站屏蔽 + 入站还原」，本模块把引擎、序号注册表与还原状态机编排为单次使用的 `OneShotMasker`
//! ——屏蔽 → 发送 → 还原 → 注销，生命周期最短（§7.3 单次使用语义）。
//!
//! 配置面（`OneshotOptions`）与规则集合由调用方给出：Python 侧的 `[desensitization]` 配置模块
//! 尚未搬进内核，`build_enabled_rules` 的输出与阈值一起传进来即可。

use std::sync::Arc;

use super::engine::{mask_text, MaskContext, SensitiveMatcher};
use super::ner::NerLayer;
use super::rules::PatternRule;
use super::stream::StreamRestorer;
use super::DesensitizationError;
use super::{
    collect_placeholder_numbers, DesensitizationStats, PlaceholderCycle, SequenceRegistry,
};

/// 一次性脱敏器的配置面。
pub struct OneshotOptions {
    pub entropy_enabled: bool,
    pub entropy_min_length: usize,
    pub entropy_min_bits: f64,
    pub entropy_pure_letters: bool,
    pub entropy_pure_digits: bool,
    pub strict_restore: bool,
}

impl Default for OneshotOptions {
    fn default() -> Self {
        Self {
            entropy_enabled: true,
            entropy_min_length: 20,
            entropy_min_bits: 3.5,
            entropy_pure_letters: false,
            entropy_pure_digits: false,
            strict_restore: false,
        }
    }
}

/// 单次「屏蔽 → 还原」调用的脱敏器（一个实例对应一次出站请求）。
pub struct OneShotMasker {
    matcher: SensitiveMatcher,
    rules: Vec<PatternRule>,
    /// gitleaks 规则（运行时正则）：排在值类型规则之后，与 Python `build_enabled_rules`
    /// 把 gitleaks 追加到内置规则尾部同一顺序；未启用时为空。
    gitleaks_rules: Vec<super::gitleaks::GitleaksRule>,
    options: OneshotOptions,
    /// NER 语义兜底层：由调用方注入（`[desensitization].ner_*` 由配置层解析），
    /// 未启用或权重不可用时为 `None`。
    ///
    /// 与 Python `OneShotMasker` 一致——旁路调用同样接这一层兜底（`oneshot.py:52`）；
    /// 权重加载与池化不在本模块做，调用方用
    /// [`build_runtime_ner_layer`](super::live::build_runtime_ner_layer) 拿到层再注入。
    ner: Option<Arc<NerLayer>>,
    stats: DesensitizationStats,
    registry: SequenceRegistry,
    cycle: Option<PlaceholderCycle>,
}

impl OneShotMasker {
    pub fn new(
        options: OneshotOptions,
        rules: Vec<PatternRule>,
        extra_keys: &[String],
        exempt_keys: &[String],
    ) -> Self {
        Self::with_registry(
            options,
            rules,
            extra_keys,
            exempt_keys,
            SequenceRegistry::new(),
        )
    }

    /// 注入注册表（测试 / 旁路定制：注入自增计数器后序号确定）。
    pub fn with_registry(
        options: OneshotOptions,
        rules: Vec<PatternRule>,
        extra_keys: &[String],
        exempt_keys: &[String],
        registry: SequenceRegistry,
    ) -> Self {
        Self {
            matcher: SensitiveMatcher::new(extra_keys, exempt_keys),
            rules,
            gitleaks_rules: Vec::new(),
            options,
            ner: None,
            stats: DesensitizationStats::default(),
            registry,
            cycle: None,
        }
    }

    /// 附带 NER 语义兜底层（与 Python `OneShotMasker._ner_layer` 对应）。
    pub fn with_ner(mut self, layer: Arc<NerLayer>) -> Self {
        self.ner = Some(layer);
        self
    }

    /// 附带 gitleaks 规则（与 Python `build_enabled_rules` 追加 gitleaks 的行为对应）。
    ///
    /// 规则顺序不变：值类型规则在前，gitleaks 规则在后，重叠区间由先命中者占位。
    pub fn with_gitleaks(mut self, gitleaks_rules: Vec<super::gitleaks::GitleaksRule>) -> Self {
        self.gitleaks_rules = gitleaks_rules;
        self
    }

    /// 屏蔽文本并登记本次周期；返回替换后的文本。
    pub fn mask(&mut self, text: &str) -> String {
        let (mut cycle, _) = self.registry.begin_cycle(text);
        cycle.reserved = collect_placeholder_numbers([text]);
        let masked = {
            let mut context = MaskContext {
                matcher: &self.matcher,
                cycle: &mut cycle,
                stats: &mut self.stats,
                entropy_enabled: self.options.entropy_enabled,
                entropy_min_length: self.options.entropy_min_length,
                entropy_min_bits: self.options.entropy_min_bits,
                entropy_pure_letters: self.options.entropy_pure_letters,
                entropy_pure_digits: self.options.entropy_pure_digits,
                pattern_rules: &self.rules,
                gitleaks_rules: &self.gitleaks_rules,
                // NER 兜底层由调用方注入（见 `with_ner`）。
                ner: self.ner.as_deref(),
                // 一次性脱敏器不持有计划缓存（每次调用都是新文本，没有复用收益）。
                plan_cache: None,
                plan_builder: None,
            };
            mask_text(text, &mut context)
        };
        // 稳定序号复用计数（只到计数粒度，§10.2）。
        self.stats.sequence_reuses += cycle.stable_reuses;
        self.cycle = Some(cycle);
        masked
    }

    /// 还原文本中的完整占位符；未注册序号保留原样（严格模式抛错）。
    pub fn restore(&mut self, text: &str) -> Result<String, DesensitizationError> {
        let Some(cycle) = self.cycle.as_ref() else {
            return Ok(text.to_string());
        };
        let mut restorer = StreamRestorer::new(cycle, &mut self.stats, self.options.strict_restore);
        restorer.restore_string(text)
    }

    /// 注销本周期全部序号（单次使用、不落盘、不可恢复，§7.3）。
    pub fn close(&mut self) {
        self.registry.drop_all();
        self.cycle = None;
    }

    /// 审计计数（只到「数量级」粒度，不含任何原文）。
    pub fn stats(&self) -> &DesensitizationStats {
        &self.stats
    }
}
