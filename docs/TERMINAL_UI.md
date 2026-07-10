# 终端 UI 设计说明

OmniCrawl 使用**增强型内联终端 UI**：所有对话、工具记录和确认记录保留在终端滚动历史中，不接管全屏缓冲区。该选择优先保证 Windows Terminal、PowerShell 和 VS Code 集成终端中的稳定性，避免全屏控制、滚动和窗口缩放互相干扰。

## 设计目标

- **现代深色专业感**：采用低饱和深色终端主题中的语义色，主色仅用于当前焦点、模型输出和关键操作；边框、分隔和辅助内容保持弱化。
- **信息层级明确**：用户输入、AI 输出、工具执行、等待状态和人工确认具有稳定且不同的视觉语义。
- **稳定优先**：不依赖已滚动历史的相对行数重写；已提交输入、工具结果和确认选择均追加为可回读记录。
- **窄屏可读**：启动面板、工具参数、确认内容、预输入和等待状态按真实终端列宽换行，不使用虚构的最小宽度。
- **无新增渲染依赖**：继续使用标准库和 ANSI SGR，课程环境可直接运行。

## 视觉与交互约定

```text
  █ OmniCrawl
  ─────────────────────────────
  模型
  ▸ thinking  [已启用]，推理强度：xhigh
  审批
  ▸ approval  人工确认
  输入问题开始对话 · /help 查看命令

▸ 石狮市的天气怎么样

◆ 天气查询思路
  • 抱歉，我无法实时查询天气信息。

  ◌ 步骤 2 · run_command
  │ ▸ command  echo hello
  │ ✓ 成功 · 退出码 0
  ╰─ hello
```

| 内容 | 表示方式 | 设计意图 |
| --- | --- | --- |
| 用户输入 | `▸` | 保留用户输入原样，不在提交后回跳替换。 |
| AI 输出 | `◆` 与两列续行缩进 | 让连续回答易于扫描。 |
| 等待状态 | 静态 Braille 帧、弱化文字与耗时 | 提供反馈，不使用终端兼容性不稳定的闪烁效果。 |
| 工具执行 | `◌ 步骤 N · 工具名` | 紧凑显示次级执行信息；完成结果追加记录。 |
| 成功/失败 | `✓` / `✗` + 语义色 + 文本 | 不只依赖颜色表达状态。 |
| 人工确认 | `⚠ 确认执行` 卡片、`Enter/Y 允许 · N 拒绝` | 卡片按真实宽度换行，最终选择追加留痕；方向键不会直接执行。 |

当 `approval.mode` 为 `manual` 时，受限工具显示确认卡；`auto` 与 `review` 仅显示步骤和执行结果。输入 `/approval:manual`、`/approval:auto`、`/approval:review` 可切换模式。

## 稳定性策略

1. **能力检测**：只有 `stdout` 为交互式 TTY 且未设置 `NO_COLOR` 时才启用 ANSI；重定向输出不会产生控制序列。
2. **Unicode 单元**：宽度、截断与换行将 ZWJ emoji、旗帜、肤色修饰、键帽和变体选择符作为不可拆分显示单元；Windows `msvcrt` 输入会先合并 UTF-16 代理对，再执行移动、删除和换行，复杂文本仍走追加式流渲染，避免列宽回退残字。
3. **追加而非回跳**：已提交输入、确认框和工具结果不再依赖 `CSI n A` 回到可能已被滚动或 resize 改变的位置。等待和正在编辑的输入区仍只在当前动态区域内清理。
4. **真实尺寸**：所有可换行组件以当前终端实际列宽计算物理行数；窗口变窄时优先折行、缩短装饰，而不是溢出。
5. **串行输出**：模型回调、spinner 和动态输入栏共享 `TerminalUI._lock`，避免并发输出打散行结构；spinner 与 token 状态均先按未着色文本折行，再让每个物理行独立闭合 ANSI SGR，避免样式跨行泄漏。

## 文件边界

- `omnicrawl/ui/tui/__init__.py`：终端能力、颜色、显示宽度、Markdown、工具记录、确认卡、状态栏、输入栏和 `TerminalUI`。
- `omnicrawl/ui/__init__.py`：Windows 行内输入编辑、补全菜单、对话编排与 Windows 启动支持。
- `tests/test_terminal_ui.py`：终端样式、窄屏、Unicode、确认、工具、spinner 和回归测试。
- `tests/test_inline_input.py`：输入编辑、删除键和历史记录测试。
- `docs/TERMINAL_UI.md`：本文档。

## 兼容范围与限制

- 验收目标为 Windows Terminal、PowerShell 和 VS Code 集成终端（ANSI/UTF-8 已启用）。
- 非 ANSI、重定向、`TERM=dumb` 或 `NO_COLOR` 环境退化为纯文本输出，功能不依赖颜色或动画。
- 终端不能控制字体、像素级布局或所有字体对 ambiguous-width 字符的策略；极少见的 Unicode 组合仍采用保守追加输出。
- Windows 下的逐字符输入、历史和命令补全依赖 `msvcrt`；其他平台回退到标准 `input()`。
- 当前 Markdown 渲染覆盖常见标题、列表、引用、代码、表格、加粗、行内代码和链接，不是完整 HTML/CSS 渲染器。

## 验证清单

每次修改 TUI 后至少执行：

```powershell
python -m unittest tests.test_terminal_ui tests.test_inline_input -v
python -m unittest discover -s tests -v
python -m compileall -q omnicrawl main.py
git diff --check
```

手工冒烟时覆盖 20、40、80 列宽；普通输入、长/CJK/emoji 输入；等待、工具成功/失败、人工确认、模型输出、错误和 `Ctrl+C` 路径。
