# OmniCrawl 工具调用协议

OmniCrawl 采用 Provider 原生支持的「顶层注册所有工具」方案：

- Provider 顶层 `tools` 注册当前 Agent 的**所有可见工具**（真实工具名 + 压缩描述 + 紧凑 Schema）。
- 模型在生成中直接**原生调用**这些真实工具名（返回正式 `tool_calls`）；Host 读取 `name`、`arguments`、`call_id`，按真实工具目录二次校验参数、走审批并执行，最后按 `call_id` 回传 `role=tool` 结果。
- 不再使用 `search_tools` 元工具，也没有动态声明注入：工具从对话第一轮就对模型完全可见。

真实工具的 Python 执行器、完整 Schema、审批策略、MCP 连接和工作区权限仍由 Host 持有。

## 调用流程

```text
Provider -> 顶层 tools = [read, bash, grep, ...]（全部可见工具，压缩声明）
模型 -> 直接原生调用真实工具（例如 read / bash）
Host -> 读取 name / arguments / call_id -> 完整 Schema 校验 -> 插件钩子 -> 审批 -> 执行
Host -> role=tool 结果（tool_call_id 回填模型发起的 call_id）
模型 -> 继续调用或输出最终答案
```

### 单次执行与重复调用规则

- 一个意图只发起一次实际操作；必须等待对应 `tool_call_id` 的完整 `role=tool` 结果后再决定下一步。
- 同一回合按“归一化工具名 + 参数”去重，Provider 生成不同 `call_id` 不代表可以重复执行。Host 会缓存同一回合已执行结果，后续完全相同调用只回填“复用上次结果”，不再次产生副作用。
- 成功结果直接作为事实继续；`invalid_arguments` 只修正参数后重试，`approval_denied` 不得绕过，`execution_failed` 必须先分析错误，超时尤其不能立即重放写入/命令调用，因为底层进程可能仍在运行。
- 工具调用本身不得被模型输出文字替代；工具结果回填后必须保留为模型观察，不得由 Provider 适配层追加“最终总结”等新的用户指令来改写任务意图；任务完成后停止调用并输出最终答复，不要为了“确认成功”机械重复同一只读或命令调用。

## 声明压缩

顶层注册所有工具后，每次请求都会携带全部声明，因此描述与 Schema 必须在**注册时**压缩：

- `description`：折叠空白（多行拼接归并为单空格），完整描述原样发送，不做长度截断。
- `parameters`：紧凑 Schema，只保留 `type`、`properties`、`required`、`enum` 和边界约束；删除长描述与默认值/示例。
- 压缩在 `build_provider_tools` / `chat_completion_tools` 两个入口同时生效（主 Agent、SubAgent、独立 Profile 共用同一套工具面）。

Host 分发前仍用工具目录的**完整 Schema** 二次校验。模型遵循压缩声明不是安全边界；参数错误时错误结果携带 `issues` 和紧凑 `contract`，模型据此修正重试。

## 函数名规范（去哈希化）

顶层 `tools` 注册的函数名与真实工具名一致（例如 `read` 就是 `read`），不再追加 SHA-1 哈希后缀。模型看到的名字短、可读、可直接回显。

- 对符合函数名规范（字母/数字/下划线/连字符，长度 ≤64）的工具名直接使用原名。
- 仅对含非法字符的工具名（如 MCP 的 `server.tool`）做归一化并追加短哈希兜底，保证网关侧合法且不与其他工具冲突。

Host 侧同时保留对旧式哈希函数名的宽容反查：模型若回显被截断的哈希名（如 `tool_search_e960b0242f`），Host 按 digest 段（SHA-1 前 10 位）反查真实工具并正常分发；无法识别时，错误信息会提示疑似正确名称，帮助模型下一轮纠正。

## Provider 兼容

- OpenAI Chat Completions / Responses、Anthropic、Gemini 等协议均支持顶层 `tools`，无消息内 `tools` 的兼容问题。
- 工具声明使用统一格式（`type=function` + `function.name/description/parameters`），由各 Provider Runtime 适配层转换。

## Host 侧边界

分发顺序：读取 `tool_calls` → 按名称解析（含哈希名宽容反查）→ 完整 Schema 校验 → 插件钩子 → 审批 → 执行 → 按 `call_id` 回传。

校验失败返回结构化错误：

- `unknown_tool`：目录中不存在或当前 Agent 不可见。
- `invalid_arguments`：缺少必填字段、类型不匹配、枚举值错误或包含额外字段。
- `approval_denied`：用户或审批策略拒绝执行。
- `execution_failed`：真实工具执行失败。

审批发生在解析真实工具之后，因此确认页显示真实工具名和经过脱敏的真实参数。

## `invoke_tool` 兼容说明

`invoke_tool` 已从 Provider 工具面移除。Host 分发器仍保留其解析路径，供旧测试和直接构造的内部调用使用；生产模型应直接原生调用真实工具名。

## 兼容范围

- 内置文件、命令、后台任务、记忆、知识库、Windows 桌面和 SubAgent 工具保留在 Host 目录，顶层全部注册。
- MCP Tool、Resource 和 Prompt 继续由 MCP Manager 发现并写入 Host 目录，同样顶层注册。
- AgentLoop 的批次审批、所有工具并发执行、按每个工具真实完成时刻独立计时、模型上下文有界输出摘要（头尾预览）、完整 `full_output` UI 展示、视觉图片回填、视觉模型故障转移和 Session 事件保持不变；模型观察仍按原始调用顺序回填。
- `HostToolCatalog` 的工具搜索、声明构建与 `invoke_tool` 解析保留为内部能力，不再暴露给 Provider。

命令工具的主命令允许裁剪测试/构建输出（如 `tail`、`head`、`grep`、`rg` 或 PowerShell 输出筛选），便于快速定位失败原因。Bash 启动时默认启用 `pipefail`，裁剪不得掩盖管道上游失败的真实退出码；需要完整输出时，Host 会保留首尾并给出完整日志路径，也可通过独立的 `diagnostic_command` 摘取诊断。

## 结构化 git 工具

`git` 工具在工作区执行结构化 git 操作，参数不再走 shell 字符串，而是：

- `action`：git 子命令（枚举：status/diff/log/show/add/commit/branch/checkout/stash/push/pull/reset/...）。
- `args`：子命令标志与位置参数（`--short`、`--oneline`、`-n 20`、分支名、stash 子动词等）。
- `message`：commit（或 annotated tag）的提交信息。
- `paths`：工作区内相对路径，自动以 `--` 追加在命令末尾。

执行器以 `git <action> <args> ...` 的 argv 形式直接启动子进程（不经 shell、无管道、无重定向），`cwd` 固定为工作区，并设置 `GIT_PAGER=cat`、`GIT_TERMINAL_PROMPT=0`、`GIT_EDITOR=true`；输出做首尾采样有界化。

审批按 action 风险分级：

- **只读**（status/diff/log/show/ls-files/rev-parse/...）：直接放行。
- **本地变更**（add/commit/branch/stash/restore/...）：review 模式直接放行（与文件写入同档），manual 模式人工确认。
- **高风险**（push/rebase/merge/pull/clean、reset --hard、checkout/switch -f、branch -D、tag -d/-f、stash drop/clear 等）：review 模式进入模型审查，manual 模式人工确认。

执行前 Host 还会拒绝：`--git-dir`/`--work-tree`/`--no-verify` 等逃逸类参数、`config --global/--system/--file`、`archive -o/--output`、`clone`/`init` 目标目录越界、commit 缺少 `message` 且无 `--no-edit`，以及 `paths` 越出工作区。

SubAgent 只读 profile 不暴露 `git` 工具；只读 git 查询通过被只读命令包装的 `bash` 完成。

- `omnicrawl/agent/toolkit/host_tools.py`：Host 目录、可见性过滤、声明构建、`build_provider_tools` 和参数校验。
- `omnicrawl/agent/core.py`：`LocalToolAgent` 组合门面（`AgentConfig` + `__init__` + 16 个领域 Mixin 继承），对外 API 不变。
- `omnicrawl/agent/controllers/tools/building.py`：工具表构建、`_build_tools` / `_load_system_prompt_template`，Provider 顶层工具面。
- `omnicrawl/agent/controllers/tools/approval.py`：工具审批与自动审查（`_approve_tool_for_batch` 等）。
- `omnicrawl/agent/controllers/turn/loop.py`：`run_stream()` 主循环、工具批执行与统一分发（`_execute_tool_batch`）。
- `omnicrawl/agent/controllers/tools/output.py`：工具输出预算、落盘与结果格式化。
- `omnicrawl/agent/context/prompt_context.py`：只注入工具能力说明的上下文消息。
- `omnicrawl/agent/runtime/vision_proxy.py`：独立视觉 Runtime、图片请求构造、文本分析和故障转移。
- `omnicrawl/config/vision.py`：视觉代理开关与模型引用配置。
- `omnicrawl/agent/runtime/execution.py`：模型回合与工具观察循环。
