"""网页搜索工具：查询 Bing、DuckDuckGo 或雅虎并返回标题、链接与摘要。

实现要点：
- 使用 httpx（项目既有依赖）发起请求，不引入新依赖。
- 请求自带桌面 Chrome 浏览器环境模拟（User-Agent、Accept、Sec-Fetch-* 等
  请求头、跟随重定向、干净会话），降低被搜索引擎当作脚本请求拦截的概率。
- 只访问公开搜索页面，不做验证码或明确反爬页面的绕过：检测到验证码或
  “unusual traffic” 等拦截特征时返回明确错误提示，由用户换引擎或稍后重试。
- 各引擎解析独立，输出统一为 标题/链接/摘要 结构；链接会还原雅虎的
  跳转参数与 DuckDuckGo 的 uddg= 跳转参数。
"""

from __future__ import annotations

import html
import re
import sys
import time
import urllib.parse
from dataclasses import dataclass, replace
from typing import Any, Mapping, Sequence

import httpx


DEFAULT_TIMEOUT_SECONDS = 10.0
MAX_RESULTS_PER_ENGINE = 10
DEFAULT_USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"
)
SUPPORTED_ENGINES = ("bing", "duckduckgo", "yahoo")

# 桌面浏览器常规请求头：与搜索引擎期望的浏览器环境一致。
# Accept-Encoding 由 httpx 自动处理（默认 gzip, deflate, br 并自动解码），不手动设置。
_BROWSER_HEADERS = {
    "User-Agent": DEFAULT_USER_AGENT,
    "Accept": (
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,"
        "image/webp,image/apng,*/*;q=0.8"
    ),
    "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8",
    "Connection": "keep-alive",
    "Upgrade-Insecure-Requests": "1",
    "Sec-Fetch-Dest": "document",
    "Sec-Fetch-Mode": "navigate",
    "Sec-Fetch-Site": "none",
    "Sec-Fetch-User": "?1",
    "Cache-Control": "no-cache",
}

# 拦截特征标记：出现这些内容说明引擎要求人机验证或拒绝脚本访问，此时如实报错。
_CAPTCHA_MARKERS = (
    "captcha",
    "unusual traffic",
    "enable cookies",
    "verify you're human",
    "请输入验证码",
    "人机验证",
    "检测到异常流量",
)

# DuckDuckGo 区域参数（kl）映射；未知语言默认英文区。
_DDG_REGION = {
    "zh": "cn-zh",
    "zh-cn": "cn-zh",
    "zh-tw": "tw-zh",
    "en": "us-en",
    "en-us": "us-en",
    "en-gb": "uk-en",
    "ja": "jp-ja",
}

_TAG_RE = re.compile(r"<[^>]+>")


class WebSearchError(RuntimeError):
    """网页搜索失败（参数、网络、拦截或解析错误）。"""


@dataclass(frozen=True)
class WebSearchResult:
    """单条搜索结果：标题、链接与摘要。"""

    title: str
    url: str
    snippet: str = ""


def _strip_tags(text: str) -> str:
    """剥离 HTML 标签、还原实体并把连续空白压缩为单个空格。"""

    cleaned = html.unescape(_TAG_RE.sub(" ", text or ""))
    return re.sub(r"\s+", " ", cleaned).strip()


def _clean_yahoo_url(raw: str) -> str:
    """还原雅虎搜索结果的跳转参数；普通链接原样返回。

    雅虎跳转链接为 r.search.yahoo.com/_ylt=.../RU=<编码目标>/RK=...，
    RU 可能是 query 参数（?RU=）或路径段（/RU=），两种都处理；
    HTML 中的 & 可能被编码为 &amp;，先还原再解析。
    """

    decoded = html.unescape(raw)
    match = re.search(r"(?:/|&|\?)RU=([^/&]+)", decoded)
    if match:
        return urllib.parse.unquote(match.group(1))
    return decoded


def _clean_duckduckgo_url(raw: str) -> str:
    """还原 DuckDuckGo 的 //duckduckgo.com/l/?uddg=<编码目标> 跳转链接。"""

    query = urllib.parse.parse_qs(urllib.parse.urlsplit(raw).query)
    target = query.get("uddg", [""])
    if target and target[0]:
        return urllib.parse.unquote(target[0])
    return raw


def _parse_yahoo(html_text: str) -> list[WebSearchResult]:
    """解析雅虎搜索结果页：compTitle/algo 容器内 a>h3 标题锚点 + compText 摘要。

    雅虎新版 HTML 结构为 ``<a href="r.search.yahoo.com/...RU=..."><h3>标题</h3></a>``，
    链接位于 h3 外层；摘要块 class 为 compText。
    """

    results: list[WebSearchResult] = []
    for match in re.finditer(
        r'<a[^>]*class="[^"]*d-ib[^"]*"[^>]*href="([^"]*)"[^>]*>.*?<h3[^>]*class="[^"]*title[^"]*"[^>]*>(.*?)</h3>',
        html_text,
        re.DOTALL,
    ):
        url = _clean_yahoo_url(match.group(1))
        title = _strip_tags(match.group(2))
        if not url.startswith("http") or not title:
            continue
        results.append(WebSearchResult(title=title, url=url))
    snippets = re.findall(
        r'<div[^>]*class="[^"]*compText[^"]*"[^>]*>(.*?)</div>',
        html_text,
        re.DOTALL,
    )
    # compText 会出现推广位等非正文块，只取与结果数匹配的前几段。
    for index, snippet in enumerate(snippets[: len(results)]):
        text = _strip_tags(snippet)
        if text and len(text) > 4:
            results[index] = replace(results[index], snippet=text)
    return results


def _parse_bing(html_text: str) -> list[WebSearchResult]:
    """解析 Bing 搜索结果页：b_algo 块内 h2>a 锚点 + 首个 <p> 摘要。"""

    results: list[WebSearchResult] = []
    for block in re.findall(
        r'<li[^>]*class="[^"]*b_algo[^"]*"[^>]*>(.*?)</li>',
        html_text,
        re.DOTALL,
    ):
        anchor = re.search(
            r'<h2[^>]*>\s*<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>',
            block,
            re.DOTALL,
        )
        if not anchor:
            continue
        url = anchor.group(1)
        title = _strip_tags(anchor.group(2))
        if not url.startswith("http") or not title:
            continue
        snippet = ""
        paragraph = re.search(r"<p[^>]*>(.*?)</p>", block, re.DOTALL)
        if paragraph:
            snippet = _strip_tags(paragraph.group(1))
        results.append(WebSearchResult(title=title, url=url, snippet=snippet))
    return results


def _parse_duckduckgo(html_text: str) -> list[WebSearchResult]:
    """解析 DuckDuckGo HTML 版结果页：result__a 标题 + result__snippet 摘要。"""

    results: list[WebSearchResult] = []
    for match in re.finditer(
        r'<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]*)"[^>]*>(.*?)</a>',
        html_text,
        re.DOTALL,
    ):
        url = _clean_duckduckgo_url(match.group(1))
        title = _strip_tags(match.group(2))
        if not url.startswith("http") or not title:
            continue
        results.append(WebSearchResult(title=title, url=url))
    snippets = re.findall(
        r'<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>',
        html_text,
        re.DOTALL,
    )
    for index, snippet in enumerate(snippets[: len(results)]):
        text = _strip_tags(snippet)
        if text:
            results[index] = replace(results[index], snippet=text)
    return results


_PARSERS = {
    "bing": _parse_bing,
    "duckduckgo": _parse_duckduckgo,
    "yahoo": _parse_yahoo,
}


def _friendly_network_error(exc: httpx.HTTPError) -> str:
    """把 httpx 异常翻译成中文提示，便于 Agent 直接向用户说明。"""

    if isinstance(exc, httpx.TimeoutException):
        return "请求超时。"
    if isinstance(exc, httpx.ConnectError):
        return "无法连接到搜索引擎（网络不可达）。"
    if isinstance(exc, httpx.HTTPStatusError):
        status = exc.response.status_code
        if status == 429:
            return "请求过于频繁（HTTP 429），请稍后重试。"
        if status in (403, 451):
            return "搜索引擎拒绝访问（HTTP 403/451），可能触发了反爬限制。"
        return f"搜索引擎返回 HTTP {status}。"
    return f"网络请求失败：{exc}"


# Windows 系统代理注册表路径与键名。
_PROXY_REGISTRY_PATH = (
    r"Software\Microsoft\Windows\CurrentVersion\Internet Settings"
)


def _detect_windows_proxy(winreg_module: Any = None) -> str | None:
    """读取 Windows 系统代理（注册表 ProxyEnable/ProxyServer）。

    未启用系统代理、非 Windows 平台或读取失败时返回 None；
    ProxyServer 可能形如 ``127.0.0.1:7890`` 或 ``http=127.0.0.1:7890;https=...``，
    多协议形式时优先取 http 条目，并自动补齐 ``http://`` 前缀。
    """

    if sys.platform != "win32":
        return None
    try:
        if winreg_module is None:
            import winreg as winreg_module
    except ImportError:
        return None
    try:
        key = winreg_module.OpenKey(
            winreg_module.HKEY_CURRENT_USER, _PROXY_REGISTRY_PATH
        )
    except OSError:
        return None
    try:
        enabled, _ = winreg_module.QueryValueEx(key, "ProxyEnable")
        server, _ = winreg_module.QueryValueEx(key, "ProxyServer")
    except OSError:
        return None
    finally:
        winreg_module.CloseKey(key)
    if not enabled or not server:
        return None
    server = str(server).strip()
    if not server:
        return None
    if "=" in server:
        for part in server.split(";"):
            part = part.strip()
            if part.lower().startswith("http="):
                server = part.split("=", 1)[1].strip()
                break
    if not server:
        return None
    if "://" not in server:
        server = "http://" + server
    return server


class WebSearch:
    """多引擎网页搜索：参数校验、浏览器模拟请求、拦截检测与结果格式化。

    ``http_client`` 仅用于测试注入；生产环境每次调用使用干净的 httpx 会话
    （独立连接、不共享 Cookie），避免跨查询状态污染。
    """

    def __init__(
        self,
        *,
        http_client: Any = None,
        request_timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
        max_retries: int = 1,
        request_delay_seconds: float = 0.0,
        user_agent: str = DEFAULT_USER_AGENT,
        proxy: str | None = None,
    ) -> None:
        self.http_client = http_client
        self.request_timeout_seconds = max(1.0, float(request_timeout_seconds))
        self.max_retries = max(0, int(max_retries))
        self.request_delay_seconds = max(0.0, float(request_delay_seconds))
        self.user_agent = user_agent
        # proxy：None 表示自动检测 Windows 系统代理；空字符串表示不使用代理；
        # 其他非空字符串为显式代理地址。
        self.proxy = proxy

    def search(self, arguments: Mapping[str, Any]) -> str:
        """执行一次搜索并返回可直接展示的文本结果。"""

        query = str(arguments.get("query") or "").strip()
        if not query:
            raise WebSearchError("query 不能为空。")
        engine = str(arguments.get("engine") or "bing").strip().lower()
        if engine not in SUPPORTED_ENGINES:
            raise WebSearchError(
                f"engine 必须是 {'、'.join(SUPPORTED_ENGINES)} 之一。"
            )
        max_results = _clamp_int(
            arguments.get("max_results"),
            default=5,
            minimum=1,
            maximum=MAX_RESULTS_PER_ENGINE,
        )
        language = str(arguments.get("language") or "").strip() or None

        started = time.monotonic()
        last_error: Exception | None = None
        for attempt in range(self.max_retries + 1):
            try:
                results = self._fetch_and_parse(
                    engine, query, max_results=max_results, language=language
                )
                elapsed = time.monotonic() - started
                return self._format_results(engine, query, results, elapsed)
            except WebSearchError as exc:
                # 参数、拦截、解析类错误重试无意义，直接抛出。
                raise exc
            except httpx.HTTPError as exc:
                last_error = exc
                if attempt < self.max_retries:
                    time.sleep(self.request_delay_seconds or 0.4 * (attempt + 1))
        assert last_error is not None
        raise WebSearchError(
            f"搜索失败（{engine}）：{_friendly_network_error(last_error)}"
        )

    def _fetch_and_parse(
        self,
        engine: str,
        query: str,
        *,
        max_results: int,
        language: str | None,
    ) -> list[WebSearchResult]:
        html_text = self._request_html(engine, query, language)
        results = _PARSERS[engine](html_text)
        if not results:
            raise WebSearchError("未找到相关搜索结果（或页面结构无法解析）。")
        return results[:max_results]

    def _resolve_proxy(self) -> str | None:
        """解析代理配置：None 自动检测 Windows 系统代理，空字符串禁用，其他显式指定。"""

        if self.proxy is None:
            return _detect_windows_proxy()
        return self.proxy or None

    def _request_html(self, engine: str, query: str, language: str | None) -> str:
        url, params = self._endpoint(engine, query, language)
        headers = dict(_BROWSER_HEADERS)
        headers["User-Agent"] = self.user_agent
        if self.http_client is not None:
            response = self.http_client.get(url, params=params)
        else:
            # trust_env=False：只按显式 proxy 参数或自动检测到的系统代理请求，
            # 避免环境变量代理与注册表设置不一致导致行为漂移。
            with httpx.Client(
                headers=headers,
                follow_redirects=True,
                timeout=self.request_timeout_seconds,
                proxy=self._resolve_proxy(),
                trust_env=False,
            ) as client:
                response = client.get(url, params=params)
        response.raise_for_status()
        self._check_captcha(response.text)
        return response.text

    @staticmethod
    def _endpoint(
        engine: str, query: str, language: str | None
    ) -> tuple[str, dict[str, str]]:
        if engine == "bing":
            return (
                "https://www.bing.com/search",
                {
                    "q": query,
                    "count": str(MAX_RESULTS_PER_ENGINE),
                    "setlang": language or "zh-CN",
                },
            )
        if engine == "yahoo":
            return (
                "https://search.yahoo.com/search",
                {
                    "p": query,
                    "n": str(MAX_RESULTS_PER_ENGINE),
                },
            )
        region = _DDG_REGION.get(
            (language or "zh-CN").strip().lower(), "us-en"
        )
        return (
            "https://html.duckduckgo.com/html/",
            {"q": query, "kl": region},
        )

    @staticmethod
    def _check_captcha(text: str) -> None:
        """检测人机验证/反爬拦截特征；发现时如实报错，不尝试绕过。"""

        low = (text[:4000] + text[-1000:]).lower()
        if any(marker in low for marker in _CAPTCHA_MARKERS):
            raise WebSearchError(
                "搜索引擎要求人机验证或检测到异常流量，已停止请求（不会绕过验证码）。"
                "可稍后重试或换用其他引擎。"
            )

    @staticmethod
    def _format_results(
        engine: str,
        query: str,
        results: Sequence[WebSearchResult],
        elapsed: float,
    ) -> str:
        lines = [
            f"来源：{engine}｜查询：{query}｜用时：{elapsed:.2f}s｜共 {len(results)} 条"
        ]
        for index, result in enumerate(results, 1):
            title = result.title.strip().replace("\n", " ")
            lines.append(f"{index}. {_truncate(title, 150)}")
            lines.append(f"   {result.url}")
            snippet = result.snippet.strip().replace("\n", " ")
            if snippet:
                lines.append(f"   {_truncate(snippet, 300)}")
        return "\n".join(lines)


def _clamp_int(value: Any, *, default: int, minimum: int, maximum: int) -> int:
    try:
        number = int(value)
    except (TypeError, ValueError):
        return default
    return max(minimum, min(number, maximum))


def _truncate(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[: limit - 1] + "…"
