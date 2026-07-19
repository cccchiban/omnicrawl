# 任务：Windows 原生截图与视觉模型注入

状态：已完成
创建：2026-07-18T19:21:03
更新：2026-07-18T19:56:10

## 需求摘要

- 在现有 Windows 原生桌面 Agent 工具组中新增截图能力。
- 支持整个虚拟桌面、指定区域和指定窗口截图。
- 截图保存为 `.agent_tmp/images/` 下的 PNG，并返回路径与尺寸等结构化元数据。
- 将截图作为下一轮多模态上下文交给支持 vision 的模型，而不是只返回文本路径。
- 保持当前仅 Windows 注册、人工审批、串行执行、敏感边界和跨平台安全导入规则。

## 关键决策

- 用户确认范围：视觉模型直接识别截图（1B），截图目标覆盖虚拟桌面、指定区域和指定窗口（2C）。
- 截图使用 Win32 GDI `BitBlt` 捕获当前可见屏幕像素，GDI+ 编码 PNG，不引入新依赖。
- PNG 默认最大边 2048，并按跨 Provider 保守的 5 MiB 内联限制继续等比缩放。
- 视觉图片通过 `ToolImageAttachment` 只存在于当前 Agent 工具循环；Session 事件和长期历史只保存文本路径/元数据，不保存 Base64。
- 同一工具批次先回填全部 Tool Result，再追加 synthetic user 图片消息，保持 Provider 工具协议顺序。
- 只有当前 Runtime 明确声明 `capabilities.vision=true` 时注入图片；其他模型仅获得文本结果和路径。
- 窗口部分移出虚拟桌面或包含 DWM 桌面外边框时截取可见交集，完全位于桌面外或最小化时拒绝。

## 实现计划

- [x] 1. 梳理 ToolResult、会话历史和各 Provider 的消息转换链路
- [x] 2. 设计并实现 Windows 原生截图、PNG 保存和参数校验
- [x] 3. 接入 Agent 工具注册、审批展示、串行屏障和视觉内容注入
- [x] 4. 补充单元/集成测试与 Windows 本机冒烟验证
- [x] 5. 同步 README、Windows 工具文档并完成自审

## 已修改文件

- `omnicrawl/agent/windows_desktop.py`
- `omnicrawl/agent/types.py`
- `omnicrawl/agent/execution.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/agent/tools.py`
- `omnicrawl/agent/system_prompt.md`
- `omnicrawl/commands/slash.py`
- `omnicrawl/llm/protocol.py`
- `omnicrawl/llm/providers/openai_chat.py`
- `omnicrawl/llm/providers/openai_responses.py`
- `omnicrawl/llm/providers/anthropic.py`
- `omnicrawl/llm/providers/gemini.py`
- `tests/test_windows_desktop_tools.py`
- `tests/test_vision_tool_images.py`
- `tests/test_workspace_switch.py`
- `README.md`
- `docs/README.md`
- `docs/WINDOWS_DESKTOP_TOOLS.md`

## 验证

- `python -m compileall -q omnicrawl tests`：通过。
- `git diff --check`：通过。
- `python -m unittest discover -s tests`：639 项测试通过。
- Windows 本机冒烟：创建内容受控的 Tk 临时窗口，调用 `windows_screenshot(target=window)`，验证 PNG 文件头、450×300 物理像素尺寸、4726 字节，并清理临时截图；通过。
- 独立 Reviewer：未发现 blocker；指出的窗口越界可用性问题已改为截取可见交集，并补回归测试；Responses 图片 detail 和 Session Base64 不持久化测试已补充。

## 残余限制

- `BitBlt` 只捕获当前可见屏幕像素，被其他窗口遮挡的窗口截图会包含遮挡内容。
- DRM、硬件叠加、安全桌面或应用保护内容可能为空白/黑色，工具不会绕过系统保护。
- OpenAI/Anthropic/Gemini 真实外部 API 未使用付费凭据联调；Provider 映射已通过本地契约测试，Gemini 当前 SDK 的 Base64 Blob 解析也已做本机类型验证。
