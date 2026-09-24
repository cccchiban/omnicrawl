//! 消息脱敏的运行时装饰器（Python `middleware.py` 的 `DesensitizationRuntime`）。
//!
//! 包住任意 [`ModelRuntime`]：出站请求先屏蔽再发，入站事件逐条还原（尾部挂起缓冲照样生效），
//! 工具参数里出现「本进程分配过、但已无法还原」的占位符时按 fail-closed 中止——
//! 宁可失败，也不把占位符写进文件。
//!
//! 与原实现的两处实现差异（语义等价，见 README）：
//! ① 不复用上一周期的掩码请求（`begin_cycle` 的复用标记只用于周期配对），总是重新脱敏——
//!    稳定序号索引保证同值同号，结果一致，代价只是重复扫描；
//! ② 计数分成两层：周期计数在注册表里，屏蔽/还原计数在本层的 `stats` 里。

use std::sync::{Arc, Mutex};

use omnicrawl_protocol::{
    aggregate_stream_events, ModelReply, ModelStreamEvent, ReasoningDelta, TextDelta,
    ToolCallArgumentsDelta,
};

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::engine::{MaskContext, SensitiveMatcher};
use super::gitleaks::{load_rules, GitleaksRule};
use super::middleware::MessageMaskMemo;
use super::ner::{
    build_ner_layer, model_path_env_name, resolve_runtime_model_path, NerExtractorPool, NerLayer,
    NerLayerOptions,
};
use super::ner_weights::NerWeights;
use super::plan_cache::{MaskPlanCache, PlanCacheStats};
use super::rules::{build_enabled_rules, PatternRule};
use super::stream::StreamRestorer;
use super::{collect_placeholder_numbers, DesensitizationStats, SequenceRegistry};
use crate::errors::RuntimeError;
use crate::request::ChatRequestInput;
use crate::runtime::{ModelRuntime, SinkFlow, TurnSink};

/// 随包权重的存放目录：`<crate>/data`（与 `gitleaks.toml` 同址）。
fn ner_data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("data")
}

/// 进程级的抽取器池：权重 5 MB，多个运行期（会话 / 工具表重建）共用一个实例。
static NER_POOL: OnceLock<NerExtractorPool> = OnceLock::new();

/// 按配置构建 NER 兜底层：未启用、权重缺失或读取失败都返回 `None`（静默降级）。
///
/// 与 Python 一致：`[desensitization].ner_enabled` 默认关，且加载失败不阻断屏蔽路径——
/// 少一层语义兜底仍然是一套可用的脱敏，而报错会让整个回合发不出请求。
///
/// 公开给**旁路调用方**：Python 的 `OneShotMasker` 也自己 `build_ner_layer(config)`
/// （`oneshot.py:52`），审查模型那条链路因此共用同一个抽取器池。
pub fn build_runtime_ner_layer(options: &NerLayerOptions) -> Option<NerLayer> {
    if !options.enabled {
        return None;
    }
    let pool = NER_POOL.get_or_init(NerExtractorPool::new);
    let env_model_path = std::env::var(model_path_env_name()).ok();
    let model_path = resolve_runtime_model_path(
        Some(options.model_path.as_str()),
        env_model_path.as_deref(),
        &ner_data_dir(),
    );
    build_ner_layer(options, pool, &model_path, |path| {
        NerWeights::load(path)
            .ok()
            .map(|weights| Arc::new(weights) as Arc<dyn super::ner::NerBackend>)
    })
}

/// 脱敏层的配置（Python `DesensitizationConfig` 的内核子集）。
///
/// 值类型规则层与熵兜底默认不启用：出厂默认值属于配置层
/// （`omnicrawl/config/features/desensitization.py`），等 `[desensitization]` 搬进内核再对齐。
#[derive(Debug, Clone)]
pub struct DesensitizationOptions {
    pub enabled: bool,
    pub strict_restore: bool,
    /// 脱敏失败时中止请求（默认开：宁可失败也不发原文）。
    pub fail_closed: bool,
    pub entropy_enabled: bool,
    pub entropy_min_length: usize,
    pub entropy_min_bits: f64,
    pub entropy_pure_letters: bool,
    pub entropy_pure_digits: bool,
    /// 值类型规则层要启用的类别（空集就是不启用该层）。
    pub rule_categories: Vec<String>,
    /// gitleaks 规则表（Python 出厂默认开启；内核侧默认关，等配置层搬完再对齐）。
    pub gitleaks_enabled: bool,
    /// 自定义 gitleaks.toml 路径；`None` 用内嵌快照。
    pub gitleaks_config_path: Option<String>,
    pub extra_sensitive_keys: Vec<String>,
    pub exempt_keys: Vec<String>,
    /// NER 语义兜底层：默认关闭（与 Python 出厂默认一致），启用后接在屏蔽路径的最后一层。
    pub ner: NerLayerOptions,
}

impl Default for DesensitizationOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            strict_restore: false,
            fail_closed: true,
            entropy_enabled: false,
            entropy_min_length: 0,
            entropy_min_bits: 0.0,
            entropy_pure_letters: false,
            entropy_pure_digits: false,
            rule_categories: Vec::new(),
            gitleaks_enabled: false,
            gitleaks_config_path: None,
            extra_sensitive_keys: Vec::new(),
            exempt_keys: Vec::new(),
            ner: NerLayerOptions::default(),
        }
    }
}

/// 消息脱敏装饰器：屏蔽出站请求、还原入站事件、管理序号生命周期。
pub struct DesensitizationRuntime {
    inner: Box<dyn ModelRuntime>,
    options: DesensitizationOptions,
    matcher: SensitiveMatcher,
    rules: Vec<PatternRule>,
    gitleaks_rules: Vec<GitleaksRule>,
    registry: Mutex<SequenceRegistry>,
    stats: Mutex<DesensitizationStats>,
    /// 屏蔽计划缓存：按运行时实例持有，随运行时关闭清空（Python 的 `self._plan_cache`）。
    plan_cache: Arc<MaskPlanCache>,
    /// 逐消息屏蔽结果缓存：历史每轮全量重发，逐字未变的消息不必重跑引擎
    /// （Python 的 `self._memo`）。
    memo: MessageMaskMemo,
    /// NER 兜底层：未启用或权重不可用时为 `None`（静默降级，与 Python 的
    /// `build_ner_layer` 同义）。抽取器按 (路径, 设备, 缓存容量) 池化，关闭时一起清掉。
    ner: Option<NerLayer>,
}

impl DesensitizationRuntime {
    pub fn new(inner: Box<dyn ModelRuntime>, options: DesensitizationOptions) -> Self {
        let categories: Vec<&str> = options.rule_categories.iter().map(String::as_str).collect();
        let matcher = SensitiveMatcher::new(&options.extra_sensitive_keys, &options.exempt_keys);
        let gitleaks_rules = if options.gitleaks_enabled {
            load_rules(options.gitleaks_config_path.as_deref())
        } else {
            Vec::new()
        };
        let ner = build_runtime_ner_layer(&options.ner);
        Self {
            ner,
            inner,
            rules: build_enabled_rules(&categories),
            gitleaks_rules,
            matcher,
            registry: Mutex::new(SequenceRegistry::new()),
            stats: Mutex::new(DesensitizationStats::default()),
            plan_cache: Arc::new(MaskPlanCache::default()),
            memo: MessageMaskMemo::default(),
            options,
        }
    }

    /// 按配置决定是否包装（Python `maybe_wrap_runtime`）：未启用时零成本原样返回。
    pub fn maybe_wrap(
        runtime: Box<dyn ModelRuntime>,
        options: &DesensitizationOptions,
    ) -> Box<dyn ModelRuntime> {
        if !options.enabled {
            return runtime;
        }
        Box::new(Self::new(runtime, options.clone()))
    }

    /// 关闭：丢弃全部未注销序号与两处缓存（不落盘、不恢复），再交给调用方关闭内层。
    pub fn close(&self) {
        self.memo.clear();
        self.plan_cache.clear();
        // 抽取器是跨会话复用的池化资源（权重约 5 MB）：这里只丢自己的那份引用，
        // 池本身是进程级的（还有别的运行期可能正用着同一实例）。
        lock(&self.registry).drop_all();
        *lock(&self.stats) = DesensitizationStats::default();
    }

    /// NER 兜底层的缓存计数；未启用时为 `None`（观测 / 测试用）。
    pub fn ner_stats(&self) -> Option<super::ner::NerCacheStats> {
        self.ner.as_ref().map(|layer| layer.stats())
    }

    pub fn stats(&self) -> DesensitizationStats {
        lock(&self.stats).clone()
    }

    /// 屏蔽计划缓存（观测 / 测试用）。
    pub fn plan_cache(&self) -> &MaskPlanCache {
        &self.plan_cache
    }

    /// 计划缓存的计数与占用。
    pub fn plan_cache_stats(&self) -> PlanCacheStats {
        self.plan_cache.stats()
    }

    /// 逐消息屏蔽缓存的 (命中, 未命中) 计数（观测 / 测试用）。
    pub fn memo_stats(&self) -> (u64, u64) {
        self.memo.stats()
    }

    /// 一次回合：屏蔽 → 内层 → 还原 → 归并。
    pub fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        let fingerprint = request_fingerprint(input);
        let mut registry = lock(&self.registry);
        let mut stats = lock(&self.stats);
        let (mut cycle, _reused) = registry.begin_cycle(&fingerprint);

        let masked_messages = {
            let mut context = MaskContext {
                matcher: &self.matcher,
                cycle: &mut cycle,
                stats: &mut stats,
                entropy_enabled: self.options.entropy_enabled,
                entropy_min_length: self.options.entropy_min_length,
                entropy_min_bits: self.options.entropy_min_bits,
                entropy_pure_letters: self.options.entropy_pure_letters,
                entropy_pure_digits: self.options.entropy_pure_digits,
                pattern_rules: &self.rules,
                gitleaks_rules: &self.gitleaks_rules,
                ner: self.ner.as_ref(),
                plan_cache: Some(Arc::clone(&self.plan_cache)),
                plan_builder: None,
            };
            // 与 Python 的 `_mask_message_cached` 路径一致：逐消息缓存命中即复用，
            // 未命中才走引擎（引擎内部再走文本级的计划缓存）。
            super::middleware::mask_messages_cached(input.messages, &mut context, Some(&self.memo))
        };
        // 请求里已经写好的占位符样式序号要登记为保留号，避免新值抢号。
        let request_texts = super::middleware::collect_request_texts(
            input.system_prompt,
            &masked_messages,
            input.tools,
        );
        cycle.reserved = collect_placeholder_numbers(request_texts.iter().map(String::as_str));

        let masked_input = ChatRequestInput {
            model: input.model,
            system_prompt: input.system_prompt,
            messages: &masked_messages,
            tools: input.tools,
            options: input.options,
            profile_request_timeout_seconds: input.profile_request_timeout_seconds,
            prompt_cache_capable: input.prompt_cache_capable,
            prompt_cache_identity: input.prompt_cache_identity,
        };

        let mut collector = CollectSink::default();
        // 失败或取消：周期保持开放，好让调用方重试时复用同一批序号（与 Python 一致）。
        self.inner.run_turn(&masked_input, &mut collector)?;

        let mut restorer = StreamRestorer::new(&cycle, &mut stats, self.options.strict_restore);
        let mut events: Vec<ModelStreamEvent> = Vec::new();
        for event in &collector.events {
            let mapped = match crate::desensitization::middleware::map_event(
                event,
                &mut restorer,
                &registry,
            ) {
                Ok(items) => items,
                Err(error) => return Err(desensitization_error(error.message())),
            };
            for item in mapped {
                if emit(&mut events, sink, item) == SinkFlow::Cancel {
                    return Err(RuntimeError::cancelled());
                }
            }
            for warning in restorer.take_warnings() {
                if emit(
                    &mut events,
                    sink,
                    ModelStreamEvent::ProviderWarning(warning),
                ) == SinkFlow::Cancel
                {
                    return Err(RuntimeError::cancelled());
                }
            }
        }

        let (text_tail, reasoning_tail) = match restorer.flush() {
            Ok(tails) => tails,
            Err(error) => return Err(desensitization_error(error.message())),
        };
        for event in split_tails(text_tail, reasoning_tail) {
            if emit(&mut events, sink, event) == SinkFlow::Cancel {
                return Err(RuntimeError::cancelled());
            }
        }
        let argument_tails = match restorer.flush_tool_arguments() {
            Ok(tails) => tails,
            Err(error) => return Err(desensitization_error(error.message())),
        };
        for (call_id, tail) in argument_tails {
            let event = ModelStreamEvent::ToolCallArgumentsDelta(ToolCallArgumentsDelta {
                call_id,
                delta: tail,
            });
            if emit(&mut events, sink, event) == SinkFlow::Cancel {
                return Err(RuntimeError::cancelled());
            }
        }
        for warning in restorer.take_warnings() {
            if emit(
                &mut events,
                sink,
                ModelStreamEvent::ProviderWarning(warning),
            ) == SinkFlow::Cancel
            {
                return Err(RuntimeError::cancelled());
            }
        }

        let usable = restorer.reply_usable();
        if usable {
            registry.close_cycle(&cycle);
        }
        Ok(aggregate_stream_events(events))
    }
}

impl ModelRuntime for DesensitizationRuntime {
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        DesensitizationRuntime::run_turn(self, input, sink)
    }
}

/// 收集内层事件的接收端：本层先拿全量事件，再统一还原后转发给外层。
#[derive(Default)]
struct CollectSink {
    events: Vec<ModelStreamEvent>,
}

impl TurnSink for CollectSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        self.events.push(event);
        SinkFlow::Continue
    }
}

fn emit(
    events: &mut Vec<ModelStreamEvent>,
    sink: &mut dyn TurnSink,
    event: ModelStreamEvent,
) -> SinkFlow {
    let flow = sink.on_event(event.clone());
    events.push(event);
    flow
}

fn split_tails(text: String, reasoning: String) -> Vec<ModelStreamEvent> {
    let mut events = Vec::new();
    if !text.is_empty() {
        events.push(ModelStreamEvent::TextDelta(TextDelta::new(text)));
    }
    if !reasoning.is_empty() {
        events.push(ModelStreamEvent::ReasoningDelta(ReasoningDelta::new(
            reasoning,
        )));
    }
    events
}

/// 周期复用的判定依据：同一份请求（模型 + 系统提示 + 消息 + 工具）才算同一个逻辑请求。
fn request_fingerprint(input: &ChatRequestInput<'_>) -> String {
    let payload = serde_json::json!({
        "model": input.model,
        "system_prompt": input.system_prompt,
        "messages": input.messages,
        "tools": input.tools,
    });
    crate::json::dumps(&payload)
}

fn desensitization_error(message: &str) -> RuntimeError {
    RuntimeError::configuration(message)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
