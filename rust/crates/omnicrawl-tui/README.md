# omnicrawl-tui

内核协议 v1 的 Rust 宿主前端：全屏终端工作台（HUD + 消息流 + 输入 + 审批/提问面板）。

它是协议里既定的「宿主」角色之一（另两个是启动器与过渡期的 Python 宿主）：起内核进程，
把内核通知渲染成界面，把用户输入、审批与提问答案按帧回给内核。协议规格见
[`rust/docs/protocol-v1.md`](../../docs/protocol-v1.md)。

## 模块

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | 二进制入口：选内核、握手、进出全屏、事件循环与终端恢复 |
| `src/args.rs` | 启动参数（命令行 → 环境变量 → 默认值）、内核路径解析 |
| `src/app.rs` | 接线层：帧 ↔ 状态机 ↔ 写回内核 |
| `src/state.rs` | 状态机：消息记录、输入框、遥测、批次挂载 |
| `../omnicrawl-tts/` | TTS 引擎已独立成 `omnicrawl-tts` crate：接口合成（OpenAI 兼容 `audio/speech`，发布默认）与可选的本地 MOSS-TTS-Nano ONNX 推理（`onnx` feature）、文本归一化、音频 I/O、声线库、模型下载与本地播放 |
| `src/commands.rs` | 斜杠命令的 TUI 宿主接线：`CommandAgent` 能力面（`TuiHostAgent`）、候选表与插件状态行映射 |
| `src/ui/` | 渲染：`hud.rs`、`conversation.rs`、`composer.rs`（含输入框上方的命令菜单）、`panels.rs`；`settings/` 是设置面板（`mod.rs` 常量与路由、`state.rs` 状态机与键位、`render.rs` 绘制） |
| `../omnicrawl-host/` | 宿主执行层已独立成 `omnicrawl-host` crate：内核进程客户端（`kernel`）、工具批次与审批策略（`host`）、工具执行体（`tools`）、审批模式（`approval`）与无头回合运行器（`turn`）；本 crate 只做界面与接线 |

## 工具执行层

工具表、参数归一化、Schema 校验与压缩复用内核已搬好的 `omnicrawl-controllers`（数据来自
`omnicrawl/agent/toolkit/tools.py`）；**执行体**与声明生成在 `omnicrawl-host` 的 `tools/` 下，下表是那张表的内容（TUI 按原路径再导出）：

| 工具 | 状态 | 说明 |
| --- | --- | --- |
| `read` | 已实现 | 行窗口、超长行截断、行号与续读 footer；`text` 片段定位已实现 |
| `read_image` | 已实现 | 本机图片读取：拒绝 URL/data URI、相对路径不得越出工作区、按签名识别 PNG/JPEG/GIF/WebP、Base64 编码为视觉附件；图片去向由路由定：原生视觉（`--native-vision`）直送主模型，配了 `[vision]` 则交给独立视觉模型代理换成文本结论，两者都没有时只回文本载荷 |
| `image_gen` | 已实现 | OpenAI 兼容 Image API：`/images/generations` 与 `/images/edits`（multipart 参考图），`b64_json` 落盘或按 URL 下载，保存位置支持目录 / 文件名 / 多张编号；未启用时调用给出明确错误（配置来自开关与环境变量，见已知差异） |
| `write_file` | 已实现 | overwrite / append，父目录自动创建 |
| `Edit_file` | 已实现 | count 语义、行尾风格恢复、版本指纹校验、文件锁 + 原子写、错误码 |
| `bash` / `powershell` | 已实现 | 显式解释器（Windows 上 Bash 优先 Git Bash）、超时回收进程树、输出头尾采样与完整日志落盘 |
| `list` | 已实现 | 目录（可递归）清单、受保护路径过滤、500 项截断提示 |
| `find` | 已实现 | 工作区内限定、kind/大小写/glob 与子串匹配、按修改时间倒序、超限落盘 |
| `grep` | 已实现 | 正则（`regex`，与 rg 同族）或精确子串、上下文行、count / files 模式、include/exclude、二进制与非 UTF-8 处理、超限落盘 |
| `git` | 已实现 | argv 直调不经 shell、逃逸参数拒绝、输出首尾有界化；风险分级复用 `controllers::approval` |
| `monitor` | 已实现 | `start` / `poll` / `stop` / `list`（含 `status`/`log`/`logs`/`read` 别名）后台命令：内存环形缓冲（1000 条、单条 4000 字符切块）、游标增量轮询、最多 20 个并发任务、进程树回收；任务随宿主关闭与回合取消终止 |
| `kb_search` / `kb_read` / `kb_write` / `kb_append` / `kb_list` | 已实现 | Markdown + frontmatter 知识库：路径安全（拒绝 `..` 与越界）、读写/覆盖/追加、关键词与字段检索、`INDEX.md` 自动维护；默认根 `~/.OmniCrawl/knowledge` |
| `memory_search` / `memory_read` / `memory_expand_related` / `memory_write` | 已实现（按配置进出表） | 按 `scope` 路由到三处记忆目录，读写交给 `omnicrawl-session` 的 `MemoryStore`；`memory_enabled` 缺省开启（与 Python `entry.py` 的 `default=True` 一致），未启用的作用域回「X 级记忆系统未启用。」 |
| `web_search` | 已实现 | Bing / DuckDuckGo / 雅虎三引擎：桌面浏览器请求头、端点与查询参数逐字对齐、正则解析结果页（含雅虎 `RU=` 与 DDG `uddg=` 跳转还原）、验证码/异常流量如实报错、网络错误重试 |
| `fetcher` | 已实现 | 多 URL 并行抓取、手动跟随 301/302/307/308 与 `<meta refresh>`、内网/本机目标直连、`insecure=true` 跳过证书校验、正文提取（`main` → `article` → `body`，剔除脚本样式）且 HTML5 容错解析 |
| `windows_window` / `windows_control` / `windows_input` / `windows_clipboard` / `windows_screenshot` | 已实现 | 整组注册：窗口枚举/详情/前台激活、控件 UI Automation（Windows PowerShell）、受约束的 SendInput 键鼠、剪贴板文本读写、桌面/区域/窗口截图（GDI 抓屏 + 缩放 + PNG，>5MiB 继续缩小）并作为视觉附件回模型 |
| `advisor` | 已实现 | 零参数顾问：判定与分支裁剪复用内核 `controllers::advisor`，运行期用独立 LLM Runtime 做单轮无工具补全（系统提示词取自 `templates/advisor_system.md`），返回 plan/correction/stop 指导；只在 `--advisor-model`（或 `OMNICRAWL_ADVISOR_MODEL`）给出时进表 |
| `update_todos` / `ask_user` / `pause_work` | 已实现 | 由界面侧判定与面板交互 |
| `tts_synthesize` | 已实现 | 引擎在 `omnicrawl-tts`（文本归一化、音频 I/O 与声线库、greedy 生成帧逐帧一致），执行体在宿主工具表 `omnicrawl-host/src/tools/registry.rs`；`[tts]` 未启用时工具不进表（与 Python 一致） |
| `subagent` | 由并行内核侧改造覆盖 | `controllers/subagents/*` 与本 crate 的 `subagent_types` 接线正在推进中，本 crate 不重复开工 |

声明由工具表生成：`read` / `Edit_file` / `write_file` 的参数契约在 Python 侧写的是**示例值**，
`tool_parameters_schema`（原在未搬的 `agent/runtime/llm_protocol.py`）按示例推断类型并加
`minProperties` 兜底，本 crate 照搬了这一层，声明与 Python 逐字对齐（有对照数据集钉住）。

搜索类工具用 `ignore` crate 遍历：读 `.gitignore` / `.ignore` / `.rgignore`（非 git 目录也读）、
保留隐藏文件、剪枝受保护路径——与随包 Go 扩展（`native/ocsearch`）声明的语义一致，纯 Rust、无外部二进制。

审批语义与 Python `_approve_tool_call` 对齐：`manual` 模式**只对 shell 命令（`bash` / `powershell`）与
非只读 git 操作弹确认**，文件、搜索与后台监控类工具直接放行（`auto` 全部放行）。同一批审批完成后工具并发执行，
最后按模型调用顺序回观察（顺序与数量都不能变）。`Esc` 取消会先回收正在跑的进程树。

拒绝结果与 Python 对齐：文案取 `user_cancelled_reason`（「用户取消执行：<工具>。」），回观察时带
`error_code = denied`，内核据此把这次拒绝落成会话事件（`tool_call_denied`）。

`monitor` 的后台进程归当前回合：批次派发时记下回合号，`Esc` 取消该回合时只回收本回合启动的任务
（事件里写明「当前回合已取消，后台任务已强制终止。」），宿主退出时回收全部任务。输出只在内存环形缓冲里，
不写持久化日志；模型用游标增量轮询。Windows 上后台进程同样纳入 kill-on-close Job
（`KillOnCloseJob::assign`，与 `bash` / `powershell` 路径同一实现）：宿主正常退出或崩溃时由操作系统
递归回收整棵进程树，`taskkill /T` 只在拿不到 Job（例如当前进程已在不可嵌套的 Job 里）时作退路。
遇到进程已被外部回收、管道写端仍被孤儿持有时，宿主仍会在 5 秒内把任务落成终态
（与 Python 的 `reader.join(timeout=5)` 一致），不会让回合无限期挂在 `running` 上。

## 运行

```bash
cargo build -p omnicrawl-cli              # 先有内核二进制
cargo run -p omnicrawl-tui -- --model deepseek-v4-flash --session-root ../.agent_sessions
```

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--kernel <路径>` | `$OMNICRAWL_BINARY` → 同目录 `omnicrawl` → PATH | 内核可执行文件 |
| `--model <名称>` | `$OMNICRAWL_MODEL` / `$OPENAI_MODEL` → config.toml 的当前模型 | 命令行与环境变量都没有时退回配置（`[llm.active_model]` / `[llm] model`，对齐 Python 的读配置语义）；三处都没有才报错 |
| `--base-url <地址>` | `$OPENAI_BASE_URL` | 模型接口基地址 |
| `--api-key-env <变量名>` | `OPENAI_API_KEY` | 凭据只给环境变量名，不进帧 |
| `--session-root <目录>` | 空 | 给了就让内核自己持有会话（转录与压缩） |
| `--context-window <N>` | 空 | HUD 上下文占用条的分母 |
| `--approval <manual\|review\|auto>` | config.toml 的 `[approval] mode`（没配时为 `review`） | 命令行给的模式优先；不给时读配置（对齐 Python `load_approval_mode`，含别名与默认值），配置读不出来或取值非法直接报错而不静默降级；`manual` 下非自持工具先弹确认 |
| `--command-timeout <秒>` | `360` | 命令类工具默认超时（与 `MAX_COMMAND_TIMEOUT_SECONDS` 一致，上限 360） |
| `--tool-timeout <秒>` | `$AGENT_TOOL_TIMEOUT_SECONDS` → `600` | 单批工具执行的最长等待（上限 3600）：超时把未完成的调用写成超时观察、回合继续推进，后台结果被丢弃 |

按键：`Enter` 提交、`Ctrl+J` 换行、`Esc` 取消当前回合（空闲时清空输入）、`↑`/`↓`/`PageUp`/`PageDown`
滚动消息区、`Ctrl+L` 清屏、`Ctrl+C` 清空输入、`Ctrl+Q` 退出；审批面板用 `Y`/`N`，
提问面板用 `↑`/`↓` 选项 + `Enter` 确认（没有选项时在输入框写答案）。

## 界面层对映移植（进行中）

Python 的 Textual 工作台（`omnicrawl/ui/`，18,867 行）正按目录逐层对映到 `src/ui/fullscreen/`，
取代早先另写的简化页面（`src/ui/{hud,conversation,composer,panels}.rs`）。约定：目录结构对齐、
行为与视觉对齐为准；Textual 专属机制（CSS 变量表、Select 挂载竞态补丁、门面 monkeypatch）不照搬，
由组件状态、布局函数、事件分派与显式按键处理承接。设置屏（`screens/`，22 个文件）留后续批。

已落地（底座批）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `src/ui/fullscreen/text.rs` | Rich `Text` | `StyledText` 分段样式文本、`char_styles`、`to_spans` |
| `src/ui/fullscreen/random.rs` | stdlib `random` | xorshift64\* 随机源（固定种子可复现） |
| `src/ui/fullscreen/terminal/theme.rs` | `terminal/theme.py` | 取色令牌同名同值 + Rich 风格串解析（含 `on <颜色>` 背景语法：`"on #272822"` 必须落到 `bg`，否则代码块会变成暗底暗字） |
| `src/ui/fullscreen/status/hud.rs` | `status/hud.py` | HUD 纯格式化（CTX/遥测/状态段、解密扫描帧） |
| `src/ui/fullscreen/status/indicators.rs` | `status/indicators.py` | 轮播状态机与排队预览行（组件热区留给装配层） |
| `src/ui/fullscreen/mod.rs` | | `round_half_even`（对映 Python 内建 `round`） |

已落地（渲染批 1）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `src/ui/fullscreen/rendering/difflib.rs` | stdlib `difflib` | `SequenceMatcher` 等价子集（`isjunk=None` / `autojunk=False`）：最长匹配块 + 递归队列 + 相邻块合并，opcodes 与 CPython 逐段一致（7 组对照数据集钉住） |
| `src/ui/fullscreen/rendering/tool_diff.rs` | `rendering/tool_diff.py` | 工具卡标题（色点/原名/上下文/状态/耗时）、fetcher 正文过滤、文件变更预览（append/rewrite 预览 + 旁注行号 diff + 80 行截断）、`替换 N 处` 摘要与真实起始行号解析 |
| `src/ui/fullscreen/rendering/widgets.rs` | `rendering/widgets.py`（部分） | 子任务进度树、任务清单、子任务会话面板；`AssistantMessage` / `ReasoningDisclosure` 的**渲染口径**已由活动页面直接调用（`markdown::render_markdown` + `latex::latex_to_text` / 思考段 `uniform_gray`），只剩 widget 级封装未挂载；`ToolDisclosure` 的标题与正文已接线；`ConfirmationScreen` 待补 |
| `src/ui/fullscreen/tool_labels.rs` | `ui/tool_labels.py` | 工具显示名与图标表、状态图标、耗时格式化 |

已落地（渲染批 2，工具卡与状态行）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `rendering/widgets.rs`（续） | `rendering/widgets.py` | `ToolDisclosure`（标题去重、首尾各 2 行采样 + 省略提示行、展开/收起、终态正文分块释放、`update_body`、状态 class 映射、`refresh_elapsed` 只对进行中生效）、`RuntimeStatus`（同一帧文本跳过重绘 + `[ ESC ]` 提示）、`ConfirmationScreen`（默认聚焦「允许执行」、`←/→` 切焦点、`Esc` 以拒绝收口）、`indent_body_lines` / `body_hint_text` |
| `text.rs`（续） | Rich `Text` | `split_lines` / `join_lines`（保留分段样式），供正文采样与缩进使用 |

已落地（渲染批 3，Markdown 渲染与消息组件）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `rendering/markdown.rs` | Rich `Markdown`（RichMarkdown） | 用 `pulldown-cmark`（新增依赖，已确认）解析事件流并映射为样式串：标题/段落/列表（含嵌套与任务列表 `☑`/`☐`）/围栏代码块（整块灰底 + 按语言逐 token 高亮，未知语言只加灰底）/行内代码（青字 + 灰底）/粗体/斜体/删除线/链接（下划线 + 蓝）/引用（`│ ` 前缀）/分隔线；`uniform_gray` 对映 `_UniformGrayMarkdown`（保留结构、统一下压为灰阶前景） |
| `rendering/highlight.rs` | Rich `Syntax`（monokai 主题） | 代码块逐 token 高亮：表驱动手写扫描器，色值取 monokai 原值（关键字 `#66d9ef`、函数·类型名 `#a6e22e`、字符串 `#e6db74`、数字 `#ae81ff`、注释 `#75715e` + 斜体、运算符 `#f92672`），覆盖 Rust / Python / JS·TS / JSON / TOML / YAML / Shell / SQL（含别名），块注释与 Python 三引号字符串跨行保留状态；不引入 pygments / syntect |
| `rendering/widgets.rs`（续） | `rendering/widgets.py` | `AssistantMessage`（全量重绘、流式分块首块补 `◇ ` 前缀、挂载后重绘并释放全文副本、复制用纯文本）；`ReasoningDisclosure`（增量累积 + 换行/512 字触发分块落盘、未换行尾部留缓冲、折叠 5 行、点击切换展开、收口全量灰阶重绘） |

已落地（渲染批 4，启动画面与欢迎 Logo）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `src/ui/splash.rs` | `ui/splash.py` | 启动画面：左侧亮黄 Logo + 右侧圆角日志框（级别标记 `- ` / `! ` / `× ` 按色重绘、内容变化整窗重绘）+ Windows XP 滑块滚动条（滑入/横穿/滑出/空档四阶段）；`StartupLogSink` 线程安全收集、`run_startup_splash` 后台准备 + 主线程渲染、非交互流同步执行、`attach_startup_log_handler` / `report_startup_log` 承接 Python 的 logging 桥 |
| `src/ui/fullscreen/rendering/welcome_logo.rs` | `rendering/welcome_logo.py` | 8 行块字 Logo（字面量与 Python 渲染结果逐字节一致、行首缩进保留）、`LOGO_STYLE` 纯白、静态 `StyledText` |
| `src/ui/fullscreen/rendering/logo_anim.rs` | `rendering/logo_anim.py` | 解密扫描入场动画：行/列错位进度、共享乱码字符集（78 字符）、0.16 闪现概率、进度 1.0 终态快路径；`LogoAnimation` 承接 `app/core.py` 的播放游标（只播一次、播满 24 帧落定静态） |

已落地（渲染批 5，LaTeX 转换）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `src/ui/fullscreen/rendering/latex.rs` | `rendering/latex.py` | 行内 `$..$`、块级 `$$..$$` / `\[..\]`、`\(..\)`、数学 fenced block 与整行裸公式统一转 Unicode 近似文本：分数 / 根号 / 上下标 / 希腊字母与符号表 / 矩阵与方程组按列对齐 / 文本样式与重音 / `\mathbb` / 转义符号 / 未知命令保留原文；`$$` 与 fenced 归一化、普通代码围栏占位保护、多余美元收敛、误转义美元恢复；另提供 `split_blocks`（块级分段，供图像渲染管线）与 `has_block_formula`（快速判定）。Python 侧的四处正则（含 `(?<!\\)` / `(?!\$)` 环视）在本层是手写扫描器：语义等价、不引依赖 |

LaTeX 接线：`AssistantMessage`（全量重绘先剥离 `◇ ` 前缀再转换，数学 fenced 必须从行首开始）与 `ReasoningDisclosure`（增量与全量两条路径）在渲染前都走 `latex_to_text`，与 Python 的调用点一一对应。

已落地（输入批，斜杠命令菜单）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `src/ui/fullscreen/input/mod.rs` | `input/__init__.py` | 输入区模块出口 |
| `src/ui/fullscreen/input/menu.rs` | `input/menu.py` | 命令菜单逻辑：按 `search` 子串筛选并稳定排序（前缀命中先于子串命中、同级保留统一命令源的产品顺序）、命令名后出现空白改按参数前缀筛选（已输入完整参数不再提示）、完整命令名后追加参数候选、`COMMAND_MENU_VISIBLE_OPTIONS` 可见窗口随选择位滚动、`Up`/`Down` 环绕、`Enter`/`Tab` 只补全而完整命令放行提交、渲染为 `› ` 选中行 + `  · ` 弱化描述段（空白描述折叠为空） |

斜杠命令接线：

- **候选来源**：`src/commands.rs` 的 `command_options()` 取统一命令源的 `registry().options(false)`，启动时交给输入框（`Composer::set_commands`）；输入框每次文本变化自己重刷菜单，不必在每个改动点手动同步。
- **选择键**：菜单开着时 `Up`/`Down` 在候选间移动、`Enter`/`Tab` 只补全而**完整命令放行提交**（否则 `/settings` 这类无参数命令永远打不开）；菜单收起时这些键照旧归输入框与消息区。
- **提交分派**：命中注册表即交给命令层 `dispatch()`，未命中的输入照旧当成一轮对话；生成期间按 `CommandType::immediate()`（纯界面 / 只读查询）当场执行、其余排队。
- **能力面**（`commands::TuiHostAgent`）按「有什么报什么」实现：审批模式、模型与推理强度（写盘后随 `session.settings` 热更新内核）、插件状态、后台任务查询、**会话生命周期与历史**（`/sessions`、`/archives`、`/history`、`/rename`、`/new`、`/archive`、`/resume` 各走一次内核往返）、工作区根、只读 git 探测、评审报告注入、**顾问策略**（`/advisor`：命令层写盘后由宿主同步运行期选项并重建工具表，顾问工具即时进出表）、**记忆清理**（`/memory:clean`：按项目 → 会话 → 用户清理过期记忆）、**工作区切换**（`/workspace`：见下文「工作区切换」）可用。`/skills` 与本地 API 同口径：宿主自己按工作区发现 Skill 目录。
- **工作区切换**（`/workspace <路径>`）：宿主按 Python `WorkspaceSwitchingMixin` 的主体重排运行态——解析校验目标目录 → **子 Agent 排空**（活跃任务逐个 `subagent.query cancel` 并轮询到退出，超期报 `subagent_drain_error`）→ **pending worktree 拦阻**（`subagent.query list_worktrees` + `pending_worktrees_error`）→ 本地预备新工具表（含新工作区的 MCP 连接与全新后台任务管理器）与提示词运行时（**候选装配在工作线程**，见下文「慢命令」）→ 插件 `workspace.switch.before`（拒绝即中止，旧 Worker 不动；失败时补发 `workspace.switch.error`）→ 关闭旧工作区的 MCP 与后台任务 → 暂定/恢复 `monitor` 轮询并废弃旧游标 → 提交新状态 → 下发 `session.settings`（新工具表与上下文消息）并请内核在同一会话转录 `workspace_switched`。与 Python 的已知差异：`before`/`after` 由 `PluginHost::switch_workspace` 一次发出，因此钩子相对「候选装配」的先后与 Python 不同源（见「本阶段的边界」）。
- **会话状态的唯一真相在内核**：`App` 只记一个 `session_id`（握手回包的 `result.session_id` 给出，各会话命令的回执再校准）。`/resume` 与 `/undo` 之后宿主向内核索取 `session.events`（回退投影后的有效事件流）并用 `AppState::replay_events` 重建对话视图——消息、工具卡（含未收口/被拒绝的收口文案）、计划清单、SubAgent 进度树与压缩边界都按事件重建，而不是只投影 user/assistant 文本；回执里的 `history` 只在事件流读不到时兜底（`AppState::replay_history`）。默认会话根与 Python、本地 API 同址（`~/.OmniCrawl/.agent_sessions`），`--session-root` / `OMNICRAWL_SESSION_ROOT` 可覆盖。
  内核按 Python 的口径把工具事件一并落进转录：每次模型请求工具先落 `tool_call_requested`（公开参数 + 本批 assistant 原文的 `assistant_content` / 思考回传字段 / `function_name`），整批执行且输出预算/视觉/压缩处理过之后落 `tool_result`（`output` 展示全文、`model_output` 模型可见输出，超长输出由会话存储落 artifact），被拒绝的调用另有 `tool_call_denied`；因此回放出的历史页能还原工具卡，`/undo` 的 `event_ids` 也自然覆盖这些事件。已知缺口只剩两处：`tool_call_approved` 仍未落盘（审批在宿主侧完成，协议里还没有宿主→内核的审批通知），以及宿主协议观察不带 `ui_artifact`，事件里按 Python 缺省写 `{}`。子代理内部的工具调用**不落父会话**（Python 的 `persist_session_events=False` 口径）。
- **异步内核往返**：命令层的接口是同步的、内核链路是异步帧，因此 `/undo`、`/compact` 与 `/review` 由宿主在进命令层之前拦下、异步下发（响应按请求 id 回填，状态行随回执收起）。`/review` 必须走异步：评审子 Agent 的工具批次要回到宿主执行，同步等待会与 `tool.batch` 互相卡死；它先在宿主侧跑 git 预检（复用命令层的 `check_review_preconditions`），回执到了先渲染报告、再发一条 `session.append` 把报告注入内核上下文（下一轮请求可见）。
- **同步往返**：只读或本地毫秒级的几条用宿主侧快速往返（`/tasks`、`/task`、`/sessions`、`/archives`、`/history`、`/rename`、`/new`、`/archive`、`/resume`）；等待期间让路的帧收进 `deferred_frames`，下次 `drain_frames` 按原顺序处理，通知不丢。
- **慢命令**（对映 Python `CommandOutcome.execution == "slow"`）：命令层的延迟执行体是同步接口，`App` 又不跨线程共享，因此宿主直接接管真正慢的两条并只把慢的那一段放进线程——`/workspace` 的候选装配（工具表 + MCP + 提示词运行时）在工作线程、提交与内核下发仍在主线程（`App::tick_slow_command` 每帧取回结果）；`/mcp` 的状态文本也在工作线程读（`format_status` 会触发 MCP 发现与连接）。两条都在后台期间把状态行改成「正在准备新工作区…」/「正在读取 MCP 状态」，并在途时拒绝第二条慢命令。其余命令（含 `/undo`、`/compact`、`/review`）走宿主的内核往返或内联延迟体。

已落地（接线批，子任务进度树）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `src/state.rs` | `rendering/pipeline.py` 的 `_handle_subagent_event` | `Record::SubagentTree`：`subagent.task.*` 事件按 `batch_id` 找树、没有就新开一棵挂在消息流末尾；状态表外的子代理事件（对话/工具/文本）不进树；载荷缺字段按 `or` 口径回落（`task_id` → `task`、`batch_id` → `batch-<task_id>`、`description` → `task_id`）；`refresh_subagent_trees` 只推进活跃树 |
| `src/ui/conversation.rs` | `rendering/widgets.py` 的 `SubAgentProgressTree` 挂载点 | 进度树记录铺开为显示行（每行自带图标与状态取色），不参与点击命中 |
| `src/app.rs` / `src/main.rs` | `app/core.py` 的耗时定时器 | 每帧调用 `tick_subagent_trees`，批次收口后不再重算 |

子任务进度树接线：同一批次的任务事件在多行树上原地更新，而不是每来一条事件就追加一行通知；终态节点不会被迟到的活动事件回退、树的耗时只在仍有非终态任务时刷新（与 Python 的 `is_active` / `refresh_elapsed` 同规则）。

启动画面接线：`main.rs` 把内核进程、工具表与 MCP 能力、内核握手整体放进后台准备线程，进备用屏幕之前在普通屏幕上显示 splash；
准备失败立即收画面并把错误交给普通终端打印，准备线程 panic 同样先复位终端再原样抛出（对映 Python worker 的 `except BaseException` + `has_error()` 提前收画面）。

欢迎 Logo 接线：空会话（尚无任何记录）时 `ui/conversation.rs` 只渲染 Logo，首条记录进来即让位，清空会话后重新出现（此时动画已落定为静态字形）。

已落地（交互批：排队预览、鼠标与输入自愈）：

| Rust 文件 | 对映 Python | 说明 |
| --- | --- | --- |
| `src/ui/queue.rs` | `status/indicators.py` 的 `PendingQueue` / `QueueDelete` / `QueueToggle` | 排队预览条（标题 + FIFO 摘要行 + 行尾 `[ DELETE ]` 热区 + 展开/收起提示行）；行数、摘要、行序、命中区与展开关卡都复用对映层纯函数 |
| `src/ui/conversation.rs` | `rendering/widgets.py` 的 `ToolDisclosure` / `ReasoningDisclosure` 点击 | 每条显示行携带 `LineHit`（提示行 / 卡片 / 思考段）；`collapsed_body` 按「有效行」首尾各 2 行采样，提示行文案 `点击展开 N 行`；`hit_test` 按窗口起止下标把区内行号换算成绝对行号 |
| `src/ui/hud.rs` | `status/hud.py` 的内容驱动分段 | 不再用固定列宽：分段贴齐、超长值 `compact_hud_value` 截断、窄屏逐段收缩（版本号优先保留），任何宽度都恰好填满一行 |
| `src/ui/fullscreen/terminal/console_heal.rs` | `terminal/handling.py` 的 `_restore_windows_vt_input_mode_if_needed` | 控制台模式自愈（只声明 `GetStdHandle`/`GetConsoleMode`/`SetConsoleMode`，不引入 `windows-sys` 到 TUI）；目标值是 crossterm 鼠标捕获的整值 `0x0098`，不是 Python 的 VT 输入位 |
| `src/main.rs` | `terminal/handling.py` 的周期看门狗 | 切备用屏幕时开鼠标与焦点报告（`Drop` 里关掉），每秒核对一次控制台模式，恢复后重发协议序列；每帧把终端区域交给 `App::set_viewport` 供命中判定 |
| `src/ui/mod.rs` | | `ui::layout` + `UiAreas`：渲染与鼠标命中共用同一套区域计算 |

终端模式自愈与 Python 的**必守差异**：目标模式必须与 crossterm 自己设的值整值一致。crossterm 0.29 的 Windows 事件源走
win32 控制台记录路径（`INPUT_RECORD` 交给 `handle_key_event` / `handle_mouse_event`），**不需要** VT 输入位；它的
`EnableMouseCapture` 是把模式整体覆盖成 `0x0010 | 0x0080 | 0x0008`（`ENABLE_MOUSE_MODE` = `0x0098`）。
Python 侧要求 `ENABLE_VIRTUAL_TERMINAL_INPUT` 是因为那是 Textual 的 VT 驱动——照抄到 crossterm 上会让自愈与鼠标捕获
互相覆盖：自愈补 VT 位 → 重发鼠标捕获又抹掉 → 下一秒再次判定「被外部重置」，界面上刷满提示（曾真实报障）。
因此这里的目标值就是 `0x0098`（顺带压掉了快速编辑/回显/行输入/处理输入位），并有编译期断言与单测锁住它。
轮询间隔 1 秒；提示另做限流（`NOTICE_MIN_INTERVAL` 一分钟内只提示一次），恢复本身照常执行。
Python 的「工具子进程结束就立刻核对」相当于把这一秒的窗口收到工具结束时刻。

已知差异：

- 鼠标滚轮一格按 3 行推进（Textual 的滚轮步长）；设置面板的点击与悬停已接线（见下文「设置批」）；
- 轮播留言文案（`carousel_messages.txt`）编译期内嵌：Python 从包资源在运行期读取，可随 pip 分发并手工编辑，Rust 侧脱离宿主单文件分发时仍可用但不能在运行期改；
- `decrypt_frame` 的乱码随机源是内部 xorshift64\*（Python 用 Mersenne Twister）：字符集与概率一致，随机序列不同；
- `rgba(...)` 令牌（用户消息背景）只取 RGB 分量：ratatui 无 alpha 混合，Python 侧由 Textual 与终端底色混合；
- `difflib` 等价层自实现（不引入 diff crate）：与 CPython 在 7 组对照用例上 opcodes 逐段一致，未覆盖 `isjunk` / `autojunk` 路径（Python 侧两处均传 `autojunk=False` 且无 junk 函数）；
- Markdown 渲染为**近似样式映射**：走 `pulldown-cmark` 事件流 + 本项目主题令牌，不复刻 Rich 的 Markdown 主题（各段精确色值、标题色相不同）；代码块已做逐 token 高亮（`rendering/highlight.rs`，色值取 monokai 原值），但分词是手写扫描器、不是 pygments 等价：只覆盖 8 门常见语言（其余语言保持整块灰底），且不处理 f-string 内部表达式、Rust 宏体内部、正则字面量等嵌套分词，未知 token 一律落回纯文本；
- **LaTeX 转换层已接入**（`rendering/latex.rs`）：AI 回复与思考在渲染前都走 `latex_to_text`。两处 Unicode 细节与 Python 有出入，且都只作用于启发式判定：裸公式行的 `str.isdigit()` 用 `char::is_numeric()` 近似、`str.isalpha()` 用 `char::is_alphabetic()`，`str.strip()` 用 `str::trim()`；
- 启动画面的日志桥改为显式上报：Python 给 root logger 挂 `logging.Handler`（只转发 WARNING/ERROR），Rust 无 logging 框架，由 `report_startup_log` 承担——有桥时写日志框且不落 stderr（避免打乱画面），无桥时回落 stderr（管道/测试下诊断不丢）；`logo_anim` 的随机源同样是内部 xorshift64\*；
- 终端尺寸不做环境变量回退：`shutil.get_terminal_size` 会先读 `COLUMNS`/`LINES`，Rust 直接问终端（`crossterm::terminal::size`），取不到时同样回落 80x24；
- 欢迎 Logo 动画按「起始时刻 → 经过时间」换算帧号（Python 用 Textual 定时器计次推进）：总帧数（24）与落定时刻（1.2s）一致，首帧是进度 0 的纯乱码帧（Python 首帧为 1/24）；
- 渲染入口（`src/ui/mod.rs::render`）已是工作台本体：消息流 / 任务清单 / 面板 / 底部单行轮播 HUD / 输入卡与补全菜单各一模块，设置页与文件选择弹层另行接管整屏。
- **页面版式已按 Python 当前版对齐**（主页面，不含设置屏）：底部单行轮播 HUD（遥测 → 工作区路径 → 留言，各 10s + 解密扫描过渡）、悬浮圆角输入卡（占位文案 `› 输入消息或 / 命令`）、用户消息的 `user：` 标签 + 青色竖条、思考块暗底、无边框工具卡（`●` 状态点 + 缩进正文）与会话流内的运行状态行；设置屏与审批/提问面板沿用原样式。
- **工具卡正文与 Python 同一来源**：正文不再用「工具输出拆行」，而是走对映层 `tool_diff::tool_disclosure_body` —— `write_file`/`Edit_file` 从**参数**画出旁注行号 diff（运行中就能看到）且豁免五行折叠，`fetcher` 只留 URL/状态/标题，`read` 与记忆/知识库类工具正文**为空且不出现提示行**，其余工具原样输出；超宽行按显示宽度**软折行**（对映 Textual 的默认 `text-wrap`，不再截断加 `…`），空行不加缩进（对映 `_indent_body_lines`），缩略态点卡片不再展开（只提醒示行展开，与 Python `ToolDisclosure.on_click` 一致）。
- 工具卡的两处非视觉差异：Python 在终态后按 12 块 × 30ms 释放正文（逐行出现的观感），Rust 一次铺满；Python 渲染完会清空 `arguments`/`result_text` 省内存，Rust 保留（展开/收起需要正文源）。

## 本阶段（骨架）的边界

**明确不在本阶段范围**（写在这里避免误读）：

- **工具未搬完**：已实现 read / read_image / image_gen / tts_synthesize / write_file / Edit_file / bash /
  powershell / list / find / grep / git / monitor / kb_* / memory_* / web_search / fetcher / windows_* /
  advisor 与三个自持工具；语音合成的执行体走 `omnicrawl-tts`（ONNX 推理、模型下载、纯 Rust 分词、
  音频播放），配置在启动期读 config.toml 的 `[tts]` 段，未启用时工具不进表（与 Python 一致）；
  `subagent` 由内核侧改造覆盖；
- **入口与分发**：默认启动路径已归 `omnicrawl-host`（`omnicrawl` npm 启动器 → 平台包的
  `host/omnicrawl-host`，见 `packages/cli/scripts/build-host.mjs`）；Python 侧 Textual 工作台
  （`omnicrawl/ui/`、`main.py`）保留为迁移期对照，不再是默认入口。

**已实现**：底部单行轮播 HUD（遥测 → 工作区路径 → 留言，各 10s 循环、切换时解密扫描；遥测页是
`上下文占用 ⁕ ↑/↓/† CH% ⁕ t/s ⁕ 模型 ⁕ THK 推理强度  APR 审批模式  MCP n  QUE n`，与 Python 的
`#bottom-carousel` 同款式；不再有顶部两行 HUD，也不再在 HUD 里显示版本号）、
消息流（用户 `user：` 灰斜标签 + 白色正文 + 左侧青色竖条、思考段暗底灰字斜体并折叠为最新五行、
助手正文 `◇` 且**走对映层 Markdown + LaTeX 渲染**（标题/加粗/列表/围栏代码高亮/行内代码青字暗底，
`$E=mc^2$` 一律先归一成 Unicode；刻意差异：`◇ ` 前缀在渲染后捕到首行行首，Python 是把它拼进
Markdown 源码，因此 Python 那边「首行就是标题」解析不出来，Rust 这边能）、无边框工具卡（`●` 状态点随状态着色 + 缩进正文，正文由对映层生成：文件变更预览 / 
隐藏类正文为空 / 超宽行折行，头尾采样限五行但文件变更工具豁免）、右侧 1 格细滚动条、
会话流内的运行状态行（Braille spinner + `[ ESC ]`））、
悬浮圆角输入卡（上方留一行、框内左右各缩进 4 格；空输入显示占位文案 `› 输入消息或 / 命令`，
五行上限、超出后随光标滚动）、任务清单条（`▣/▢` 无表头）、
审批面板与提问面板、`Esc` 取消、内核退出与终端恢复、`/undo`（请内核整轮回退，结论落消息流，
命令本身不进模型对话）。

完整的斜杠命令框架与 27 条内置命令在 `omnicrawl-commands`（Python `omnicrawl/commands/` 的 Rust 移植，
含 `CommandRegistry`、`CommandAgent` 能力 trait 与全部展示文案），本 crate 负责把能力面接上：

- 进命令层之前由宿主特判的有五条——`/undo`、`/compact`、`/review`（原因见上文「异步内核往返」）与慢命令 `/workspace`、`/mcp`（原因见下文「慢命令」）；
- `/settings` 仍是本 crate 直接打开设置面板；
- 其余命令都经统一注册表分发。命令处理器是同步接口，`App` 不实现 `Send + Sync`，因此没有另建
  线程安全命令代理：需要内核往返的命令在 `TuiHostAgent` 里就地做一次同步往返（`request_kernel`），
  列表/归档/历史这类毫秒级命令照旧内联跑完。

尚未接线（一律给出「暂不可用＋原因」，不退化成一次模型对话）：无。
已接线的更新：`/workspace` 走宿主的运行中切换（解析校验 → 子 Agent 排空与 worktree 拦阻 →
工作线程装配新工具表/MCP/提示词运行时 → 插件 `workspace.switch.before`（拒绝即中止）→
收尾旧工作区的 MCP 与后台任务 → 提交并下发 `session.settings`，再请内核在同一会话转录
`workspace_switched`）；
`/mcp` 走宿主工具表的 `McpClientManager::format_status()`（工作线程读）；
`/settings --chat` 打开配置对话页（`omnicrawl-config-chat`），保存后由宿主重建工具表并下发内核；
`/advisor` 由宿主同步顾问选项并重建工具表（写盘由命令层负责，宿主重建失败时回滚运行期选项）；
`/memory:clean` 由宿主按三个作用域调 `omnicrawl-session` 的过期清理，不需要新协议入口。

`/workspace` 的已接线范围是窄口径（见上文「工作区切换」）。仍缺、且已在 Python 侧有对等实现的两项：

- **切换前的子 Agent 排空与 pending worktree 阻止**：Python 先 `coordinator.cancel_and_wait`（不合作就拒绝切换、
  保持旧状态），再拒绝仍有未处理 worktree 的切换（避免新项目的父 Agent 仍能 apply 旧仓库分支）。
  Rust 侧决策层已就位（`subagent_drain_error` / `pending_worktrees_error`），但宿主还拿不到
  “当前会话的 pending worktree”清单，因此未挂。当前只拒绝“回合进行中”的切换。
- **`workspace.switch.error` 钩子**：`workspace.switch.before` / `after` 已由 `PluginHost::switch_workspace`
  发出；准备阶段失败时的 `error`（notify 类）还没有调用点。

`/plan` 已接线：`TuiHostAgent::activate_mode` → `App::command_activate_mode` 重新装配
system prompt（末尾追加 `<active_mode_prompt name="plan">`）与上下文消息，再经
`session.settings` 下发内核；内核拒绝时状态行会写明「只在本宿主生效」，不静默。

### 生成期间的 FIFO 输入队列

对映 Python `_pending_inputs`：回合进行中按 `Enter` 的消息不丢也不并发提交，而是排进队列
（`AppState::pending_inputs`），输入框上方的预览条显示总数、FIFO 摘要与行尾 `[ DELETE ]`；
回合落地（含 `Esc` 取消）后由 `drain_pending_inputs` 按序提交，模态页（设置面板）打开时不排空
（同 Python 的 `len(self.screen_stack) == 1` 守卫）。`/settings` 属于「立即命令」，生成期间也当场
打开面板、不入队；`/undo` 需要回合空闲，因此照常排队。

纯计算（行数、摘要、行序、可撤回/可展开判定）复用 `ui/fullscreen/status/indicators.rs` 的对映层，
`ui/queue.rs` 只负责「对映行 → ratatui 行 + 点击命中区」；预览条占几行由 `pending_queue_rows`
算出后参与 `ui::layout`，鼠标命中与渲染因此共用同一套布局计算（`ui::UiAreas`）。

## 设置面板（`/settings`）

`src/ui/settings/` 对映 Python 的 `ui/fullscreen/screens/`：全屏两区（左侧一级项 + 右侧二级面板）、
准星焦点边框（四角 `⇘ ⇙ ⇗ ⇖`，焦点在哪一栏就在哪一栏）、圆角边框、折叠态下拉是 3 行细边框
（聚焦/展开换白色粗边框）、展开的候选列表是白框浮层且高亮项用琥珀底色。

键位逐项对映 Python：左侧 `↑`/`↓` 移动并实时预览、`Enter`/`→` 进入右侧、`Esc` 退出；
右侧 `Esc`/`←` 先回左栏；上下文页 `Tab` 切换字段，Textual `Select` 的 `Enter`/`↑`/`↓`/`空格` 展开候选，
展开后 `↑`/`↓` 移动、`Enter` 确认并立即保存、`Esc` 收起（单选页与上下文页共用这套键位）；
工具开关页 `↑`/`↓` 选行、`←`/`→`/`Enter`/`空格` 切换。

本批落地的一级项与二级面板：

| 一级项 | 状态 | 说明 |
| --- | --- | --- |
| 模型 | 已实现（离线版） | 候选 = config.toml 的 profiles + models.toml 条目合成的渠道（`load_channel_configuration`），显示渠道名、取值是渠道 key；选定后写 `llm.active_model`（legacy 配置写 `llm.model`）并把模型 id + 整条渠道（Provider/协议/基地址/凭据变量名）推给内核，本会话即刻生效。**未迁**：Python 那套双列选择器（左列渠道 + 右列远端自动发现的模型） |
| 模型渠道 | 已实现 | 渠道列表 + 单条渠道表单（渠道名称 / Provider / 请求协议 / Base URL / **API Key** / API Key 环境变量 / 模型 ID / User-Agent（可选） / 启用，标签与字段口径对齐 Python 渠道编辑器）：列表 `↑↓` 选、`Enter` 编辑、`N` 新建（草稿在 Ctrl+S 前不落列表）、`D` 删除（至少留一条）；表单 `↑↓`/`Tab` 换字段、`Enter` 文本字段进输入态 / 枚举展开候选 / 开关就地翻转、`Ctrl+S` 保存、`Esc` 丢弃并返回。保存走 `save_channel_configuration`（config.toml 与 models.toml 原子写 + 失败回滚），随后重新解析模型视图并把新渠道推给内核。**API Key 行**：只渲染掩码（`（未配置）` / `****` / `****…末 4 位`），输入态从空开始、**留空＝不改动**（误触不会抹掉已存密钥），填了就把内联 `api_key` 写进 config.toml（Python 编辑器同能力），宿主起内核时再把它注入子进程环境。**未迁**：多列宽表单与鼠标交互（Python 的渠道编辑器强制填 API Key，Rust 允许留空走环境变量——用户确认的差异） |
| 上下文 | 已实现 | 两个下拉：上下文长度（32K–2048K，折算到最近档）与压缩阈值（5%–95%，5% 一档，按当前窗口换算 Token）；写回 `llm.context_window_tokens` 与 `context_compaction.trigger_context_*` |
| 推理强度 | 已实现 | 六档（关闭/低/中/高/超高/最大）；写回 `llm.reasoning_effort`，并经 `session.settings` 推给内核的生成选项 |
| 思考显示 | 已实现 | 开启/关闭；写回 `ui.show_thinking`，本机消息流立刻按它过滤思考段（关掉时思考段整段不出现） |
| 记忆功能 | 已实现 | 开关；写回 `memory.enabled` 并立刻重建工具表（记忆四件套整组进/出表） |
| 插件功能 | 已实现（写配置） | 写回 `plugins.enabled`；插件运行期在内核（它拉起独立 Node 插件宿主），协议上没有运行期开关，状态行明确写「重启后生效」 |
| 工具设置 | 已实现（内置工具开关节） | 逐工具启用/关闭，写回 config.toml 的 `tools` 段，随即重建宿主工具表（禁用的工具不进声明，模型不可见即不可调）；「（未注册）」标注对映 Python |
| 顾问设置 / 工具输出压缩 / 消息脱敏 / 持续运转 / 隔离工作区 / 图像生成 / TTS / 视觉 / 子任务设置 / MCP | 已实现 | 各面板的落点与键位见 `src/ui/settings/mod.rs` 的模块注释与各面板实现；MCP 另有一条写端点（`PUT /settings/mcp`）可在运行期重连 |

TTS 页比 Python 面板多 7 行（Python 侧没有接口合成）：**合成后端**（接口 / 本地，后端行会写明
当前选了哪个、本地推理本构建是否编译、接口密钥是否已配）、**接口地址 / 模型 / 音色 / 密钥 /
密钥环境变量 / 语速**。这 6 个字符串行直接编辑，语速按候选档位循环。两条约束值得记住：

- 发布构建不带 `onnx`（`omnicrawl_tts::local_engine_available() == false`）时，后端行**强制停在接口**
  且不响应切换——不允许用户在界面上配出跑不通的组合；
- 密钥行只展示末 4 位（`****…1234`），且**空的输入不修改**已存密钥（想清空请改配置文件），
  避免误触抹掉凭据。

面板保存时同时写 `[tts]` 与 `[tts_api]` 两段（接口段里界面上没编辑的 `response_format` /
`timeout_seconds` 按磁盘原值保留），随后重建工具表让新后端即时生效。
| 通过对话修改设置 | 已实现 | 本行是**动作行**：`Enter`/`→` 关闭设置面板并打开配置对话弹层（与 `/settings --chat` 同一入口）。一句话经 `omnicrawl-config-chat` 的本地路由器折成命令，全部校验通过后原子写盘并同步运行态；不经过模型、不带上下文 |

### Provider 配置接线（`initialize.model`）

握手不再发空壳：没显式给 `--base-url`（或 `OPENAI_BASE_URL`）时，`initialize.model` 的
Provider、协议、基地址、凭据变量名、生成选项（推理强度/温度/最大输出/请求超时/重试/provider_options）
与上下文窗口全部来自 `config.toml` + `models.toml` 的解析结果（`load_llm_config` / `load_channel_configuration`），
独立运行不必再靠命令行参数喂模型配置。

显式给了基地址时视为「外部渠道」：Provider/协议/生成选项/超时/重试/凭据变量名一律用命令行给的值，
内核按运行时默认语义发请求——这条规则让「传给测试回环服务端的那套参数」保持完全确定，
也避免把配置里那条渠道的协议塞给另一个端点。`--model` 始终优先（它是必填项）。

凭据交付：协议帧只带**变量名**（`KernelModelConfig.api_key_env`），内核的 `read_api_key` 只读环境，
而 `config.toml` 里的字面 `api_key` 不在环境里——所以 `prepare_startup` 在起内核时把它按同一个名字
注入子进程环境（`kernel_credentials_env`）；渠道没写 `api_key_env` 时按 Provider 默认名下发
（`effective_api_key_env`）。走外部渠道（`--base-url`）时不注入：那种用法下凭据来自用户自己的环境变量，
子进程直接继承。

`build_prompt_cache_identity` 已接线：宿主在 `handshake()` 里按稳定前缀算出七字段身份
（`omnicrawl-host::prompt_cache::build_prompt_cache_identity`）交给内核的 `initialize.model`。
哈希规则与 Python 逐字节对齐（文本 → `sha256`；结构化值 → `python_dumps_compact_sorted` 后再 sha256），
由 `tests/prompt_cache_parity.rs` + `rust/tools/gen_prompt_cache_fixture.py` 钉住（含中文、空数组、
键序与 `.strip()` 四类边界，并复核最终 `prompt_cache_key`）。

`prompt_cache_capable` 取自 `LlmConfig.prompt_cache`（自定义模型条目声明的 `capabilities.prompt_cache`）；
未声明时对应 Python 在未声明能力时的行为：内核 `should_send_prompt_cache_key` 对 GPT 系列有回退分支，
依旧会带上 key；非 GPT 且显式声明能力的 Provider 声明真时才拿到 key。
另一处差异：`active_skill_context_hash` 在握手时按空列表计算（活动 Skill 取决于用户本轮输入，
而 `prompt_cache_identity` 是会话级静态映射），Python 是逐调用计算。

应用路径分三层，**不假装即时生效**：

1. 写盘：复用 `omnicrawl-config` 的 `save_context_window_tokens` / `save_context_compaction_trigger_percent` /
   `save_tool_switch` / `save_reasoning_effort` / `save_show_thinking` / `save_feature_enabled`，
   不做第二套 TOML 读写；上下文页按 Python 的口径「先写窗口、再写阈值，第二段失败就把窗口改回旧值」，
   不留「新窗口 + 旧百分比」的自相矛盾配置。
2. 宿主侧：工具开关与记忆开关立刻重建工具表（`rebuild_registry`）；重建时把旧表的 `MonitorManager` 与
   `CancelToken` 带过去，否则一次开关会把在跑的后台任务从宿主账上抹掉。思考显示改的是界面状态本身。
3. 内核侧：发协议 v1 的 `session.settings` 做热更新（模型与整条渠道、工具声明、上下文窗口与压缩阈值、
   推理强度）。内核拒绝（如未持有模型配置）时，状态行如实追加「内核未接受即时更新（原因），将在下次会话生效。」
   ——配置已落盘、宿主侧已生效，不回滚也不掩饰。没有对应协议字段的项（思考显示、记忆/插件开关）不发帧。

已知差异：左栏列表与工具列表超出可视高度时按选中项滚动，不做 Python 的滚动条与实际滚动动画；
右侧面板状态文本按面板各自保存（Python 是每个面板实例各存一份，效果一致）；
跨栏鼠标的悬停是**加下划线**而不是 Textual 的底色变化（面板已用琥珀色表示选中态，再叠底色会与选中态混在一起）。

**鼠标点选（设置批）**：`ui/settings/hit.rs` 定了三种可点动作——左栏一级项 `Row`、右栏行 / 字段
`PaneRow`、展开浮层的候选 `Option`。各页在画每一行之前把区域记进 `SettingsState::record_hit`，
事件层用 `hit_at` 按落点反查；这与工作台「渲染与命中共用同一套区域计算」同源，但设置页一页一种
布局，把几何记在画它的那一刻比再描述一遍更不容易失同步（后画的区域优先，所以浮层盖住面板时
命中的是浮层）。

- 左栏：单击即切页并进入右栏（等价 `↑`/`↓` + `Enter`；切页不写配置，但「通过对话修改设置」
  这一行的 `Enter` 就是打开配置对话，点它等同于此）；
- 右栏：首次点击只把该页的选中行 / 字段移到落点，**再点当前行**才等同 `Enter`——工具开关、
  枚举循环这些行被误触一次就是一次配置变更；
- 浮层候选：单击即选中并确认（浮层本来就是为这一次选择展开的），包括渠道表单的内联候选；
- 悬停：`App::handle_settings_mouse` 只记录动作，加亮在渲染末尾统一涂（按行而不是按格：中文标签
  的续格 `skip` 为真、刷新时会被跳过）；
- 未接线：设置页各页的滚动仍然只跟随选中项，滚轮不接管。

**工具执行**：已接入三十个真实执行体（`read` / `read_image` / `image_gen` / `tts_synthesize` /
`write_file` / `Edit_file` / `bash` / `powershell` / `monitor` / `list` / `find` / `grep` / `git` / `kb_*` /
`memory_*` / `web_search` / `fetcher` / `windows_*` / `advisor`）与内核自持的
三个工具（`update_todos` / `ask_user` / `pause_work`）；`manual` 模式下逐个确认，**审批完成后同批并发执行**，
结果按模型调用顺序回填。批准后真的会读写文件、真的会起进程、真的会发包。执行层的两条兜底：

- **批次截止时间**：整批共用一个绝对截止时间（`--tool-timeout`，默认 600 秒，与 Python
  `AGENT_TOOL_TIMEOUT_SECONDS` 同义），到点把仍未完成的调用写成超时观察、收口运行中的工具卡并提示用户，
  后台线程继续跑但结果被丢弃——单个卡死的工具不会把回合一挂不起；
- **panic 兜底**：执行线程里的 panic 会转成一条 `tool_panicked` 失败观察，避免线程静默消失让内核
  永远等一个不会来的 `tool.batch` 响应。

**尚未实现（按优先级）**：

1. 工具层收尾：记忆工具已按配置进出表——开关与 Python 同源（`load_feature_enabled("memory", true)`，
   缺省开启）；作用域里项目级与用户级常开，**会话级不开**（内核自持会话时宿主拿不到 session id，
   强行开启只会让模型调 `scope="session"` 时拿到「未启用」）；
   视觉路径：`read_image` 始终在工具表里（与 Python `_build_tools` 一致），图片的去向由路由决定——
   打开 `--native-vision`（或 `OMNICRAWL_NATIVE_VISION`）时图片直送主模型（握手随
   `initialize.model.native_vision` 告知内核，内核不再走代理）；未打开但配了 `[vision]` 时图片
   交给独立视觉模型代理分析、结论作为不可信观察回填；两者都没有时图片不进请求，主模型只收到
   图片元数据。Python 侧「未显式配置时回落运行时模型能力」的判定需要模型能力表，本 crate 目前没有，
   因此默认关闭而不是自动判断；
   顾问的已知差异：顾问看到的「工作分支」由对话记录（user/assistant 文本）投影而成（Python 用 turn 级注入的完整工作消息），
   顾问模型/凭据来自 `--advisor-*` 与 `OMNICRAWL_ADVISOR_*`（基地址与凭据变量默认回落主模型），
   推理强度按 `--advisor-effort` 直接下发；
   Windows 桌面工具的已知差异：截图 PNG 由 Rust 侧编码器生成（与 Python 的 GDI+ 编码字节不同，尺寸/阈值行为一致），
   截图固定落在工作区 `.omnicrawl/.agent_tmp/images/`（Python 由工作区配置提供目录），
   `windows_control` 依赖系统 Windows PowerShell 5.1（回退 `pwsh`），UI Automation 超时 30 秒；
   图像生成的已知差异：配置来自 `--image-gen*` 开关与环境变量（Python 读 config.toml 的 image_gen 段）；
   未启用 / 缺 API Key 的文案只保留共同前缀（Python 引导去它的设置面板）；HTTP 错误报
   `图像生成请求失败：HTTP <码>：<响应体摘要>`（Python 直出 SDK 异常文本）；默认输出目录以工作区为基准
   （Python 是相对进程 cwd，正常启动下两者一致）；
   联网工具的已知差异：`fetcher` 的 `impersonate`（浏览器 TLS 指纹）已**生效**——默认传输是
   wreq + wreq-util 设备档案，不再退回库指纹；族名到具体档案版本的映射取各族最新档案
   （与 curl_cffi 同名别名指向的版本可能不同），构建环境要求见 `rust/docs/python-free-build.md`；响应体按 UTF-8 宽容解码（非 UTF-8 页面可能替换字符），
   传输层错误文案按「超时 / 连接 / 证书」三类归纳（Python 直出底层异常类名）；
   搜索侧已知差异：Windows 下 glob 展开按大小写敏感匹配（Python 的 `glob` 走 `normcase`），
   目录条目的排序在两侧可能不同（Rust 侧无法设置目录 mtime，对照数据集对这类用例只比对条目集合）；
2. ~~`monitor` 任务的界面展示~~（已完成）：`src/monitor.rs` 对映 Python 的
   `ui/fullscreen/support/monitor.py`——本界面自持日志消费游标，主循环每帧调
   `App::tick_monitor_events` 并按 `MONITOR_POLL_INTERVAL`（0.5 秒，与 Python 同值）节流，
   把增量批次按 `Monitor · id · status` + `[流] 文本` 渲染成可折叠的工具卡（`call_id`
   加 `monitor:` 前缀，与真实工具调用区分）；单任务取不到（已被回收）静默跳过且不推进
   游标，下一轮重试。工作区切换的暂停/恢复（`suspend_for_workspace_switch` /
   `resume_polling`）已随切换接线：`/workspace` 提交前暂停并废弃旧工作区游标，提交完成后恢复；后台进程的
   kill-on-close Job 已与 `command` 工具对齐（见「Windows Job Object」段）；
3. ~~`read` 的 `function_name` 定位（AST 与声明括号扫描）与 `omnicrawl://docs/` 内置文档~~
   （已完成）：`.py` 走缩进块解析（限定名、装饰器起始行、嵌套类/函数、多匹配歧义、语法
   错误回退），其它语言走声明正则等价的大括号扫描；`omnicrawl://docs/<name>.md` 直接读
   `omnicrawl-mcp` 的编译期内嵌文档表并套用同一套行窗口/片段渲染。对照数据集见
   `tests/fixtures/workspace_tools_parity.json` 的 `locators` 段（11 例）；
4. ~~FIFO 输入队列、鼠标滚轮/点击展开工具卡正文~~（已完成）：运行中 `Enter` 进队列、回合落地
   后按序提交，预览条行尾 `[ DELETE ]` 点击撤回、提示行点击展开/收起；鼠标滚轮滚动消息区、
   点击工具卡省略提示行展开完整正文、展开态点击卡片收起、点击思考段切换折叠（见上文「生成期间的
   FIFO 输入队列」）。命中判定与渲染共用 `ui::layout`，因此缩放窗口后点击位置不会错位；
5. ~~Markdown 语法高亮~~（代码块已按语言逐 token 高亮，见 `rendering/highlight.rs`；仍非 pygments 等价，差异见上文「已知差异」）；~~子任务进度树~~、~~斜杠命令的补全菜单与命令分派~~（已完成，见上文「输入批 / 斜杠命令接线」）。
   斜杠命令的可用边界：`/settings`、`/quit`、`/approval*`、`/reasoning`、`/model`、`/plugins`、`/skills`、`/tasks`、`/task`、`/undo`、`/compact`、
   `/new`、`/sessions`、`/resume`、`/archive`、`/rename`、`/history`、`/review`、`/plan`、`/workspace` 能真正执行；
   `/mcp` 与 `/settings --chat` 已接线（见上文）；
   菜单不列运行期 Skill 候选（内核侧发现，宿主给不出同一份清单）；设置面板的一级菜单全部落地
   （模型、模型渠道、上下文、工具、推理强度、思考显示、记忆、插件、顾问、工具输出压缩、视觉、
   图像生成、TTS、持续运转、隔离工作区、消息脱敏、子任务、MCP、通过对话修改设置）；
6. ~~Windows 输入自愈、窄屏 HUD 弹性收缩~~（已完成）；会话与模型选择仍待做：
   自愈由 `TerminalGuard::heal_if_needed` 每秒核对一次控制台模式（目标值 = crossterm 鼠标捕获的
   `0x0098`，输出侧重开 VT 处理），恢复后重发鼠标与焦点报告序列，并对提示做一分钟限流；
   顶部的两行 HUD 已改为 Python 当前的底部单行轮播（见上文「已知差异」与「已实现」）：
   超长值仍经 `compact_hud_value` 保留首尾，窄屏由轮播行的显示宽度截断（`…` 收尾）；
7. `model.reply` 代答路径（内核自带 provider runtime 后不需要，当前显式回 `-32601`）；
8. ~~非 Windows 平台的进程树回收~~（已完成，见下）；
9. ~~Windows Job Object 的 kill-on-close~~（已完成）：`omnicrawl-host` 的
   `process_control::KillOnCloseJob` 把子进程纳入 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job，
   `bash` / `powershell`（`tools/command.rs`）与 `monitor`（`tools/monitor.rs`）两条路径都在 spawn
   后立即纳入；Job 句柄随任务存活、终态或停止时关闭，宿主崩溃时由操作系统关闭句柄递归回收，
   消除了「杀树时最内层刚出现」的孤儿窗口。Unix 侧仍是自成进程组 + `kill(-pgid, SIGKILL)`。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p omnicrawl-cli          # 端到端用例需要内核二进制
python rust/tools/gen_tui_tools_fixture.py   # 改了工具语义时重生成对照数据集
python rust/tools/gen_latex_fixture.py       # 改了 LaTeX 语义时重生成对照数据集
python rust/tools/gen_tool_diff_fixture.py   # 改了工具卡标题/正文渲染时重生成对照数据集
cargo test -p omnicrawl-tui
```

分片到达节奏（排障用）：内核 `ProtocolSink` 每收到一个模型分片就发一条 `turn.delta` 并立即 flush
（`Conn::send` 每帧 `write_all + flush`），宿主读线程逐行转发、界面每 50 ms 排空一次——**链路本身
不做批处理**。本地复现：起一个分片间带延迟的假 SSE 端点，直接驱动内核二进制并打印 `turn.delta`
的到达时刻（实测端点 450 ms 间隔 → 落点 94/547/1000 ms）。若界面上正文「整块一次性出现」，先怀疑
网关把 SSE 攒成一整块，或模型先长时间思考再一次性吐正文（底行轮播的 `t/s` 会在末尾突刺）。

测试分五层：

- 模块内单测：状态聚合、输入编辑（含斜杠命令菜单的筛选、补全与参数字段）、轮播装配与底部行截断、消息流各消息类型（用户标签/思考底色/工具卡无边框 + 正文来源与折叠规律）、面板高度、路径安全、命令采样、工具执行体、命令能力面与插件行映射；
- `tests/workspace_tools_parity.rs`：与 Python 真实现的对照（声明逐字、read/write/edit 用例、采样、
  已记录的定位缺口）；
- `tests/search_tools_parity.rs`：list / find / grep / git 的对照（mtime 钉死、落盘随机文件名归一、
  目录排序只比集合）；
- `tests/latex_parity.rs`：LaTeX 转换层与 Python 真实现的逐字对照（107 例转换 + 9 例块级分段 +
  26 例块级公式快判，含 `$$`/`$$$`、未闭合块级、末尾反斜杠、超长公式一类边界）；
- `tests/tool_diff_parity.rs`：工具卡渲染层与 Python 真实现的对照（14 例标题 + 11 例正文 +
  3 例纯文本标题，每条都比对纯文本与 `(样式, 文本)` 运行段——diff 的 `+/−` 着色与
  「read / 记忆 / 知识库正文为空」两个约束都在运行段里）；
- `tests/host_flow.rs`：脚本化假内核驱动完整宿主流程（握手、审批、真执行、提问、拒绝、慢工具超时收口、
  后台命令监控的 start/poll/stop 三批、内核退出、斜杠命令分派：菜单补全→`/settings` 打开面板、`/quit` 退出、
  未支持命令给出原因而不发 `turn.submit`、`/tasks` 查询内核回执、`/undo` 异步下发与回执回填）；
- `tests/render_smoke.rs`：`TestBackend` 渲染断言底部轮播/消息流（用户标签行、工具卡、计划条）/面板/输入卡与光标位置、滚动窗口与命令菜单（菜单紧贴输入卡上方）；
- `tests/settings_screen.rs`：设置面板的 `TestBackend` 回归——两栏与准星边框随焦点转移、上下文候选
  下拉（含高亮底色）、工具开关行状态文本、状态行回填与内核拒绝提示、窄屏左栏收缩、极窄极矮不 panic；
- `tests/kernel_e2e.rs`：真内核 + 本机回环模型服务端。六个用例让模型**真的请求**工具：
  `read` / `Edit_file` / `grep` / `git status` 直接执行并断言磁盘与搜索结果回填，`bash` 走人工批准，
  另有纯文本回合；请求体里核对工具输出与已声明工具名。
