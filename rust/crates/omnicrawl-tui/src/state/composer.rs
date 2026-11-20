//! 输入框本体：文本/历史/粘贴占位/图片与文档占位/光标与换行滚动。
//!
//! 由 `state.rs` 按职责拆出，语义未变。

use std::sync::Arc;

use super::*;

/// 输入框可见窗口里的一行：文本 + 它在全文里的**字符区间** `[start, end)`。
///
/// 区间用的是字符下标（与 [`Composer::cursor`] 同一套），渲染鼠标选区高亮时按它把
/// 选区换算到行内位置。行与行之间的换行符不属于任何一行，所以上一行的 `end` 不等于
/// 下一行的 `start`——换算时必须按区间来，不能用行长度累加。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerRow {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// 单行起步、按显示宽度软折行的输入框；最多显示 [`COMPOSER_MAX_LINES`] 行。
///
/// 输入框自己带着斜杠命令菜单与候选表：菜单跟着文本变化刷新（对映 Textual 的
/// `on_text_area_changed` → `_refresh_command_menu`），因此不必在每个改动点手动
/// 同步——漏掉一处就会让菜单与实际输入脱节。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composer {
    pub(super) text: String,
    pub(super) cursor: usize,
    /// 鼠标在输入框里拖出来的选区：两个**字符下标**（锚点、拖动端），不保证大小顺序。
    ///
    /// 只在鼠标按下到松手之间活着：松手即复制并清掉（同消息区拖选），文本一改也作废——
    /// 旧下标在改动后会指向别的字符，留着高亮就会出现「选中的和复制出去的不一致」。
    pub(super) selection: Option<(usize, usize)>,
    pub(super) commands: Vec<CommandOption>,
    pub(super) menu: CommandMenu,
    /// 粘贴折叠：占位符 → 原始文本（对映 Python `_compact_pastes`）。
    /// 提交时按占位符还原，删除时整块删掉。
    pub(super) pastes: Vec<(String, String)>,
    pub(super) paste_sequence: usize,
    /// 粘贴的图片：占位符 → 图片附件（Ctrl+V 贴进来的剪贴板位图）。
    ///
    /// 与 `pastes` 同一套占位符机制：`[ #1 Image ]` 在输入框里是一整块，
    /// 删除/左右移动都按块处理，提交时按顺序抽出图片随 prompt 一起送出。
    pub(super) images: Vec<(String, PendingImage)>,
    pub(super) image_sequence: usize,
    /// 粘贴的文档：占位符 → 文档（`[ #1 .pdf ]`）。
    ///
    /// 同样是一整块；提交时占位符展开成**文件名**，完整路径另挂在 user 消息下方。
    pub(super) documents: Vec<(String, PendingDocument)>,
    pub(super) document_sequence: usize,
    /// 已发送消息的历史，供上下键回看（对映 Python `Composer._history`）。
    pub(super) history: Vec<String>,
    /// -1 表示输入框里是用户自己的草稿；>= 0 表示正在浏览第 N 条历史。
    pub(super) history_index: isize,
    /// 进入历史浏览前输入框里的内容，翻到最新一条之后再按一次下键即恢复。
    pub(super) history_draft: String,
    /// 滚轮翻看时窗口的起点行；`None` 表示窗口跟随光标。
    ///
    /// 只在滚轮滚动输入框时变成 `Some`：一旦编辑文本、移动光标或替换全文就回到跟随光标，
    /// 否则光标会在看不见的地方打字（插入点跑到窗口之外，用户以为按键丢了）。
    pub(super) scroll: Option<usize>,
    /// 撤回栈：每次「一组编辑」之前压入当时的全文与三类块表。
    pub(super) undo: VecDeque<ComposerSnapshot>,
    /// 复原栈：撤回时把撤回前的状态压进来，复原时再交换回去。
    pub(super) redo: VecDeque<ComposerSnapshot>,
    /// 当前连续编辑的合并口径：同一段打字或退格只压一次快照。
    pub(super) run: EditRun,
}

/// 连续编辑的合并口径（`Ctrl+Z` 的粒度）。
///
/// 逐字符都压快照的话，「撤回」就退一个字母，敲一句话得按几十次；这里把**一段连续输入**
/// 或**一次连续删除**合成一组，撤回一次退掉整组。换一处改、粘贴、清空都自成一组的边界
/// （[`Composer::begin_single_edit`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum EditRun {
    #[default]
    None,
    Types,
    Deletes,
}

/// 撤回栈最多保留的组数：更早的既按不回来，也没有保留价值（与历史条数同一套想法）。
pub(super) const COMPOSER_UNDO_LIMIT: usize = 100;

/// 把快照压进栈顶，超出上限时从最早的一端丢掉。
pub(super) fn push_bounded(stack: &mut VecDeque<ComposerSnapshot>, snapshot: ComposerSnapshot) {
    stack.push_back(snapshot);
    if stack.len() > COMPOSER_UNDO_LIMIT {
        stack.pop_front();
    }
}

/// 输入框的一个可撤回快照：全文、插入点与三类块表（粘贴折叠、图片、文档）。
///
/// 序号也一起存：撤回后接着粘贴的块编号要接在撤回前的位置上，否则会出现
/// 「`[ #1 Image ]` 有两块、文本里却各指一份」的错位。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ComposerSnapshot {
    text: String,
    cursor: usize,
    pastes: Vec<(String, String)>,
    images: Vec<(String, PendingImage)>,
    documents: Vec<(String, PendingDocument)>,
    paste_sequence: usize,
    image_sequence: usize,
    document_sequence: usize,
}

/// 终端粘贴常带 CRLF/CR，先统一成 LF（对映 Python `_normalize_pasted_text`）。
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 按编辑器语义数行数：末尾空行也算一行（对映 Python `_count_paste_lines`）。
fn paste_line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.matches('\n').count() + 1
}

/// 占位符文本（对映 Python `[粘贴 #{n} +{lines} 行]`）。
fn paste_placeholder(sequence: usize, lines: usize) -> String {
    format!("[粘贴 #{sequence} +{lines} 行]")
}

/// 粘贴进输入框、尚未随 prompt 发出的图片（`Ctrl+V` 贴进来的剪贴板位图，或粘贴进来的图片路径）。
///
/// `placeholder` 是它在输入框文本里的锚点（`[ #1 Image ]`）：撤回排队消息时据此把附件
/// 挂回同一块，不靠重新编号或位置猜测。只留原始字节：Base64 编码在提交时一次性做
/// （输入框按住不动时不必先膨胀 33%）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingImage {
    pub placeholder: String,
    pub media_type: String,
    /// 位图字节。撤回栈每存一次快照就引用同一份字节，不复制（一张图可能几 MB）。
    pub bytes: Arc<Vec<u8>>,
    /// 发送前要不要把字节复制一份到工作区临时目录。
    ///
    /// 只有 `Ctrl+V` 的剪贴板位图需要（磁盘上原本没有这份图，模型事后想再读就没有了）；
    /// 粘贴进来的图片路径本来就指向磁盘上的原文件，不必再复制。
    pub archive: bool,
}

/// 一条「已从输入框交出、尚未发往内核」的提问：文本 + 随行的图片与文档。
///
/// 生成期间进来的提问先进 FIFO 队列，回合结束后按顺序提交；图片与文档必须与文本同行，
/// 否则排队等一轮就会把它们丢掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSubmission {
    pub text: String,
    pub images: Vec<PendingImage>,
    pub documents: Vec<PendingDocument>,
}

/// 粘贴图片的占位符（对映用户要求的 `[ #1 Image ]`）。
///
/// `#` 后是图片序号（粘贴多张时递增），`Image` 是类型名，整个方括号是一块：
/// 删除、左右移动都按块处理，不会停在块中间。
fn image_placeholder(sequence: usize) -> String {
    format!("[ #{sequence} Image ]")
}

/// 粘贴文档的占位符：`[ #1 .pdf ]`——与图片块同形，把类型名换成扩展名。
fn document_placeholder(sequence: usize, extension: &str) -> String {
    format!("[ #{sequence} .{extension} ]")
}

impl Composer {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// 当前全文（集成测试与命令分派核对用）。
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self) {
        self.begin_single_edit();
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.images.clear();
        self.documents.clear();
        self.exit_browse_mode();
        self.refresh_menu();
    }

    /// 用给定文本替换全文，并把光标放到末尾（撤回排队消息、菜单补全时用）。
    pub fn set_text(&mut self, text: &str) {
        self.begin_single_edit();
        self.text = text.to_string();
        self.cursor = self.text.chars().count();
        self.prune_pastes();
        self.exit_browse_mode();
        self.refresh_menu();
    }

    /// 撤回上一步编辑；没有可撤回的内容时返回 `false`。
    ///
    /// 撤回的是**文本与三类块表（粘贴折叠 / 图片 / 文档）的同一份快照**：只退文本会让
    /// 图片块回到输入框却没有附件，提交时那条 `[ #1 Image ]` 就成了空占位。
    pub fn undo(&mut self) -> bool {
        loop {
            let Some(previous) = self.undo.pop_back() else {
                return false;
            };
            let current = self.snapshot();
            if previous == current {
                // 空操作（例如在空输入框上再清一次）占下的那一层直接跳过：
                // 撤回它屏幕上什么都不会变，按一次却白费一步。
                continue;
            }
            push_bounded(&mut self.redo, current);
            self.restore(previous);
            self.run = EditRun::None;
            return true;
        }
    }

    /// 复原刚撤回的那一步；没有可复原的内容时返回 `false`。
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop_back() else {
            return false;
        };
        let current = self.snapshot();
        if current != next {
            push_bounded(&mut self.undo, current);
        }
        self.restore(next);
        self.run = EditRun::None;
        true
    }

    /// 一段连续打字之前压一次撤回点：同一段连打只压第一次，撤回一次退掉整段。
    fn begin_typing(&mut self) {
        if self.run == EditRun::Types {
            return;
        }
        self.run = EditRun::Types;
        self.push_undo();
    }

    /// 一次连续删除（退格/删除）之前压一次撤回点；与打字段互不相通。
    fn begin_deleting(&mut self) {
        if self.run == EditRun::Deletes {
            return;
        }
        self.run = EditRun::Deletes;
        self.push_undo();
    }

    /// 一次独立改动（粘贴、清空、替换全文、撤回排队消息）自成一组的边界。
    fn begin_single_edit(&mut self) {
        self.run = EditRun::None;
        self.push_undo();
    }

    /// 这段编辑已结束（移动光标、点选之后的下一次改动重新压撤回点）。
    fn end_run(&mut self) {
        self.run = EditRun::None;
    }

    /// 当前状态压进撤回栈；与栈顶相同则不重复压（清空空输入这类空操作不该占一步）。
    ///
    /// 压新撤回点即作废复原栈：改了东西之后，先前撤回掉的那条分支已经不存在了。
    fn push_undo(&mut self) {
        let snapshot = self.snapshot();
        self.redo.clear();
        if self.undo.back() == Some(&snapshot) {
            return;
        }
        push_bounded(&mut self.undo, snapshot);
    }

    fn snapshot(&self) -> ComposerSnapshot {
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
    fn restore(&mut self, snapshot: ComposerSnapshot) {
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
    fn replace_history_text(&mut self, text: &str) {
        // 翻历史也是一次可撤回的改动：否则浏览到某条之后按下一次编辑，
        // 撤回点还停在浏览之前，Ctrl+Z 会把刚看到的那一整条一起抹掉。
        self.begin_single_edit();
        self.text = text.to_string();
        self.cursor = self.text.chars().count();
        self.follow_caret();
        self.prune_pastes();
        self.menu.hide();
    }

    /// 取走内容并清空；提交时用。
    ///
    /// 返回的是**展开后**的全文：粘贴折叠块在这里还原成真实内容（对映 Python
    /// 提交前调 `_expand_compact_paste_placeholders` 再清空映射）。
    ///
    /// 图片块不展开成文本（`[ #1 Image ]` 照旧留在文本里），附件由 [`Self::take_images`]
    /// 单独取走；文档块相反——它没有附件通路，模型只能按路径读文件，因此占位符在这里
    /// 展开成**文件名**，完整路径另挂在 user 消息下方（用户选定）。
    ///
    /// 文档表与图片表**不在这里清**：图片附件由 [`Self::take_images`] 取走，文档展开要用
    /// 它自己的表、路径由 [`Self::take_documents`] 取走。调用顺序固定为
    /// `take()` → `take_images()` → `take_documents()`（见 `AppState::submit`），
    /// 三者合起来才算把输入框交空。
    pub fn take(&mut self) -> String {
        let text = self.expanded_text();
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.exit_browse_mode();
        self.refresh_menu();
        // 提交是撤回栈的硬边界：内容已经发出去了，撤回它只会把同一句话连同附件再摆回输入框，
        // 一按回车就重发一遍。
        self.undo.clear();
        self.redo.clear();
        self.run = EditRun::None;
        text
    }

    /// 粘贴入口：多行（> [`PASTE_COMPACT_LINE_THRESHOLD`] 行）折成一个 `[粘贴 #n +N 行]`
    /// 占位符，原文按序号存起来、提交时还原（对映 Python `_compact_paste_if_needed`）。
    ///
    /// 短粘贴（含单行）与原来一样直接插进去：折起来反而更难改。
    pub fn insert_paste(&mut self, text: &str) {
        let normalized = normalize_paste(text);
        let lines = paste_line_count(&normalized);
        self.begin_single_edit();
        if lines <= PASTE_COMPACT_LINE_THRESHOLD {
            self.insert_raw(&normalized);
            return;
        }
        self.paste_sequence += 1;
        let placeholder = paste_placeholder(self.paste_sequence, lines);
        self.pastes.push((placeholder.clone(), normalized));
        self.insert_raw(&placeholder);
    }

    /// 图片粘贴入口：在光标处插入 `[ #n Image ]` 占位符，附件按序号存起来等提交。
    ///
    /// 与文本粘贴折叠同一套占位符语义：整块删除、左右移动不停在块中间；占位符被删掉后
    /// 附件随之丢弃（[`Self::prune_pastes`]），不会被静默发出去。
    ///
    /// 粘贴进来的图片路径走这里：原文件本来就在磁盘上，不必再复制一份。
    pub fn insert_image(&mut self, media_type: &str, bytes: Vec<u8>) {
        self.push_image(media_type, bytes, false);
    }

    /// 剪贴板位图的粘贴入口：与 [`Self::insert_image`] 同形，但发送前会复制一份到工作区
    /// 临时目录（`Ctrl+V` 的图只在内存与内核请求里存在，事后想再读就得有这份副本）。
    pub fn insert_clipboard_image(&mut self, media_type: &str, bytes: Vec<u8>) {
        self.push_image(media_type, bytes, true);
    }

    fn push_image(&mut self, media_type: &str, bytes: Vec<u8>, archive: bool) {
        self.begin_single_edit();
        self.image_sequence += 1;
        let placeholder = image_placeholder(self.image_sequence);
        self.images.push((
            placeholder.clone(),
            PendingImage {
                placeholder: placeholder.clone(),
                media_type: media_type.to_string(),
                bytes: Arc::new(bytes),
                archive,
            },
        ));
        self.insert_raw(&placeholder);
    }

    /// 取走待发图片（按粘贴顺序），并清掉图片块状态。
    ///
    /// 提交路径专用：`take()` 只交出文本，图片得单独取——两者一起清掉才不会漏发。
    pub fn take_images(&mut self) -> Vec<PendingImage> {
        self.images.drain(..).map(|(_, image)| image).collect()
    }

    /// 粘贴文档的入口：在光标处插入 `[ #n .ext ]` 占位符，路径按序号存起来。
    ///
    /// 与图片块同一套语义（整块删除、左右移动不停在块中间）；不同之处是不读文件字节：
    /// 文档只能由模型按路径读，附件里不带内容。
    pub fn insert_document(&mut self, path: &str) {
        self.begin_single_edit();
        self.document_sequence += 1;
        let extension = crate::document_path::PendingDocument::extension_of(path);
        let placeholder = document_placeholder(self.document_sequence, &extension);
        self.documents.push((
            placeholder.clone(),
            PendingDocument {
                placeholder: placeholder.clone(),
                path: path.to_string(),
            },
        ));
        self.insert_raw(&placeholder);
    }

    /// 取走待发文档（按粘贴顺序），并清掉文档块状态。
    pub fn take_documents(&mut self) -> Vec<PendingDocument> {
        self.documents.drain(..).map(|(_, doc)| doc).collect()
    }

    /// 把这些文档按各自的占位符挂回输入框（撤回排队消息时用）。
    pub fn restore_documents(&mut self, documents: Vec<PendingDocument>) {
        for document in documents {
            let placeholder = document.placeholder.clone();
            self.documents.push((placeholder, document));
        }
    }

    /// 把这些图片按各自的占位符挂回输入框（撤回排队消息时用）。
    ///
    /// 占位符已在文本里，因此只重建映射、不重新编号：撤回的那条消息回到输入框后，
    /// 图片块与提交前完全一致。
    pub fn restore_images(&mut self, images: Vec<PendingImage>) {
        for image in images {
            let placeholder = image.placeholder.clone();
            self.images.push((placeholder, image));
        }
    }

    /// 输入框里当前有几张待发图片（提交时判断要不要走图片路径）。
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// 输入框里当前有几个待发文档。
    pub fn document_count(&self) -> usize {
        self.documents.len()
    }

    /// 展开全部占位符；`take()` 与命令分派核对都读它。
    ///
    /// 三类块的处理不同：粘贴文本还原成原文，文档换成文件名，图片块原样留着
    /// （它靠 `take_images` 单独交出附件）。
    pub fn expanded_text(&self) -> String {
        let mut expanded = self.text.clone();
        for (placeholder, original) in &self.pastes {
            expanded = expanded.replace(placeholder.as_str(), original.as_str());
        }
        for (placeholder, document) in &self.documents {
            // 相邻两个文档块之间补一个空格：文件名直接贴在一起会读成一条
            // （`报告 (1).pdf名单.docx`），模型也难分辨这是两个文件。
            expanded = expanded.replace(
                placeholder.as_str(),
                &format!("{} ", document.file_name()),
            );
        }
        expanded
    }

    /// 字符下标 `index` 落在哪个占位符里（左闭右开），没有就是 `None`。
    ///
    /// 粘贴文本块、图片块与文档块共用这套判定：三者在输入框里都是一整块。
    fn placeholder_span(&self, index: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = self.text.chars().collect();
        let placeholders = self
            .pastes
            .iter()
            .map(|(placeholder, _)| placeholder.as_str())
            .chain(
                self.images
                    .iter()
                    .map(|(placeholder, _)| placeholder.as_str()),
            )
            .chain(
                self.documents
                    .iter()
                    .map(|(placeholder, _)| placeholder.as_str()),
            );
        for placeholder in placeholders {
            let needle: Vec<char> = placeholder.chars().collect();
            if needle.is_empty() || needle.len() > chars.len() {
                continue;
            }
            let found = (0..=chars.len() - needle.len())
                .find(|start| chars[*start..*start + needle.len()] == needle[..]);
            if let Some(start) = found {
                if index >= start && index < start + needle.len() {
                    return Some((start, start + needle.len()));
                }
            }
        }
        None
    }

    /// 光标落在占位符里就整块删掉：粘贴块不逐字符删（用户要求，也是与 Python 的差异点）。
    fn remove_placeholder(&mut self, index: usize) -> bool {
        let Some((start, end)) = self.placeholder_span(index) else {
            return false;
        };
        let mut chars: Vec<char> = self.text.chars().collect();
        chars.drain(start..end);
        self.text = chars.into_iter().collect();
        self.cursor = start;
        self.prune_pastes();
        self.refresh_menu();
        true
    }

    /// 丢掉已经不在文本里的占位符，避免原文一直挂着占内存
    /// （对映 Python `_prune_compact_paste_placeholders`）。
    ///
    /// 图片块与文档块同样在这里回收：占位符被删掉后附件就不该再随提交发出去。
    fn prune_pastes(&mut self) {
        if self.pastes.is_empty() && self.images.is_empty() && self.documents.is_empty() {
            return;
        }
        let text = self.text.clone();
        self.pastes
            .retain(|(placeholder, _)| text.contains(placeholder.as_str()));
        self.images
            .retain(|(placeholder, _)| text.contains(placeholder.as_str()));
        self.documents
            .retain(|(placeholder, _)| text.contains(placeholder.as_str()));
    }

    /// 装载命令菜单候选表（统一命令源）；运行期 Skill 上下线时由宿主重装。
    pub fn set_commands(&mut self, commands: Vec<CommandOption>) {
        self.commands = commands;
        self.refresh_menu();
    }

    pub fn commands(&self) -> &[CommandOption] {
        &self.commands
    }

    /// 斜杠命令菜单状态（渲染与高度预算读它）。
    pub fn menu(&self) -> &CommandMenu {
        &self.menu
    }

    /// 把菜单选择键交给菜单；完整命令时返回 [`MenuAction::Passthrough`]（放行提交）。
    pub fn menu_handle_key(&mut self, key: MenuKey) -> MenuAction {
        self.menu.handle_key(key, &self.text)
    }

    /// 按当前文本重新筛选候选（对映 `on_text_area_changed` 的刷新时机）。
    ///
    /// 文本改动都会走到这里（插入、删字、清空、替换全文），所以鼠标选区也在这里作废：
    /// 选区按字符下标记，改动之后旧下标指向别处，留着高亮就会出现「选中的和复制出去的
    /// 不一致」。只动光标的 [`Self::move_left`] 等不刷新菜单，那边单独清。
    fn refresh_menu(&mut self) {
        self.clear_mouse_selection();
        self.follow_caret();
        self.menu.refresh(&self.text, &self.commands);
    }

    /// 作废鼠标选区（见 [`Self::refresh_menu`] 的说明）。
    fn clear_mouse_selection(&mut self) {
        self.selection = None;
    }

    /// 把可见窗口交还给光标：文本一改或光标一动，滚轮翻过的位置就作废，
    /// 否则插入点会停在窗口之外，用户打字看不到自己刚敲的字。
    fn follow_caret(&mut self) {
        self.scroll = None;
    }

    pub fn insert(&mut self, text: &str) {
        self.begin_typing();
        self.insert_raw(text);
    }

    /// 插入文本但不压撤回点：粘贴与块插入已各自开好一组。
    fn insert_raw(&mut self, text: &str) {
        self.exit_browse_mode();
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        let inserted: Vec<char> = normalized.chars().collect();
        let count = inserted.len();
        chars.splice(at..at, inserted);
        self.text = chars.into_iter().collect();
        self.cursor = at + count;
        self.refresh_menu();
    }

    pub fn newline(&mut self) {
        // 换行自成一组的边界：敲完一句按回车，前面那一整句与后面那一行要能分开撤回。
        self.begin_single_edit();
        self.insert_raw("\n");
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.begin_deleting();
        self.exit_browse_mode();
        // 光标左侧落在粘贴折叠块里：整块删掉，而不是逐字符删。
        if self.remove_placeholder(self.cursor - 1) {
            // 一整块一次就删完了：下一段删除是新的撤回点，否则连删两块只能整段退回来。
            self.end_run();
            return;
        }
        let mut chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        chars.remove(at - 1);
        self.text = chars.into_iter().collect();
        self.cursor = at - 1;
        self.prune_pastes();
        self.refresh_menu();
    }

    pub fn delete(&mut self) {
        let mut chars: Vec<char> = self.text.chars().collect();
        if self.cursor >= chars.len() {
            return;
        }
        self.begin_deleting();
        self.exit_browse_mode();
        // 光标右侧落在粘贴折叠块里：整块删掉（含块首与块内）。
        if self.remove_placeholder(self.cursor) {
            self.end_run();
            return;
        }
        chars.remove(self.cursor);
        self.text = chars.into_iter().collect();
        self.prune_pastes();
        self.refresh_menu();
    }

    /// 左右移动把占位符当一格：光标停在块首/块尾，不会停到块中间。
    pub fn move_left(&mut self) {
        self.clear_mouse_selection();
        self.follow_caret();
        self.end_run();
        if self.cursor == 0 {
            return;
        }
        match self.placeholder_span(self.cursor - 1) {
            // 已在块首则跨出块外一格，否则跳到块首。
            Some((start, _)) => {
                self.cursor = if self.cursor > start {
                    start
                } else {
                    start - 1
                }
            }
            None => self.cursor -= 1,
        }
    }

    pub fn move_right(&mut self) {
        self.clear_mouse_selection();
        self.follow_caret();
        self.end_run();
        let length = self.text.chars().count();
        if self.cursor >= length {
            return;
        }
        match self.placeholder_span(self.cursor) {
            Some((_, end)) => self.cursor = end,
            None => self.cursor += 1,
        }
    }

    pub fn move_home(&mut self) {
        self.clear_mouse_selection();
        self.follow_caret();
        self.end_run();
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[..at]
            .iter()
            .rposition(|ch| *ch == '\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        self.clear_mouse_selection();
        self.follow_caret();
        self.end_run();
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[at..]
            .iter()
            .position(|ch| *ch == '\n')
            .map(|index| at + index)
            .unwrap_or(chars.len());
    }

    /// 鼠标选区（**归一化**后的字符区间）；空选区（刚按下还没拖）返回 `None`。
    pub fn selection(&self) -> Option<(usize, usize)> {
        let (anchor, head) = self.selection?;
        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        (start < end).then_some((start, end))
    }

    /// 在输入框里按下左键：把插入点挪过去，同时起一个新选区的锚点。
    ///
    /// 与消息区同一套语义：只按下不拖（空选区）就是「点一下定位插入点」，拖出去才是选取。
    pub fn begin_selection(&mut self, index: usize) {
        let index = self.clamped_index(index);
        self.selection = Some((index, index));
        self.cursor = index;
        self.follow_caret();
        // 点到别处再打字是一段新的编辑：不结束当前组的话，撤回会把两处改动一起退掉。
        self.end_run();
    }

    /// 拖动中：把选区的另一端（也是插入点，供粗光标与窗口跟随）挪到新位置。
    pub fn extend_selection(&mut self, index: usize) {
        let index = self.clamped_index(index);
        if let Some((anchor, _)) = self.selection {
            self.selection = Some((anchor, index));
            self.cursor = index;
            self.follow_caret();
        }
    }

    /// 清掉鼠标选区（复制完成 / 点到别处 / 文本改了）。
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 选区里的文本（空选区返回 `None`）；写剪切板用。
    ///
    /// 取的是输入框里**显示出来的**文本：粘贴折叠块不展开（「所选即所见」，折叠原文
    /// 只在提交时还原）。全是空白的选区照样返回，要不要写剪切板由调用方判定。
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text.chars().skip(start).take(end - start).collect())
    }

    /// 屏幕坐标反查：输入框可见窗口里的（行, 显示列）→ 字符下标。
    ///
    /// 折行、宽字符、超出行数上限后的窗口滚动都用 [`Self::visible_rows`] 同一套结果，
    /// 否则鼠标点到的字符会和落下去的光标对不上。点在某行末尾之后就算该行末尾（下一行的
    /// 行首在那个换行符之后），点在整个文本之后就是末尾；落在占位符块中间时按块取近端
    /// （块在输入框里是一整块，光标不停在块里，与左右键同一套语义）。
    pub fn char_index_at(&self, width: u16, row: usize, column: usize) -> usize {
        let (_, _, start_row, _) = self.visible_rows(width);
        let width = width.max(1) as usize;
        let target = start_row + row;
        let chars: Vec<char> = self.text.chars().collect();
        let mut row_now = 0usize;
        let mut cell = 0usize;
        let mut found = chars.len();
        for (index, ch) in chars.iter().enumerate() {
            if *ch == '\n' {
                // 行尾：点在这一行最后一个字符之后，插入点落在这个换行符之前（渲染出来
                // 就在本行末尾；落到 `index + 1` 会画到下一行行首）。
                if row_now == target && column >= cell {
                    found = index;
                    break;
                }
                row_now += 1;
                cell = 0;
                continue;
            }
            let char_width = unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
            if cell + char_width > width && cell > 0 {
                row_now += 1;
                cell = 0;
            }
            if row_now > target || (row_now == target && cell >= column) {
                found = index;
                break;
            }
            cell += char_width;
        }
        self.snap_to_placeholder(found)
    }

    /// 把下标夹进文本长度以内（鼠标算出的行尾列可能落在文本之后）。
    fn clamped_index(&self, index: usize) -> usize {
        index.min(self.text.chars().count())
    }

    /// 落在占位符块中间时取近端的一半：块内不停光标（与左右键、整块删除同一套语义）。
    pub(crate) fn snap_to_placeholder(&self, index: usize) -> usize {
        match self.placeholder_span(index) {
            Some((start, end)) => {
                if index - start <= end - index {
                    start
                } else {
                    end
                }
            }
            None => index,
        }
    }

    /// 按显示宽度软折行；阶段一按列断行，不做英文单词级避断。
    pub fn wrapped_lines(&self, width: u16) -> Vec<String> {
        let width = width.max(1) as usize;
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut used = 0usize;
        for ch in self.text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut current));
                used = 0;
                continue;
            }
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cell > width && used > 0 {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(ch);
            used += cell;
        }
        lines.push(current);
        lines
    }

    /// 光标所在的行号与列（列按显示宽度，行按 `width` 软折行后的行）。
    pub fn cursor_position(&self, width: u16) -> (usize, usize) {
        let width = width.max(1) as usize;
        let mut row = 0usize;
        let mut column = 0usize;
        for (index, ch) in self.text.chars().enumerate() {
            if index >= self.cursor {
                break;
            }
            if ch == '\n' {
                row += 1;
                column = 0;
                continue;
            }
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if column + cell > width && column > 0 {
                row += 1;
                column = 0;
            }
            column += cell;
        }
        (row, column)
    }

    /// 需要展示的行与光标在其中的行号：超过行数上限时随光标滚动。
    pub fn visible_lines(&self, width: u16) -> (Vec<String>, Option<usize>) {
        let (lines, cursor, _, _) = self.visible_window(width);
        (lines, cursor)
    }

    /// 滚轮翻看：把可见窗口按 `delta` 行上下移动（负为向上）；返回是否由输入框消费。
    ///
    /// 内容不超过 [`COMPOSER_MAX_LINES`] 行时窗口里本来就是全文，没有可翻的，返回 `false`
    /// 让滚轮落回消息区。翻过之后窗口不再跟随光标——滚到哪看哪；滚回光标所在那一屏
    /// 就交还给跟随，之后继续打字不会把插入点顶到窗口外。
    pub fn scroll_rows(&mut self, delta: isize, width: u16) -> bool {
        let total = self.wrapped_lines(width).len();
        if total <= COMPOSER_MAX_LINES {
            // 内容变短（删行、窗口变宽）后不再有可翻的：顺手清掉旧起点，
            // 免得下次内容又变长时沿用一个过时的窗口。
            self.scroll = None;
            return false;
        }
        let limit = total - COMPOSER_MAX_LINES;
        let (row, _) = self.cursor_position(width);
        let follow = row.saturating_sub(COMPOSER_MAX_LINES - 1).min(limit);
        let current = self.scroll.unwrap_or(follow) as isize;
        // 上下都夹在可翻范围内：越界会让窗口露出空白。
        let next = (current.saturating_add(delta)).clamp(0, limit as isize) as usize;
        self.scroll = (next != follow).then_some(next);
        true
    }

    /// 可见窗口：行（带全文字符区间）+ 光标在窗口里的行号 + 窗口起点行号 + 总行数。
    ///
    /// 窗口算法只在这一处：折行、跟着光标滚动、滚轮翻看、上限裁剪都在这里定，
    /// [`Self::visible_lines`] 与 [`Self::visible_window`] 都从它派生——高度计算、
    /// 渲染与鼠标点击换算因此共用同一套换行结果，不会各算一份而对不上。
    pub fn visible_rows(&self, width: u16) -> (Vec<ComposerRow>, Option<usize>, usize, usize) {
        let lines = self.wrapped_lines(width);
        let total = lines.len();
        let (row, _) = self.cursor_position(width);
        let (start, count, cursor) = if total <= COMPOSER_MAX_LINES {
            (0, total, Some(row.min(COMPOSER_MAX_LINES - 1)))
        } else {
            let follow = row
                .saturating_sub(COMPOSER_MAX_LINES - 1)
                .min(total - COMPOSER_MAX_LINES);
            // `scroll` 只在 [`Self::scroll_rows`] 里写入；窗口变窄/变宽后总行数会变，
            // 这里再夹一次，避免沿用一个已经越过末尾的起点（那样窗口会整块空白）。
            let start = self.scroll.unwrap_or(follow).min(total - COMPOSER_MAX_LINES);
            // 滚轮把窗口挪开后，插入点可能落在窗口之外：那里没有行号，渲染也就不画光标。
            let cursor = (row >= start && row - start < COMPOSER_MAX_LINES).then(|| row - start);
            (start, COMPOSER_MAX_LINES, cursor)
        };
        // 每行在全文里的字符区间：折行行内不含换行符，行与行之间把它跳过去。
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = 0usize;
        let mut rows: Vec<ComposerRow> = Vec::with_capacity(count);
        for (position, text) in lines.into_iter().enumerate() {
            while chars.get(index) == Some(&'\n') {
                index += 1;
            }
            let row_start = index;
            index += text.chars().count();
            if position >= start && rows.len() < count {
                rows.push(ComposerRow {
                    text,
                    start: row_start,
                    end: index,
                });
            }
        }
        (rows, cursor, start, total)
    }

    /// 与 [`Self::visible_rows`] 同一个窗口，另外给出窗口起点与总行数。
    ///
    /// 输入卡右侧的细线滚动条要画滑块，需要知道「当前可见的是哪一段」。
    pub fn visible_window(&self, width: u16) -> (Vec<String>, Option<usize>, usize, usize) {
        let (rows, cursor, start, total) = self.visible_rows(width);
        (
            rows.into_iter().map(|row| row.text).collect(),
            cursor,
            start,
            total,
        )
    }
}
