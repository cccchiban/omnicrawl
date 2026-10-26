# 抓取接口（fetcher 的本地 REST 通道）

`fetcher` 工具本来只在 Agent 回合内可用：模型要抓网页就走一次工具调用。**抓取接口**
把同一份抓取能力以 REST 形式暴露给本机其它程序——Python 等批量脚本、CLI、Skill——
它们不必经过主模型与对话回合就能抓网页。

一句话：**它是 `fetcher` 的无状态前置服务**，接口收 JSON、回结构化结果，抓取语义与工具完全同源。

## 1. 适用场景

| 场景 | 用法 |
|---|---|
| Python 批量脚本要抓几十上百个页面并落库 | `POST /v1/fetch`（一次多 URL、并行抓取） |
| 脚本要原始 HTML 自己做解析 | `POST /v1/fetch` 带 `"max_html": true` |
| 目标站点正文靠脚本渲染（SPA 外壳） | 默认就会在本机沙箱重建，必要时加 `"force_scripts": true` |
| 内网自签名证书站点 | `"insecure": true` |
| 想先确认服务在不在、跑了什么参数 | `GET /v1/status` |

典型用法：脚本里 `requests.post("http://127.0.0.1:8768/v1/fetch", json={...})`，
拿回 `results` 后按 `url` 对齐自己的数据表。

## 2. 启动与配置

### 2.1 配置（`config.toml` 的 `[fetch_api]` 段）

```toml
[fetch_api]
enabled = false
host = "127.0.0.1"
port = 8768
impersonate = ""
max_urls = 0
```

| 字段 | 默认值 | 说明 |
|---|---|---|
| `enabled` | `false` | 是否随宿主启动常驻服务 |
| `host` | `127.0.0.1` | 只接受回环地址（`127.0.0.1` / `localhost` / `::1`） |
| `port` | `8768` | 与本地 API（8765）、决策接口（8767）错开 |
| `impersonate` | 空 | 固定浏览器指纹族（`chrome`/`firefox`/`safari`/`edge`）；空表示按请求参数（默认 `chrome`） |
| `max_urls` | `0` | 单次请求的 URL 上限；`0` 表示用默认上限 20，否则按 1–20 收窄 |

段缺失、读盘失败或取值非法（非回环地址、端口越界、`max_urls` 越界）时按「未启用」处理并
回落到默认值：抓取接口是可选的外部入口，配置坏掉不应影响宿主内部工具。

### 2.2 启动方式

随 OmniCrawl 启动时按配置自动拉起（已在跑则直接复用，不重复拉起）；也可以手动控制：

```powershell
omnicrawl fetch ensure   # 确保本机有一份可用的服务（已就绪则复用，否则拉起并等就绪）
omnicrawl fetch serve    # 前台常驻监听（由 ensure 以脱离宿主的方式拉起）
omnicrawl fetch status   # 打印配置与监听状态
omnicrawl fetch stop     # 停止本机共用的服务
```

服务是**本机共用**的常驻进程：按 PID 文件（`~/.OmniCrawl/fetch_api.pid`）与服务日志
（`~/.OmniCrawl/fetch_api.log`）认定是否已就绪（不发起网络探测），就绪则复用、否则拉起；
进程以脱离宿主的方式创建，OmniCrawl 退出（含崩溃）都不会回收它。因此 Python 脚本随时可以
调用，不必依赖宿主是否在跑。

默认地址：`http://127.0.0.1:8768`。

## 3. 访问控制与响应格式

**接口不做鉴权、也不做参数类型校验**：只要监听地址可达就能直接抓取。准入控制只有两条：

- `host` 只允许回环地址（配置装载时校验），因此默认只有本机程序能访问；
- 服务默认关闭（`enabled = false`），不显式打开不会监听。

「不做参数校验」是刻意的：批量脚本写起来的顺手程度比严格更重要，所有取值都走 `fetcher`
工具那套宽松归一化（`"max_chars": "20000"`、`"insecure": "true"` 都照常生效），
真正的边界（URL 数量上限、体积上限、robots.txt、超时）在抓取执行体里收口。

成功响应：

```json
{"data": {}}
```

错误响应：

```json
{"error": {"code": "INVALID_REQUEST", "message": "缺少字段 urls（或 url）：请提供至少一个要抓取的地址。"}}
```

| HTTP | code | 说明 |
|---|---|---|
| 400 | `INVALID_REQUEST` | 请求体不是对象，或没有 `urls` / `url` 字段 |
| 404 | `NOT_FOUND` | 路径不存在 |
| 500 | `INTERNAL_ERROR` | 抓取任务异常结束（线程崩溃等本机故障） |

**单个 URL 抓取失败不算请求失败**：那是 `results[i].error` 里的内容，HTTP 仍是 200。
这与工具语义一致——批量抓取时一条坏链接不该让整批白跑。

## 4. 接口清单

### `GET /health`

```json
{"status": "ok", "service": "omnicrawl-fetch"}
```

### `GET /v1/status`

```json
{
  "data": {
    "ready": true,
    "listen": "127.0.0.1:8768",
    "max_urls": 20,
    "impersonate": null,
    "unavailable_reason": null
  }
}
```

### `POST /v1/fetch` — 抓取

```json
{
  "urls": ["https://example.com/a", "https://example.com/b"],
  "max_chars": 20000,
  "max_html": false,
  "timeout": 20,
  "parallel": true,
  "insecure": false,
  "impersonate": "chrome",
  "run_scripts": true,
  "force_scripts": false
}
```

响应：

```json
{
  "data": {
    "count": 2,
    "elapsed_seconds": 1.42,
    "results": [
      {
        "url": "https://example.com/a",
        "error": null,
        "status": 200,
        "final_url": "https://example.com/a",
        "title": "示例页",
        "content": "正文内容……"
      },
      {
        "url": "https://example.com/b",
        "error": "HTTP 404（目标返回错误状态码）",
        "status": 0,
        "final_url": "",
        "title": "",
        "content": ""
      }
    ]
  }
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `urls` | 二选一 | 地址数组，最多 20 条（多于此按上限截断）；也接受逗号或换行分隔的字符串 |
| `url` | 二选一 | 单地址简写；与 `urls` 同时出现时以 `urls` 为准 |
| `max_chars` | 否 | 每页正文上限，200–200000，默认 8000 |
| `max_html` | 否 | `true` 时 `content` 给原始 HTML，不做正文提取 |
| `timeout` | 否 | 单请求超时（秒），1–60，默认 15 |
| `parallel` | 否 | 多 URL 是否并行（最多 8 个线程），默认 `true` |
| `insecure` | 否 | 跳过 TLS 证书校验（仅内网自签名站点），默认 `false` |
| `impersonate` | 否 | 浏览器指纹族 `chrome`/`firefox`/`safari`/`edge`，默认 `chrome`。配置里设了 `impersonate` 时以配置为准 |
| `run_scripts` | 否 | 正文近乎为空时是否在本机沙箱执行页面脚本重建，默认 `true` |
| `force_scripts` | 否 | 正文够长也照常执行脚本（指纹站点、动态面板），默认 `false` |

结果里的 `content` 已按 `max_chars` 截断；`status` 是最终状态码（重定向后），
`final_url` 是跟随 HTTP 重定向与 `<meta refresh>` 之后的地址。

## 5. 调用示例

Python（`requests`）：

```python
import requests

response = requests.post(
    "http://127.0.0.1:8768/v1/fetch",
    json={"urls": ["https://example.com/"], "max_chars": 20000},
    timeout=120,
)
for item in response.json()["data"]["results"]:
    if item["error"]:
        print(item["url"], "失败：", item["error"])
    else:
        print(item["title"], len(item["content"]))
```

Python（仅标准库）：

```python
import json, urllib.request

request = urllib.request.Request(
    "http://127.0.0.1:8768/v1/fetch",
    data=json.dumps({"urls": ["https://example.com/"]}).encode(),
    headers={"Content-Type": "application/json"},
)
with urllib.request.urlopen(request, timeout=120) as response:
    print(json.load(response)["data"]["results"][0]["title"])
```

PowerShell：

```powershell
$body = @{ urls = @("https://example.com/"); max_html = $false } | ConvertTo-Json -Depth 5
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:8768/v1/fetch `
  -ContentType application/json -Body $body | ConvertTo-Json -Depth 8
```

curl：

```bash
curl -s http://127.0.0.1:8768/v1/fetch \
  -H "Content-Type: application/json" \
  -d '{"urls":["https://example.com/"],"max_chars":4000}'
```

## 6. 语义与安全边界

- **只监听回环**：非回环地址在配置装载时就被拒绝；服务进程本身也只绑定配置里的回环地址。
- **不做鉴权**：任何能访问监听地址的调用方都能直接抓取。因此「只监听回环」是唯一的准入控制——
  不要把 `host` 放宽到非回环地址，那等于把抓取能力暴露给整个网络。
- **仍然遵守 robots.txt**：每个 URL（含重定向与脚本外链）都先查该站点的 robots.txt，
  明确禁止的路径直接失败并回报原因；不绕过验证码与挑战页。
- **每次请求一份独立 cookie 罐**：不同脚本、不同 Python 进程的会话 cookie 不共享，
  一个脚本登录后的身份不会漏给另一个。罐不落盘，请求结束即失。
- **与工具同一套指纹传输**：默认走浏览器指纹栈（TLS 档案 + HTTP/2），
  `impersonate` 切换档案族；JS 沙箱内的脚本请求继承同一身份。
- **体积与数量收口**：单次请求 URL ≤ 上限（默认 20，可配小不可配大）；外链脚本单份 8 MB、
  一份文档最多 16 个且合计 16 MB；脚本内 `fetch`/XHR 请求体 ≤ 256 KB、响应体 ≤ 4 MB，
  且只允许页面同站。
- **不落盘、不进会话**：请求与响应都不写会话记录，也不进任何会话转录。
- **失败不整批作废**：单个 URL 的失败写进该条结果的 `error`，其余 URL 照常返回。

## 7. 源码边界

```
rust/crates/omnicrawl-fetch/
├── src/lib.rs        # 公共导出门面与模块边界
├── src/main.rs       # 二进制：serve / ensure / stop / status
├── src/config.rs     # [fetch_api] 段 + 抓取执行体的装配
├── src/app.rs        # 路由、{"data"} / {"error"} 信封
├── src/routes.rs     # /v1/fetch 的参数透传与结果映射
└── src/server.rs     # 常驻服务生命周期：按记录复用、拉起、停止、PID 与日志
```

配置域在 `rust/crates/omnicrawl-config/src/features/fetch_api.rs`（`FetchApiConfig`、
`load_fetch_api_configuration` / `save_fetch_api_configuration`）。

**不写第二套抓取逻辑**：入参归一化、robots 校验、指纹传输、正文提取与本地 JS 沙箱重建都复用
`rust/crates/omnicrawl-host/src/tools/fetcher.rs` 的 `fetch_batch`（`fetcher` 工具本身也调它），
因此这个接口与工具在语义上天然一致——参数同名、默认值同值、失败文案同源。
