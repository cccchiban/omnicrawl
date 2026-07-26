# 任务：首次启动多渠道配置向导

状态：已完成
创建：2026-07-25
更新：2026-07-25

## 需求摘要

- 首次启动通过终端菜单配置 OpenAI、Anthropic、Gemini 渠道。
- OpenAI 渠道可选择 Chat Completions 或 Responses 协议。
- 每个渠道配置名称、Base URL、API Key、模型 ID，并选择默认渠道/模型。
- 同一请求方式允许配置多个不同渠道。
- `/settings` 提供后续新增、编辑、启用/禁用、删除和默认渠道管理。

## 关键决策

- 复用现有 `llm.profiles` 和 `models.yaml`，不新增配置格式。
- API Key 只写 `config.yaml`；`models.yaml` 不保存凭据，`ChannelConfig` 的 repr 也隐藏 Key。
- 首次启动与运行设置复用同一个 Textual `ChannelManagerScreen`。
- 首次向导预置三种 Provider；列表使用方向键导航和空格勾选，编辑字段使用标准密码 Input。
- 不同 Profile 禁止回退复用当前模型的 API Key，避免跨渠道泄露凭据。
- `config.yaml` 与 `models.yaml` 写入失败时回滚已写文件，避免半套配置。

## 实现计划

- [x] 1. 添加渠道配置读写模型与多渠道持久化测试
- [x] 2. 实现渠道列表和渠道编辑 Textual Screen
- [x] 3. 接入首次启动 bootstrap 与应用入口
- [x] 4. 接入 `/settings` 渠道管理入口和运行态刷新
- [x] 5. 完成定向测试、完整回归、构建与安装验证
- [x] 6. Quick review 并归档任务记录

## 已修改文件

- `omnicrawl/config/channels.py`：渠道模型、校验、默认选择、双 YAML 写回与回滚。
- `omnicrawl/config/bootstrap.py`：首次启动调用渠道向导并重载保存后的配置。
- `omnicrawl/config/llm_multi.py`：禁止不同 Profile 回退复用旧 API Key/Base URL。
- `omnicrawl/config/templates/*.yaml`：预置 OpenAI、Anthropic、Gemini 渠道和模型。
- `omnicrawl/ui/fullscreen/channel_manager.py`：渠道列表、编辑页和首次启动轻量 App。
- `omnicrawl/ui/fullscreen/settings.py`：增加“模型渠道”入口。
- `omnicrawl/ui/fullscreen/__init__.py`：渠道保存后热加载默认模型并刷新状态。
- `omnicrawl/entry.py`：首次启动渠道向导接线。
- `README.md`：记录首次向导、键盘操作和后续渠道管理方式。
- `tests/test_channel_configuration.py`、`tests/test_channel_manager.py`：配置、凭据隔离、回滚和 Textual 交互覆盖。
- `tests/test_startup_setup.py`、`tests/test_runtime_config.py`、`tests/test_settings_panel.py`、`tests/test_fullscreen_tui.py`：启动与设置入口回归。

## 验证

- `python -m unittest discover -s tests`：781 项通过。
- `python -m compileall -q omnicrawl tests`：通过。
- `git diff --check`：通过。
- `twine check`：Wheel 与 sdist 均通过。
- 隔离虚拟环境安装 Wheel：`ocl --help` 通过，渠道模块、Textual Screen 和模板资源可导入。
- 独立审查发现的跨 Profile Key 回退风险已增加失败测试并修复。
