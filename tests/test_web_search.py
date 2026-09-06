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
    _clean_yahoo_url,
    _detect_windows_proxy,
    _parse_bing,
    _parse_duckduckgo,
    _parse_yahoo,
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


BING_HTML = """<html><body><ol id="b_results">
<li class="b_algo"><h2><a href="https://bing.example.com/1">Bing 结果一</a></h2><div class="b_caption"><p>摘要第一段。</p></div></li>
<li class="b_algo"><h2><a href="https://bing.example.com/2">Bing 结果二</a></h2><p>第二条摘要。</p></li>
</ol></body></html>"""

DDG_HTML = """<html><body><div class="results">
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fddg.example.com%2Fpage&amp;rut=x">DDG 标题</a>
<a class="result__snippet">DDG 摘要文本</a>
</div></body></html>"""

YAHOO_HTML = """<html><body><ol class="searchCenterMiddle">
<li><div class="compTitle options-toggle"><a class="d-ib va-top" href="https://yahoo.example.com/1"><h3 class="title">雅虎结果一</h3></a></div>
<div class="compText">第一条摘要。</div></li>
<li><div class="compTitle options-toggle"><a class="d-ib va-top" href="https://r.search.yahoo.com/_ylt=abc/RV=2/RU=https%3a%2f%2fyahoo.example.com%2f2/RK=2"><h3 class="title">雅虎结果二</h3></a></div>
<div class="compText">第二条摘要。</div></li>
</ol></body></html>"""


class TestParsers:
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

    def test_parse_yahoo(self):
        results = _parse_yahoo(YAHOO_HTML)
        assert len(results) == 2
        assert results[0].title == "雅虎结果一"
        assert results[0].url == "https://yahoo.example.com/1"
        assert results[0].snippet == "第一条摘要。"
        assert results[1].url == "https://yahoo.example.com/2"
        assert results[1].snippet == "第二条摘要。"

    def test_url_cleaners(self):
        assert (
            _clean_yahoo_url(
                "https://r.search.yahoo.com/_ylt=abc/RV=2/RU=https%3a%2f%2fyahoo.example%2fa/RK=2"
            )
            == "https://yahoo.example/a"
        )
        assert (
            _clean_yahoo_url(
                "//search.yahoo.com/click?x=1&RU=https%3A%2F%2Fyahoo.example%2Fb"
            )
            == "https://yahoo.example/b"
        )
        assert (
            _clean_yahoo_url("https://direct.example/x")
            == "https://direct.example/x"
        )
        assert (
            _clean_duckduckgo_url(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fddg.example%2Fa&amp;rut=x"
            )
            == "https://ddg.example/a"
        )
        assert (
            _clean_duckduckgo_url("https://direct.example/x")
            == "https://direct.example/x"
        )

    def test_empty_results_raise(self):
        searcher = WebSearch(http_client=FakeClient("<html>no results</html>"))
        with pytest.raises(WebSearchError, match="未找到相关搜索结果"):
            searcher.search({"query": "x"})


class TestValidation:
    def test_empty_query(self):
        with pytest.raises(WebSearchError, match="query 不能为空"):
            WebSearch(http_client=FakeClient(BING_HTML)).search({"query": "  "})

    def test_invalid_engine(self):
        with pytest.raises(WebSearchError, match="engine 必须是"):
            WebSearch(http_client=FakeClient(BING_HTML)).search(
                {"query": "x", "engine": "baidu"}
            )

    def test_max_results_clamped(self):
        searcher = WebSearch(http_client=FakeClient(BING_HTML))
        assert "共 1 条" in searcher.search({"query": "x", "max_results": 0})
        assert "共 1 条" in searcher.search({"query": "x", "max_results": -3})
        assert "共 2 条" in searcher.search({"query": "x", "max_results": 999})

    def test_engine_default_bing(self):
        client = FakeClient(BING_HTML)
        WebSearch(http_client=client).search({"query": "示例"})
        assert client.calls[0][0] == "https://www.bing.com/search"

    def test_supported_engines(self):
        assert SUPPORTED_ENGINES == ("bing", "duckduckgo", "yahoo")


class TestFormatting:
    def test_output_contains_source_and_count(self):
        out = WebSearch(http_client=FakeClient(BING_HTML)).search(
            {"query": "示例", "engine": "bing"}
        )
        assert "来源：bing" in out
        assert "查询：示例" in out
        assert "共 2 条" in out
        assert "Bing 结果一" in out
        assert "https://bing.example.com/1" in out

    def test_max_results_limits_output(self):
        out = WebSearch(http_client=FakeClient(BING_HTML)).search(
            {"query": "示例", "engine": "bing", "max_results": 1}
        )
        assert "2. Bing 结果二" not in out


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
        from omnicrawl.agent.toolkit.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        mcp = MCPClientManager(MCPConfig(enabled=False))
        tools = build_agent_tools(
            mcp_manager=mcp,
            memory_enabled=False,
            list=lambda a: None,
            read=lambda a: None,
            grep=lambda a: None,
            web_search=lambda a: None,
            edit_file=lambda a: None,
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
        assert "bing" in definition.description.lower()

    def test_web_search_omitted_when_none(self):
        from omnicrawl.agent.toolkit.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        mcp = MCPClientManager(MCPConfig(enabled=False))
        tools = build_agent_tools(
            mcp_manager=mcp,
            memory_enabled=False,
            list=lambda a: None,
            read=lambda a: None,
            grep=lambda a: None,
            edit_file=lambda a: None,
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
        from omnicrawl.config.features.tools import TOOL_SWITCH_KEYS, TOOL_SWITCH_LABELS

        assert "web_search" in TOOL_SWITCH_KEYS
        assert TOOL_SWITCH_LABELS["web_search"].startswith("web_search")


    def test_tool_labels_include_web_search(self):
        from omnicrawl.ui.tool_labels import tool_display

        display = tool_display("web_search")
        assert display.name == "web_search"
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
                    '<ol id="b_results"><li class="b_algo"><h2><a href="https://x.example/">T</a></h2></li></ol>'
                )

        monkeypatch.setattr(httpx, "Client", FakeHttpxClient)
        searcher = WebSearch(proxy="http://explicit.proxy:3128")
        searcher.search({"query": "x", "engine": "bing"})
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
                    '<ol id="b_results"><li class="b_algo"><h2><a href="https://x.example/">T</a></h2></li></ol>'
                )

        monkeypatch.setattr(httpx, "Client", FakeHttpxClient)
        searcher = WebSearch(proxy="")
        searcher.search({"query": "x", "engine": "bing"})
        assert captured["proxy"] is None
        assert captured["trust_env"] is False
