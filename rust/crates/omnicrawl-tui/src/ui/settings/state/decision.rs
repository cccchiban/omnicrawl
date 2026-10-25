//! 结构化决策模型页：渠道列表、功能开关与自部署分区。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    // ---------- 结构化决策模型页：列表与表单 ----------
    //
    // 与渠道页同形，但表单更窄（决策服务只有一种接口）且没有「模型 ID 自动检测」：
    // 决策模型名是固定的版本字符串，没有远端列表可拉。

    /// 决策页：列表态与表单态各有一组键位。
    ///
    /// 列表态的行由「决策渠道 + 功能开关」拼成：渠道行上 `Enter`/`→` 进表单，
    /// 开关行上 `←`/`→`/`Enter`/空格 就地切换（选中即保存，与工具开关页同义）。
    pub(super) fn handle_decision_models_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        if self.decision_models.form.is_some() {
            return self.handle_decision_form_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_decision_channel(-1);
                None
            }
            KeyCode::Down => {
                self.move_decision_channel(1);
                None
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                if self.decision_switch_selected().is_some() {
                    return self.toggle_decision_switch();
                }
                if let Some(row) = self.decision_local_selected() {
                    return self.activate_decision_local(row);
                }
                if key == KeyCode::Char(' ') {
                    return None;
                }
                self.edit_decision_channel();
                None
            }
            KeyCode::Left => {
                if self.decision_switch_selected().is_some() {
                    return self.toggle_decision_switch();
                }
                if let Some(row) = self.decision_local_selected() {
                    // 左键在枚举行上回退一档，其余行与 Esc 同义（回一级菜单）。
                    if matches!(row, DECISION_LOCAL_ROW_SIZE | DECISION_LOCAL_ROW_DEVICE) {
                        self.decision_models.local.cycle(row, -1);
                        return self.save_decision_local();
                    }
                    self.back_to_list();
                    return None;
                }
                self.back_to_list();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if self.decision_switch_selected().is_some()
                    || self.decision_local_selected().is_some()
                {
                    self.decision_models.status =
                        "只有决策渠道行参与新建；按 ↑ 选中决策渠道再按 N。".to_string();
                    return None;
                }
                self.new_decision_channel();
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if let Some(row) = self.decision_local_selected() {
                    return self.activate_decision_local(row);
                }
                self.delete_decision_channel();
                None
            }
            KeyCode::Esc => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    /// 决策表单：输入态 → 候选展开态 → 字段导航，三层各管各的键位。
    pub(super) fn handle_decision_form_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let input_active = self
            .decision_models
            .form
            .as_ref()
            .map(|form| form.input.is_some())
            .unwrap_or(false);
        if input_active {
            return self.handle_decision_input_key(key);
        }
        let dropdown_open = self
            .decision_models
            .form
            .as_ref()
            .map(|form| form.dropdown.is_some())
            .unwrap_or(false);
        if dropdown_open {
            return self.handle_decision_dropdown_key(key);
        }
        match key {
            KeyCode::Up => {
                self.move_decision_field(-1);
                None
            }
            KeyCode::Down | KeyCode::Tab => {
                self.move_decision_field(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_decision_field(),
            KeyCode::Esc | KeyCode::Left => {
                self.leave_decision_form();
                None
            }
            _ => None,
        }
    }

    /// 候选展开态：`↑`/`↓` 移动、`Enter` 确认、`Esc` 收起。
    pub(super) fn handle_decision_dropdown_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.decision_models.form.as_mut()?;
        let dropdown = form.dropdown.as_mut()?;
        match key {
            KeyCode::Up => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize - 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Down => {
                let count = dropdown.options.len() as isize;
                if count > 0 {
                    dropdown.selected =
                        ((dropdown.selected as isize + 1).rem_euclid(count)) as usize;
                }
                None
            }
            KeyCode::Enter => {
                let value = dropdown.options.get(dropdown.selected).cloned();
                let field = dropdown.field;
                form.dropdown = None;
                if let Some(value) = value {
                    set_decision_field(&mut form.row, field, value);
                }
                None
            }
            KeyCode::Esc => {
                form.dropdown = None;
                None
            }
            _ => None,
        }
    }

    /// 文本编辑态：字符插入、光标移动、`Enter` 提交、`Esc` 取消。
    pub(super) fn handle_decision_input_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        let form = self.decision_models.form.as_mut()?;
        let field = form.field;
        let composer = form.input.as_mut()?;
        match key {
            KeyCode::Enter => {
                let value = composer.take();
                form.input = None;
                set_decision_field(&mut form.row, field, value);
            }
            KeyCode::Esc => form.input = None,
            KeyCode::Char(character) => composer.insert(&character.to_string()),
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Left => composer.move_left(),
            KeyCode::Right => composer.move_right(),
            KeyCode::Home => composer.move_home(),
            KeyCode::End => composer.move_end(),
            _ => {}
        }
        None
    }

    /// 选中的是开关行时返回它在开关表里的下标；选中的是渠道行时返回 `None`。
    pub(super) fn decision_switch_selected(&self) -> Option<usize> {
        self.decision_models
            .selected
            .checked_sub(self.decision_models.rows.len())
            .filter(|index| *index < self.decision_models.switches.len())
    }

    /// 自部署分区被选中的行号（不在该分区时为 `None`）。
    pub(super) fn decision_local_selected(&self) -> Option<usize> {
        self.decision_models
            .selected
            .checked_sub(self.decision_models.rows.len() + self.decision_models.switches.len())
            .filter(|index| *index < DECISION_LOCAL_ROW_COUNT)
    }

    /// 自部署分区：`Enter`/方向键触发的动作。
    ///
    /// 长任务期间按住重复动作，但状态行给出**当前阶段**（进度行也一直在跑），
    /// 不再只回一句「请稍候」——用户能看出到底卡在哪一步。
    pub(super) fn activate_decision_local(&mut self, row: usize) -> Option<SettingsEvent> {
        if self.decision_models.local.busy {
            let (stage, _, _) = self.decision_progress_line();
            self.decision_models.status = if stage.is_empty() {
                "自部署任务进行中，进度见「任务进度」行。".to_string()
            } else {
                format!("自部署任务进行中：{stage}")
            };
            return None;
        }
        match row {
            DECISION_LOCAL_ROW_SIZE | DECISION_LOCAL_ROW_DEVICE => {
                self.decision_models.local.cycle(row, 1);
                self.save_decision_local()
            }
            DECISION_LOCAL_ROW_PREPARE => Some(SettingsEvent::Apply(
                SettingsChange::DecisionLocal(DecisionLocalChange::PrepareEnvironment),
            )),
            DECISION_LOCAL_ROW_DOWNLOAD => {
                let size = self.decision_models.local.size.clone();
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::Download { size },
                )))
            }
            DECISION_LOCAL_ROW_DELETE => {
                let size = self.decision_models.local.size.clone();
                if !self.decision_models.local.size_downloaded() {
                    self.decision_models.status = format!("{size} 还没有下载到本机，无需删除。");
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::Delete { size },
                )))
            }
            DECISION_LOCAL_ROW_REMOVE_ENV => {
                if !self.decision_models.local.env_ready {
                    self.decision_models.status = "运行环境本来就没装，无需删除。".to_string();
                    return None;
                }
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::RemoveEnvironment,
                )))
            }
            DECISION_LOCAL_ROW_CHANNEL => {
                let size = self.decision_models.local.size.clone();
                Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                    DecisionLocalChange::UseChannel { size },
                )))
            }
            DECISION_LOCAL_ROW_START => Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                DecisionLocalChange::StartService,
            ))),
            DECISION_LOCAL_ROW_STOP => Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
                DecisionLocalChange::StopService,
            ))),
            _ => None,
        }
    }

    /// 尺寸或设备改动：就地写盘（与功能开关同一口径：选中即保存）。
    pub(super) fn save_decision_local(&mut self) -> Option<SettingsEvent> {
        let local = &self.decision_models.local;
        Some(SettingsEvent::Apply(SettingsChange::DecisionLocal(
            DecisionLocalChange::Save {
                size: local.size.clone(),
                device: local.device.clone(),
            },
        )))
    }

    /// 开关行：就地翻转并交给宿主写盘（选中即保存）。
    pub(super) fn toggle_decision_switch(&mut self) -> Option<SettingsEvent> {
        let index = self.decision_switch_selected()?;
        let row = self
            .decision_models
            .switches
            .get(index)
            .map(|row| (row.key.clone(), !row.enabled))?;
        Some(SettingsEvent::Apply(SettingsChange::DecisionSwitch {
            key: row.0,
            enabled: row.1,
        }))
    }

    pub(super) fn move_decision_channel(&mut self, delta: isize) {
        let count = self.decision_row_count() as isize;
        if count == 0 {
            return;
        }
        self.decision_models.selected =
            ((self.decision_models.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 进入表单：编辑选中的决策渠道（选中开关行时不进表单）。
    pub(super) fn edit_decision_channel(&mut self) {
        let Some(row) = self
            .decision_models
            .rows
            .get(self.decision_models.selected)
            .cloned()
        else {
            self.decision_models.status = "还没有决策渠道可编辑；按 N 新建一条。".to_string();
            return;
        };
        let index = self.decision_models.selected;
        self.decision_models.status =
            format!("正在编辑决策渠道 {}；Ctrl+S 保存，Esc 返回列表。", row.key);
        self.decision_models.form = Some(DecisionForm {
            row,
            field: DecisionField::Name,
            input: None,
            dropdown: None,
            is_new: false,
            index: Some(index),
        });
    }

    /// 新建决策渠道：草稿只在表单里，`Ctrl+S` 才落进列表（`Esc` 直接丢弃）。
    pub(super) fn new_decision_channel(&mut self) {
        let mut row = self.decision_models.template.clone();
        let base = if row.key.trim().is_empty() {
            "决策渠道".to_string()
        } else {
            row.key.trim().to_string()
        };
        let mut candidate = base.clone();
        let mut index = 2;
        while self
            .decision_models
            .rows
            .iter()
            .any(|item| item.key == candidate)
        {
            candidate = format!("{base}{index}");
            index += 1;
        }
        row.key = candidate.clone();
        if row.name.trim().is_empty() {
            row.name = candidate.clone();
        }
        self.decision_models.status =
            format!("正在新建决策渠道 {candidate}；填好后按 Ctrl+S 保存，Esc 放弃。");
        self.decision_models.form = Some(DecisionForm {
            row,
            field: DecisionField::Name,
            input: None,
            dropdown: None,
            is_new: true,
            index: None,
        });
    }

    /// 删除选中决策渠道；至少保留一条（与配置侧的校验同口径）。
    ///
    /// 选中开关行时不动列表，只提示当前要删的是渠道。
    pub(super) fn delete_decision_channel(&mut self) {
        if self.decision_switch_selected().is_some() {
            self.decision_models.status =
                "功能开关行不能删除；按 ↑ 选中决策渠道再按 D。".to_string();
            return;
        }
        if self.decision_local_selected().is_some() {
            self.decision_models.status =
                "自部署分区的行不能删除；按 ↑ 选中决策渠道再按 D。".to_string();
            return;
        }
        if self.decision_models.rows.len() <= 1 {
            self.decision_models.status = "至少要保留一个决策渠道。".to_string();
            return;
        }
        let removed = self
            .decision_models
            .rows
            .remove(self.decision_models.selected);
        self.decision_models.selected = self
            .decision_models
            .selected
            .min(self.decision_models.rows.len() - 1);
        if self.decision_models.default_key == removed.key {
            self.decision_models.default_key = self
                .decision_models
                .rows
                .iter()
                .find(|row| row.enabled)
                .or_else(|| self.decision_models.rows.first())
                .map(|row| row.key.clone())
                .unwrap_or_default();
        }
        self.decision_models.status =
            format!("已删除决策渠道 {}；Ctrl+S 保存后才会写盘。", removed.key);
    }

    /// 表单里切换字段（`↑`/`↓` 与 `Tab` 同义）。
    pub(super) fn move_decision_field(&mut self, delta: isize) {
        let Some(form) = self.decision_models.form.as_mut() else {
            return;
        };
        let count = DecisionField::ORDER.len() as isize;
        let current = form.field.index() as isize;
        let next = (current + delta).rem_euclid(count) as usize;
        form.field = DecisionField::ORDER[next];
    }

    /// `Enter`/空格：文本字段进编辑态、枚举展开候选、开关就地翻转。
    pub(super) fn activate_decision_field(&mut self) -> Option<SettingsEvent> {
        let Some(form) = self.decision_models.form.as_mut() else {
            return None;
        };
        match form.field {
            DecisionField::Enabled => form.row.enabled = !form.row.enabled,
            DecisionField::Mode => {
                let options: Vec<String> = DECISION_MODE_OPTIONS
                    .iter()
                    .map(|mode| (*mode).to_string())
                    .collect();
                let selected = options
                    .iter()
                    .position(|option| option == &form.row.mode)
                    .unwrap_or(0);
                form.input = None;
                form.dropdown = Some(DecisionDropdown {
                    field: DecisionField::Mode,
                    options,
                    selected,
                });
            }
            field => {
                // API Key 的输入态从空开始：既不回显明文，也让「留空=不改动」成为默认动作。
                form.dropdown = None;
                form.input = Some(decision_input_composer(&form.row, field));
            }
        }
        None
    }

    /// `Esc`：退出表单回列表（未保存的改动丢弃）。
    pub(super) fn leave_decision_form(&mut self) {
        let drafting = self
            .decision_models
            .form
            .as_ref()
            .map(|form| form.is_new)
            .unwrap_or(false);
        self.decision_models.form = None;
        self.decision_models.status = if drafting {
            "已放弃新建决策渠道（未写盘）。".to_string()
        } else {
            "已返回决策渠道列表（未保存的改动已丢弃）。".to_string()
        };
    }

    /// `Ctrl+S`：把表单落进列表、校验后交给宿主写盘。
    pub(super) fn save_decision_channels(&mut self) -> Option<SettingsEvent> {
        let form = self.decision_models.form.clone()?;
        let mut rows = self.decision_models.rows.clone();
        match form.index {
            Some(index) if index < rows.len() => rows[index] = form.row.clone(),
            _ => rows.push(form.row.clone()),
        }
        if let Err(message) = validate_decision_rows(&rows) {
            self.decision_models.status = message;
            return None;
        }
        let default_key = pick_default_decision_channel(&rows, &self.decision_models.default_key);
        self.decision_models.status = "正在保存决策模型配置…".to_string();
        self.decision_models.rows = rows.clone();
        self.decision_models.default_key = default_key.clone();
        self.decision_models.form = None;
        Some(SettingsEvent::Apply(SettingsChange::DecisionModels {
            rows,
            default_key,
        }))
    }
}
