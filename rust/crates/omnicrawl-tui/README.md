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
| `src/kernel.rs` | 内核进程客户端：NDJSON 帧读写、读线程、请求/响应配对 |
| `src/app.rs` | 接线层：帧 ↔ 状态机 ↔ 写回内核 |
| `src/state.rs` | 状态机：消息记录、输入框、遥测、批次挂载 |
| `src/host.rs` | 宿主侧工具批次：审批/提问判定、执行派发、观察构造 |
| `src/tools/tts/` | TTS 纯逻辑层：`TtsConfig`（`tts/config.py`）、`normalize_tts_text` 管道（`tts/normalize.py`：保护 URL/路径/日期、清理 emoji 与装饰符、分隔符与空白归一化、结构标点与重复标点收敛、逐行补终止标点）、`audio`（`tts/audio.py`：8/16/24/32 位 PCM 读取、线性重采样、参考音频声道转换、16 位 PCM 写出且字节与 Python 一致）、`voices`（`tts/custom_voices.py`：音色名校验、自定义音色库读写、manifest 内置音色行） |
| `src/tools/` | 工作区工具执行体：`paths`（保护路径）、`read`、`read_image`（本机图片与视觉附件）、`image_gen`（图像生成与编辑）、`write`、`edit`、`command`、`monitor`（后台命令）、`finding`、`grep`、`listing`、`git`、`knowledge`（知识库）、`memory`（三处作用域记忆）、`web_transport`（共享 HTTP 传输/代理/重定向）、`web_search`（三引擎搜索）、`fetcher`（网页抓取与正文提取）、`sample`、`declarations`、`registry` |
| `src/ui/` | 渲染：`hud.rs`、`conversation.rs`、`composer.rs`、`panels.rs` |

## 工具执行层

工具表、参数归一化、Schema 校验与压缩复用内核已搬好的 `omnicrawl-controllers`（数据来自
`omnicrawl/agent/toolkit/tools.py`）；本 crate 负责**执行体**与声明生成：

| 工具 | 状态 | 说明 |
| --- | --- | --- |
| `read` | 已实现 | 行窗口、超长行截断、行号与续读 footer；`text` 片段定位已实现 |
| `read_image` | 已实现 | 本机图片读取：拒绝 URL/data URI、相对路径不得越出工作区、按签名识别 PNG/JPEG/GIF/WebP、Base64 编码为视觉附件；`--native-vision` 打开后图片作为下一条观察注入模型请求（默认关闭则只回文本载荷） |
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
| `memory_search` / `memory_read` / `memory_expand_related` / `memory_write` | 已实现（默认不进表） | 按 `scope` 路由到三处记忆目录，读写交给 `omnicrawl-session` 的 `MemoryStore`；`memory_enabled` 打开后整组注册，未启用的作用域回「X 级记忆系统未启用。」 |
| `web_search` | 已实现 | Bing / DuckDuckGo / 雅虎三引擎：桌面浏览器请求头、端点与查询参数逐字对齐、正则解析结果页（含雅虎 `RU=` 与 DDG `uddg=` 跳转还原）、验证码/异常流量如实报错、网络错误重试 |
| `fetcher` | 已实现 | 多 URL 并行抓取、手动跟随 301/302/307/308 与 `<meta refresh>`、内网/本机目标直连、`insecure=true` 跳过证书校验、正文提取（`main` → `article` → `body`，剔除脚本样式）且 HTML5 容错解析 |
| `windows_window` / `windows_control` / `windows_input` / `windows_clipboard` / `windows_screenshot` | 已实现 | 整组注册：窗口枚举/详情/前台激活、控件 UI Automation（Windows PowerShell）、受约束的 SendInput 键鼠、剪贴板文本读写、桌面/区域/窗口截图（GDI 抓屏 + 缩放 + PNG，>5MiB 继续缩小）并作为视觉附件回模型 |
| `advisor` | 已实现 | 零参数顾问：判定与分支裁剪复用内核 `controllers::advisor`，运行期用独立 LLM Runtime 做单轮无工具补全（系统提示词取自 `templates/advisor_system.md`），返回 plan/correction/stop 指导；只在 `--advisor-model`（或 `OMNICRAWL_ADVISOR_MODEL`）给出时进表 |
| `update_todos` / `ask_user` / `pause_work` | 已实现 | 由界面侧判定与面板交互 |
| `tts_synthesize` | 未接入 | 纯逻辑层已落地并对照（文本归一化 50 例 + 音频 I/O 与声线库 27 例）；缺 ONNX 运行时（`ort`/`tract`）、MOSS-TTS 模型文件与 sentencepiece，因此工具本身尚未进表 |
| `subagent` | 由并行内核侧改造覆盖 | `controllers/subagents/*` 与本 crate 的 `subagent_types` 接线正在推进中，本 crate 不重复开工 |

声明由工具表生成：`read` / `Edit_file` / `write_file` 的参数契约在 Python 侧写的是**示例值**，
`tool_parameters_schema`（原在未搬的 `agent/runtime/llm_protocol.py`）按示例推断类型并加
`minProperties` 兜底，本 crate 照搬了这一层，声明与 Python 逐字对齐（有对照数据集钉住）。

搜索类工具用 `ignore` crate 遍历：读 `.gitignore` / `.ignore` / `.rgignore`（非 git 目录也读）、
保留隐藏文件、剪枝受保护路径——与随包 Go 扩展（`native/ocsearch`）声明的语义一致，纯 Rust、无外部二进制。

审批语义与 Python `_approve_tool_call` 对齐：`manual` 模式**只对 shell 命令（`bash` / `powershell`）与
非只读 git 操作弹确认**，文件、搜索与后台监控类工具直接放行（`auto` 全部放行）。同一批审批完成后工具并发执行，
最后按模型调用顺序回观察（顺序与数量都不能变）。`Esc` 取消会先回收正在跑的进程树。

`monitor` 的后台进程归当前回合：批次派发时记下回合号，`Esc` 取消该回合时只回收本回合启动的任务
（事件里写明「当前回合已取消，后台任务已强制终止。」），宿主退出时回收全部任务。输出只在内存环形缓冲里，
不写持久化日志；模型用游标增量轮询。已知限制：Windows 上停止进程树走 `taskkill /T`，MSYS bash 在
启动瞬间会连起三层进程，若杀树发生在最内层出现之前，那次调用可能留下一个孤儿（Python 侧靠 Job Object 的
kill-on-close 规避，Rust 侧要同样的语义需要引入 Win32 绑定）。遇到这种情况宿主仍会在 5 秒内把任务落成
终态（与 Python 的 `reader.join(timeout=5)` 一致），不会让回合无限期挂在 `running` 上。

## 运行

```bash
cargo build -p omnicrawl-cli              # 先有内核二进制
cargo run -p omnicrawl-tui -- --model deepseek-v4-flash --session-root ../.agent_sessions
```

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--kernel <路径>` | `$OMNICRAWL_BINARY` → 同目录 `omnicrawl` → PATH | 内核可执行文件 |
| `--model <名称>` | `$OMNICRAWL_MODEL` / `$OPENAI_MODEL` | 必填 |
| `--base-url <地址>` | `$OPENAI_BASE_URL` | 模型接口基地址 |
| `--api-key-env <变量名>` | `OPENAI_API_KEY` | 凭据只给环境变量名，不进帧 |
| `--session-root <目录>` | 空 | 给了就让内核自己持有会话（转录与压缩） |
| `--context-window <N>` | 空 | HUD 上下文占用条的分母 |
| `--approval <manual\|auto>` | `manual` | `manual` 下非自持工具先弹确认 |
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
| `src/ui/fullscreen/terminal/theme.rs` | `terminal/theme.py` | 取色令牌同名同值 + Rich 风格串解析 |
| `src/ui/fullscreen/status/hud.rs` | `status/hud.py` | HUD 纯格式化（CTX/遥测/状态段、解密扫描帧） |
| `src/ui/fullscreen/status/indicators.rs` | `status/indicators.py` | 轮播状态机与排队预览行（组件热区留给装配层） |
| `src/ui/fullscreen/mod.rs` | | `round_half_even`（对映 Python 内建 `round`） |

已知差异：

- 轮播留言文案（`carousel_messages.txt`）编译期内嵌：Python 从包资源在运行期读取，可随 pip 分发并手工编辑，Rust 侧脱离宿主单文件分发时仍可用但不能在运行期改；
- `decrypt_frame` 的乱码随机源是内部 xorshift64\*（Python 用 Mersenne Twister）：字符集与概率一致，随机序列不同；
- `rgba(...)` 令牌（用户消息背景）只取 RGB 分量：ratatui 无 alpha 混合，Python 侧由 Textual 与终端底色混合；
- 尚未接线：`src/ui/mod.rs` 的渲染入口仍是旧简化页面，对映层按后续批次逐个接入替换。

## 本阶段（骨架）的边界

**明确不在本阶段范围**（写在这里避免误读）：

- **工具未搬完**：已实现 read / read_image / image_gen / write_file / Edit_file / bash / powershell / list /
  find / grep / git / monitor / kb_* / memory_* / web_search / fetcher / windows_* / advisor 与三个自持工具；
  语音合成（`tts_synthesize`）尚未接入（需 ONNX 运行时与模型文件），`subagent` 由内核侧改造覆盖；
- **不替换现有入口**：Python 侧 Textual 工作台（`omnicrawl/ui/`、`omnicrawl` / `main.py` 启动路径）照旧，本 crate 是并列的新二进制；
- **未接入分发**：不参与 `packages/cli` 启动器与 npm 平台分包，本轮只保证 `cargo run -p omnicrawl-tui` 可用。

**已实现**：两行 HUD（工作区/模型/审批模式/队列，上下文占用条与 IN/OUT/CA/tok/s）、
消息流（用户 `$`、思考段折叠为最新五行、正文 `◇`、工具卡边框随状态着色、正文头尾采样限五行）、
单行起步按显示宽度软折行的输入框（五行上限、超出后随光标滚动）、任务清单条、状态行（Braille spinner + `[ ESC ]`）、
审批面板与提问面板、`Esc` 取消、内核退出与终端恢复。

**工具执行**：已接入二十九个真实执行体（`read` / `read_image` / `image_gen` / `write_file` / `Edit_file` /
`bash` / `powershell` / `monitor` / `list` / `find` / `grep` / `git` / `kb_*` / `memory_*` / `web_search` /
`fetcher` / `windows_*` / `advisor`）与内核自持的
三个工具（`update_todos` / `ask_user` / `pause_work`）；`manual` 模式下逐个确认，**审批完成后同批并发执行**，
结果按模型调用顺序回填。批准后真的会读写文件、真的会起进程、真的会发包。执行层的两条兜底：

- **批次截止时间**：整批共用一个绝对截止时间（`--tool-timeout`，默认 600 秒，与 Python
  `AGENT_TOOL_TIMEOUT_SECONDS` 同义），到点把仍未完成的调用写成超时观察、收口运行中的工具卡并提示用户，
  后台线程继续跑但结果被丢弃——单个卡死的工具不会把回合一挂不起；
- **panic 兜底**：执行线程里的 panic 会转成一条 `tool_panicked` 失败观察，避免线程静默消失让内核
  永远等一个不会来的 `tool.batch` 响应。

**尚未实现（按优先级）**：

1. 工具层收尾：`tts_synthesize` 的推理层（需引入 ONNX 运行时 `ort`/`tract` + MOSS-TTS 模型下载 + sentencepiece + 音频播放）；
   `src/tools/tts/` 里的配置、文本归一化、音频 I/O 与声线库已就绪，推理层接上后即可注册工具；
   记忆工具已实现但默认不进表（TUI 尚无记忆配置面，`memory_enabled` 为假）；
   视觉路径的已知差异：`read_image` 的图片只在 `--native-vision`（或 `OMNICRAWL_NATIVE_VISION`）打开时
   注入请求；Python 侧「未显式配置时回落运行时模型能力」的判定需要模型能力表，本 crate 目前没有，
   因此默认关闭而不是自动判断；独立视觉模型代理（把图片交给另一个视觉模型分析、再把结论作为不可信
   观察回填）未接；
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
   联网工具的已知差异：`fetcher` 的 `impersonate`（浏览器 TLS 指纹）参数被接受但**不生效**
   （ureq + rustls 不做 JA3/JA4 模拟），响应体按 UTF-8 宽容解码（非 UTF-8 页面可能替换字符），
   传输层错误文案按「超时 / 连接 / 证书」三类归纳（Python 直出底层异常类名）；
   搜索侧已知差异：Windows 下 glob 展开按大小写敏感匹配（Python 的 `glob` 走 `normcase`），
   目录条目的排序在两侧可能不同（Rust 侧无法设置目录 mtime，对照数据集对这类用例只比对条目集合）；
2. `monitor` 任务的界面展示（Python 的 Textual 工作台会定时轮询并在消息流里显示后台任务，
   本 crate 目前只在模型调用 `monitor` 时以工具卡呈现）；
3. `read` 的 `function_name` 定位（AST 与声明括号扫描）与 `omnicrawl://docs/` 内置文档；
   两者当前返回 `FS_UNSUPPORTED_FEATURE` 明确报错，不静默读错内容；
4. 工具输出预算与落盘归档（Python 侧单工具 50K / 批次 200K，内核已有 `controllers::output` 可复用）；
5. FIFO 输入队列、鼠标滚轮/点击展开工具卡正文；
6. Markdown 与 LaTeX 渲染（现为纯文本）、子任务进度树（现为一行通知）、斜杠命令菜单与设置面板；
7. Windows 输入自愈（锁屏/息屏后恢复控制台模式）、窄屏 HUD 弹性收缩、会话与模型选择；
8. `model.reply` 代答路径（内核自带 provider runtime 后不需要，当前显式回 `-32601`）；
9. 非 Windows 平台的进程树回收（当前只杀直接子进程，需要 setsid/libc 才能回收整个进程组）；
10. Windows Job Object 的 kill-on-close（`bash` / `powershell` / `monitor` 三处共用同一条 `taskkill /T`
    回收规则，Job Object 才能消除「杀树时最内层刚出现」的孤儿窗口）。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p omnicrawl-cli          # 端到端用例需要内核二进制
python rust/tools/gen_tui_tools_fixture.py   # 改了工具语义时重生成对照数据集
cargo test -p omnicrawl-tui
```

测试分五层：

- 模块内单测：状态聚合、输入编辑、HUD 对齐、面板高度、路径安全、命令采样、工具执行体；
- `tests/workspace_tools_parity.rs`：与 Python 真实现的对照（声明逐字、read/write/edit 用例、采样、
  已记录的定位缺口）；
- `tests/search_tools_parity.rs`：list / find / grep / git 的对照（mtime 钉死、落盘随机文件名归一、
  目录排序只比集合）；
- `tests/host_flow.rs`：脚本化假内核驱动完整宿主流程（握手、审批、真执行、提问、拒绝、慢工具超时收口、
  后台命令监控的 start/poll/stop 三批、内核退出）；
- `tests/render_smoke.rs`：`TestBackend` 渲染断言 HUD/消息流/面板/光标位置与滚动窗口；
- `tests/kernel_e2e.rs`：真内核 + 本机回环模型服务端。六个用例让模型**真的请求**工具：
  `read` / `Edit_file` / `grep` / `git status` 直接执行并断言磁盘与搜索结果回填，`bash` 走人工批准，
  另有纯文本回合；请求体里核对工具输出与已声明工具名。
