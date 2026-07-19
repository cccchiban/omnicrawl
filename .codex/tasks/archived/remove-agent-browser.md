# 任务：移除 Agent 浏览器能力

状态：已完成
创建：2026-07-18
更新：2026-07-18

## 已确认范围

- 删除 Host 内置 `bb_browser_cli` 及 `BBBrowserCLI` 实现。
- 删除项目级 `.agents/skills/bb-browser/` 与 `.claude/skills/bb-browser/`。
- 从 npm 清单、锁文件和本地 `node_modules` 移除 `bb-browser`。
- 清理关联测试、SubAgent 配置、系统提示词和文档。
- 保留 `web-fetcher`、业务爬取、`display_html`、Windows 桌面工具以及非 Agent 浏览器用途的代码。

## 完成内容

- [x] 盘点代码、依赖、Skill、测试和文档
- [x] 用户确认破坏性删除范围 1A + 2A
- [x] 删除 `omnicrawl/agent/browser_cli.py` 与 Host 注册、调用、别名和生命周期代码
- [x] 删除 `.agents/skills/bb-browser/`、`.claude/skills/bb-browser/`
- [x] 卸载本地 `bb-browser`；因其为唯一根 npm 依赖，删除空的 `package.json` 和 `package-lock.json`
- [x] 删除专用测试并调整审批、初始化、模块边界、工具注册测试
- [x] 清理 README、MCP 文档、系统提示词、SubAgent 定义和重构文档
- [x] 验证无运行时残留且保留指定能力

## 验证

- 浏览器删除契约：实现文件、专用测试、两份 Skill、本地 npm 包及可执行文件均不存在。
- 残留扫描：运行时代码和文档中无 `bb_browser_cli`、`BBBrowserCLI`、`bb-browser`、`BB_BROWSER_COMMAND` 残留；唯一保留引用是回归断言 `assertNotIn("bb_browser_cli", tools)`。
- `python -m compileall -q omnicrawl tests`：通过。
- `git diff --check`：通过。
- `python -m unittest discover -s tests`：633 项测试通过。
- 删除后关联定向测试：74 项通过。
- 独立审查：无 blocker / major；发现的 README 陈旧说明已删除。

## 影响与保留边界

- Agent 不再提供真实浏览器、登录态页面、点击、表单、CDP、网络观察或 site adapter 自动化能力。
- `web-fetcher`、OmniCrawl 业务爬取、公开 HTTP/API 数据入口、`display_html`、Windows 桌面工具和截图能力保持不变。
- 根目录 `node_modules` 中其他既有缓存/孤立包未扩大范围删除；已确认不存在 `node_modules/bb-browser` 和对应 `.bin` 可执行文件。
