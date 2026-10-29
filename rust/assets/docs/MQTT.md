# MQTT 自动化回路（遥测进、动作出）

OmniCrawl 的 MQTT 接入不是「又一个聊天平台」，而是**事件源 + 执行器**：

- 入口不是人发的消息，而是传感器报文（温湿度、土壤墒情、光照……）；
- 出口不是给人的回复，而是一条带 request_id 与 TTL 的**下发指令**（开棚、关棚、通风、灌溉）；
- 模型不是常驻进程，只在规则命中时被**调用一次**，拿到动作 JSON 就结束。

实现位于 `rust/crates/omnicrawl-connectors/src/mqtt/`，宿主侧装配在
`rust/crates/omnicrawl-api/src/mqtt.rs`。

## 三段漏斗：为什么不让模型看每条报文

```
MQTT 报文 → ① 窗口聚合 + 确定性规则 → ② 规划器（两种，按 [mqtt] planner 选）
                                        ├─ agent    ：内核回合（可用白名单工具）
                                        └─ decision ：单次结构化决策
```

1. **规则层永远在线**，判断「要不要叫醒模型」：阈值 + 持续时长 + 迟滞 + 冷却。
   稳态数据在这一层就消化掉，只更新窗口，不产生任何模型调用。
2. **命中后**才把窗口快照拼成固定模板，交给规划器选动作。模型的回答被限制在
   `[mqtt.actions]` 的动作表内，参数按 `min/max` 夹取。
3. 规划器失败、超时或输出不合法时，回落规则声明的 `fallback`；两者都没有就只记审计，
   不会「判断不了就不管」。

模型在两次触发之间**不存在**：LLM 是无状态函数，所谓唤醒就是拿新拼的 prompt 再跑一次。
不存在「模型沉睡等待条件」的进程，也就不存在长期占用运行名额、上下文漂移的问题。

## 配置

写在 `~/.OmniCrawl/config.toml`（模板见 `rust/assets/config-templates/config.example.toml`）：

```toml
[mqtt]
enabled = true
broker = "mqtt://192.168.1.10:1883"      # 也支持 mqtts://host:8883（rustls + webpki 根证书）
client_id = "omnicrawl-farm"
username = "farm"
password_env = "OMNICRAWL_MQTT_PASSWORD"  # 优先读环境变量；未设置时用 password 字段
keepalive_seconds = 120
qos = 1
topics = ["farm/+/env"]
zone_field = "zone"                       # payload 里的棚号字段
zone_topic_level = 1                      # payload 没有棚号时取主题第几段（0 起）
window_seconds = 30                       # 窗口聚合跨度
cooldown_seconds = 1800                   # 同棚同规则两次动作的最小间隔
poll_seconds = 1
command_topic = "farm/{zone}/command"
ack_topic = "farm/{zone}/ack"             # 留空表示不等回执
ack_timeout_seconds = 15
ttl_seconds = 300                         # 指令有效期：设备侧应丢弃过期指令
recent_actions = 5                        # 进模板的最近动作条数
# audit_path = "~/.OmniCrawl/mqtt/audit.jsonl"  # 默认就在这个位置

# 模型只能从这张表里选「动作」，不能自造。
[[mqtt.actions]]
id = "open_vent"
label = "打开通风口"
description = "温度偏高、湿度偏大时通风降温除湿"

[[mqtt.actions]]
id = "close_vent"
label = "关闭通风口"
description = "温度偏低或外界大风时保温"

[[mqtt.actions]]
id = "irrigate"
label = "启动灌溉"
description = "土壤湿度低于阈值时补水"
  [mqtt.actions.params.duration_minutes]
  min = 1
  max = 30
  default = 10

[[mqtt.actions]]
id = "do_nothing"
label = "维持现状"
description = "指标已在合适范围，无需干预"

# 规则：命中并持续 sustain_seconds 后唤醒一次模型。
[[mqtt.rules]]
id = "high_temp"
metric = "temp"
op = "gt"
value = 28.0
sustain_seconds = 300
clear_value = 26.0                        # 迟滞：回落到 26℃ 以下才算恢复
actions = ["open_vent", "do_nothing"]
fallback = "open_vent"                    # 模型不可用时的安全动作

[[mqtt.rules]]
id = "critical_heat"
metric = "temp"
op = "gt"
value = 38.0
sustain_seconds = 0
fail_safe = true                          # 不叫模型，直接执行 fallback
actions = ["open_vent"]
fallback = "open_vent"
```

`mqtt.actions` 必须包含 `do_nothing`：它是模型「不干预」的唯一出口，也保证规则候选里
永远有一个零副作用的选项。

## 报文契约

**遥测（订阅侧）**：UTF-8 JSON 对象，数值字段各算一个指标，嵌套对象展开一层：

```json
{"zone": "3", "temp": 31.2, "humidity": 64, "soil": {"moisture": 21.5}, "ts": 1793000000000}
```

- 棚号优先取 `zone` 字段，缺失时取主题层级（`zone_topic_level`）；
- `ts` 接受毫秒（>= 1e12）或秒（>= 1e9），缺失时用到达时间；
- 非 JSON、非对象、取不到棚号的报文直接丢弃，坏包不进窗口。

**指令（发布侧）**：

```json
{
  "request_id": "1793000000000-7",
  "zone": "3",
  "action": "irrigate",
  "params": {"duration_minutes": 10},
  "reason": "土壤湿度 21.5% 低于阈值",
  "confidence": 0.82,
  "source": "decision",
  "ttl_seconds": 300,
  "issued_at": "2026-10-29T10:00:00+08:00"
}
```

`source` 取 `decision`（决策模型）/ `main_model`（主模型回落）/ `fail_safe`（规则兜底）。

**回执（可选）**：同主题、带相同 `request_id` 的报文即视为本次指令的回执。
QoS1 只能保证报文到达 broker，**不保证设备执行**——真正的执行结果以回执与后续遥测为准。

## 安全边界

| 边界 | 做法 |
|---|---|
| 动作白名单 | 模型只能选 `[mqtt.actions]` 里的 id；不认识的 id 直接否决 |
| 参数范围 | `min`/`max` 夹取（模型说 90 分钟灌溉、上限 30，就按 30 执行） |
| 陈旧指令 | 指令带 `request_id` 与 `ttl_seconds`，设备侧应丢弃过期指令 |
| 反复开关 | 迟滞（`clear_value`）+ 冷却（`cooldown_seconds`）+ 触发后须恢复才重新武装 |
| 重启不重复动作 | 启动时从审计回放最后动作，重建冷却 |
| 模型故障 | 回落规则声明的 `fallback`；`fail_safe = true` 的规则根本不问模型 |
| 审计 | 每次决策一行 JSONL：触发原因、窗口快照、模型输出、指令、回执、失败原因 |

审计默认写到 `~/.OmniCrawl/mqtt/audit.jsonl`，是事后追溯与调阈值的唯一依据。

## 启动与验证

启动 TUI 时若 `[mqtt]` 已配置齐全，会自动拉起；也可以手动起一个：

```powershell
omnicrawl-host connector mqtt     # 或 omnicrawl connector mqtt
```

日志（stderr）会打印当前规划器、订阅主题、规则数量与每次动作，例如：

```
[mqtt] 规划器：结构化决策模型（jev / jev-latest）
[mqtt] 启动：broker=mqtt://192.168.1.10:1883 订阅=["farm/+/env"] 规则=2 动作=4 规划器=decision
[mqtt] 已连接并完成订阅。
[mqtt] 3：下发 open_vent → farm/3/command（温度 31.2℃ 持续 5 分钟）
```

本地验证不必接真设备：`cargo test -p omnicrawl-connectors --test mqtt_loop` 用回环假 broker
跑完整回路（含握手、订阅、触发、下发、回执、审计与兜底）。

## 两种规划器

### agent（默认）：内核回合 + 工具白名单

拉起内核子进程跑一个回合：模型能查天气、翻知识库、读记忆、多步推理，最后输出动作 JSON。

- **每次唤醒都从干净会话开始**：上一次遥测的推理对本次没有价值，留着只会累积上下文。
- **审批固定 review**：无人值守没人点确认，危险调用交给审查模型判定（fail-closed）。
- **工具表收窄到 `[mqtt] tools` 白名单**：不在表里的工具既不声明给模型，宿主也不执行
  （声明与执行查的是同一份 specs，过滤一次两头都挡住）。
- **回合超时**（`turn_timeout_seconds`，默认 120 秒）会取消并要求内核收口，避免占用
  单活动运行名额——否则下一次触发会直接撞上 `RUN_ACTIVE`。

默认白名单（只读 + 记忆/知识库 + 联网检索）：

```
read, read_image, list, find, grep,
memory_search, memory_read, memory_expand_related, memory_write,
kb_search, kb_read, web_search, fetcher
```

**没有** shell、文件写入、桌面控制、子代理与配置改写：这些在无人值守下没有安全的失败模式。

要放开就显式写 `[mqtt] tools`。注意风险面：MQTT 报文来自现场设备，报文内容会进模型上下文；
把 `bash` 放进白名单等于把「谁能发报文」变成「谁能执行命令」。启动日志会打印生效的白名单。

### decision：单次结构化决策

不拉内核、不碰工具，只问一次「选哪个动作」：

1. `decision_models.toml` 里有可用渠道 → 结构化 choice（答案约束在动作表内，不生成正文、
   不需要解析文本，更快也更省）；
2. 没有 → 回落主模型（`llm` 段），一次单轮补全 + JSON 解析，与工具审查的对话通道同源；
3. 两者都没有 → 启动即报错。

适合「阈值明确、动作单一」的棚：一条规则命中只看温度，没必要让模型翻资料。

## 当前边界

- **自动启动**：与其它连接器一致——启动 TUI 时若 `[mqtt]` 自洽（broker、topics、actions 齐全）
  就自动拉起 `omnicrawl-host connector mqtt`；配置不全不会启动，也不阻塞 TUI。
  设置 `OMNICRAWL_AUTO_START_CONNECTORS=0` 可一并关闭。无宿主载荷的平台（armv7 等）不会
  产生这条 spec：内核没有 MQTT 连接器。
- **单实例、单连接**：一个进程一条 MQTT 连接、一个循环。多棚/多 broker 用多个进程
  （各自独立配置与审计），不要在一个进程里塞两组 broker。
- **决策期间不处理新遥测**：动作完成后的下一帧遥测会重建窗口；决策本身是低频事件，
  这个取舍换掉了并发读写与半包交错。
- **回合模式每次触发都重启会话**：换来确定性的上下文，代价是一次内核启动的延迟
  （百毫秒级，远小于一次模型调用）。觉得重就把 `planner` 切到 `decision`。
- **白名单不区分调用来源**：它作用于整个回合，包括模型自己发起的调用；MCP 工具不在默认
  名单里，要用就显式写进 `[mqtt] tools`。
