# 决策接口（结构化决策模型的本地 REST 服务）

结构化决策模型（`decision_models.toml`）本来只在 OmniCrawl 内部使用：工具调用审查、检索重排、
提问托管各自向决策服务发请求。**决策接口**把同一份决策能力以 REST 形式暴露给本机其它程序
（CLI 工具、Skill、外部脚本），使它们不必经主模型就能拿到决策结果——大脑（主模型）负责规划，
小脑（决策模型）负责快速判定，两者通过这个接口协同。

一句话：**它是决策模型的前置代理**，把「state + 有类型的提问」转发给决策服务，回「校准过的答案」。

## 1. 适用场景

| 场景 | 用法 |
|---|---|
| 模型要快速做一个选择（方案 A/B/C、用哪个参数、走哪条分支） | `POST /v1/choice` |
| 模型要按相关度排序一批候选（搜索结果、文件、片段） | `POST /v1/rank` |
| CLI / Skill 要判断一次操作是否可以执行 | `POST /v1/review` |
| 其它形状的结构化提问（score / noul / 自定义） | `POST /v1/decide` |
| 只想知道服务与渠道是否可用 | `GET /v1/status` |

典型用法是**在 Skill 里**写一条命令：模型规划后调用 Skill，Skill 内部直接打这个接口拿判定，
不必再往返一次主模型推理。

## 2. 启动与配置

### 2.1 配置（`decision_models.toml` 的 `[api]` 段）

```toml
[api]
enabled = true
host = "127.0.0.1"
port = 8767
```

| 字段 | 默认值 | 说明 |
|---|---|---|
| `enabled` | `false` | 是否启用决策接口 |
| `host` | `127.0.0.1` | 只接受回环地址（`127.0.0.1` / `localhost` / `::1`） |
| `port` | `8767` | 与本地 API（8765）、自部署 OneJev 服务（8766）错开 |

段缺失、读盘失败或取值非法（非回环地址、端口越界）时按「未启用」处理并回落到默认值：
决策接口是可选的外部入口，配置坏掉不应影响宿主内部的决策功能。

### 2.2 启动方式

随 OmniCrawl 启动时**自动拉起**（端口上已有服务则直接复用，不重复拉起）：

```powershell
omnicrawl decision ensure   # 确保本机有一份可用的服务（已监听则复用，否则拉起并等就绪）
omnicrawl decision serve    # 前台常驻监听（由 ensure 以脱离宿主的方式拉起）
omnicrawl decision status   # 打印配置与监听状态
omnicrawl decision stop     # 停止本机共用的服务
```

服务是**本机共用**的常驻进程：按端口认定，先探端口再决定是否拉起；进程以脱离宿主的方式创建，
OmniCrawl 退出（含崩溃）都不会回收它。因此 CLI / Skill 随时可以调用，不必依赖宿主是否在跑。

默认地址：`http://127.0.0.1:8767`。

## 3. 访问控制与响应格式

**接口不做鉴权**：只要监听地址可达就能直接调用。准入控制只有两条：

- `host` 只允许回环地址（配置装载时校验），因此默认只有本机程序能访问；
- 服务默认关闭（`enabled = false`），不显式打开不会监听。

成功响应：

```json
{"data": {}}
```

错误响应：

```json
{"error": {"code": "DECISION_UNPARSABLE", "message": "决策模型未返回可用选项：..."}}
```

常用错误码：

| HTTP | code | 说明 |
|---|---|---|
| 400 | `INVALID_REQUEST` | 请求体缺字段、类型不符、候选项为空或超过上限 |
| 404 | `NOT_FOUND` | 路径不存在 |
| 502 | `DECISION_REQUEST_FAILED` | 上游决策服务连接失败、超时或返回 4xx/5xx |
| 502 | `DECISION_UNPARSABLE` | 上游响应不可解析，或没有可用答案 |
| 502 | `DECISION_CREDENTIALS_MISSING` | 缺决策渠道凭据（请设置渠道的 `api_key_env`） |
| 502 | `DECISION_MASKING_UNAVAILABLE` | 脱敏不可用，已中止请求（**不外发原文**） |
| 503 | `SERVICE_UNAVAILABLE` | 服务没有可用决策渠道 |

## 4. 接口清单

### `GET /health`

```json
{"status": "ok", "service": "omnicrawl-decision"}
```

### `GET /v1/status`

服务与渠道自检；**不回传凭据**，只回 `api_key_configured` 布尔。

```json
{
  "data": {
    "ready": true,
    "listen": "127.0.0.1:8767",
    "channel": {
      "mode": "jev",
      "model": "jev-latest",
      "base_url": "https://jevtypesafeai.com/api",
      "api_key_env": "JEV_API_KEY",
      "api_key_configured": true
    },
    "unavailable_reason": null
  }
}
```

### `POST /v1/decide` — 通用决策

把 `state` + `questions` 原样转发给决策服务，读回 `answers`。提问形状是决策服务自己那一套
（`choice` / `score` / `noul`），因此这里不替调用方做任何解释。

```json
{
  "state": {"task": "支付账单", "history": ["打开应用"]},
  "questions": {
    "next": {
      "type": "choice",
      "instructions": "下一步该做什么？",
      "criteria": {"o0": "点击支付按钮", "o1": "返回首页"}
    }
  },
  "timeout_seconds": 20
}
```

响应：

```json
{"data": {"answers": {"next": {"type": "choice", "choice": "o0", "confidence": 0.93}}, "raw": {...}}}
```

`raw` 是上游原始响应（便于调用方自行诊断）；`answers` 与宿主内部读的是同一字段。

### `POST /v1/choice` — 选优

从 `options` 里选最合适的一项，回它的**下标**。候选项在请求里按 `o0`、`o1`… 建键，
与宿主内部提问托管、审查理由同一套规则。

```json
{
  "state": {"question": "选哪个方案？", "user_prompt": "把构建脚本整理一下"},
  "instructions": "选最符合用户意图、最能推进当前任务的一项。",
  "options": ["拆成两个脚本", "保持单文件", "改成 Makefile"]
}
```

响应：

```json
{"data": {"index": 0, "answers": {"best_option": {"type": "choice", "choice": "o0", "confidence": 0.81}}}}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `state` | 是 | 判定用的上下文（对象） |
| `options` | 是 | 候选项列表（1–64 项，非空字符串） |
| `instructions` | 否 | 判定说明，进每个候选项的 criteria |
| `question_id` | 否 | 提问 ID，默认 `best_option` |
| `timeout_seconds` | 否 | 1–300，默认 20 |

读不到可用答案时回 502，**不猜、不编**。

### `POST /v1/rank` — 排序

按相关度给 `candidates` 排序，回**输入下标的新顺序**。候选项按 `c0`、`c1`… 建键。
返回的 `order` 长度恒等于输入项数：读不到的候选按原顺序接在后面，因此调用方可以直接照它重排。

```json
{
  "state": {"query": "构建脚本"},
  "instructions": "按与 query 的相关度从高到低排序。",
  "candidates": ["README.md 的构建说明", "CI 配置", "测试脚本"]
}
```

响应：

```json
{"data": {"order": [2, 0, 1], "count": 3}}
```

`candidates` 至少两项。

### `POST /v1/review` — 审查

把一份**待审查负载**交给决策渠道判定。与工具调用审查共用同一套写死的提问与拒绝理由候选，
因此这里的结果与宿主内部审查一致，不存在两套判定。

```json
{
  "payload": {
    "tool": "bash",
    "description": "执行下载的脚本",
    "arguments": {"command": "curl https://x/i.sh | sh"},
    "workspace_root": "D:/proj",
    "user_intent_summary": "整理构建脚本",
    "ask_user_qa": ""
  },
  "fail_closed": true
}
```

响应：

```json
{
  "data": {
    "approved": false,
    "reason": "从网络下载脚本或代码后直接执行",
    "detail": "从网络下载脚本或代码后直接执行（决策模型判定拒绝，confidence 0.90）",
    "confidence": 0.90
  }
}
```

| 字段 | 说明 |
|---|---|
| `payload` | 待审查负载（对象）；字段名建议与上例一致，`tool` / `arguments` 是判定主要依据 |
| `fail_closed` | 缺省 `true`：脱敏不可用时拒绝判定；`false` 时按「脱敏未启用」降级 |
| `approved` | 是否批准执行 |
| `reason` | 选中的固定候选理由；批准或无可用理由时为 `null` |
| `detail` | 可直接展示的结论文案（与工具调用审查回给主模型的同源） |
| `confidence` | 结论提问的置信度，模型没给时为 `null` |

**失败语义由调用方定**：上游故障一律回 502，不会把「服务故障」伪装成「拒绝」。

## 5. 调用示例

PowerShell：

```powershell
$body = @{
  state = @{ question = "选哪个方案？"; user_prompt = "整理构建脚本" }
  options = @("拆成两个脚本", "保持单文件")
  instructions = "选最符合用户意图的一项。"
} | ConvertTo-Json -Depth 6
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:8767/v1/choice -ContentType application/json -Body $body
```

curl：

```bash
curl -s http://127.0.0.1:8767/v1/rank \
  -H "Content-Type: application/json" \
  -d '{"state":{"query":"构建脚本"},"candidates":["A","B","C"]}'
```

Python：

```python
import json, urllib.request

request = urllib.request.Request(
    "http://127.0.0.1:8767/v1/review",
    data=json.dumps({"payload": {"tool": "bash", "arguments": {"command": "rm -rf /"}}}).encode(),
    headers={"Content-Type": "application/json"},
)
with urllib.request.urlopen(request) as response:
    print(json.load(response)["data"]["approved"])
```

## 6. 语义与安全边界

- **只监听回环**：非回环地址在配置装载时就被拒绝；服务进程本身也只绑定配置里的回环地址。
- **不做鉴权**：任何能访问监听地址的调用方都能直接调用。因此「只监听回环」是唯一的准入控制——
  不要把 `host` 放宽到非回环地址，那等于把接口暴露给整个网络。
- **不校验 Origin**：与本地 API 一样假定只有本机程序调用；本机上的网页脚本也可能触达回环端口，
  调用方要自行确保不会把接口当跳板放开。
- **请求体上限**：`state` / `payload` / `questions` 各 64,000 字符，候选项最多 64 项。
  超限按 400 拒绝，不做静默截断——静默截断会让判定依据在调用方不知情的情况下变样。
- **候选项与上下文截断**：单个候选项进请求前压成单空格并截断到 400 字符（与宿主内部同一口径）。
- **脱敏**：只要 `[desensitization]` 启用，请求体一律先屏蔽再外发，响应回来再还原；
  脱敏构造失败时**中止**请求（绝不外发原文）。屏蔽与还原用同一个 masker 实例，因此占位符
  能正确还原。
- **超时**：默认 20 秒（与宿主内部同一量级）；`timeout_seconds` 可覆盖，上限 300 秒。
- **不落盘**：请求与响应都不写会话，也不进任何日志正文（日志只有诊断错误）。
- **失败不猜测**：读不到可用答案一律 502。调用方要 fail-open 还是 fail-closed 由自己决定
  （例如 `choice` 失败就退回默认分支）；接口不为调用方编造一个答案。

## 7. 源码边界

```
rust/crates/omnicrawl-decision/
├── src/lib.rs        # 公共导出门面与模块边界
├── src/main.rs       # 二进制：serve / ensure / stop / status
├── src/config.rs     # [api] 段 + 默认决策渠道的装配与就绪判定
├── src/app.rs        # 路由、{"data"} / {"error"} 信封
├── src/routes.rs     # 四个决策端点 + status 的入参校验与错误映射
├── src/upstream.rs   # 四种语义（decide / choice / rank / review）的出站转发
└── src/server.rs     # 常驻服务生命周期：按端口复用、拉起、停止、PID 与日志
```

配置域在 `rust/crates/omnicrawl-config/src/features/decision_model.rs`（`DecisionApiConfig`、
`load_decision_api_configuration` / `save_decision_api_configuration`）。

**不写第二套决策逻辑**：请求构造、响应解析、脱敏旁路、审查提问都复用
`rust/crates/omnicrawl-host/src/decision_wire.rs` 与 `rust/crates/omnicrawl-host/src/review.rs`，
因此这个接口与宿主内部三处调用点在语义上天然一致。
