//! 视觉设置页：代理开关、故障转移列表与原生视觉三态。
//!
//! 从 `state.rs` 的单个 `impl SettingsState` 里按面板拆出：一页的键位与表单语义只在
//! 这一页改动时才需要看，混在一起会让每页的改动互相挤兑。

use super::*;

impl SettingsState {

    /// 视觉设置页：`↑↓` 选列表项、空格 开关、`A` 添加、`D` 删除、`N` 原生视觉三态。
    pub(super) fn handle_vision_key(&mut self, key: KeyCode) -> Option<SettingsEvent> {
        match key {
            KeyCode::Up => {
                self.move_vision(-1);
                None
            }
            KeyCode::Down => {
                self.move_vision(1);
                None
            }
            KeyCode::Char(' ') => {
                self.vision.enabled = !self.vision.enabled;
                self.vision.status = format!(
                    "视觉代理已{}；按 Ctrl+S 保存。",
                    if self.vision.enabled {
                        "启用"
                    } else {
                        "停用"
                    }
                );
                None
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.dropdown = Some(Dropdown {
                    field: DropdownField::VisionAdd,
                    selected: 0,
                });
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.delete_vision_model();
                None
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.cycle_native_vision();
                None
            }
            KeyCode::Esc | KeyCode::Left => {
                self.back_to_list();
                None
            }
            _ => None,
        }
    }

    pub(super) fn move_vision(&mut self, delta: isize) {
        let count = self.vision.models.len() as isize;
        if count == 0 {
            return;
        }
        self.vision.selected = ((self.vision.selected as isize + delta).rem_euclid(count)) as usize;
    }

    /// 故障转移顺序：把选中项与相邻项对调（对映 Python 的 `action_move_priority_*`）。
    pub(super) fn move_vision_priority(&mut self, delta: isize) {
        let selected = self.vision.selected;
        let target = selected as isize + delta;
        let count = self.vision.models.len();
        if selected >= count || target < 0 || target as usize >= count {
            return;
        }
        self.vision.models.swap(selected, target as usize);
        self.vision.selected = target as usize;
        self.vision.status = format!(
            "已{}故障转移优先级；按 Ctrl+S 保存。",
            if delta < 0 { "提高" } else { "降低" }
        );
    }

    /// 模型原生视觉三态循环：未配置 → 开启 → 关闭 → 未配置。
    pub(super) fn cycle_native_vision(&mut self) {
        self.vision.native = match self.vision.native {
            None => Some(true),
            Some(true) => Some(false),
            Some(false) => None,
        };
        let text = self.vision_native_text();
        self.vision.status = format!("{text}；按 Ctrl+S 保存。");
    }

    pub(super) fn delete_vision_model(&mut self) {
        if self.vision.models.is_empty() {
            self.vision.status = "当前没有可删除的视觉模型。".to_string();
            return;
        }
        let removed = self.vision.models.remove(self.vision.selected);
        self.vision.selected = self
            .vision
            .selected
            .min(self.vision.models.len().saturating_sub(1));
        self.vision.status = format!("已移除视觉模型「{}」；按 Ctrl+S 保存。", removed.label());
    }

    /// `A` 选中一条渠道：作为 `custom` 引用加进列表（重复项只提示，不重复添加）。
    pub(super) fn add_vision_model(&mut self, key: &str) {
        let reference = VisionModelRef::custom(key);
        if self
            .vision
            .models
            .iter()
            .any(|item| item.label() == reference.label())
        {
            self.vision.status = format!("视觉模型「{}」已经在列表中。", reference.label());
            return;
        }
        self.vision.models.push(reference);
        self.vision.selected = self.vision.models.len() - 1;
        let label = self.vision.models[self.vision.selected].label();
        self.vision.status = format!("已添加「{label}」；按 Ctrl+S 保存。");
    }

    /// 视觉页头部两行的文本（渲染层用）。
    pub fn vision_enabled_text(&self) -> String {
        format!(
            "视觉代理：{}    已配置模型：{} 个",
            if self.vision.enabled {
                "已启用"
            } else {
                "已停用"
            },
            self.vision.models.len()
        )
    }

    pub fn vision_native_text(&self) -> String {
        match self.vision.native {
            None => "模型原生视觉：未配置（按模型能力）".to_string(),
            Some(true) => "模型原生视觉：已开启".to_string(),
            Some(false) => "模型原生视觉：已关闭".to_string(),
        }
    }

    /// 视觉列表的行文本（`› [1] 渠道名`），渲染层用。
    pub fn vision_rows(&self) -> Vec<String> {
        self.vision
            .models
            .iter()
            .enumerate()
            .map(|(index, reference)| {
                format!(
                    "{} [{}] {}",
                    if index == self.vision.selected {
                        "›"
                    } else {
                        " "
                    },
                    index + 1,
                    reference.label()
                )
            })
            .collect()
    }

    pub fn vision_selected(&self) -> usize {
        self.vision.selected
    }
}
