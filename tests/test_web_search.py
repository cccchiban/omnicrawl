# -*- coding: utf-8 -*-
"""web_search 工具：三引擎解析、参数校验、拦截检测与注册。"""

from __future__ import annotations

import pytest

from omnicrawl.mcp.config import MCPConfig
from omnicrawl.web_search import (
    MAX_RESULTS_PER_ENGINE,
    SUPPORTED_ENGINES,
    WebSearch,
    WebSearchError,
    _clean_duckduckgo_url,
    _clean_google_url,
    _detect_windows_proxy,
    _parse_bing,
    _parse_duckduckgo,
    _parse_google,
)


class FakeResponse:
    def __init__(self, text: str, status: int = 200):
        self.text = text
        self.status_code = status

    def raise_for_status(self):
        if self.status_code >= 400:
            import httpx

            raise httpx.HTTPStatusError(
                "status", request=None, response=self
            )


class FakeClient:
    """记录请求并把固定 HTML 返回给 WebSearch。"""

    def __init__(self, html: str):
        self.html = html
        self.calls: list[tuple[str, dict]] = []

    def get(self, url: str, params: dict | None = None):
        self.calls.append((url, params or {}))
        return FakeResponse(self.html)


GOOGLE_HTML = """<html><head><title>s - Google Search</title></head><body>
<div class="g"><a href="/url?q=https%3A%2F%2Fexample.com%2Fdoc&sa=U&ved=2"><h3 class="LC20lb">Example Doc - 官方文档</h3></a>
<div class="VwiC3b">这是 <b>示例</b> 摘要内容。</div></div>
<div class="g"><a href="https://other.org/page"><h3>Other Page</h3></a>
<div class="IsZvec">第二条摘要。</div></div>
</body></html>"""

BING_HTML = """<html><body><ol id="b_results">
<li class="b_algo"><h2><a href="https://bing.example.com/1">Bing 结果一</a></h2><div class="b_caption"><p>摘要第一段。</p></div></li>
<li class="b_algo"><h2><a href="https://bing.example.com/2">Bing 结果二</a></h2><p>第二条摘要。</p></li>
</ol></body></html>"""

DDG_HTML = """<html><body><div class="results">
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fddg.example.com%2Fpage&amp;rut=x">DDG 标题</a>
<a class="result__snippet">DDG 摘要文本</a>
</div></body></html>"""


class TestParsers:
    def test_parse_google(self):
        results = _parse_google(GOOGLE_HTML)
        assert len(results) == 2
        assert results[0].title == "Example Doc - 官方文档"
        assert results[0].url == "https://example.com/doc"
        assert "摘要内容" in results[0].snippet
        assert results[1].url == "https://other.org/page"
        assert results[1].snippet == "第二条摘要。"

    def test_parse_bing(self):
        results = _parse_bing(BING_HTML)
        assert len(results) == 2
        assert results[0].title == "Bing 结果一"
        assert results[0].url == "https://bing.example.com/1"
        assert results[0].snippet == "摘要第一段。"
        assert results[1].snippet == "第二条摘要。"

    def test_parse_duckduckgo(self):
        results = _parse_duckduckgo(DDG_HTML)
        assert len(results) == 1
        assert results[0].title == "DDG 标题"
        assert results[0].url == "https://ddg.example.com/page"
        assert results[0].snippet == "DDG 摘要文本"

    def test_url_cleaners(self):
        assert _clean_google_url("/url?q=https%3A%2F%2Fa.com%2Fx&sa=U") == "https://a.com/x"
        assert _clean_google_url("https://plain.example/page") == "https://plain.example/page"
        assert (
            _clean_duckduckgo_url(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fddg.example%2Fa&amp;rut=x"
            )
            == "https://ddg.example/a"
        )
        assert _clean_duckduckgo_url("https://direct.example/x") == "https://direct.example/x"

    def test_empty_results_raise(self):
        searcher = WebSearch(http_client=FakeClient("<html>no results</html>"))
        with pytest.raises(WebSearchError, match="未找到相关搜索结果"):
            searcher.search({"query": "x"})


class TestValidation:
    def test_empty_query(self):
        with pytest.raises(WebSearchError, match="query 不能为空"):
            WebSearch(http_client=FakeClient(GOOGLE_HTML)).search({"query": "  "})

    def test_invalid_engine(self):
        with pytest.raises(WebSearchError, match="engine 必须是"):
            WebSearch(http_client=FakeClient(GOOGLE_HTML)).search(
                {"query": "x", "engine": "yahoo"}
            )

    def test_max_results_clamped(self):
        searcher = WebSearch(http_client=FakeClient(GOOGLE_HTML))
        assert "共 1 条" in searcher.search({"query": "x", "max_results": 0})
        assert "共 1 条" in searcher.search({"query": "x", "max_results": -3})
        assert "共 2 条" in searcher.search({"query": "x", "max_results": 999})

    def test_engine_default_google(self):
        client = FakeClient(GOOGLE_HTML)
        WebSearch(http_client=client).search({"query": "示例"})
        assert client.calls[0][0] == "https://www.google.com/search"

    def test_supported_engines(self):
        assert SUPPORTED_ENGINES == ("google", "bing", "duckduckgo")


class TestFormatting:
    def test_output_contains_source_and_count(self):
        out = WebSearch(http_client=FakeClient(GOOGLE_HTML)).search(
            {"query": "示例", "engine": "google"}
        )
        assert "来源：google" in out
        assert "查询：示例" in out
        assert "共 2 条" in out
        assert "Example Doc - 官方文档" in out
        assert "https://example.com/doc" in out

    def test_max_results_limits_output(self):
        out = WebSearch(http_client=FakeClient(GOOGLE_HTML)).search(
            {"query": "示例", "engine": "google", "max_results": 1}
        )
        assert "2. Other Page" not in out


class TestCaptcha:
    @pytest.mark.parametrize(
        "marker",
        ["captcha", "unusual traffic", "enable cookies", "verify you're human"],
    )
    def test_captcha_markers_detected(self, marker):
        searcher = WebSearch(
            http_client=FakeClient(f"<html><title>{marker}</title></html>")
        )
        with pytest.raises(WebSearchError, match="人机验证|异常流量"):
            searcher.search({"query": "x"})


class TestRegistration:
    def test_build_agent_tools_contains_web_search(self):
        from omnicrawl.agent.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        mcp = MCPClientManager(MCPConfig(enabled=False))
        tools = build_agent_tools(
            mcp_manager=mcp,
            memory_enabled=False,
            list_files=lambda a: None,
            read_file=lambda a: None,
            grep=lambda a: None,
            web_search=lambda a: None,
            replace_text=lambda a: None,
            write_file=lambda a: None,
            bash=lambda a: None,
            powershell=lambda a: None,
            monitor=lambda a: None,
            memory_search=lambda a: None,
            memory_read=lambda a: None,
            memory_expand_related=lambda a: None,
            memory_write=lambda a: None,
            mcp_call=lambda m, a: None,
            mcp_read_resource=lambda u: None,
            mcp_get_prompt=lambda n, a: None,
        )
        assert "web_search" in tools
        definition = tools["web_search"]
        assert definition.requires_confirmation is True
        assert "Google" in definition.description

    def test_web_search_omitted_when_none(self):
        from omnicrawl.agent.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        mcp = MCPClientManager(MCPConfig(enabled=False))
        tools = build_agent_tools(
            mcp_manager=mcp,
            memory_enabled=False,
            list_files=lambda a: None,
            read_file=lambda a: None,
            grep=lambda a: None,
            replace_text=lambda a: None,
            write_file=lambda a: None,
            bash=lambda a: None,
            powershell=lambda a: None,
            monitor=lambda a: None,
            memory_search=lambda a: None,
            memory_read=lambda a: None,
            memory_expand_related=lambda a: None,
            memory_write=lambda a: None,
            mcp_call=lambda m, a: None,
            mcp_read_resource=lambda u: None,
            mcp_get_prompt=lambda n, a: None,
        )
        assert "web_search" not in tools

    def test_config_switch_keys_include_web_search(self):
        from omnicrawl.config.tools import TOOL_SWITCH_KEYS, TOOL_SWITCH_LABELS

        assert "web_search" in TOOL_SWITCH_KEYS
        assert TOOL_SWITCH_LABELS["web_search"].startswith("网页搜索")


    def test_tool_labels_include_web_search(self):
        from omnicrawl.ui.tool_labels import tool_display

        display = tool_display("web_search")
        assert display.name == "网页搜索"
        assert display.icon == "W"


class FakeWinreg:
    """模拟 winreg 模块：按 (键名, 值) 表应答查询。"""

    HKEY_CURRENT_USER = "HKCU"

    def __init__(self, values: dict):
        self.values = values
        self.closed = False

    def OpenKey(self, root, path):
        return (root, path)

    def QueryValueEx(self, key, name):
        if name in self.values:
            return self.values[name], 1
        raise OSError(f"no value {name}")

    def CloseKey(self, key):
        self.closed = True


class TestWindowsProxyDetection:
    def test_disabled_proxy_returns_none(self):
        fake = FakeWinreg({"ProxyEnable": 0, "ProxyServer": "127.0.0.1:7890"})
        assert _detect_windows_proxy(fake) is None
        assert fake.closed

    def test_enabled_proxy_gets_http_prefix(self):
        fake = FakeWinreg({"ProxyEnable": 1, "ProxyServer": "127.0.0.1:7890"})
        assert _detect_windows_proxy(fake) == "http://127.0.0.1:7890"

    def test_http_entry_preferred(self):
        fake = FakeWinreg(
            {
                "ProxyEnable": 1,
                "ProxyServer": "socks=127.0.0.1:1080;http=10.0.0.1:8080",
            }
        )
        assert _detect_windows_proxy(fake) == "http://10.0.0.1:8080"

    def test_missing_keys_returns_none(self):
        fake = FakeWinreg({})
        assert _detect_windows_proxy(fake) is None

    def test_empty_http_entry_returns_none(self):
        fake = FakeWinreg({"ProxyEnable": 1, "ProxyServer": "http=;https=10.0.0.1:8443"})
        assert _detect_windows_proxy(fake) is None

    def test_open_key_failure_returns_none(self):
        class BrokenWinreg:
            HKEY_CURRENT_USER = "HKCU"

            def OpenKey(self, root, path):
                raise OSError("denied")

            def CloseKey(self, key):
                pass

        assert _detect_windows_proxy(BrokenWinreg()) is None


class TestProxyResolution:
    def test_explicit_proxy(self):
        searcher = WebSearch(proxy="http://proxy.local:8080")
        assert searcher._resolve_proxy() == "http://proxy.local:8080"

    def test_empty_proxy_disables(self):
        searcher = WebSearch(proxy="")
        assert searcher._resolve_proxy() is None

    def test_none_auto_detects(self, monkeypatch):
        searcher = WebSearch(proxy=None)
        monkeypatch.setattr(
            "omnicrawl.web_search._detect_windows_proxy",
            lambda winreg_module=None: "http://auto.proxy:7890",
        )
        assert searcher._resolve_proxy() == "http://auto.proxy:7890"

    def test_client_receives_proxy(self, monkeypatch):
        import httpx

        captured = {}

        class FakeHttpxClient:
            def __init__(self, **kwargs):
                captured.update(kwargs)

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def get(self, url, params=None):
                return FakeResponse(
                    '<div class="g"><a href="https://x.example/"><h3>T</h3></a></div>'
                )

        monkeypatch.setattr(httpx, "Client", FakeHttpxClient)
        searcher = WebSearch(proxy="http://explicit.proxy:3128")
        searcher.search({"query": "x", "engine": "google"})
        assert captured["proxy"] == "http://explicit.proxy:3128"
        assert captured["trust_env"] is False

    def test_client_no_proxy_when_disabled(self, monkeypatch):
        import httpx

        captured = {}

        class FakeHttpxClient:
            def __init__(self, **kwargs):
                captured.update(kwargs)

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def get(self, url, params=None):
                return FakeResponse(
                    '<div class="g"><a href="https://x.example/"><h3>T</h3></a></div>'
                )

        monkeypatch.setattr(httpx, "Client", FakeHttpxClient)
        searcher = WebSearch(proxy="")
        searcher.search({"query": "x", "engine": "google"})
        assert captured["proxy"] is None
        assert captured["trust_env"] is False
