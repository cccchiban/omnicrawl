# Agent 改造 Review 交接记录

记录时间：2026-05-29

## 背景

当前项目已初始化 Git，并新增了 `agent.py`，将原本的语音/文字对话改造成默认 Agent 模式。

已实现能力：

- 读文件：`list_files`、`read_file`、`search_text`
- 受限副作用工具：`replace_text`、`write_file`、`run_command`
- 写文件、替换、执行命令前会在终端请求人工确认
- `main.py` 已直接接入 `LocalToolAgent`

## Review 发现

### 1. 高优先级：API Key 仍硬编码

位置：`llm.py:8`

问题：

- `DEFAULT_API_KEY` 仍写在源码里。
- 现在仓库已经初始化，后续一旦提交或推送，密钥很容易泄露。

建议：

- 删除硬编码默认 key，改为必须从 `OPENAI_API_KEY` 读取。
- README 保留环境变量配置示例即可。
- 如果该 key 是真实可用的，应轮换。

### 2. 高优先级：命令工具未真正限制在工作区内

位置：`agent.py:417-439`

问题：

- `run_command` 只是设置了 `cwd=workspace_root`。
- 因为使用 `shell=True`，用户确认后命令仍可访问或修改工作区外路径。
- README 和工具描述里的“在工作区内执行命令”容易让人误以为有沙箱隔离。

建议：

- 短期：修改 README 和工具描述，明确“命令在工作区启动，但不是系统沙箱”。
- 中期：改成命令白名单，例如只允许 `python -m py_compile`、`pytest`、`git status/diff/log/show` 等低风险命令。
- 更稳妥：避免 `shell=True`，改为结构化参数执行。

### 3. 中优先级：Agent 环境变量解析可能导致程序崩溃

位置：`agent.py:55`、`agent.py:58`，入口在 `main.py:238`

问题：

- `AGENT_MAX_STEPS`、`AGENT_COMMAND_TIMEOUT_SECONDS` 直接用 `int(os.getenv(...))`。
- 如果用户设置了非数字值，会抛 `ValueError`。
- `main.py` 只捕获 `AgentError`，因此程序会直接崩。

建议：

- 在 `AgentConfig.__post_init__` 或专门的解析函数里校验。
- 非法值应抛 `AgentError`，并给中文提示。

### 4. 中优先级：搜索工具会递归进入受保护目录后再过滤

位置：`agent.py:360-364`

问题：

- 当前 `root.rglob("*")` 会先遍历 `.git`、`.venv`、`.codex-ref` 等目录，再在循环内过滤。
- 如果虚拟环境或参考仓库很大，搜索会明显变慢。

建议：

- 改为 `os.walk` 并在 `dirnames` 上剪枝。
- 或实现一个自定义递归迭代器，遇到受保护目录直接跳过整棵子树。

### 5. 低优先级：Agent 输出不再是真流式

位置：`agent.py:164-181`，调用处 `main.py:258-262`

问题：

- `run_stream` 名字保留了流式语义，但内部使用非流式 `responses.create`。
- 只有最终回答会调用 `on_delta`。
- 长任务期间只有工具状态提示，纯模型生成时不会边输出边播报。

建议：

- 若要恢复语音“边生成边播报”体验，需要对最终回答做流式解析。
- 可以先保留现状，因为 Agent 工具循环比普通聊天更重要。

## 待确认问题

1. “安全版”是否允许用户确认后执行任意命令？

当前实现是：用户确认后可以执行任意 shell 命令。

如果期望更严格，应改成命令白名单或禁用命令工具。

2. 是否要把 Agent 工具协议升级为 Responses 原生 tool call？

当前实现使用文本协议：

```text
<tool>{"name":"read_file","arguments":{"path":"main.py"}}</tool>
<final>最终回答</final>
```

这样兼容性更好，但依赖模型遵守格式。后续如果确认网关支持原生工具调用，可以升级。

## 建议修复顺序

1. 先移除 `llm.py` 中硬编码 API Key。
2. 明确 `run_command` 的安全边界：改文档，或做命令白名单。
3. 给 `AgentConfig` 增加环境变量解析校验。
4. 优化 `search_text` 的目录剪枝。
5. 按需要恢复最终回答流式输出。

## 建议补充测试

- 路径越界：`../`、绝对路径、`.env`、`.git`、`.codex-ref`
- 环境变量非法值：`AGENT_MAX_STEPS=abc`
- 工具协议解析：合法 `<tool>`、合法 `<final>`、非 JSON 回复
- 搜索工具跳过受保护目录
- 用户拒绝受限工具时，Agent 能继续得到“用户拒绝”的观察结果
