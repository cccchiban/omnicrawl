# 终端 UI 设计说明

OmniCrawl 使用基于 Textual 的**全屏终端工作台**。框架统一管理屏幕重绘、滚动、焦点、流式内容和模态确认，避免内联 ANSI 输出与终端历史、光标位置和窗口缩放相互竞争。

## 设计目标

- **终端原生透明工作台**：所有主界面、菜单和模态面板背景均为透明，正文继承终端默认前景色，完成、运行、工具、异常和推理状态分别使用终端 ANSI 绿、蓝、黄、红和品红；布局保持两行 HUD、无卡片消息流和三行总高的单行输入舱。
- **信息层级明确**：用户输入、AI 思考、AI 输出、工具执行、等待状态和人工确认具有稳定且不同的视觉语义。
- **稳定优先**：所有视图变化通过 Textual 主事件循环完成；后台 Agent 线程只发送事件，不直接写终端。
- **窄屏可读**：启动面板、工具参数、确认内容、预输入和等待状态按真实终端列宽换行，不使用虚构的最小宽度。
- **成熟渲染框架**：依赖 `textual`，安装 `requirements.txt` 后即可运行。

## 视觉与交互约定

```text
◆ OMNICRAWL  PRJ project  ·  MDL deepseek-v4-flash  ·  THK MAX  ·  APR MAN
                   IN 18.6K  OUT 2.4K  CA 7.1K  CTX 18.6K/128K
▸ 用户问题
▾ 思考过程（点击折叠）
◇ AI 回复内容与 Markdown
⌁ read_file · 成功 · 120ms
· ● 正在调用（跟随最新记录）
› 输入消息或 / 命令
```

| 内容 | 表示方式 | 设计意图 |
| --- | --- | --- |
| 用户输入 | `▸` 锐角前缀，无实体背景 | 通过符号区分用户内容，同时保留终端原生背景。 |
| AI 思考 | 默认展开的“思考过程”，点击标题折叠/展开 | 每次模型推理独立成段，不设置展开快捷键；模型不返回 `reasoning_content` 时不显示空段。 |
| AI 输出 | `◇` 前缀 Markdown，无背景 | 流式分片更新同一条受控记录，不移动终端光标；所有消息记录统一保留 1 行下间距，避免相邻内容挤在一起。 |
| 运行状态 | 消息流中紧跟最新内容的临时单行状态 | 仅任务活动期间显示并闪烁 `●` 状态点；随阶段更新为“正在思考 / 正在回复 / 正在调用 / 等待”，始终原位移动到最新推理、正文或工具记录之后，而非固定在对话框上方；完成后移除，不写入持久对话。 |
| 工具执行 | 默认折叠的工具记录；文件变更走 git 旁注 diff | 普通工具：`⌁ 工具名 · 状态 · 耗时`。`write_file`/`replace_text`：`M path \| +N -M` 标题，展开后旁注行号 `+`/`-` 预览；覆盖写无旧内容时显示 `rewrite +N lines`，不编造假 diff。执行中与完成后均默认折叠。 |
| 成功/失败 | 折叠标题文本标记 + 语义色 | 不只依赖颜色表达状态。 |
| 顶部 HUD | 两行稳态：`PRJ`/`MDL`/`THK`/`APR` + Token 遥测 | 第一行品牌与上下文摘要对齐；第二行跳过外边距和 18 列品牌栏，使 Token 起点与上下文摘要对齐。布局高度为 2（1 行内容 + 1 行底边框），避免 Textual `border-bottom` 把内容高度压成 0。隐藏完整项目路径；审批压缩为 `MAN`/`AUTO`/`REV`；长字段截断；`IN`/`OUT`/`CA`/`CTX` 按 `llm.context_window_tokens` 绘进度条。 |
| 输入区 | 三行总高的单行输入舱；输入 `/` 时向上展开命令菜单 | 菜单合并内置命令和动态 `/skill:*`，实时过滤且最多显示 8 条；上下键选择，Enter 或 Tab 只填入输入框，不立即执行。生成期间按 Enter 提交的内容进入 FIFO 队列，并显示排队数量。 |
| 人工确认 | 透明模态层、ANSI 蓝色边框与绿色批准操作 | 风险操作保持明确，同时不遮盖终端背景。 |

Token 遥测中的 `IN` 是最近一次模型请求的输入 Token，`OUT` 是输出 Token，`CA` 是缓存命中的输入 Token，`CTX` 使用 `IN / llm.context_window_tokens` 表示当前上下文占用。进度条低于 60% 使用终端 ANSI 绿色，60%–84% 使用黄色，85% 及以上使用红色。终端自适应令牌统一定义在 `omnicrawl/ui/fullscreen/theme.py`：Textual CSS 使用 `ansi_default`/`ansi_*`，Rich 文本使用 `default` 与标准 ANSI 色名。

主界面首屏显示后会立即在后台发现 MCP 能力；发现期间静默锁定输入，不显示瞬时“等待”状态，避免初始化闪屏和首条消息重复触发能力加载。加载失败仍会显示错误记录，但不会阻止后续使用内置工具。

当 `approval.mode` 为 `manual` 时，受限工具按模型调用顺序逐个显示确认卡；`auto` 与 `review` 按现有策略直接执行或审查。一次模型响应包含多个工具调用时，审批全部完成后，除 `write_file` 和显式删除行为外的工具并行执行；写文件与删除调用作为串行屏障，不与同批其他工具重叠。Host 等待整批结束，即使部分调用失败也继续收集其余结果，最后按模型原始调用顺序统一回传。工具调用记录默认折叠，标题展示工具名、调用状态和耗时，点击后显示参数及执行结果。输入 `/approval:manual`、`/approval:auto`、`/approval:review` 可切换模式。输入 `/settings` 可打开中文设置面板，使用方向键和 Enter/空格修改模型、推理强度、审批模式、记忆、MCP、插件和子任务开关；修改会立即应用并写入当前项目配置。模型项会复用双列模型选择器。关闭 MCP、插件或子任务前会先完成对应资源/任务收尾，失败时保留原运行态；开启 SubAgent 会同步重建 Coordinator 和 Host 工具表。关闭设置面板后 HUD 会刷新为最新模型、推理强度和审批模式。

## 稳定性策略

1. **主线程渲染**：Agent 在 Textual worker 线程执行；每个 `delta/status/tool/usage` 事件均经 `call_from_thread` 回到主循环更新组件。
2. **受控流式记录**：连续正文分片合并进当前助手记录；推理分片合并进当前默认展开的思考段。Markdown 由组件重绘，不使用 ANSI 光标左移或手工清行。
3. **模态审批**：受限工具请求以模态框呈现；后台线程等待回调结果，界面状态不会被并发终端输出打散。
4. **可控滚动与焦点**：消息区独立滚动，输入框固定在底部；每轮完成后自动恢复输入焦点并继续处理 FIFO 队列。斜杠菜单打开时由输入区接管上下键、Enter 和 Tab，补全后关闭菜单；用户再次按 Enter 才提交命令。
5. **取消与输入语义**：`Esc` 在任务执行中请求取消，在空闲时聚焦输入框；`Ctrl+C` 在输入框有选区时复制，无选区时清空输入框；任务执行期间按 Enter 的消息和斜杠命令进入 FIFO 队列，当前回合结束或取消后继续发送；`Ctrl+L` 仅清空当前视图，不清空会话数据，`Ctrl+Q` 退出工作台。

## 文件边界

- `omnicrawl/ui/fullscreen/`：Textual 应用、全屏布局、流式事件桥接和人工确认模态框。
  - `__init__.py`：`OmniCrawlApp` 入口、Widget 生命周期与渲染。
  - `turns.py`：Agent 回合生命周期控制器（无 Textual 依赖）。
  - `commands.py`：斜杠命令分派（无 Textual 依赖）。
  - `settings.py`：中文运行设置模态面板与即时保存。
  - `theme.py`：透明终端主题、Textual Theme 注册，以及 CSS/Rich 对应的终端自适应色彩令牌。
  - `monitor.py`：Monitor 游标/轮询状态适配（无 Textual 依赖）。
  - `widgets.py` / `hud.py` / `tool_diff.py`：界面组件、顶部遥测与文件变更 diff 渲染。
- `main.py`：默认创建 Agent 后直接启动全屏工作台。
- `omnicrawl/ui/tui/`、`stream_turn.py`、`chat_session.py`：保留为兼容输出与既有测试支持，不再作为默认交互入口。
- `tests/test_fullscreen_tui.py`、`tests/test_fullscreen_turns.py`、`tests/test_fullscreen_commands.py`、`tests/test_fullscreen_monitor.py`：全屏工作台与状态边界回归。
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

手工冒烟时覆盖普通输入、长/CJK/emoji 输入、流式 Markdown、工具成功/失败、人工确认、错误、`Esc` 取消、`Ctrl+C` 复制/清空和生成期间多条消息 FIFO 排队路径；再检查窗口缩放后的 HUD、消息滚动和固定输入框。
