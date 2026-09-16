"""共享 HTTP 客户端配置。

集中管理出站 HTTP 连接的关键参数，避免各调用点各自 ``httpx.Client()``
落回 httpx 默认值。

**为什么需要这个模块**

1. **keepalive 窗口**：httpx 默认 ``Limits(keepalive_expiry=5.0)``，空闲超过
   5 秒的连接会被连接池主动关闭。OmniCrawl 的 TUI / API / 连接器都是长时间
   常驻进程，相邻两次请求之间经常穿插工具执行、审批等待和用户输入停顿，
   间隔普遍超过 5 秒，连接因此每次都被丢弃。实测非流式请求（模型发现、
   MCP 工具调用）抬高该窗口后可复用连接，省下一次 TCP + TLS 握手。

2. **HTTP/2**：仅抬高 keepalive 不足以复用模型请求的连接。Chat Completions
   走 SSE 流式响应，SDK 读到 ``data: [DONE]`` 即停止迭代，HTTP body 没有
   读到 EOF，httpcore 只能关闭连接而不能归还连接池 —— 这是协议层限制，
   与 keepalive 无关（实测：流式请求 2 次 = 2 次握手；``http2=True`` 时
   2 次 = 1 次）。HTTP/2 下关闭单个 stream 不影响底层连接，连接得以复用，
   每个模型请求因此省下一次握手（实测本网关约 0.8–1.3 秒/次）。

**取值权衡**：keepalive 窗口过长时，服务端可能已单方面关闭空闲连接，复用时
会触发一次 SDK 自动重试（OpenAI SDK 默认 ``max_retries=2``）。重试一次的
代价远小于每次都重新握手，因此选择较长的窗口。

httpx 只在真正创建客户端时导入，模块本身不引入重依赖。
"""

from __future__ import annotations

import importlib.util
import logging
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:  # pragma: no cover - 仅用于类型标注
    import httpx

LOGGER = logging.getLogger(__name__)

# 空闲连接保留时长。需覆盖工具执行 + 审批 + 用户思考停顿的常见间隔。
HTTP_KEEPALIVE_EXPIRY_SECONDS = 300.0
HTTP_MAX_CONNECTIONS = 100
HTTP_MAX_KEEPALIVE_CONNECTIONS = 20

# 见模块 docstring：流式（SSE）请求只有走 HTTP/2 才能复用连接。
HTTP2_ENABLED = True

_http2_unavailable_logged = False


def connection_limits() -> "httpx.Limits":
    """返回 OmniCrawl 统一的连接池参数（显式抬高 keepalive 窗口）。"""

    import httpx

    return httpx.Limits(
        max_connections=HTTP_MAX_CONNECTIONS,
        max_keepalive_connections=HTTP_MAX_KEEPALIVE_CONNECTIONS,
        keepalive_expiry=HTTP_KEEPALIVE_EXPIRY_SECONDS,
    )


def http2_available() -> bool:
    """h2 已声明为必装依赖；此检查只防御未同步安装依赖的旧环境。

    ``httpx.Client(http2=True)`` 在缺少 h2 时抛 ImportError，会让所有模型请求
    直接失败。旧环境降级为 HTTP/1.1 仍可正常工作（只是不复用连接），因此
    这里降级并只告警一次，而不是让整个 Agent 不可用。
    """

    global _http2_unavailable_logged

    if not HTTP2_ENABLED:
        return False
    if importlib.util.find_spec("h2") is not None:
        return True
    if not _http2_unavailable_logged:
        _http2_unavailable_logged = True
        LOGGER.warning(
            "未安装 h2，HTTP/2 已降级为 HTTP/1.1：流式模型请求将无法复用连接，"
            "每个请求会多一次 TLS 握手。请执行 pip install 'h2>=4.0.0,<5'。"
        )
    return False


def create_direct_client(*, timeout: Any = None) -> "httpx.Client":
    """创建直连目标的 httpx 客户端。

    禁用 ``trust_env``：OpenAI SDK 默认读取系统代理，Windows 上本地代理常把
    HTTPS 代理声明为 ``https://127.0.0.1:port`` 但实际只支持明文 CONNECT，
    会在代理握手阶段抛 ``SSLEOFError``。OmniCrawl 没有 Provider 代理配置，
    因此默认直连。
    """

    import httpx

    kwargs: dict[str, Any] = {
        "trust_env": False,
        "follow_redirects": True,
        "limits": connection_limits(),
    }
    if http2_available():
        # 服务端不支持 h2 时由 ALPN 协商回退到 HTTP/1.1，无需另加分支。
        kwargs["http2"] = True
    if timeout is not None:
        kwargs["timeout"] = timeout
    return httpx.Client(**kwargs)
