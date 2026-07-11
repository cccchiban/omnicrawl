# 终端 UI 设计说明

OmniCrawl 使用基于 Textual 的**全屏终端工作台**。框架统一管理屏幕重绘、滚动、焦点、流式内容和模态确认，避免内联 ANSI 输出与终端历史、光标位置和窗口缩放相互竞争。

## 设计目标

- **现代深色专业感**：采用深灰背景、蓝色导航、青绿色成功态和暖黄色工具态；布局为工作区侧栏、对话主区和固定输入区。
- **信息层级明确**：用户输入、AI 输出、工具执行、等待状态和人工确认具有稳定且不同的视觉语义。
- **稳定优先**：所有视图变化通过 Textual 主事件循环完成；后台 Agent 线程只发送事件，不直接写终端。
- **窄屏可读**：启动面板、工具参数、确认内容、预输入和等待状态按真实终端列宽换行，不使用虚构的最小宽度。
- **成熟渲染框架**：依赖 `textual`，安装 `requirements.txt` 后即可运行。

## 视觉与交互约定

```text
┌─ OmniCrawl ─ Workspace / Agent / Runtime ─┬─ 当前对话 ───────────────────┐
│ D:\project                                  │ ◆ 回复内容与 Markdown         │
│ 思考 已启用 · 深度 max                       │ ▸ 用户问题                    │
│ 审批 人工确认 · Skill 25 已加载              │ ◌ 工具步骤与结果               │
│ 会话 / 临时目录                              │                                 │
│                                               ├─────────────────────────────┤
│                                               │ 输入消息，Enter 发送          │
└───────────────────────────────────────────────┴─────────────────────────────┘
```

| 内容 | 表示方式 | 设计意图 |
| --- | --- | --- |
| 用户输入 | 蓝色左边框消息卡 | 与模型和工具记录稳定区分。 |
| AI 输出 | 青绿色左边框 Markdown 卡 | 流式分片更新同一张受控卡，不移动终端光标。 |
| 等待状态 | 顶栏运行状态 | 不占用对话历史，完成后统一变为“就绪”。 |
| 工具执行 | 暖黄色步骤卡 | 展示工具名、参数和执行结果。 |
| 成功/失败 | 文本标记 + 语义色 | 不只依赖颜色表达状态。 |
| 人工确认 | 居中模态框 | 线程等待明确的允许或拒绝结果，主界面保持受控。 |

当 `approval.mode` 为 `manual` 时，受限工具显示确认卡；`auto` 与 `review` 仅显示步骤和执行结果。输入 `/approval:manual`、`/approval:auto`、`/approval:review` 可切换模式。

## 稳定性策略

1. **主线程渲染**：Agent 在 Textual worker 线程执行；每个 `delta/status/tool/usage` 事件均经 `call_from_thread` 回到主循环更新组件。
2. **单一流式卡片**：连续正文分片合并进当前助手卡，Markdown 由组件重绘，不使用 ANSI 光标左移或手工清行。
3. **模态审批**：受限工具请求以模态框呈现；后台线程等待回调结果，界面状态不会被并发终端输出打散。
4. **可控滚动与焦点**：消息区独立滚动，输入框固定在底部；每轮完成后自动恢复输入焦点。
5. **取消语义**：`Ctrl+C` 在任务执行中请求取消，在空闲时退出工作台；`Ctrl+L` 仅清空当前视图，不清空会话数据。

## 文件边界

- `omnicrawl/ui/fullscreen/`：Textual 应用、全屏布局、流式事件桥接和人工确认模态框。
- `main.py`：默认创建 Agent 后直接启动全屏工作台。
- `omnicrawl/ui/tui/`、`stream_turn.py`、`chat_session.py`：保留为兼容输出与既有测试支持，不再作为默认交互入口。
- `tests/test_terminal_ui.py`：终端样式、窄屏、Unicode、确认、工具、spinner 和回归测试。
- `tests/test_inline_input.py`：输入编辑、删除键和历史记录测试。
- `docs/TERMINAL_UI.md`：本文档。

## 兼容范围与限制

- 验收目标为 Windows Terminal、PowerShell 和 VS Code 集成终端。
- 必须安装 `textual`；缺少依赖时执行 `pip install -r requirements.txt`。
- 当前全屏版支持普通对话、流式 Markdown、工具记录、取消、斜杠命令和手动审批；窗口过窄时由 Textual 负责折行和滚动。
- 旧 ANSI 模块仍保留，供测试与非交互输出使用。

## 验证清单

每次修改 TUI 后至少执行：

```powershell
python -m unittest tests.test_fullscreen_tui tests.test_terminal_ui tests.test_inline_input -v
python -m unittest discover -s tests -v
python -m compileall -q omnicrawl main.py
git diff --check
```

手工冒烟时覆盖普通输入、长/CJK/emoji 输入、流式 Markdown、工具成功/失败、人工确认、错误和 `Ctrl+C` 路径；再检查窗口缩放后的侧栏、消息滚动和固定输入框。
