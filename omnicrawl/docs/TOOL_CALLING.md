# OmniCrawl 工具调用协议

OmniCrawl 的 Provider 工具面固定为两个元工具：

- `search_tools`：搜索当前 Agent 可见的 Host 工具目录。
- `invoke_tool`：按工具名和参数调用 Host 目录中的真实工具。

真实工具的 Python 执行器、完整 Schema、审批策略、MCP 连接和工作区权限不再逐个注册给 Provider，也不再把完整工具目录注入模型上下文。

## 调用流程

```text
模型 -> search_tools(query)
Host -> 候选工具摘要 + 紧凑参数 Schema
模型 -> invoke_tool(tool_name, arguments)
Host -> 解析真实工具 -> Schema 校验 -> 插件钩子 -> 审批 -> 执行
Host -> role=tool 结果
模型 -> 继续调用或输出最终答案
```

## `read_image`

`read_image` 是 Host 侧的只读图片工具，支持 PNG、JPEG、WebP 和 GIF。`path` 可以是当前工作区相对路径或本机绝对路径；不支持 HTTP/HTTPS URL。工具会校验图片文件头，完整读取文件并生成 `ToolImageAttachment`。当当前主模型声明 `vision=true` 时，图片以内联 Base64 观察消息发送给主模型；当主模型不支持视觉且设置中的视觉代理已启用时，Host 会按配置顺序把图片发送给独立视觉模型，视觉模型只返回文本分析，再以文本观察回填给主 Agent；多个视觉模型按顺序故障转移，全部失败时返回明确错误。视觉代理未启用时，非视觉主模型只收到不含 Base64 的元数据结果。按照当前配置，该工具不设置文件大小或图片尺寸上限，因此超大图片可能导致内存占用、Base64 膨胀和模型请求超时。

`windows_screenshot` 使用同一套图片代理路径。视觉模型配置复用现有 `llm.profiles` 和 `models.yaml`，写入顶层 `vision.enabled` 与有序 `vision.models` 引用，不在视觉配置中重复保存 API Key。

图片 Base64 只存在于当前 Agent 工具循环和视觉模型请求，不写入 Session 事件、长期历史或普通工具结果；工具结果和 UI 只保留路径、MIME 类型、字节数及文本分析。该工具仍沿用 Host 的读取审批策略。

`search_tools` 对高匹配度结果返回紧凑 Schema，只保留参数填写所需的字段，例如 `type`、`properties`、`required`、`enum` 和边界约束。长描述、默认值和示例不会进入搜索结果。

## Host 侧边界

`invoke_tool` 的外层 Schema 只保证 `tool_name` 是字符串、`arguments` 是对象。真实参数必须由 Host 使用工具目录中的 Schema 再次校验。模型遵循契约不是安全边界。

校验失败返回结构化错误：

- `unknown_tool`：目录中不存在或当前 Agent 不可见。
- `invalid_arguments`：缺少必填字段、类型不匹配、枚举值错误或包含额外字段。
- `approval_denied`：用户或审批策略拒绝执行。
- `execution_failed`：真实工具执行失败。

参数错误包含 `issues` 和紧凑 `contract`，模型可以据此修正后重试。未知工具只返回有限候选建议，不暴露完整 Host 目录。

审批发生在解析真实工具之后，因此确认页显示真实工具名和经过脱敏的真实参数，而不是只显示 `invoke_tool`。搜索操作不需要审批；真实工具仍遵循原有的人工、自动或审查模式。

## 兼容范围

- 内置文件、命令、后台任务、记忆、Windows 桌面和 SubAgent 工具保留在 Host 目录。
- MCP Tool、Resource 和 Prompt 继续由 MCP Manager 发现并写入 Host 目录。
- 主 Agent 和 SubAgent 都只向其 Provider 暴露 `search_tools` 与 `invoke_tool`。
- AgentLoop 的批次审批、并发只读调用、写入/删除串行屏障、输出截断、视觉图片回填、视觉模型故障转移和 Session 事件保持不变。
- 旧的内部测试夹具仍可直接构造真实 `ToolCall`，但生产 Provider 不会注册真实工具名。

## 实现位置

- `omnicrawl/agent/host_tools.py`：Host 目录、工具搜索、紧凑 Schema、参数校验和固定 Provider 工具。
- `omnicrawl/agent/tools.py`：真实内置工具和 MCP 工具定义。
- `omnicrawl/agent/core.py`：Provider 工具面、统一分发、审批和执行。
- `omnicrawl/agent/vision_proxy.py`：独立视觉 Runtime、图片请求构造、文本分析和故障转移。
- `omnicrawl/config/vision.py`：视觉代理开关与模型引用配置。
- `omnicrawl/agent/execution.py`：模型回合与工具观察循环。
- `omnicrawl/agent/prompt_context.py`：只注入两个固定工具的上下文说明。
