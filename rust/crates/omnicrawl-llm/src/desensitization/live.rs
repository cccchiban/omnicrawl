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

use std::sync::Mutex;

use omnicrawl_protocol::{
    aggregate_stream_events, ModelReply, ModelStreamEvent, ReasoningDelta, TextDelta,
    ToolCallArgumentsDelta,
};

use super::engine::{MaskContext, SensitiveMatcher};
use super::rules::{build_enabled_rules, PatternRule};
use super::stream::StreamRestorer;
use super::{collect_placeholder_numbers, DesensitizationStats, SequenceRegistry};
use crate::errors::RuntimeError;
use crate::request::ChatRequestInput;
use crate::runtime::{ModelRuntime, SinkFlow, TurnSink};

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
    pub extra_sensitive_keys: Vec<String>,
    pub exempt_keys: Vec<String>,
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
            extra_sensitive_keys: Vec::new(),
            exempt_keys: Vec::new(),
        }
    }
}

/// 消息脱敏装饰器：屏蔽出站请求、还原入站事件、管理序号生命周期。
pub struct DesensitizationRuntime {
    inner: Box<dyn ModelRuntime>,
    options: DesensitizationOptions,
    matcher: SensitiveMatcher,
    rules: Vec<PatternRule>,
    registry: Mutex<SequenceRegistry>,
    stats: Mutex<DesensitizationStats>,
}

impl DesensitizationRuntime {
    pub fn new(inner: Box<dyn ModelRuntime>, options: DesensitizationOptions) -> Self {
        let categories: Vec<&str> = options.rule_categories.iter().map(String::as_str).collect();
        let matcher = SensitiveMatcher::new(&options.extra_sensitive_keys, &options.exempt_keys);
        Self {
            inner,
            rules: build_enabled_rules(&categories),
            matcher,
            registry: Mutex::new(SequenceRegistry::new()),
            stats: Mutex::new(DesensitizationStats::default()),
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

    /// 关闭：丢弃全部未注销序号（不落盘、不恢复），再交给调用方关闭内层。
    pub fn close(&self) {
        lock(&self.registry).drop_all();
        *lock(&self.stats) = DesensitizationStats::default();
    }

    pub fn stats(&self) -> DesensitizationStats {
        lock(&self.stats).clone()
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
            };
            super::middleware::mask_messages(input.messages, &mut context)
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
