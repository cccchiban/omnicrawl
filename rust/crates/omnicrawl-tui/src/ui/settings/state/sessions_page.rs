//! 会话管理页：列出会话、二次确认后删除。
//!
//! 与配置项页面不同，这一页不写配置文件，而是把「删哪个会话」交给宿主走内核链路
//! （`session.delete`）。列表由宿主在打开这一页时注入（`SettingsState::set_sessions`），
//! 因为读列表同样是内核往返，界面层不直接碰 IPC。
//!
//! 删除不可逆，因此按两次才真正发出：第一次按 `D` 进入待确认状态并把删除键名与目标
//! 会话都记进来，再按一次 `D`（或 `Y`）才产出事件；`Esc` 或移动选择会取消待确认。

/// 删除确认的待确认目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelete {
    pub session_id: String,
    pub title: String,
}

#[derive(Debug, Default)]
pub struct SessionsState {
    pub(crate) rows: Vec<SessionRow>,
    pub(crate) selected: usize,
    /// 待确认的删除：`None` 表示没有在确认中。
    pub(crate) confirming: Option<PendingDelete>,
    /// 最近一次操作的结果文案。
    pub status: String,
}

/// 一行的展示数据（会话元信息，取自内核返回的索引条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub session_id: String,
    pub title: String,
    pub message_count: usize,
    /// 是否为当前正在使用的会话（内核拒绝删除，界面标出来并禁用删除）。
    pub is_current: bool,
}

impl SessionRow {
    /// 「标题 + 条数」的单行摘要。
    pub fn summary(&self) -> String {
        let title = if self.title.trim().is_empty() {
            "（未命名）"
        } else {
            self.title.trim()
        };
        format!("{title}  {} 条", self.message_count)
    }
}

impl SessionsState {
    pub fn rows(&self) -> &[SessionRow] {
        &self.rows
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn confirming(&self) -> Option<&PendingDelete> {
        self.confirming.as_ref()
    }

    /// 当前选中的会话。
    pub fn selected_row(&self) -> Option<&SessionRow> {
        self.rows.get(self.selected)
    }

    /// 覆盖列表：尽量让选中行停在原来的会话上，列表变短时收到末尾。
    pub fn set_rows(&mut self, rows: Vec<SessionRow>) {
        let previous = self.selected_row().map(|row| row.session_id.clone());
        self.rows = rows;
        self.selected = previous
            .and_then(|id| self.rows.iter().position(|row| row.session_id == id))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        self.confirming = None;
    }

    /// 上下移动选中行；移动即取消待确认（避免确认的目标和当前选中项不一致）。
    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let count = self.rows.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(count);
        self.selected = next as usize;
        self.confirming = None;
    }

    /// 直接把选中行移到第 `index` 行（鼠标点击用），同时取消待确认。
    pub fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = index.min(self.rows.len() - 1);
        self.confirming = None;
    }

    /// 请求删除当前选中的会话。
    ///
    /// 第一次返回 `None`（转入待确认）；已经在确认同一目标时返回该目标的 id，由调用方发出事件。
    /// 当前会话不可删：直接给出拒绝文案，不进确认态。
    pub fn request_delete(&mut self) -> Option<String> {
        let Some(row) = self.rows.get(self.selected) else {
            self.status = "没有可删除的会话。".to_string();
            return None;
        };
        if row.is_current {
            self.status = "不能删除当前会话，请先切换到其他会话。".to_string();
            self.confirming = None;
            return None;
        }
        match self.confirming.take() {
            Some(pending) if pending.session_id == row.session_id => Some(row.session_id.clone()),
            _ => {
                self.confirming = Some(PendingDelete {
                    session_id: row.session_id.clone(),
                    title: row.summary(),
                });
                self.status.clear();
                None
            }
        }
    }

    /// 放弃待确认。
    pub fn cancel_confirm(&mut self) {
        self.confirming = None;
    }
}
