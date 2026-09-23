# omnicrawl-mcp

`omnicrawl/mcp/`（7 个模块、2,759 行）的 Rust 移植：把 MCP 子系统的**全部逻辑**搬进内核，
让 Rust 侧不再依赖 Python 宿主提供的 MCP 能力。语义基准是 Python 实现，逐项对照见下文。

```
crates/omnicrawl-mcp/
├── src/config.rs    # [mcp] 配置段、环境变量覆盖、Server/传输/风险等级校验（config.py 444 行）
├── src/registry.rs  # Tool/Resource/Prompt 元数据、去重诊断、命名空间化（registry.py 128 行）
├── src/security.rs  # 参数体积与轻量 JSON Schema 校验、密钥脱敏（security.py 188 行）
├── src/audit.rs     # 工作区内 JSONL 审计、预览截断、时间源注入（audit.py 143 行）
├── src/jsonrpc.rs   # Content-Length 分帧、JSON-RPC 拆包、SSE 解析、能力分页、结果文本化
├── src/stdio.rs     # stdio 传输：子进程、常驻读线程、stderr 排空、超时回收重启（client.py 1465 行）
├── src/http.rs      # Streamable HTTP 传输：会话头、协议头、SSE 响应
├── src/client.rs    # 多 Server 管理器：并发发现、状态与诊断、失败降级、调用与审计
├── src/server.rs    # 本地 stdio MCP Server：只读文档 / 内置文档 / 健康检查 / 四个 Prompt（server.py 347 行）
├── src/bundled.rs   # 随包内置文档（`include_str!` 打进二进制）
├── src/ids.rs       # 会话与审计 ID（形态同 Python 的 `uuid4().hex[:12]`）
├── src/bin/omnicrawl_mcp_server.rs  # `omnicrawl-mcp-server` 二进制（对映 `python -m omnicrawl.mcp.server`）
└── tests/           # 六组对照/端到端测试 + `tests/fixtures/mcp_parity.json`
```

## 对照工作流

Python 侧是语义基准，不靠人读代码对齐：

```bash
python rust/tools/gen_mcp_fixture.py       # 用 omnicrawl/mcp/ 真实现生成期望值
cd rust && cargo test -p omnicrawl-mcp
```

`tests/fixtures/mcp_parity.json` 覆盖八组：

| 组 | 覆盖 |
| --- | --- |
| `config` | 21 个配置用例：stdio/HTTP Server、环境变量覆盖、字符串布尔、各类校验错误文案、同名 Server 去重 |
| `registry` | 命名空间化、重复能力跳过与诊断顺序、空 Schema 与含中文键的紧凑 Schema |
| `security` | 19 个参数校验用例（必填、类型、长度、项数、上下界、超限）与 7 个脱敏用例 |
| `audit` | 审计行逐字节（固定时刻）、先脱敏后截断、关闭时不写文件、越界路径回落 |
| `protocol` | 11 个分帧用例、9 个 SSE 用例、13 个结果文本化用例、8 个拆包用例、能力声明与 5 组分页 |
| `server` | 24 个本地 Server 请求的完整响应（含内置文档与受保护路径） |
| `manager` | 13 个场景：注册、状态、失败降级、外部 Server 拦截、传输不支持、关闭后调用、审计行 |
| `http` | 14 个场景：JSON/SSE 响应、会话头推进、协议头、自定义头覆盖、错误映射、超时 |

另有 `tests/stdio_e2e.rs`：管理器真的拉起 `omnicrawl-mcp-server` 子进程，握手、分帧、
stderr 排空、降级与关闭都在真进程上验证（不依赖 Python）。

> `omnicrawl/docs/*.md` 改过之后要重新跑一次生成脚本：内置文档是编译期嵌进二进制的，
> 数据集里的内容哈希会随之失效（这是有意的——它保证二进制里的文档与安装包一致）。

## 与 Python 的已知差异

1. **错误类型统一**：Python 用 `MCPConfigError` / `MCPClientError` / `TimeoutError` / `ValueError`
   四种异常区分来源，Rust 统一为 `Result` 加错误枚举（`McpConfigError`、`McpClientError`、
   `McpCallError::{Timeout,Invalid,Failed}`），**文案逐字保留**（数据集里的 `error_kind`
   记录了 Python 的异常类型，Rust 不区分；例如 `mcp.policy` 不是对象时 Python 抛
   `RuntimeConfigError`，Rust 归一到配置错误）。
2. **本地 Server 的非法 `Content-Length`**：Python 的 `server.py` 会让 `int()` 抛 `ValueError`
   直接打断 stdio 循环，Rust 降级为可诊断的协议错误（`MCP Content-Length 不是整数。`）并结束循环。
3. **非法 JSON 的细节文案**：Python 附带 `json` 模块的解码错误原文（`Expecting value: line 1 column 1`），
   Rust 附带 `serde_json` 的错误原文，前缀一致、细节不同。
4. **传输层错误文案**：Python 用 httpx 的异常文本，Rust 用 ureq 的（例如连接被拒时
   `MCP Server HTTP 请求失败：HttpError(… )`），前缀一致、细节不同。
5. **进程收尾方式**：Python 先 `terminate()` 再 `kill()`，Rust 直接 `kill()`（Windows 上没有
   SIGTERM 语义，与仓库既有实现一致）。
6. **ID 生成**：Python 用 `uuid4().hex[:12]`，Rust 用「时间 + 进程 + 计数器」混合出的 12 位
   十六进制；形态一致、取值不可复现（对照测试把 ID 折成 `{id}`）。
7. **时间源**：Rust 的审计时间源可注入，运行期用 `chrono::Local`（与 Python
   `datetime.now().astimezone().isoformat(timespec="seconds")` 同形）。
8. **内置文档**：Python 从安装目录读 `docs/*.md`，Rust 用 `include_str!` 打进二进制；
   文件名、顺序与内容哈希由对照测试钉住（`tests/server_parity.rs`）。
9. **`which` 解析**：`shutil.which` 的 PATH/PATHEXT 子集（Windows 上补 `.COM/.EXE/.BAT/.CMD`），
   解析不到时原样交给 spawn 报错。
10. **HTTP 连接池**：Python 每个 Server 一个 httpx Client（含 keepalive 配置），Rust 每个
    连接一个 `ureq::Agent`，没有显式的 `close()`（`close()` 是空实现）。

## 宿主接线（`omnicrawl-tui`）

- 启动期读 `config.toml` 的 `[mcp]` 段（`load_mcp_config` + 进程环境变量），配置读取失败只
  警告不阻断启动。
- **能力发现发生在握手之前**：协议 v1 的工具声明固定在 `initialize.model.tools`，而 Python 是
  「首次使用时懒加载」。这是有意的差异：Rust 宿主用启动期的一次阻塞换取工具面完整，
  发现耗时与 Server 数量成正比（并发上限 8）。
- 已发现的 Tool 以 `server.tool` 进表，Resource/Prompt 各生成一个适配工具
  （`mcp_read_resource__<logical_uri>` / `mcp_get_prompt__<logical_name>`），
  执行体走 `omnicrawl-controllers` 的 `mcp_*_result` 结果信封（与 Python 同一份文案）。
- 退出时 `ToolRegistry::close_mcp()` 关闭全部 MCP 连接，不留孤儿子进程。
- HUD 第一行新增 `MCP n` 段（n = 启用的 Server 数）。

**宿主缺口**（本 crate 的接口已就绪，缺的是宿主侧接线，不属于内核移植范围）：

- Textual 的三个 MCP 设置屏（`ui/fullscreen/screens/mcp_*`）→ Rust 全屏设置面板；
- 本地 API 的 MCP 设置路由（`api/routes/settings.py`）；
- `/mcp` 状态命令（Rust TUI 尚未接线斜杠命令面，框架与 `/mcp` 命令定义见 `omnicrawl-commands`）；
  `McpClientManager::format_status()` 已对照就绪，
  拿到命令面即可直接输出；
- `ui/fullscreen/status` 的完整状态行（Python 对映层）尚未接进渲染路径，MCP 计数目前只进
  简化版 HUD；
- 审批 `review` 分支里「MCP 工具被拒绝时写审计」的编排：`record_denied_tool_call`
  （拒绝结果 + `approval_result=denied` 审计）已移植并有对照，缺的是宿主审批拒绝路径调用它。

内核侧 MCP 接口按 Python 公开面逐项核对无缺口：配置读写、能力发现、工具/资源/提示调用、
注册表与安全策略、审计、状态查询、stdio 与 HTTP 传输都在 `src/` 落地。
