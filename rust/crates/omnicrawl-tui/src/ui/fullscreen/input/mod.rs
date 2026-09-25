//! 输入区（对映 Python `ui/fullscreen/input/`）。
//!
//! Python 侧这里放输入框与它的附属交互：`composer.py`（多行输入框与粘贴归一）、
//! `editing.py`（编辑按键与布局高度）、`menu.py`（斜杠命令补全菜单）、
//! `sessions_menu.py`（会话选择菜单）。
//!
//! Rust 侧按文件顺序迁移：`menu` 已落斜杠命令菜单的筛选、选择与渲染；
//! `sessions_menu` 已落 `/sessions` 的输入框上方可选列表；
//! `composer` / `editing` 随后续批次补入。
//! Textual 的 `TextArea` 挂载与焦点由装配层承接，这里只保留纯状态与显示行。

pub mod menu;
pub mod sessions_menu;
