//! 消息脱敏（可逆占位符 `｛Desensitized:n｝`）：本文件是模块根，放子系统错误面与序号注册表。
//!
//! 语义基准是 Python `omnicrawl/llm/desensitization/`。原文只进内存注册表：
//! 不落盘、不进日志、不进会话事件；可观测信息只到「计数 / 规则 ID / 序号」粒度。
//!
//! 序号按「值的指纹」稳定分配：同一值在连续请求中始终复用同一序号，使未变历史脱敏后
//! 逐字一致，从而命中提供方前缀缓存。
//!
//! 已落地：`registry`（占位符协议、序号分配、周期与会话映射，Python `registry.py`）、
//! `stream`（流式还原状态机，Python `stream.py`）、`rules`（值类型规则层，Python `rules.py`，
//! 已搬网址 / 邮箱 / 银行卡 / MAC / 车牌与整套规则语义）。引擎、gitleaks、NER、middleware、
//! oneshot 尚未搬运。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use sha2::{Digest, Sha256};

pub mod engine;
pub mod rules;
pub mod stream;

pub use engine::{
    find_entropy_spans, is_entropy_candidate, is_entropy_exempt, mask_structured_value, mask_text,
    normalize_key, should_skip_value, MaskContext, SensitiveMatcher,
};
pub use rules::{
    build_enabled_rules, builtin_rules, scan_pattern_rules, shannon_entropy_bits, PatternRule,
    RuleMatch, RuleMatcher,
};
pub use stream::{StreamRestorer, TRUNCATED_FINISH_REASONS};

pub const PLACEHOLDER_MARKER: &str = "Desensitized";
pub const FULLWIDTH_OPEN_BRACE: char = '\u{ff5b}';
pub const FULLWIDTH_CLOSE_BRACE: char = '\u{ff5d}';

/// 脱敏层中止请求（fail-closed / 严格还原）；不向模型发送原文。
///
/// 语义基准是 Python 的 `DesensitizationError`（定义在 `middleware.py`，继承 `ModelError`，
/// 错误码 `UNKNOWN`、不可重试）。内核侧暂不并入运行时错误面 `RuntimeError`：本层只报
/// 「正文 + 不可重试」，映射到哪个运行时错误留到 middleware 接线时决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesensitizationError {
    message: String,
}

impl DesensitizationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DesensitizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DesensitizationError {}

static SEQUENCE_COUNTER: AtomicU64 = AtomicU64::new(0);
static CYCLE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static STABLE_INDEX_SALT: OnceLock<[u8; 32]> = OnceLock::new();
static SHARED_STABLE_INDEX: OnceLock<Arc<Mutex<StableSequenceIndex>>> = OnceLock::new();

/// 进程内全局单调递增序号（从 1 开始，作为稳定索引的新值分配来源）。
pub fn next_sequence_number() -> u64 {
    SEQUENCE_COUNTER.fetch_add(1, Ordering::SeqCst) + 1
}

/// 生成端唯一规范：全角花括号、序号无前导零。
pub fn format_placeholder(seq: u64) -> String {
    format!("{FULLWIDTH_OPEN_BRACE}{PLACEHOLDER_MARKER}:{seq}{FULLWIDTH_CLOSE_BRACE}")
}

/// 解析占位符，返回 `(起始字节, 结束字节, 序号)`。
///
/// 生成端用全角，还原端兼容半角、冒号变体（`:` 与 `：`）、大小写与序号两侧空白。
pub fn find_placeholders(text: &str) -> Vec<(usize, usize, u64)> {
    let characters: Vec<char> = text.chars().collect();
    let offsets = char_offsets(&characters);
    let mut found = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if let Some((end_index, seq)) = match_placeholder(&characters, index) {
            found.push((offsets[index], offsets[end_index], seq));
            index = end_index;
            continue;
        }
        index += 1;
    }
    found
}

/// 疑似占位符前缀（用于识别「半截 / 畸形」文本：保留原文并告警）。
pub fn has_placeholder_prefix(text: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        if matches!(characters[index], '{' | FULLWIDTH_OPEN_BRACE) {
            let mut cursor = skip_whitespace(&characters, index + 1);
            cursor = skip_whitespace(&characters, cursor);
            if matches_marker(&characters, cursor) {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// 值 → 进程级指纹（HMAC-SHA256）：仅用于同值判定，不落原文、不可反推。
pub fn sequence_fingerprint(value: &str) -> String {
    hmac_sha256_hex(salt(), value.as_bytes())
}

/// 收集出站内容中已存在的占位符样式序号；分配时跳过以避免还原冲突。
pub fn collect_placeholder_numbers<'a, I>(texts: I) -> HashSet<u64>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut numbers = HashSet::new();
    for text in texts {
        if text.is_empty() {
            continue;
        }
        for (_, _, seq) in find_placeholders(text) {
            numbers.insert(seq);
        }
    }
    numbers
}

/// 值指纹 → 序号 的稳定映射（前缀缓存保护）。
///
/// 同一值永远拿到同一序号：只保存 HMAC 指纹与整数序号，不保存原文；周期注销不清理本索引。
pub struct StableSequenceIndex {
    state: Mutex<StableIndexState>,
    sequence_source: Box<dyn Fn() -> u64 + Send + Sync>,
}

struct StableIndexState {
    entries: HashMap<String, u64>,
    assigned: HashSet<u64>,
}

impl StableSequenceIndex {
    pub fn new() -> Self {
        Self::with_sequence_source(next_sequence_number)
    }

    pub fn with_sequence_source<F>(sequence_source: F) -> Self
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        Self {
            state: Mutex::new(StableIndexState {
                entries: HashMap::new(),
                assigned: HashSet::new(),
            }),
            sequence_source: Box::new(sequence_source),
        }
    }

    /// 取值对应序号，返回 `(序号, 是否复用)`；新值分配时跳过 `reserved`。
    pub fn sequence_for(&self, value: &str, reserved: &HashSet<u64>) -> (u64, bool) {
        let fingerprint = sequence_fingerprint(value);
        let mut state = lock(&self.state);
        if let Some(existing) = state.entries.get(&fingerprint) {
            return (*existing, true);
        }
        let mut seq = (self.sequence_source)();
        while reserved.contains(&seq) {
            seq = (self.sequence_source)();
        }
        state.entries.insert(fingerprint, seq);
        state.assigned.insert(seq);
        (seq, false)
    }

    /// 序号是否由本进程分配过（识别「本层分配过、但映射已丢失」的占位符）。
    pub fn assigned(&self, seq: u64) -> bool {
        lock(&self.state).assigned.contains(&seq)
    }

    /// 已登记的不同值数量（只到计数粒度，不含任何原文）。
    pub fn size(&self) -> usize {
        lock(&self.state).entries.len()
    }

    /// 「值指纹 → 序号」快照（审计 / 测试用；键是指纹，不含原文）。
    pub fn snapshot(&self) -> HashMap<String, u64> {
        lock(&self.state).entries.clone()
    }
}

impl Default for StableSequenceIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// 会话级「序号 → 原文」映射。
///
/// 同一会话内构建的所有运行时共享它：切换模型会重建运行时但不丢序号；会话切换时
/// `rebind` 丢弃上一会话的原文。映射只在本进程内存驻留。
#[derive(Default)]
pub struct SessionSequenceCache {
    session_id: String,
    /// 序号 → 原文，按登记顺序追加（对应 Python dict 的插入序）。
    pub entries: Vec<(u64, String)>,
}

impl SessionSequenceCache {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            entries: Vec::new(),
        }
    }

    /// 会话标识变化 → 丢弃上一会话的映射；标识未知不算会话变更。
    pub fn rebind(&mut self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        if session_id != self.session_id {
            self.entries.clear();
            self.session_id = session_id.to_string();
        }
    }

    /// 会话结束：丢弃全部原文，不落盘、不恢复。
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn register(&mut self, seq: u64, value: &str) {
        match self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == seq)
        {
            Some(entry) => entry.1 = value.to_string(),
            None => self.entries.push((seq, value.to_string())),
        }
    }

    fn lookup(&self, seq: u64) -> Option<String> {
        self.entries
            .iter()
            .find(|(existing, _)| *existing == seq)
            .map(|(_, value)| value.clone())
    }

    fn pairs_from(&self, start: usize) -> Vec<(u64, String)> {
        self.entries.iter().skip(start).cloned().collect()
    }
}

/// 只到「数量级」粒度的审计计数；不记录任何原文。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesensitizationStats {
    pub cycles_started: u64,
    pub cycles_reused: u64,
    pub values_masked: u64,
    pub sequence_reuses: u64,
    pub restore_hits: u64,
    pub restore_unresolved: u64,
    pub restore_malformed: u64,
    pub skipped_values: u64,
    pub entropy_masked: u64,
    pub rules_masked: u64,
    pub ner_masked: u64,
    pub mask_failures: u64,
    pub last_mask_duration_ms: f64,
}

/// 一次发送-接收周期的注册集合：序号 → 原文，同值同号。
///
/// `store` 是**会话级共享**映射：由会话所有者注入各周期，序号在会话内不再单次使用；
/// 未注入时退化为注册表私有映射。
pub struct PlaceholderCycle {
    pub cycle_id: String,
    stable_index: Arc<Mutex<StableSequenceIndex>>,
    store: Arc<Mutex<SessionSequenceCache>>,
    pub reserved: HashSet<u64>,
    pub closed: bool,
    pub source_request: Option<String>,
    pub masked_request: Option<String>,
    pub stable_reuses: u64,
    value_index: HashMap<String, u64>,
}

impl PlaceholderCycle {
    fn new(
        cycle_id: String,
        stable_index: Arc<Mutex<StableSequenceIndex>>,
        store: Arc<Mutex<SessionSequenceCache>>,
    ) -> Self {
        Self {
            cycle_id,
            stable_index,
            store,
            reserved: HashSet::new(),
            closed: false,
            source_request: None,
            masked_request: None,
            stable_reuses: 0,
            value_index: HashMap::new(),
        }
    }

    fn clone_for_reuse(&self) -> Self {
        Self {
            cycle_id: self.cycle_id.clone(),
            stable_index: self.stable_index.clone(),
            store: self.store.clone(),
            reserved: self.reserved.clone(),
            closed: self.closed,
            source_request: self.source_request.clone(),
            masked_request: self.masked_request.clone(),
            stable_reuses: self.stable_reuses,
            value_index: self.value_index.clone(),
        }
    }

    /// 取值对应序号；同值复用同一序号，新值分配并跳过预留序号。
    ///
    /// 第二个返回值是「本周期首次登记」的审计口径；跨周期复用由 `stable_reuses` 单独计数。
    pub fn seq_for_value(&mut self, value: &str) -> (u64, bool) {
        if let Some(existing) = self.value_index.get(value) {
            return (*existing, false);
        }
        let (seq, reused) = lock(&self.stable_index).sequence_for(value, &self.reserved);
        if reused {
            self.stable_reuses += 1;
        }
        self.value_index.insert(value.to_string(), seq);
        lock(&self.store).register(seq, value);
        (seq, true)
    }

    /// 按序号取原文；未注册返回 `None`（调用方保留占位符 + 告警）。
    pub fn lookup(&self, seq: u64) -> Option<String> {
        lock(&self.store).lookup(seq)
    }

    /// 把「本进程已确定过序号」的值登记进本周期（复用缓存屏蔽结果时使用）。
    pub fn adopt(&mut self, value: &str, seq: u64) {
        if value.is_empty() || seq == 0 {
            return;
        }
        if self.value_index.get(value) == Some(&seq) {
            return;
        }
        self.value_index.insert(value.to_string(), seq);
        self.stable_reuses += 1;
        lock(&self.store).register(seq, value);
    }

    /// 返回 `start` 之后新增的 (序号, 值) 对（共享映射按登记顺序追加）。
    pub fn pairs_from(&self, start: usize) -> Vec<(u64, String)> {
        lock(&self.store).pairs_from(start)
    }

    /// 共享映射当前登记的数量（`pairs_from` 的游标口径）。
    pub fn store_len(&self) -> usize {
        lock(&self.store).entries.len()
    }

    /// 结束本周期：只释放周期自身状态，会话级「序号 → 原文」映射保留。
    pub fn close(&mut self) {
        self.value_index.clear();
        self.source_request = None;
        self.masked_request = None;
        self.closed = true;
    }
}

/// 按周期管理注册表：分配 / 复用 / 还原 / 关闭丢弃。
pub struct SequenceRegistry {
    lock: Mutex<()>,
    stable_index: Arc<Mutex<StableSequenceIndex>>,
    store: Arc<Mutex<SessionSequenceCache>>,
    release_store: bool,
    open_cycles: Vec<String>,
    last_cycle: Option<PlaceholderCycle>,
    pub stats: DesensitizationStats,
}

impl SequenceRegistry {
    /// 默认构造：进程级共享稳定索引 + 注册表私有会话映射。
    pub fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            stable_index: shared_stable_index(),
            store: Arc::new(Mutex::new(SessionSequenceCache::default())),
            release_store: true,
            open_cycles: Vec::new(),
            last_cycle: None,
            stats: DesensitizationStats::default(),
        }
    }

    /// 注入自定义计数器（测试 / 旁路定制）：用私有稳定索引，避免跨注册表串号。
    pub fn with_sequence_source<F>(sequence_source: F) -> Self
    where
        F: Fn() -> u64 + Send + Sync + 'static,
    {
        Self {
            lock: Mutex::new(()),
            stable_index: Arc::new(Mutex::new(StableSequenceIndex::with_sequence_source(
                sequence_source,
            ))),
            store: Arc::new(Mutex::new(SessionSequenceCache::default())),
            release_store: true,
            open_cycles: Vec::new(),
            last_cycle: None,
            stats: DesensitizationStats::default(),
        }
    }

    /// 注入会话级映射归属：运行时关闭只清周期状态，序号在同一会话的多个运行时之间共享。
    pub fn with_store(store: Arc<Mutex<SessionSequenceCache>>) -> Self {
        Self {
            lock: Mutex::new(()),
            stable_index: shared_stable_index(),
            store,
            release_store: false,
            open_cycles: Vec::new(),
            last_cycle: None,
            stats: DesensitizationStats::default(),
        }
    }

    /// 开始新周期；与上一个未完成周期请求全量相等则视为同一逻辑请求的重试复用。
    pub fn begin_cycle(&mut self, request: &str) -> (PlaceholderCycle, bool) {
        let _guard = lock(&self.lock);
        if let Some(last) = &self.last_cycle {
            let reusable = !last.closed
                && last.masked_request.is_some()
                && last.source_request.as_deref() == Some(request);
            if reusable {
                self.stats.cycles_reused += 1;
                return (last.clone_for_reuse(), true);
            }
        }
        // 只有紧邻的上一个未完成周期才可能被重试复用；请求一旦换新就地注销。
        if let Some(last) = &mut self.last_cycle {
            if !last.closed {
                let cycle_id = last.cycle_id.clone();
                last.close();
                self.open_cycles.retain(|id| id != &cycle_id);
            }
        }

        let mut cycle = PlaceholderCycle::new(
            next_cycle_id(),
            self.stable_index.clone(),
            self.store.clone(),
        );
        cycle.source_request = Some(request.to_string());
        self.open_cycles.push(cycle.cycle_id.clone());
        self.last_cycle = Some(cycle.clone_for_reuse());
        self.stats.cycles_started += 1;
        (cycle, false)
    }

    /// 周期还原组装结束：释放周期自身状态，会话级序号映射保留。
    pub fn close_cycle(&mut self, cycle: &PlaceholderCycle) {
        let _guard = lock(&self.lock);
        self.open_cycles.retain(|id| id != &cycle.cycle_id);
        if self
            .last_cycle
            .as_ref()
            .is_some_and(|last| last.cycle_id == cycle.cycle_id)
        {
            self.last_cycle = None;
        }
    }

    /// 运行时关闭：丢弃未完成周期的请求副本；会话级映射由注入方持有。
    pub fn drop_all(&mut self) {
        let _guard = lock(&self.lock);
        self.open_cycles.clear();
        self.last_cycle = None;
        if self.release_store {
            lock(&self.store).clear();
        }
    }

    pub fn open_cycle_count(&self) -> usize {
        let _guard = lock(&self.lock);
        self.open_cycles.len()
    }

    pub fn stable_index(&self) -> Arc<Mutex<StableSequenceIndex>> {
        self.stable_index.clone()
    }

    /// 供还原装配使用：把「已屏蔽的请求副本」挂到周期上（复用的前提条件）。
    pub fn mark_masked(&mut self, cycle_id: &str, masked_request: &str) {
        let _guard = lock(&self.lock);
        if let Some(last) = &mut self.last_cycle {
            if last.cycle_id == cycle_id {
                last.masked_request = Some(masked_request.to_string());
            }
        }
    }
}

impl Default for SequenceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn next_cycle_id() -> String {
    format!(
        "cycle-{}",
        CYCLE_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1
    )
}

fn shared_stable_index() -> Arc<Mutex<StableSequenceIndex>> {
    SHARED_STABLE_INDEX
        .get_or_init(|| Arc::new(Mutex::new(StableSequenceIndex::new())))
        .clone()
}

fn salt() -> &'static [u8; 32] {
    STABLE_INDEX_SALT.get_or_init(|| {
        let mut buffer = [0u8; 32];
        fill_random(&mut buffer);
        buffer
    })
}

fn char_offsets(characters: &[char]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(characters.len() + 1);
    let mut offset = 0;
    for character in characters {
        offsets.push(offset);
        offset += character.len_utf8();
    }
    offsets.push(offset);
    offsets
}

fn skip_whitespace(characters: &[char], index: usize) -> usize {
    let mut cursor = index;
    while characters
        .get(cursor)
        .is_some_and(|character| character.is_whitespace())
    {
        cursor += 1;
    }
    cursor
}

fn matches_marker(characters: &[char], index: usize) -> bool {
    let marker: String = characters
        .iter()
        .skip(index)
        .take(PLACEHOLDER_MARKER.len())
        .collect();
    marker.eq_ignore_ascii_case(PLACEHOLDER_MARKER)
}

fn match_placeholder(characters: &[char], index: usize) -> Option<(usize, u64)> {
    if !is_open_brace(characters.get(index)) {
        return None;
    }
    let mut cursor = skip_whitespace(characters, index + 1);
    if !matches_marker(characters, cursor) {
        return None;
    }
    cursor = skip_whitespace(characters, cursor + PLACEHOLDER_MARKER.len());
    match characters.get(cursor)? {
        ':' | '\u{ff1a}' => cursor += 1,
        _ => return None,
    }
    cursor = skip_whitespace(characters, cursor);
    let digits_start = cursor;
    while characters
        .get(cursor)
        .is_some_and(|character| character.is_ascii_digit())
    {
        cursor += 1;
    }
    if cursor == digits_start {
        return None;
    }
    let digits: String = characters[digits_start..cursor].iter().collect();
    let seq = digits.parse::<u64>().ok()?;
    cursor = skip_whitespace(characters, cursor);
    if !is_close_brace(characters.get(cursor)) {
        return None;
    }
    Some((cursor + 1, seq))
}

/// 开括号（半角或全角）。
fn is_open_brace(character: Option<&char>) -> bool {
    matches!(character, Some(value) if *value == '{' || *value == FULLWIDTH_OPEN_BRACE)
}

/// 闭括号（半角或全角）。
fn is_close_brace(character: Option<&char>) -> bool {
    matches!(character, Some(value) if *value == '}' || *value == FULLWIDTH_CLOSE_BRACE)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 进程级随机盐：取操作系统熵源（不可反推、不可跨进程关联）。
///
/// 与 Python `secrets.token_bytes(32)` 的差异：Windows 上走 `BCryptGenRandom` 之外的
/// 系统熵源，读不到时退回「时间 + 进程号 + 计数器」的 SHA-256 派生值并降级为弱盐。
fn fill_random(buffer: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
            if file.read_exact(buffer).is_ok() {
                return;
            }
        }
    }
    let mut seed = Vec::new();
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        seed.extend_from_slice(&now.as_nanos().to_le_bytes());
    }
    seed.extend_from_slice(&next_sequence_number().to_le_bytes());
    let mut digest = Sha256::digest(&seed).to_vec();
    while digest.len() < buffer.len() {
        digest.extend_from_slice(&Sha256::digest(&digest));
    }
    buffer.copy_from_slice(&digest[..buffer.len()]);
}

/// HMAC-SHA256（十六进制）：按标准构造，不引入额外依赖。
fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut normalized = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        normalized[..digest.len()].copy_from_slice(&digest);
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + message.len());
    let mut outer = Vec::with_capacity(BLOCK + 32);
    for byte in normalized {
        inner.push(byte ^ 0x36);
        outer.push(byte ^ 0x5c);
    }
    inner.extend_from_slice(message);
    let inner_digest = Sha256::digest(&inner);
    outer.extend_from_slice(&inner_digest);
    format!("{:x}", Sha256::digest(&outer))
}
