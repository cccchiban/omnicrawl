# Windows 原生桌面工具

## 1. 范围与注册条件

OmniCrawl 在 Windows Host 上把以下系统能力注册为内置 Agent Tool，而不是 MCP Server 或任意 Shell 命令：

| 工具 | 能力 | 原生基础 |
|---|---|---|
| `windows_window` | 顶层窗口枚举、标题/类名/进程/几何读取、前台激活 | `user32.dll`：`EnumWindows`、`GetWindowRect`、`SetForegroundWindow` |
| `windows_control` | 在指定窗口内发现及语义化操作 UI 控件 | Windows UI Automation（`UIAutomationClient`） |
| `windows_input` | 鼠标移动、点击、滚轮、按键、组合键、Unicode 文本输入 | `user32.dll`：`SendInput` |
| `windows_clipboard` | Unicode 文本剪贴板读取、写入与清空 | `OpenClipboard` / `GetClipboardData` / `SetClipboardData` |
| `windows_screenshot` | 虚拟桌面、指定区域或指定窗口截图，并向视觉模型提供图片 | `GetDC` / `BitBlt` / GDI+ PNG 编码 |

- 仅 `os.name == "nt"` 时注册；非 Windows Host 不会向模型暴露这些工具。
- 不新增 Python 依赖：窗口、输入、剪贴板和截图直接调用 Windows 系统 DLL；UI Automation 使用 Windows 自带的 .NET `UIAutomationClient`，经固定 PowerShell 脚本调用。
- 截图保存到 `.agent_tmp/images/`。只有当前模型明确声明 `vision=true` 时，Host 才在同一工具循环的下一次请求中注入 Base64 图片；Base64 不写入 Session 事件或长期历史。

## 2. 调用与审批约束

五个工具都标记为 `requires_confirmation=True`，默认 `approval.mode=manual` 下会显示审批页。桌面工具在同一批模型 Tool Call 中被视为串行屏障：例如“激活窗口 → 查找控件 → 输入文本 → 截图验证”不会并发竞争前台焦点。

审批页和 Session 事件不会保留以下原文参数：

- `windows_control.set_value.value`
- `windows_input.type_text.text`
- `windows_clipboard.write_text.text`

它们只保留字符长度，以降低密码、令牌和私有文本被持久化的风险。`read_text` 的工具结果本身可能包含敏感内容，调用前仍应得到用户授权，调用后也不得将其用于未授权的外发或持久化。

> 全局 `approval.mode=auto` 或 `review` 的语义保持项目现有规则；启用这两种模式的操作者需要自行承担自动执行风险。

## 3. 工具接口

### 3.1 `windows_window`

动作：`list`、`get`、`activate`。

- `list`：可用 `title_contains`、`class_name_contains`、`visible_only`、`include_untitled`、`max_results` 过滤。返回的 `window_handle` 为十六进制字符串。
- `get`：必须传 `window_handle`，返回标题、类名、进程 ID、可见/最小化/最大化/前台状态及 `bounds`。
- `activate`：必须传 `window_handle`。最小化窗口会请求恢复，再调用 `SetForegroundWindow`；若 Windows 焦点策略拒绝，工具明确报错，**不会**使用 `AttachThreadInput` 等方式绕过用户前台控制权。

示例：

```json
{"action":"list","title_contains":"记事本","max_results":10}
```

```json
{"action":"activate","window_handle":"0x0000000000012345"}
```

### 3.2 `windows_control`

动作：`list`、`invoke`、`set_value`、`select`、`toggle`、`focus`。

所有动作必须传 `window_handle`。`list` 可按以下任意精确条件筛选：`name`、`automation_id`、`class_name`、`control_type`；非 `list` 动作至少需要一个定位条件。

支持的 `control_type`：`button`、`checkbox`、`combobox`、`edit`、`hyperlink`、`list`、`listitem`、`menu`、`menuitem`、`radiobutton`、`tab`、`tabitem`、`text`、`tree`、`treeitem`、`window`、`pane`、`document`、`custom`、`group`。

若定位条件匹配多个控件，变更性动作会拒绝执行；先调用 `list`，再传返回结果对应的 `index` 才能明确选择目标。这样避免同名按钮、多个编辑框或动态页面导致误操作。

示例：

```json
{"action":"list","window_handle":"0x0000000000012345","control_type":"edit","max_results":20}
```

```json
{
  "action":"set_value",
  "window_handle":"0x0000000000012345",
  "automation_id":"searchBox",
  "value":"待输入文本"
}
```

`set_value` 依赖目标控件支持 UIA `ValuePattern`；`invoke`、`select`、`toggle` 分别需要对应的 UIA Pattern。应用未公开自动化树或未实现该 Pattern 时，工具会返回可诊断错误，不会退化为坐标猜测点击。

### 3.3 `windows_input`

动作：`move`、`click`、`scroll`、`key`、`hotkey`、`type_text`。

- `move` / `click`：必须同时传 `x`、`y`，坐标按照整个虚拟桌面校验，支持负坐标和多显示器。Windows 10 1607+ 会临时切换当前工具线程到 Per-Monitor DPI V2，使窗口 `bounds`、UIA 控件 `bounds` 和 SendInput 坐标统一为物理像素；旧系统维持其默认坐标空间。
- `click`：`button` 取 `left`、`right`、`middle`，`clicks` 取 1–3。
- `scroll`：传非零 `wheel_delta`；可选传 `x`、`y` 以先移动到目标位置。
- `key`：传受控键名，如 `enter`、`tab`、`esc`、`left`、`f5`、`A`、`0`；可传 `presses`。
- `hotkey`：传 2–5 个受控键名组成的 `keys` 数组，例如 `["ctrl", "shift", "S"]`。
- `type_text`：使用 `KEYEVENTF_UNICODE` 输入 Unicode 文本，不依赖当前键盘布局；结果只返回长度，不回显内容。

示例：

```json
{"action":"click","x":640,"y":360,"button":"left"}
```

```json
{"action":"hotkey","keys":["ctrl","s"]}
```

### 3.4 `windows_clipboard`

动作：`read_text`、`write_text`、`clear`，仅处理 `CF_UNICODETEXT`。

- `read_text`：`max_chars` 默认为 8000，最大 32768；返回 `text`、`total_chars`、`truncated`。
- `write_text`：传 `text`，最大 32768 个字符；Windows 在成功后接管内存所有权。
- `clear`：清空当前剪贴板格式。

工具会对短暂的“剪贴板被其他程序占用”重试三次；持续占用时返回错误而不无限阻塞 Agent 回合。

### 3.5 `windows_screenshot`

`target` 支持 `desktop`、`region`、`window`：

- `desktop`：截取整个虚拟桌面，支持多显示器和负坐标。
- `region`：必须传 `x`、`y`、`width`、`height`；区域必须完整位于虚拟桌面内。
- `window`：必须传 `window_handle`，建议先通过 `windows_window.list` 获取；最小化窗口会拒绝截图，窗口部分移出屏幕或包含桌面外 DWM 边框时截取其与虚拟桌面的可见交集，完全位于桌面外时拒绝。
- `max_dimension`：模型图片最大边，默认 2048，范围 256–8192。工具保持宽高比缩放；若 PNG 仍超过 5 MiB，会继续缩小以满足跨 Provider 的保守内联限制。

示例：

```json
{"target":"desktop","max_dimension":2048}
```

```json
{"target":"region","x":100,"y":80,"width":1280,"height":720}
```

```json
{"target":"window","window_handle":"0x0000000000012345"}
```

结果返回临时 PNG 路径、原始 `source_bounds`、最终图片尺寸、字节数及是否缩放。截图使用屏幕像素 `BitBlt`，因此：

- 指定窗口被其他窗口遮挡时，遮挡内容也会进入截图；工具不会通过 `PrintWindow` 绕过用户当前可见状态。
- 受 DRM、硬件叠加、安全桌面或应用保护影响的区域可能为空白或黑色。
- 视觉模型注入仅在当前 Runtime 的 `capabilities.vision` 为真时启用；无视觉能力的模型仍会获得文本结果和本地文件路径，不会收到图片块。
- 同批存在多个工具调用时，Host 会先回填全部 Tool Result，再追加截图的临时 user 图片消息，保持各 Provider 的工具调用顺序合法。

## 4. 安全与系统边界

这些工具受 Windows 本身的隔离机制约束，不能也不应被用来绕过：

1. UAC 安全桌面、锁屏、登录界面。
2. UIPI 跨完整性级别限制（普通权限进程通常无法控制管理员窗口）。
3. DRM/受保护媒体、应用自定义安全界面。
4. 会话隔离；Windows Service 不具备当前交互用户的正常桌面自动化能力。
5. 应用未公开的 UI Automation 控件或模式。

建议优先顺序是：先 `windows_window.list` 找到窗口，再 `windows_control.list` 获取控件的稳定定位信息，最后才使用 `windows_control` 语义化操作；只有 Canvas、游戏、遗留程序等没有可用 UIA 树的场景，才使用 `windows_input` 坐标输入。

## 5. 验证覆盖

`tests/test_windows_desktop_tools.py` 与 `tests/test_vision_tool_images.py` 覆盖：

- 五项工具的注册与人工审批标记；
- 敏感文本不进入公共参数投影；
- 桌面工具在 Agent 批次中的串行执行约束；
- 窗口、控件、输入、剪贴板和三类截图目标的参数校验与结构化分发；
- 截图 ToolResult、视觉能力门控、批次消息顺序以及 OpenAI Chat / Responses、Anthropic、Gemini 四类图片映射；
- 非 Windows 平台的明确降级错误。

Windows 本机冒烟验证可创建一个内容受控的临时测试窗口并截取该窗口，随后检查 PNG 文件头、尺寸与清理结果；不应在自动化测试中实际点击、输入、清空或覆盖用户剪贴板，也不应擅自截取用户真实桌面内容。
