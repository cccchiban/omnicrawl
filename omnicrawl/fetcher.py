"""网页抓取工具：模拟浏览器指纹请求、并行抓取、容忍自签名证书并跟随页面跳转。

实现要点：
- 使用 curl_cffi 模拟 Chrome/Firefox/Safari/Edge 的 TLS（JA3/JA4）与 HTTP/2 指纹，
  配合桌面浏览器请求头，降低被基于请求指纹的简单反爬拦截的概率。
- 支持并行抓取多个 URL（ThreadPoolExecutor），单个 URL 失败不影响其他结果。
- insecure=true 时不校验 TLS 证书，用于内网自签名证书站点；默认校验。
- 跟随 HTTP 重定向（301/302/307/308）与 <meta http-equiv="refresh"> 页面跳转
  （最多 MAX_META_REDIRECTS 次）；meta refresh 的相对地址会基于当前页解析。
- 默认返回提取后的正文文本（限长 max_chars），max_html=true 时返回原始 HTML。
- 只抓取用户明确提供的 URL：不执行 JavaScript、不绕过验证码、不做搜索。
- 内网/本机目标（localhost、10.x、172.16-31.x、192.168.x）不走系统代理，直连。
"""

from __future__ import annotations

import json
import re
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor, as_completed
from typing import Any, Mapping, Sequence

from omnicrawl.web_search import _BROWSER_HEADERS, _detect_windows_proxy, _strip_tags

try:
    from curl_cffi import requests as _cffi_requests
except ImportError:  # pragma: no cover - 依赖缺失时的降级提示路径
    _cffi_requests = None

try:
    from bs4 import BeautifulSoup as _BeautifulSoup
except ImportError:  # pragma: no cover - 正文提取降级为正则去标签
    _BeautifulSoup = None

try:
    import lxml  # noqa: F401  # 仅探测 lxml 可用性，bs4 用它做 C 级解析
except ImportError:  # pragma: no cover - 解析器回退到 Python 标准库
    lxml = None

try:
    from lxml import html as _lxml_html  # 直接解析提取用，跳过 bs4 对象树
except ImportError:  # pragma: no cover - 降级到 bs4 路径
    _lxml_html = None

# bs4 解析器选择：优先 lxml（C 实现，解析快），缺失时回退 html.parser（纯 Python）。
# 实测 4.7MB 页面解析耗时 html.parser≈5.0s / lxml≈3.6s（bs4 树操作仍为 Python 层）。
_HTML_PARSER = "lxml" if lxml is not None else "html.parser"


DEFAULT_TIMEOUT_SECONDS = 15.0
DEFAULT_MAX_CHARS = 8000
MAX_URLS = 20
MAX_META_REDIRECTS = 3
MAX_WORKERS = 8
_IMPERSONATE_OPTIONS = ("chrome", "firefox", "safari", "edge")

_META_REFRESH_RE = re.compile(
    r'<meta\s+[^>]*http-equiv\s*=\s*["\']?refresh["\']?[^>]*content\s*=\s*["\']([^"\']*)["\']?',
    re.IGNORECASE,
)
_META_URL_RE = re.compile(r"url\s*=\s*([^\s;]+)", re.IGNORECASE)
_TITLE_RE = re.compile(r"<title[^>]*>(.*?)</title>", re.DOTALL | re.IGNORECASE)
_WHITESPACE_RE = re.compile(r"\s+")

# 内网/本机 IPv4 前缀：这些目标直连，避免系统代理劫持。
_PRIVATE_HOST_RE = re.compile(
    r"^(?:(?:127(?:\.\d{1,3}){3})|(?:10(?:\.\d{1,3}){3})|"
    r"(?:192\.168(?:\.\d{1,3}){2})|(?:172\.(?:1[6-9]|2\d|3[01])(?:\.\d{1,3}){2}))$"
)


class FetcherError(RuntimeError):
    """网页抓取失败（参数、网络、证书或解析错误）。"""


def _as_bool(value: Any, default: bool = False) -> bool:
    """宽松布尔转换：接受 bool、数字与常见字符串写法。"""

    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return bool(value)
    if isinstance(value, str):
        return value.strip().lower() in ("1", "true", "yes", "on")
    return default


def _clamp_int(
    value: Any, *, default: int, minimum: int, maximum: int
) -> int:
    try:
        number = int(value)
    except (TypeError, ValueError):
        return default
    return max(minimum, min(number, maximum))


def _clamp_float(
    value: Any, *, default: float, minimum: float, maximum: float
) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError):
        return default
    return max(minimum, min(number, maximum))


def _truncate(text: str, limit: int) -> str:
    return text if len(text) <= limit else text[: limit - 1] + "…"


def _parse_urls(value: Any) -> list[str]:
    """解析 urls 参数：支持逗号分隔字符串、JSON 数组字符串或 Python 列表。"""

    parts: list[str] = []
    if isinstance(value, str):
        raw = value.strip()
        if not raw:
            return []
        try:
            parsed = json.loads(raw)
            if isinstance(parsed, list):
                parts = [str(item).strip() for item in parsed]
        except (ValueError, TypeError):
            parts = [part.strip() for part in raw.split(",")]
    elif isinstance(value, (list, tuple)):
        parts = [str(item).strip() for item in value]
    return [part for part in parts if part][:MAX_URLS]


def _normalize_impersonate(value: Any) -> str:
    """校验并归一化浏览器指纹类型；未知值回退到 chrome。"""

    name = str(value or "chrome").strip().lower()
    # curl_cffi 支持带版本后缀（chrome124 等）；按前缀匹配以容忍版本差异。
    for option in _IMPERSONATE_OPTIONS:
        if name == option or name.startswith(option):
            return option
    return "chrome"


def _is_private_host(hostname: str) -> bool:
    """判断主机名是否内网/本机地址（含 localhost 与常见内网 IPv4 前缀）。"""

    if not hostname:
        return False
    lowered = hostname.lower()
    if lowered == "localhost" or lowered.endswith((".localhost", ".local")):
        return True
    return bool(_PRIVATE_HOST_RE.match(lowered))


def _resolve_proxy(url: str) -> str | None:
    """内网/本机目标直连（不走系统代理），其余按 web_search 同款规则自动检测代理。"""

    hostname = urllib.parse.urlsplit(url).hostname or ""
    if _is_private_host(hostname):
        return None
    return _detect_windows_proxy()


def _create_session(impersonate: str, timeout: float) -> Any:
    if _cffi_requests is None:
        raise FetcherError("curl_cffi 未安装，无法使用 fetcher 工具。")
    return _cffi_requests.Session(impersonate=impersonate, timeout=timeout)


def _meta_refresh_target(html_text: str, base_url: str) -> str | None:
    """解析 <meta http-equiv="refresh" content="N; url=X"> 的跳转目标。"""

    for match in _META_REFRESH_RE.finditer(html_text):
        content = match.group(1)
        url_match = _META_URL_RE.search(content)
        if not url_match:
            continue
        target = url_match.group(1).strip("'\"")
        if not target:
            continue
        return urllib.parse.urljoin(base_url, target)
    return None


def _extract_title(html_text: str) -> str:
    """提取 <title> 文本；不存在时返回空串。"""

    match = _TITLE_RE.search(html_text)
    if not match:
        return ""
    return _WHITESPACE_RE.sub(" ", _strip_tags(match.group(1))).strip()


def _extract_main_text(html_text: str, max_chars: int) -> str:
    """提取页面正文文本：优先 main/article，剔除脚本与样式后压缩空白。

    三级降级：lxml 直接提取（C 级解析+提取）→ bs4（lxml/html.parser）→ 正则去标签。
    """

    if _lxml_html is not None:
        # 快路径：lxml.html 直接解析与提取，避免 bs4 的 Python 对象树开销。
        # 实测 4.7MB 页面完整提取流程从 bs4 的 ~6.1s 降到 ~200ms（约 30x）。
        try:
            doc = _lxml_html.fromstring(html_text)
        except Exception:
            doc = None
        if doc is not None:
            for tag in ("script", "style", "noscript", "svg", "template"):
                for node in doc.xpath(f"//{tag}"):
                    parent = node.getparent()
                    if parent is not None:
                        parent.remove(node)
            container = None
            for selector in ("//main", "//article", "//body"):
                matched = doc.xpath(selector)
                if matched:
                    container = matched[0]
                    break
            if container is None:
                container = doc
            text = _WHITESPACE_RE.sub(" ", container.text_content()).strip()
            return _truncate(text, max_chars)

    if _BeautifulSoup is None:
        # 降级：去标签后整体截断，不含正文区域优选。
        text = _WHITESPACE_RE.sub(" ", _strip_tags(html_text)).strip()
        return _truncate(text, max_chars)
    soup = _BeautifulSoup(html_text, _HTML_PARSER)
    for tag in soup(["script", "style", "noscript", "svg", "template"]):
        tag.decompose()
    container = soup.find("main") or soup.find("article") or soup.body or soup
    text = _WHITESPACE_RE.sub(" ", container.get_text(" ", strip=True)).strip()
    return _truncate(text, max_chars)


def _friendly_error(exc: Exception) -> str:
    """把底层异常翻译成中文提示；证书失败时给出 insecure 建议。"""

    name = type(exc).__name__
    if "Timeout" in name:
        return "请求超时。"
    if "ConnectionError" in name:
        return "无法连接目标（网络不可达或目标拒绝连接）。"
    if "SSLError" in name or "Certificate" in name:
        return "TLS 证书校验失败；内网自签名站点可在参数中加 insecure=true 重试。"
    if "HTTPError" in name:
        return f"HTTP 请求失败：{exc}"
    return f"{name}: {exc}"


def _fetch_one(
    url: str,
    *,
    impersonate: str,
    insecure: bool,
    max_chars: int,
    max_html: bool,
    timeout: float,
) -> dict[str, Any]:
    """抓取单个 URL：跟随 HTTP 重定向与 meta refresh 跳转，返回结果条目。"""

    session = _create_session(impersonate, timeout)
    proxy = _resolve_proxy(url)
    if proxy:
        session.proxies = {"all": proxy}
    headers = dict(_BROWSER_HEADERS)

    current = url
    html_text = ""
    final_status = 0
    final_url = url
    for _ in range(MAX_META_REDIRECTS + 1):
        response = session.get(
            current, verify=not insecure, headers=headers
        )
        final_status = response.status_code
        if final_status >= 400:
            raise FetcherError(f"HTTP {final_status}（目标返回错误状态码）")
        html_text = response.text
        final_url = str(response.url)
        target = _meta_refresh_target(html_text, final_url)
        if target is None:
            break
        current = target

    return {
        "url": url,
        "status": final_status,
        "final_url": final_url,
        "title": _extract_title(html_text),
        "content": (
            html_text if max_html else _extract_main_text(html_text, max_chars)
        ),
    }


class Fetcher:
    """模拟浏览器的网页抓取器（指纹、并行、内网证书、页面跳转）。"""

    def __init__(
        self,
        *,
        request_timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
        max_workers: int = MAX_WORKERS,
    ):
        self.request_timeout_seconds = request_timeout_seconds
        self.max_workers = max_workers

    def fetch(self, arguments: Mapping[str, Any]) -> str:
        """执行一次抓取并返回可直接展示的文本结果。"""

        urls = _parse_urls(arguments.get("urls"))
        if not urls:
            raise FetcherError(
                "urls 不能为空：请提供至少一个要抓取的 URL（逗号分隔或 JSON 数组）。"
            )
        insecure = _as_bool(arguments.get("insecure"), False)
        parallel = _as_bool(arguments.get("parallel"), True)
        max_html = _as_bool(arguments.get("max_html"), False)
        max_chars = _clamp_int(
            arguments.get("max_chars"),
            default=DEFAULT_MAX_CHARS,
            minimum=200,
            maximum=200000,
        )
        timeout = _clamp_float(
            arguments.get("timeout"),
            default=self.request_timeout_seconds,
            minimum=1.0,
            maximum=60.0,
        )
        impersonate = _normalize_impersonate(arguments.get("impersonate"))

        kwargs: dict[str, Any] = {
            "impersonate": impersonate,
            "insecure": insecure,
            "max_chars": max_chars,
            "max_html": max_html,
            "timeout": timeout,
        }

        started = time.monotonic()
        if parallel and len(urls) > 1:
            entries = self._fetch_parallel(urls, kwargs)
        else:
            entries = [self._fetch_one_safe(url, kwargs) for url in urls]
        elapsed = time.monotonic() - started
        return _format_results(entries, urls, elapsed)

    def _fetch_one_safe(self, url: str, kwargs: Mapping[str, Any]) -> dict[str, Any]:
        """单 URL 抓取并兜底错误：任何异常都转为结果条目，不中断整体。"""

        try:
            return _fetch_one(url, **dict(kwargs))
        except FetcherError as exc:
            return {"url": url, "error": str(exc)}
        except Exception as exc:  # 网络/证书/解析等底层异常
            return {"url": url, "error": _friendly_error(exc)}

    def _fetch_parallel(
        self, urls: Sequence[str], kwargs: Mapping[str, Any]
    ) -> list[dict[str, Any]]:
        """并行抓取并按输入顺序返回结果。"""

        workers = max(1, min(len(urls), self.max_workers))
        entries: list[dict[str, Any]] = []
        with ThreadPoolExecutor(max_workers=workers) as pool:
            futures = [
                pool.submit(self._fetch_one_safe, url, kwargs) for url in urls
            ]
            for future in as_completed(futures):
                entries.append(future.result())
        order = {url: index for index, url in enumerate(urls)}
        entries.sort(key=lambda entry: order.get(entry.get("url", ""), 0))
        return entries


def _format_results(
    entries: Sequence[dict[str, Any]],
    urls: Sequence[str],
    elapsed: float,
) -> str:
    """把结果条目格式化为统一文本输出。"""

    lines = [f"网页抓取完成（{len(urls)} 个 URL，用时 {elapsed:.2f}s）"]
    for index, entry in enumerate(entries, 1):
        lines.append(f"{index}. {entry['url']}")
        if "error" in entry:
            lines.append(f"   失败: {entry['error']}")
            continue
        lines.append(f"   状态: {entry['status']}｜最终地址: {entry['final_url']}")
        if entry.get("title"):
            lines.append(f"   标题: {_truncate(entry['title'], 200)}")
        lines.append(f"   内容: {entry['content']}")
    return "\n".join(lines)
