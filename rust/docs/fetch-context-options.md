# 抓取引擎与上下文档位核实

核实日期：2026-11-22。以下为官方 API 资料，不将网页端产品限制、计价阈值与模型窗口混为一谈。

## Keenable 抓取

官方文档：<https://docs.keenable.ai/api-reference/fetch>

- 有 Key：`GET https://api.keenable.ai/v1/fetch`，请求头 `X-API-Key`，凭据复用 `[web_search] keenable_api_key`。
- 无 Key：`GET https://api.keenable.ai/v1/fetch/public`，请求头 `X-Keenable-Title: OmniCrawl`。匿名额度按出口 IP 共享，因此只在显式选择时匿名调用；负载均衡要求 Keenable 已配置 Key。
- 查询字段 `url` 必须完整 URL 编码，`max_chars` 限制正文字符数；使用 `live=true` 读取当前页面而非索引副本。不传 `prompt`，避免把模型抽取答案当作原文。
- 成功响应顶层 `title`、`content`；错误由 HTTP 状态和 `error` 表达。继续使用既有传输、robots 判定及 `fallback_local` 机制。

## 新增上下文候选

所有 K 均为 1000 Token；候选、持久化和显示使用精确 Token 数。保留原有 32K、64K、128K、256K、512K、1024K、2048K，不将其改成其他单位。

| 候选 | Token | 一手来源与含义 |
| --- | ---: | --- |
| 131.072K | 131,072 | [gpt-oss-120b](https://developers.openai.com/api/docs/models/gpt-oss-120b.md) 总窗口 |
| 200K | 200,000 | [o3](https://developers.openai.com/api/docs/models/o3.md) 总窗口 |
| 272K | 272,000 | [GPT-5](https://developers.openai.com/api/docs/models/gpt-5.md) **最大输入预算**，不是总窗口 |
| 400K | 400,000 | [GPT-5](https://developers.openai.com/api/docs/models/gpt-5.md) 总窗口，最大输出 128,000 |
| 500K | 500,000 | [Grok 4.7](https://docs.x.ai/developers/models/grok-4.7) 总窗口，不是 512,000 |
| 1000K | 1,000,000 | [Grok 4.3](https://docs.x.ai/developers/models/grok-4.3) 总窗口，不是 1,024,000 |
| 1047.576K | 1,047,576 | [GPT-4.1](https://developers.openai.com/api/docs/models/gpt-4.1.md) 总窗口，最大输出 32,768 |
| 1050K | 1,050,000 | [GPT-5.4](https://developers.openai.com/api/docs/models/gpt-5.4.md) 总窗口；272K 在此页是长上下文计价阈值，不是硬上限 |
| 2000K | 2,000,000 | [Grok 4.1 Fast 发布说明](https://x.ai/news/grok-4-1-fast) 历史型号的 2M 总窗口，不是 2,048,000 |

没有找到 285K 的官方模型窗口依据，因此未添加。272K 可用作保守输入预算；选择总窗口时仍须为输出和工具声明预留空间。档位只是用户选项，不按模型自动推断、不改变既有默认设置；第三方网关可能另设上限。

官方站点 robots 已核对：developers.openai.com、docs.x.ai 允许文档；x.ai 仅禁止 `/tools/`；docs.keenable.ai 禁止 `/cdn-cgi/` 与 `/_next/`，本次引用路径不在禁止范围。
