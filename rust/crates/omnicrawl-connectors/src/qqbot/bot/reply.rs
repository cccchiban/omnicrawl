//! 被动回复句柄与发消息额度。
//!
//! QQ 每条入站消息只允许 5 条被动回复（`msg_seq` 递增），超了要转主动消息并受分钟限流。
//! 这套「额度硬性递减 + 被动失效降级」的口径集中在 [`ReplyHandle`] 与 [`ActiveBudget`]，
//! 与网关循环、任务队列无关，因此独立成模块。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::qqbot::api::{QqApi, Target};
use crate::qqbot::media::MediaUpload;
// 切段口径与常量定义同源。
use super::MAX_CONTENT_CHARS;
use crate::qqbot::api::PASSIVE_UNAVAILABLE_CODES;
use crate::qqbot::media;
use crate::qqbot::markdown;

/// 主动消息的分钟额度：按未认证 Bot 的保守口径（认证后是 60 qpm）。
pub const MAX_ACTIVE_PER_MINUTE: usize = 30;

/// 主动消息的滑动窗口限流器：所有会话共用一份。
///
/// 平台按 Bot 维度计主动消息频率（未认证 30 qpm），并发任务各自降级时很容易一起打满，
/// 所以额度必须集中记账；单关系 20 qpm 与每天 1000 条由这个上限间接兜住。
pub struct ActiveBudget {
    limit: usize,
    window: Duration,
    sent: Mutex<VecDeque<Instant>>,
}

impl ActiveBudget {
    pub fn new(limit: usize, window: Duration) -> ActiveBudget {
        ActiveBudget {
            limit,
            window,
            sent: Mutex::new(VecDeque::new()),
        }
    }

    /// 按未认证 Bot 的保守口径：每分钟 [`MAX_ACTIVE_PER_MINUTE`] 条。
    pub fn per_minute() -> ActiveBudget {
        ActiveBudget::new(MAX_ACTIVE_PER_MINUTE, Duration::from_secs(60))
    }

    /// 取一个额度：窗口内还有余量就记账并返回 true。
    fn take(&self) -> bool {
        let mut sent = self.sent.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        while let Some(front) = sent.front() {
            if now.duration_since(*front) >= self.window {
                sent.pop_front();
            } else {
                break;
            }
        }
        if sent.len() >= self.limit {
            return false;
        }
        sent.push_back(now);
        true
    }
}

/// 平台下发入口：发消息、上传并发送富媒体、下载附件。
///
/// 真实实现是 [`QqApi`]，测试用记录型桩件替换。
pub trait QqPort: Send + Sync {
    fn send(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, String>;

    /// 发一条 Markdown（`msg_type=2`）。
    fn send_markdown(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, String>;

    /// 上传一个文件并作为富媒体（`msg_type=7`）发出去。
    fn send_media(
        &self,
        target: &Target,
        upload: &MediaUpload,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, String>;

    fn download(&self, url: &str) -> Result<Vec<u8>, String>;
}

impl QqPort for QqApi {
    fn send(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, String> {
        self.send_text(target, content, msg_id, msg_seq, reference)
            .map_err(|error| error.to_string())
    }

    fn send_markdown(
        &self,
        target: &Target,
        content: &str,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, String> {
        QqApi::send_markdown(self, target, content, msg_id, msg_seq, reference)
            .map_err(|error| error.to_string())
    }

    fn send_media(
        &self,
        target: &Target,
        upload: &MediaUpload,
        msg_id: &str,
        msg_seq: i64,
        reference: &str,
    ) -> Result<String, String> {
        let file_info = self
            .upload_media(target, upload.file_type(), &upload.file_name, &upload.bytes)
            .map_err(|error| error.to_string())?;
        QqApi::send_media(self, target, &file_info, msg_id, msg_seq, reference)
            .map_err(|error| error.to_string())
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        QqApi::download(self, url).map_err(|error| error.to_string())
    }
}

/// 一条入站消息的被动回复句柄：`msg_seq` 递增、额度硬性递减。
///
/// 额度口径按场景取（[`Target::max_replies`]）：群聊与单聊都是 5 条；其中**中间过程最多
/// 占总额度减 1 条**（[`ReplyHandle::send_intermediate`]），最后 1 条留给最终回答
/// （[`ReplyHandle::send_final`]）。中间额度用尽后 [`ReplyHandle::can_send_intermediate`]
/// 返回 false，调用方必须继续攒内容、直到最终文本才下发。
pub struct ReplyHandle {
    port: Arc<dyn QqPort>,
    pub target: Target,
    /// 触发本次回复的入站消息 ID。
    pub msg_id: String,
    /// 最终回答要挂的引用（`message_scene.ext` 的 `msg_idx`）：过程消息默认不带引用。
    reference: String,
    /// 是否让**过程消息也带引用**。
    ///
    /// 排队场景必须开：同一会话里可能同时堆着几条提问，后台任务先后跑完，过程消息若不引用，
    /// 用户分不清这条进度属于谁的哪一句。即时执行（没有排队）时保持关闭，过程消息照旧干净。
    ///
    /// 用原子量而不是 `bool`：句柄以 `Arc` 形式在队列里传递，入队时只能就地改这一位。
    quote_intermediate: AtomicBool,
    seq: AtomicI64,
    slots_left: AtomicUsize,
    intermediate_left: AtomicUsize,
    /// 被动不可用时改发主动消息的额度；`None` 表示不降级。
    active_budget: Option<Arc<ActiveBudget>>,
    /// 被动通道已判定不可用（`msg_id` 过期或次数超限）：后续下发直接走主动。
    passive_dead: AtomicBool,
    /// Agent 工作区根：`[FILE:相对路径]` 按它解析，而不是连接器进程的当前目录。
    workspace_root: Option<PathBuf>,
}

impl ReplyHandle {
    pub fn new(port: Arc<dyn QqPort>, target: Target, msg_id: &str) -> ReplyHandle {
        let total = target.max_replies();
        ReplyHandle {
            port,
            target,
            msg_id: msg_id.to_string(),
            reference: String::new(),
            quote_intermediate: AtomicBool::new(false),
            seq: AtomicI64::new(1),
            slots_left: AtomicUsize::new(total),
            intermediate_left: AtomicUsize::new(total - 1),
            active_budget: None,
            passive_dead: AtomicBool::new(false),
            workspace_root: None,
        }
    }

    /// 指定 Agent 工作区根：`[FILE:相对路径]` 按它解析。
    pub fn with_workspace_root(mut self, root: PathBuf) -> ReplyHandle {
        self.workspace_root = Some(root);
        self
    }

    /// 允许被动回复不可用时降级成主动消息，额度与所有会话共用。
    pub fn with_active_budget(mut self, budget: Arc<ActiveBudget>) -> ReplyHandle {
        self.active_budget = Some(budget);
        self
    }

    /// 让最终回答以引用形式挂到用户那条消息上（过程消息保持干净）。
    pub fn with_reference(mut self, reference: &str) -> ReplyHandle {
        self.reference = reference.to_string();
        self
    }

    /// 让过程消息也带上引用：排队执行时用，避免几条提问的过程消息混在一起。
    pub fn with_intermediate_quote(self, quote: bool) -> ReplyHandle {
        self.quote_intermediate.store(quote, Ordering::SeqCst);
        self
    }

    /// 就地打开过程消息的引用（句柄已在 `Arc` 里、无法再走链式构造时用）。
    pub fn quote_intermediate_in_place(&self, quote: bool) {
        self.quote_intermediate.store(quote, Ordering::SeqCst);
    }

    /// 过程消息是否需要引用。
    fn quotes_intermediate(&self) -> bool {
        self.quote_intermediate.load(Ordering::SeqCst) && !self.reference.is_empty()
    }

    /// 剩余总额度。
    pub fn slots_left(&self) -> usize {
        self.slots_left.load(Ordering::SeqCst)
    }

    /// 剩余中间额度（已含为最终回答预留的一条之外）。
    pub fn intermediate_left(&self) -> usize {
        self.intermediate_left.load(Ordering::SeqCst)
    }

    /// 还能否发中间消息：中间额度与总额度都还有余量。
    pub fn can_send_intermediate(&self) -> bool {
        self.intermediate_left() > 0 && self.slots_left() > 1
    }

    /// 发一段中间内容（正文段、进度、思考）：占中间额度。
    ///
    /// 返回实际发出的条数；`0` 表示额度已用尽或平台报错。内容里的 Markdown 语法走
    /// `msg_type=2`，`[FILE:…]` 标记转成富媒体。排队执行时同一段会挂上对用户那条消息的引用
    /// （见 [`ReplyHandle::with_intermediate_quote`]），即时执行时不挂。
    pub fn send_intermediate(&self, text: &str) -> usize {
        self.send_parts(text, true, self.quotes_intermediate())
    }

    /// 发最终回答：只受总额度约束（最多两段，末段写满即止），并挂上对用户那条消息的引用。
    pub fn send_final(&self, text: &str) -> usize {
        self.send_parts(text, false, true)
    }

    /// 发过程提示（排队回执、附件通知等）：额度口径同最终回答，默认不挂引用；
    /// 排队执行时与过程消息一致地挂上引用（[`ReplyHandle::quotes_intermediate`]）。
    pub fn send_notice(&self, text: &str) -> usize {
        self.send_parts(text, false, self.quotes_intermediate())
    }

    /// 按上限切段并逐段下发（富媒体标记先摘出来单独发）。
    ///
    /// 每发一条都在下发前预扣额度（[`ReplyHandle::reserve`]），这样一次调用切成多段
    /// 也不会越过中间过程的上限。
    fn send_parts(&self, text: &str, intermediate: bool, quote: bool) -> usize {
        let mut sent = 0_usize;
        let stripped: String;
        let mut text = text;
        // 引用只挂在本次下发第一条成功的消息上：后面的段跟着首段的上下文就够了。
        let mut referenced = false;
        if media::supports(&self.target) {
            let mut any_sent = false;
            let mut all_sent = true;
            for path in media::marker_paths(text) {
                match media::load(&path, self.workspace_root.as_deref()) {
                    Ok(upload) => {
                        let reference = if quote && !referenced {
                            self.reference.as_str()
                        } else {
                            ""
                        };
                        if self.deliver_media(&upload, intermediate, reference) {
                            any_sent = true;
                            sent += 1;
                            if quote {
                                referenced = true;
                            }
                        } else {
                            all_sent = false;
                        }
                    }
                    Err(error) => {
                        // 发不出去时保留文本路径，用户至少知道文件在哪。
                        eprintln!("[qqbot] 富媒体未发送：{error}");
                        all_sent = false;
                    }
                }
            }
            // 全部发成功才从正文里摘掉标记，否则保留路径给用户看。
            if any_sent && all_sent {
                stripped = media::strip_markers(text);
                text = &stripped;
            }
        }
        let parts = split_content(text);
        for (index, part) in parts.iter().enumerate() {
            match self.deliver_text(part, intermediate, quote && !referenced) {
                Some(true) => {
                    if quote {
                        referenced = true;
                    }
                    sent += 1;
                }
                Some(false) => break,
                None => {
                    eprintln!(
                        "[qqbot] 回复额度已用尽，跳过后续 {} 段内容。",
                        parts.len() - index
                    );
                    break;
                }
            }
        }
        sent
    }

    /// 预扣一次下发额度：中间过程同时扣中间额度与总额度，最终回答只扣总额度。
    fn reserve(&self, intermediate: bool) -> bool {
        if intermediate {
            loop {
                let left = self.intermediate_left.load(Ordering::SeqCst);
                if left == 0 {
                    return false;
                }
                if self
                    .intermediate_left
                    .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    break;
                }
            }
        }
        loop {
            let left = self.slots_left.load(Ordering::SeqCst);
            if left == 0 {
                if intermediate {
                    // 总额度先耗尽：把刚扣的中间额度还回去。
                    self.intermediate_left.fetch_add(1, Ordering::SeqCst);
                }
                return false;
            }
            if self
                .slots_left
                .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// 本次下发是否已被平台判死：被动通道失效后改走主动消息。
    fn passive_dead(&self) -> bool {
        self.passive_dead.load(Ordering::SeqCst)
    }

    /// 下发用的消息 ID：主动消息不带 `msg_id`。
    fn delivery_id(&self, active: bool) -> &str {
        if active {
            ""
        } else {
            self.msg_id.as_str()
        }
    }

    /// 取一份主动消息额度；没有配置降级时恒为 false。
    fn take_active(&self) -> bool {
        match &self.active_budget {
            Some(budget) => budget.take(),
            None => false,
        }
    }

    /// 平台是否明确告知这条 `msg_id` 不能再回（过期或次数超限）。
    fn passive_unavailable(error: &str) -> bool {
        PASSIVE_UNAVAILABLE_CODES
            .iter()
            .any(|code| error.contains(code))
    }

    /// 本次下发的放行与形态：`Some(false)` 被动、`Some(true)` 主动、`None` 丢弃。
    ///
    /// 被动优先；被动额度用尽或已判死时，只有最终回答/通知降级成主动消息，
    /// 过程消息照旧丢弃，免得进度把每分钟额度耗光。
    fn admit(&self, intermediate: bool) -> Option<bool> {
        if !self.passive_dead() && self.reserve(intermediate) {
            return Some(false);
        }
        if intermediate {
            return None;
        }
        self.take_active().then_some(true)
    }

    /// 纯文本还是 Markdown：单聊/群聊且正文含平台支持的语法时走 `markdown.content`。
    ///
    /// `referenced` 表示这一段要挂引用（只有最终回答的首段会带）。
    /// 返回 `None` 表示额度已用尽，`Some(false)` 表示平台报错或额度在重试时耗尽。
    fn deliver_text(&self, text: &str, intermediate: bool, referenced: bool) -> Option<bool> {
        let mut active = match self.admit(intermediate) {
            Some(active) => active,
            None => return None,
        };
        let reference = if referenced {
            self.reference.as_str()
        } else {
            ""
        };
        let markdown = if markdown::supports(&self.target) {
            markdown::to_markdown(text)
        } else {
            None
        };
        let seq = self.next_seq();
        match markdown {
            Some(body) => match self.port.send_markdown(
                &self.target,
                &body,
                self.delivery_id(active),
                seq,
                reference,
            ) {
                Ok(_id) => Some(true),
                Err(error) => {
                    // Markdown 被拒（无权限、内容违规等）时按纯文本再试一次，正文不能丢。
                    // 重试同样要占额度：被动再扣一条，被动已不可用则改扣主动额度。
                    eprintln!("[qqbot] Markdown 消息发送失败：{error}");
                    if !active && Self::passive_unavailable(&error) {
                        self.passive_dead.store(true, Ordering::SeqCst);
                        active = true;
                    }
                    let admitted = if active {
                        self.take_active()
                    } else {
                        self.reserve(intermediate)
                    };
                    if !admitted {
                        return Some(false);
                    }
                    Some(self.send_plain(text, reference, active))
                }
            },
            None => Some(self.send_plain(text, reference, active)),
        }
    }

    fn send_plain(&self, text: &str, reference: &str, active: bool) -> bool {
        let seq = self.next_seq();
        match self
            .port
            .send(&self.target, text, self.delivery_id(active), seq, reference)
        {
            Ok(_id) => true,
            Err(error) => {
                eprintln!("[qqbot] 发送消息失败：{error}");
                // 被动过期/超限：同一条改用主动消息补发一次，别把回答丢掉。
                if !active && Self::passive_unavailable(&error) && self.active_budget.is_some() {
                    self.passive_dead.store(true, Ordering::SeqCst);
                    if self.take_active() {
                        let seq = self.next_seq();
                        return match self.port.send(&self.target, text, "", seq, reference) {
                            Ok(_id) => true,
                            Err(error) => {
                                eprintln!("[qqbot] 主动消息补发失败：{error}");
                                false
                            }
                        };
                    }
                }
                false
            }
        }
    }

    fn deliver_media(&self, upload: &MediaUpload, intermediate: bool, reference: &str) -> bool {
        let active = match self.admit(intermediate) {
            Some(active) => active,
            None => return false,
        };
        let seq = self.next_seq();
        match self.port.send_media(
            &self.target,
            upload,
            self.delivery_id(active),
            seq,
            reference,
        ) {
            Ok(_id) => true,
            Err(error) => {
                eprintln!("[qqbot] 富媒体发送失败（{}）：{error}", upload.file_name);
                // 被动过期/超限：同一条改用主动消息补发一次。
                if !active && Self::passive_unavailable(&error) && self.active_budget.is_some() {
                    self.passive_dead.store(true, Ordering::SeqCst);
                    if self.take_active() {
                        let seq = self.next_seq();
                        return match self
                            .port
                            .send_media(&self.target, upload, "", seq, reference)
                        {
                            Ok(_id) => true,
                            Err(error) => {
                                eprintln!("[qqbot] 富媒体主动补发失败：{error}");
                                false
                            }
                        };
                    }
                }
                false
            }
        }
    }

    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }
}


/// 按上限切段：优先按行边界切，单行超长硬切。
pub fn split_content(text: &str) -> Vec<String> {
    let trimmed = text.trim_end();
    let text = if trimmed.is_empty() { text } else { trimmed };
    if text.is_empty() {
        return Vec::new();
    }
    let count = |value: &str| value.chars().count();
    if count(text) <= MAX_CONTENT_CHARS {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        let separator = usize::from(!current.is_empty());
        if count(&current) + separator + count(line) > MAX_CONTENT_CHARS && !current.is_empty() {
            parts.push(current.clone());
            current.clear();
        }
        if current.is_empty() {
            current.push_str(line);
        } else {
            current.push('\n');
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    let mut final_parts: Vec<String> = Vec::new();
    for part in parts {
        let mut rest: Vec<char> = part.chars().collect();
        while rest.len() > MAX_CONTENT_CHARS {
            final_parts.push(rest[..MAX_CONTENT_CHARS].iter().collect());
            rest = rest[MAX_CONTENT_CHARS..].to_vec();
        }
        if !rest.is_empty() {
            final_parts.push(rest.into_iter().collect());
        }
    }
    final_parts
}
