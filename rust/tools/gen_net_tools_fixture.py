#!/usr/bin/env python3
"""生成 `omnicrawl-tui` 联网工具的对照数据集（`web_search` 与 `fetcher`）。

期望值来自 Python 真实现：

- `omnicrawl/net/web_search.py`：把 httpx 客户端换成记录型桩，桩既返回给定 HTML，也记录
  每次请求的完整 URL——引擎端点、查询参数编码与页面解析因此都可逐字对照。
- `omnicrawl/net/fetcher.py`：抓取链路依赖 `curl_cffi` 会话，无法注入；因此对照其**纯函数**
  （URL 解析、内网判定、meta refresh、标题与正文提取、截断、格式化）。

用法（仓库根目录）：

    python rust/tools/gen_net_tools_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test net_tools_parity
"""

from __future__ import annotations

import json
import re
import sys
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/net_tools_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.net.fetcher import (  # noqa: E402
    _as_bool,
    _clamp_float,
    _clamp_int,
    _extract_main_text,
    _extract_title,
    _format_results,
    _is_private_host,
    _meta_refresh_target,
    _parse_urls,
    _truncate,
)
from omnicrawl.net.web_search import WebSearch, WebSearchError  # noqa: E402

if not Path(sys.modules["omnicrawl.net.web_search"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")

ELAPSED_PATTERN = re.compile(r"用时：[\d.]+s")

BING_HTML = """<html><body><ol id="b_results">
<li class="b_algo"><h2><a href="https://example.com/1">标题 &amp; 一</a></h2><p>摘要一</p></li>
<li class="b_algo"><h2><a href="/relative">相对链接</a></h2><p>摘要二</p></li>
<li class="b_algo"><h2><a href="https://example.com/3"> 第三条 <b>加粗</b> </a></h2></li>
</ol></body></html>"""

DDG_HTML = """<html><body>
<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fddg1&rut=abc">标题 <b>一</b></a>
<a class="result__snippet" href="x">摘要 &amp; 一</a>
<a class="result__a" href="https://example.com/ddg2">第二条</a>
</body></html>"""

YAHOO_HTML = """<html><body>
<a class="d-ib" href="https://r.search.yahoo.com/_ylt=x/RU=https%3A%2F%2Fexample.com%2Fy1/RK=2"><h3 class="title">雅虎<b>标题</b></h3></a>
<div class="compText">雅虎摘要文本</div>
<a class="d-ib" href="https://example.com/y2"><h3 class="title">雅虎第二条</h3></a>
</body></html>"""

CAPTCHA_HTML = "<html><body>Please verify you're human before continuing.</body></html>"
EMPTY_HTML = "<html><body><p>没有任何结果块。</p></body></html>"

SEARCH_CASES = [
    ("bing", BING_HTML, {"query": "rust 工具"}),
    ("bing", BING_HTML, {"query": "rust", "max_results": 1}),
    ("bing", BING_HTML, {"query": "rust", "engine": "Bing"}),
    ("bing", BING_HTML, {"query": "rust", "engine": "unknown"}),
    ("bing", BING_HTML, {"query": "  "}),
    ("bing", BING_HTML, {"query": "rust", "max_results": 99}),
    ("bing", BING_HTML, {"query": "rust", "max_results": "2"}),
    ("duckduckgo", DDG_HTML, {"query": "rust", "engine": "duckduckgo"}),
    ("duckduckgo", DDG_HTML, {"query": "rust", "engine": "DuckDuckGo", "language": "zh"}),
    ("duckduckgo", DDG_HTML, {"query": "rust", "engine": "duckduckgo", "language": "en-us"}),
    ("yahoo", YAHOO_HTML, {"query": "rust", "engine": "yahoo"}),
    ("yahoo", YAHOO_HTML, {"query": "rust", "engine": "yahoo", "max_results": 1}),
    ("bing", CAPTCHA_HTML, {"query": "rust"}),
    ("bing", EMPTY_HTML, {"query": "rust"}),
]

MAIN_TEXT_HTML = [
    (
        "<html><head><title>页面</title></head><body><p>body 文本</p>"
        "<article><p>article 文本</p></article><main><p>main 文本</p></main></body></html>",
        8000,
    ),
    ("<html><body><p>只有 body</p></body></html>", 8000),
    (
        "<html><body><script>var x=1;</script><style>p{}</style>"
        "<p>正文&nbsp;内容</p><noscript>降级文本</noscript><template>模板</template></body></html>",
        8000,
    ),
    ("<html><body><p>行一</p><p>行二</p></body></html>", 8000),
    ("<html><body><div>" + "字" * 50 + "</div></body></html>", 10),
    ("<html><body><main><div>嵌套 <span>元素</span> 文本</div></main></body></html>", 8000),
]

META_REFRESH_CASES = [
    ('<meta http-equiv="refresh" content="0; url=/next/page.html">', "https://example.com/a/x.html"),
    ('<meta http-equiv="Refresh" content="5;URL=https://other.example/z">', "https://example.com/a/x.html"),
    ('<meta http-equiv="refresh" content="5; url=../up.html">', "https://example.com/a/b/x.html"),
    ('<META HTTP-EQUIV="refresh" CONTENT="1; url=\'note.html\'">', "https://example.com/a/"),
    ("<html><body>no refresh</body></html>", "https://example.com/"),
    ('<meta http-equiv="refresh" content="3">', "https://example.com/"),
]

TITLE_CASES = [
    "<html><head><title>\n 标题 &amp; 副标题 \n</title></head></html>",
    "<html><head><title></title></head></html>",
    "<html></html>",
    "<html><head><TITLE>大写标签</TITLE></head></html>",
]

TRUNCATE_CASES = [
    ("短", 10),
    ("正好十个字符啊啊啊", 9),
    ("", 5),
]

BOOL_CASES = [
    (True, False),
    (False, True),
    (1, False),
    (0, True),
    ("true", False),
    (" YES ", False),
    ("on", False),
    ("off", True),
    ("2", True),
    (None, True),
    ([], False),
]

CLAMP_INT_CASES = [
    (None, 5, 1, 10),
    ("3", 5, 1, 10),
    (99, 5, 1, 10),
    (0, 5, 1, 10),
    (True, 5, 1, 10),
    ("abc", 5, 1, 10),
]

CLAMP_FLOAT_CASES = [
    (None, 15.0, 1.0, 60.0),
    ("2.5", 15.0, 1.0, 60.0),
    (0.1, 15.0, 1.0, 60.0),
    (999, 15.0, 1.0, 60.0),
    ("abc", 15.0, 1.0, 60.0),
]

FORMAT_CASES = [
    (
        ["https://a.example/1", "https://b.example/2"],
        [
            {
                "url": "https://a.example/1",
                "status": 200,
                "final_url": "https://a.example/1",
                "title": "标题一",
                "content": "正文一",
            },
            {"url": "https://b.example/2", "error": "请求超时。"},
        ],
        1.234,
    ),
    (
        ["https://c.example/3"],
        [
            {
                "url": "https://c.example/3",
                "status": 404,
                "final_url": "https://c.example/3",
                "title": "",
                "content": "",
            }
        ],
        0.5,
    ),
]

URL_CASES = [
    "https://a.example, https://b.example",
    '["https://a.example", "https://b.example"]',
    ["https://a.example", " https://b.example "],
    "",
    "   ",
    "{\"a\": 1}",
    "https://only.example",
    None,
    ["", "https://a.example"],
]

PRIVATE_HOST_CASES = [
    "localhost",
    "api.localhost",
    "printer.local",
    "127.0.0.1",
    "10.1.2.3",
    "192.168.1.10",
    "172.16.0.1",
    "172.31.255.254",
    "example.com",
    "172.32.0.1",
    "172.15.0.1",
    "8.8.8.8",
    "999.1.1.1",
    "",
]


class _StubResponse:
    def __init__(self, text: str, status: int) -> None:
        self.text = text
        self.status_code = status

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")


class _RecordingClient:
    """记录请求 URL 的 httpx 替身：返回固定 HTML，不发起任何网络请求。"""

    def __init__(self, html: str, status: int = 200) -> None:
        self.html = html
        self.status = status
        self.requests: list[str] = []

    def get(self, url: str, params=None) -> _StubResponse:
        query = urllib.parse.urlencode(params or {})
        self.requests.append(f"{url}?{query}" if query else url)
        return _StubResponse(self.html, self.status)


def normalize(text: str) -> str:
    return ELAPSED_PATTERN.sub("用时：{ELAPSED}s", text)


def search_cases() -> list[dict]:
    cases = []
    for _engine, html, arguments in SEARCH_CASES:
        client = _RecordingClient(html)
        searcher = WebSearch(http_client=client, proxy="", max_retries=0)
        try:
            output = searcher.search(arguments)
            ok = True
        except WebSearchError as exc:
            output = str(exc)
            ok = False
        cases.append(
            {
                "arguments": arguments,
                "html": html,
                "requests": client.requests,
                "ok": ok,
                "output": normalize(output),
            }
        )
    return cases


def main() -> int:
    data = {
        "source": (
            "omnicrawl/net/web_search.py + omnicrawl/net/fetcher.py"
        ),
        "web_search": {
            "options": {"max_retries": 0, "timeout_seconds": 10.0, "proxy": ""},
            "cases": search_cases(),
        },
        "fetcher": {
            "urls": [
                {"input": value, "expected": _parse_urls(value)} for value in URL_CASES
            ],
            "private_hosts": [
                {"host": host, "expected": _is_private_host(host)}
                for host in PRIVATE_HOST_CASES
            ],
            "meta_refresh": [
                {"html": html, "base": base, "expected": _meta_refresh_target(html, base)}
                for html, base in META_REFRESH_CASES
            ],
            "titles": [
                {"html": html, "expected": _extract_title(html)} for html in TITLE_CASES
            ],
            "main_text": [
                {"html": html, "max_chars": max_chars, "expected": _extract_main_text(html, max_chars)}
                for html, max_chars in MAIN_TEXT_HTML
            ],
            "truncate": [
                {"text": text, "limit": limit, "expected": _truncate(text, limit)}
                for text, limit in TRUNCATE_CASES
            ],
            "bools": [
                {"input": value, "default": default, "expected": _as_bool(value, default)}
                for value, default in BOOL_CASES
            ],
            "clamp_int": [
                {
                    "input": value,
                    "default": default,
                    "minimum": minimum,
                    "maximum": maximum,
                    "expected": _clamp_int(value, default=default, minimum=minimum, maximum=maximum),
                }
                for value, default, minimum, maximum in CLAMP_INT_CASES
            ],
            "clamp_float": [
                {
                    "input": value,
                    "default": default,
                    "minimum": minimum,
                    "maximum": maximum,
                    "expected": _clamp_float(value, default=default, minimum=minimum, maximum=maximum),
                }
                for value, default, minimum, maximum in CLAMP_FLOAT_CASES
            ],
            "format": [
                {
                    "urls": urls,
                    "entries": entries,
                    "elapsed": elapsed,
                    "expected": _format_results(entries, urls, elapsed),
                }
                for urls, entries, elapsed in FORMAT_CASES
            ],
        },
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH}（搜索 {len(data['web_search']['cases'])} 例，"
        f"fetcher 纯函数若干）"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
