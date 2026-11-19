//! 文本选区、思考内容的逐字展开态、联网审批面板的对齐状态。
//!
//! 由 `state.rs` 按职责拆出，语义未变。

use super::*;

/// 会话流的文本选区（鼠标拖选）。
///
/// 行下标是全部显示行（`ui::conversation::display_lines`）的下标，列是显示列宽，
/// 因此滚动窗口、软折行与宽字符（CJK）都不会错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    /// 按下左键时的位置。
    pub anchor: (usize, usize),
    /// 当前拖到的位置（与 `anchor` 构成选区两端）。
    pub head: (usize, usize),
}

impl TextSelection {
    /// 归一化后的（起点, 终点）：阅读顺序，起点永远不在终点之后。
    pub fn normalized(self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// 只按了一下没拖动（空选区）时为真。
    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    /// 选区跨的显示行数（单行选区为 1）。
    pub fn rows(self) -> usize {
        let ((start, _), (end, _)) = self.normalized();
        end.saturating_sub(start) + 1
    }
}

/// 思考段的逐帧显现游标。
///
/// 模型分片是**突发到达**的：网络成批投递、一次 `drain_frames` 里可能应用好几片，
/// 若每片都立刻整段重绘，画面就会「一次蹦出好几行」。这里只记「已显现到第几个字符」，
/// 渲染时按它截断，每帧推进一点，内容于是连续铺开。
///
/// 截断只发生在渲染层：记录本身仍是完整思考，复制、会话投影与收口后的全文都不受影响。
/// `index` 为 `None`（缺省）表示没有段在显现——历史回放、恢复等路径直接按全文显示。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningReveal {
    /// 正在显现的思考记录下标。
    pub(super) index: Option<usize>,
    /// 已显现的字符数。
    pub(super) revealed: usize,
    /// 当前已累积到的字符数。
    pub(super) target: usize,
    /// 是否仍逐帧揭示。
    ///
    /// 用户主动展开/折叠、回合收口、回滚之后置为 `false`：这条记录**以后一律全文显示**。
    /// `index` 必须保留——后续增量到达时要靠它认出「还是同一条记录」；若连 index 一起清掉，
    /// 下一片会走「首片到达」分支把 `revealed` 归零，整段思考就会从头重新铺一遍。
    pub(super) tracking: bool,
}

impl ReasoningReveal {
    /// 某条思考记录开始流式显现（首片到达）：从 0 起铺开。
    pub(crate) fn start(&mut self, index: usize) {
        self.index = Some(index);
        self.revealed = 0;
        self.target = 0;
        self.tracking = true;
    }

    /// 该记录又长了：按本次增量推进目标长度，不对整段重数。
    pub(crate) fn extend(&mut self, index: usize, appended: usize) {
        if self.index != Some(index) {
            return;
        }
        self.target += appended;
    }

    /// 退出逐帧揭示：这条记录此后一律全文显示。
    ///
    /// **不清 `index`**：它标记「哪一条在揭示」。清掉之后同一段思考的下一次增量会被当成
    /// 首片，从 0 重新铺一遍（展开/折叠时表现为整段重放）。没有段在揭示时仍是彻底复位。
    pub(super) fn settle(&mut self) {
        match self.index {
            Some(_) => {
                self.tracking = false;
                self.revealed = self.target;
            }
            None => *self = Self::default(),
        }
    }

    /// 彻底复位（记录被清空、回滚到不存在的下标等）。
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// 推进一帧；返回是否还有没铺完的内容（事件循环据此决定要不要继续按帧重画）。
    pub(super) fn advance(&mut self) -> bool {
        if !self.tracking || self.index.is_none() || self.revealed >= self.target {
            return false;
        }
        let backlog = self.target - self.revealed;
        let step = (backlog / REASONING_REVEAL_CATCHUP_DIVISOR)
            .clamp(REASONING_REVEAL_MIN_CHARS, REASONING_REVEAL_MAX_CHARS);
        self.revealed = (self.revealed + step).min(self.target);
        self.revealed < self.target
    }

    /// 该记录本次渲染可见的字符数。
    pub(crate) fn visible_chars(&self, index: usize, total: usize) -> usize {
        match self.index {
            Some(active) if active == index && self.tracking => self.revealed.min(total),
            _ => total,
        }
    }
}

/// 一条待决的沙盒网络审批（渲染与按键都只看这份快照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetApprovalPanel {
    /// 审批站里的编号：回答时要带回去配对。
    pub id: u64,
    /// 展示用的目标：`https://example.com:443`。
    pub target: String,
    /// 剩余秒数：到点由代理线程自己按超时拒绝。
    pub remaining_seconds: u64,
}
