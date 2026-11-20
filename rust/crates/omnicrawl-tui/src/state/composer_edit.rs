//! 输入框的撤回栈与已发送消息历史：`Ctrl+Z` / `Ctrl+Y` 的粒度与上下键回看。
//!
//! 由 `composer.rs` 按职责拆出，语义未变。

use super::composer::{push_bounded, ComposerSnapshot, EditRun};
use super::*;

impl Composer {
    /// 一段连续打字之前压一次撤回点：同一段连打只压第一次，撤回一次退掉整段。
    pub(super) fn begin_typing(&mut self) {
        if self.run == EditRun::Types {
            return;
        }
        self.run = EditRun::Types;
        self.push_undo();
    }

    /// 一次连续删除（退格/删除）之前压一次撤回点；与打字段互不相通。
    pub(super) fn begin_deleting(&mut self) {
        if self.run == EditRun::Deletes {
            return;
        }
        self.run = EditRun::Deletes;
        self.push_undo();
    }

    /// 一次独立改动（粘贴、清空、替换全文、撤回排队消息）自成一组的边界。
    pub(super) fn begin_single_edit(&mut self) {
        self.run = EditRun::None;
        self.push_undo();
    }

    /// 这段编辑已结束（移动光标、点选之后的下一次改动重新压撤回点）。
    pub(super) fn end_run(&mut self) {
        self.run = EditRun::None;
    }

    /// 当前状态压进撤回栈；与栈顶相同则不重复压（清空空输入这类空操作不该占一步）。
    ///
    /// 压新撤回点即作废复原栈：改了东西之后，先前撤回掉的那条分支已经不存在了。
    pub(super) fn push_undo(&mut self) {
        let snapshot = self.snapshot();
        self.redo.clear();
        if self.undo.back() == Some(&snapshot) {
            return;
        }
        push_bounded(&mut self.undo, snapshot);
    }

    pub(super) fn snapshot(&self) -> ComposerSnapshot {
        ComposerSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            pastes: self.pastes.clone(),
            images: self.images.clone(),
            documents: self.documents.clone(),
            paste_sequence: self.paste_sequence,
            image_sequence: self.image_sequence,
            document_sequence: self.document_sequence,
        }
    }

    /// 把快照装回输入框：窗口回到跟随光标，菜单按新文本重筛。
    pub(super) fn restore(&mut self, snapshot: ComposerSnapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor;
        self.pastes = snapshot.pastes;
        self.images = snapshot.images;
        self.documents = snapshot.documents;
        self.paste_sequence = snapshot.paste_sequence;
        self.image_sequence = snapshot.image_sequence;
        self.document_sequence = snapshot.document_sequence;
        self.exit_browse_mode();
        self.refresh_menu();
    }

    /// 记录一条已发送消息供上下键回看；与最近一条重复时不重复入列。
    ///
    /// 提交即退出浏览态：无论是否新入列，下一条上键都从最新一条开始。
    pub fn history_record(&mut self, text: &str) {
        // 先退出浏览态再裁历史：浏览中的 `history_index` 指向旧下标，裁剪会让它错位。
        self.exit_browse_mode();
        if !text.is_empty() && self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
            // 只留最近 [`COMPOSER_HISTORY_LIMIT`] 条：更早的既翻不到，也没有保留价值。
            if self.history.len() > COMPOSER_HISTORY_LIMIT {
                let overflow = self.history.len() - COMPOSER_HISTORY_LIMIT;
                self.history.drain(..overflow);
            }
        }
    }

    /// 是否正在浏览已发送消息。
    pub fn is_browsing_history(&self) -> bool {
        self.history_index >= 0
    }

    /// 退出历史浏览态（用户手动编辑输入框、或输入框被程序化重写时调用）。
    pub fn exit_browse_mode(&mut self) {
        self.history_index = -1;
        self.history_draft.clear();
    }

    /// 按上下键浏览已发送消息；返回 `true` 表示按键已被历史浏览消费。
    ///
    /// `direction` 为 -1（上键，向更早翻）或 1（下键，向更新翻）。进入浏览前先把
    /// 输入框当前内容存成草稿；下键翻过最新一条之后再按一次即恢复草稿并退出浏览，
    /// 与 bash readline 一致。没有历史、或下键时本就不在浏览态时返回 `false`，
    /// 让调用方回退到会话滚动。
    pub fn navigate_history(&mut self, direction: isize) -> bool {
        if self.history.is_empty() {
            return false;
        }
        if direction < 0 {
            if self.history_index < 0 {
                self.history_draft = self.text.clone();
                self.history_index = self.history.len() as isize - 1;
            } else if self.history_index > 0 {
                self.history_index -= 1;
            } else {
                // 已在最早一条：消费按键但不改变内容。
                return true;
            }
        } else {
            if self.history_index < 0 {
                return false;
            }
            if self.history_index >= self.history.len() as isize - 1 {
                let draft = std::mem::take(&mut self.history_draft);
                self.history_index = -1;
                self.replace_history_text(&draft);
                return true;
            }
            self.history_index += 1;
        }
        let text = self.history[self.history_index as usize].clone();
        self.replace_history_text(&text);
        true
    }

    /// 浏览历史时替换全文并把光标移到末尾；此时**不刷新命令菜单**。
    ///
    /// 否则以 `/` 开头的历史条目会弹出候选菜单，接下来的上下键被菜单抢走、
    /// 连续浏览被打断（对映 Python `_replace_history_text` + `_hide_command_menu`）。
    pub(super) fn replace_history_text(&mut self, text: &str) {
        // 翻历史也是一次可撤回的改动：否则浏览到某条之后按下一次编辑，
        // 撤回点还停在浏览之前，Ctrl+Z 会把刚看到的那一整条一起抹掉。
        self.begin_single_edit();
        self.text = text.to_string();
        self.cursor = self.text.chars().count();
        self.follow_caret();
        self.prune_pastes();
        self.menu.hide();
    }
}
