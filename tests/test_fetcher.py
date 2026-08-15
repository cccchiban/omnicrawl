# -*- coding: utf-8 -*-
"""fetcher 工具：浏览器指纹抓取、并行、内网证书、页面跳转与参数校验。"""

from __future__ import annotations

import pytest

from omnicrawl.fetcher import (
    Fetcher,
    FetcherError,
    _as_bool,
    _extract_main_text,
    _extract_title,
    _format_results,
    _is_private_host,
    _meta_refresh_target,
    _normalize_impersonate,
    _parse_urls,
)


class FakeResponse:
    def __init__(self, text: str, status: int = 200, url: str = "https://x.example/"):
        self.text = text
        self.status_code = status
        self.url = url


class FakeSession:
    """按 URL 返回预置响应，并记录每次请求的 verify/headers 参数。"""

    def __init__(self, responses):
        self.responses = responses
        self.calls: list[tuple[str, dict]] = []
        self.proxies: dict = {}

    def get(self, url, **kwargs):
        self.calls.append((url, kwargs))
        if url in self.responses:
            return self.responses[url]
        # 无精确匹配时按调用顺序消费响应（用于 meta refresh 跳转链）。
        if len(self.calls) - 1 < len(self.responses):
            return list(self.responses.values())[len(self.calls) - 1]
        return FakeResponse("<html><title>empty</title></html>", 200, url)


def patch_session(monkeypatch, responses=None):
    """把 fetcher 底层 Session 工厂替换为 FakeSession，测试结束后自动恢复。"""

    responses = responses or {}
    sessions: list[FakeSession] = []
    monkeypatch.setattr(
        "omnicrawl.fetcher._create_session",
        lambda impersonate, timeout: (
            sessions.append(FakeSession(responses)) or sessions[-1]
        ),
    )
    return sessions


class TestParse:
    def test_parse_urls_comma(self):
        assert _parse_urls("https://a.com, https://b.com") == [
            "https://a.com",
            "https://b.com",
        ]

    def test_parse_urls_json_array(self):
        assert _parse_urls('["https://a.com", "https://b.com"]') == [
            "https://a.com",
            "https://b.com",
        ]

    def test_parse_urls_python_list(self):
        assert _parse_urls(["https://a.com", "https://b.com"]) == [
            "https://a.com",
            "https://b.com",
        ]

    def test_parse_urls_empty_and_blank(self):
        assert _parse_urls("") == []
        assert _parse_urls("  ,  ") == []
        assert _parse_urls(None) == []
        assert _parse_urls("https://a.com,,https://b.com") == [
            "https://a.com",
            "https://b.com",
        ]

    def test_parse_urls_capped(self):
        urls = _parse_urls(",".join(f"https://u{i}.com" for i in range(50)))
        assert len(urls) == 20  # MAX_URLS


class TestAsBool:
    def test_bool_types(self):
        assert _as_bool(True) is True
        assert _as_bool(False) is False

    def test_string_forms(self):
        assert _as_bool("true") is True
        assert _as_bool("1") is True
        assert _as_bool("yes") is True
        assert _as_bool("on") is True
        assert _as_bool("false") is False
        assert _as_bool("0") is False

    def test_default(self):
        assert _as_bool(None, default=True) is True
        assert _as_bool("x", default=False) is False


class TestImpersonate:
    def test_default_is_chrome(self):
        assert _normalize_impersonate(None) == "chrome"

    def test_named_browsers(self):
        assert _normalize_impersonate("firefox") == "firefox"
        assert _normalize_impersonate("safari") == "safari"
        assert _normalize_impersonate("edge") == "edge"

    def test_version_suffix(self):
        assert _normalize_impersonate("chrome124") == "chrome"
        assert _normalize_impersonate("Firefox135") == "firefox"

    def test_unknown_falls_back(self):
        assert _normalize_impersonate("ie11") == "chrome"


class TestPrivateHost:
    def test_localhost(self):
        assert _is_private_host("localhost") is True
        assert _is_private_host("foo.localhost") is True
        assert _is_private_host("router.local") is True

    def test_ipv4_ranges(self):
        assert _is_private_host("127.0.0.1") is True
        assert _is_private_host("10.1.2.3") is True
        assert _is_private_host("172.16.0.1") is True
        assert _is_private_host("172.31.255.255") is True
        assert _is_private_host("192.168.1.1") is True

    def test_public(self):
        assert _is_private_host("example.com") is False
        assert _is_private_host("172.32.0.1") is False
        assert _is_private_host("8.8.8.8") is False
        assert _is_private_host("") is False


class TestHtmlHelpers:
    def test_extract_title(self):
        assert (
            _extract_title("<html><head><title>  标题 示例 </title></head></html>")
            == "标题 示例"
        )
        assert _extract_title("<html><body>no title</body></html>") == ""

    def test_extract_main_text_prefers_article(self):
        html = (
            "<html><body><script>var x=1;</script>"
            "<div class='nav'>导航</div>"
            "<article><h1>正文标题</h1><p>这是 <b>正文</b> 内容。</p></article>"
            "<footer>页脚</footer></body></html>"
        )
        text = _extract_main_text(html, 1000)
        assert "正文标题" in text
        assert "正文 内容" in text
        assert "导航" not in text
        assert "页脚" not in text

    def test_extract_main_text_truncates(self):
        html = "<html><body><p>" + "甲" * 500 + "</p></body></html>"
        assert len(_extract_main_text(html, 200)) <= 200

    def test_meta_refresh_absolute(self):
        html = '<meta http-equiv="refresh" content="0; url=https://target.example/page">'
        assert _meta_refresh_target(html, "https://src.example/a") == (
            "https://target.example/page"
        )

    def test_meta_refresh_relative(self):
        html = '<meta HTTP-EQUIV="Refresh" CONTENT="5;URL=/next">'
        assert _meta_refresh_target(html, "https://src.example/dir/a") == (
            "https://src.example/next"
        )

    def test_meta_refresh_none(self):
        assert _meta_refresh_target("<html>no meta</html>", "https://x.example") is None


class TestFetchSingle:
    def test_basic_fetch(self, monkeypatch):
        html = (
            "<html><head><title>示例站</title></head><body>"
            "<article>这是正文内容。</article></body></html>"
        )
        patch_session(
            monkeypatch,
            {"https://example.com": FakeResponse(html, 200, "https://example.com/")},
        )
        out = Fetcher().fetch({"urls": "https://example.com", "max_chars": 500})
        assert "网页抓取完成（1 个 URL" in out
        assert "状态: 200" in out
        assert "最终地址: https://example.com/" in out
        assert "标题: 示例站" in out
        assert "这是正文内容。" in out

    def test_max_html_returns_raw(self, monkeypatch):
        html = "<html><body><p>原始<em>HTML</em></p></body></html>"
        patch_session(
            monkeypatch,
            {"https://example.com": FakeResponse(html, 200, "https://example.com/")},
        )
        out = Fetcher().fetch({"urls": "https://example.com", "max_html": True})
        assert "<em>HTML</em>" in out

    def test_insecure_passes_verify_false(self, monkeypatch):
        html = "<html><title>t</title></html>"
        sessions = patch_session(
            monkeypatch,
            {"https://intra.example": FakeResponse(html, 200, "https://intra.example/")},
        )
        Fetcher().fetch({"urls": "https://intra.example", "insecure": True})
        assert sessions[0].calls[0][1]["verify"] is False

    def test_secure_verifies_by_default(self, monkeypatch):
        html = "<html><title>t</title></html>"
        sessions = patch_session(
            monkeypatch,
            {"https://example.com": FakeResponse(html, 200, "https://example.com/")},
        )
        Fetcher().fetch({"urls": "https://example.com"})
        assert sessions[0].calls[0][1]["verify"] is True

    def test_impersonate_chrome_default(self, monkeypatch):
        html = "<html><title>t</title></html>"
        sessions = patch_session(
            monkeypatch,
            {"https://example.com": FakeResponse(html, 200, "https://example.com/")},
        )
        Fetcher().fetch({"urls": "https://example.com"})
        assert sessions[0].calls[0][0] == "https://example.com"
        assert "verify" in sessions[0].calls[0][1]

    def test_http_error_reported(self, monkeypatch):
        patch_session(
            monkeypatch,
            {
                "https://example.com/404": FakeResponse(
                    "not found", 404, "https://example.com/404"
                )
            },
        )
        out = Fetcher().fetch({"urls": "https://example.com/404"})
        assert "失败: HTTP 404" in out


class TestMetaRefreshFollow:
    def test_follows_meta_refresh(self, monkeypatch):
        first = (
            '<html><meta http-equiv="refresh" content="0; url=/final">'
            "<body>跳转中</body></html>"
        )
        final = (
            "<html><title>最终页</title><body><article>最终内容</article></body></html>"
        )
        patch_session(
            monkeypatch,
            {
                "https://example.com/start": FakeResponse(
                    first, 200, "https://example.com/start"
                ),
                "https://example.com/final": FakeResponse(
                    final, 200, "https://example.com/final"
                ),
            },
        )
        out = Fetcher().fetch({"urls": "https://example.com/start"})
        assert "最终页" in out
        assert "最终内容" in out
        assert "最终地址: https://example.com/final" in out


class TestParallel:
    def test_parallel_multiple_urls(self, monkeypatch):
        html_a = "<html><title>A</title><body><article>内容A</article></body></html>"
        html_b = "<html><title>B</title><body><article>内容B</article></body></html>"
        patch_session(
            monkeypatch,
            {
                "https://a.example": FakeResponse(html_a, 200, "https://a.example/"),
                "https://b.example": FakeResponse(html_b, 200, "https://b.example/"),
            },
        )
        out = Fetcher().fetch({"urls": "https://a.example,https://b.example"})
        assert "网页抓取完成（2 个 URL" in out
        assert "1. https://a.example" in out
        assert "2. https://b.example" in out
        assert "内容A" in out
        assert "内容B" in out

    def test_parallel_single_failure_keeps_others(self, monkeypatch):
        html = "<html><title>OK</title><body><article>好内容</article></body></html>"
        patch_session(
            monkeypatch,
            {
                "https://good.example": FakeResponse(html, 200, "https://good.example/"),
                "https://bad.example": FakeResponse("err", 500, "https://bad.example/"),
            },
        )
        out = Fetcher().fetch({"urls": "https://good.example,https://bad.example"})
        assert "好内容" in out
        assert "失败: HTTP 500" in out

    def test_parallel_false_sequential(self, monkeypatch):
        html = "<html><title>X</title></html>"
        patch_session(
            monkeypatch,
            {"https://a.example": FakeResponse(html, 200, "https://a.example/")},
        )
        out = Fetcher().fetch({"urls": "https://a.example", "parallel": False})
        assert "标题: X" in out


class TestValidation:
    def test_empty_urls_raises(self, monkeypatch):
        patch_session(monkeypatch)
        with pytest.raises(FetcherError, match="urls 不能为空"):
            Fetcher().fetch({"urls": ""})

    def test_network_error_translated(self, monkeypatch):
        def boom(impersonate, timeout):
            raise TimeoutError("timed out")

        monkeypatch.setattr("omnicrawl.fetcher._create_session", boom)
        out = Fetcher().fetch({"urls": "https://slow.example"})
        assert "失败" in out

    def test_format_results_error_entry(self):
        lines = _format_results(
            [{"url": "https://a.example", "error": "请求超时。"}],
            ["https://a.example"],
            0.1,
        )
        assert "失败: 请求超时。" in lines


class TestConfigRegistration:
    def test_tool_switch_keys_include_fetcher(self):
        from omnicrawl.config.tools import TOOL_SWITCH_KEYS

        assert "fetcher" in TOOL_SWITCH_KEYS

    def test_tool_switch_labels_include_fetcher(self):
        from omnicrawl.config.tools import TOOL_SWITCH_LABELS

        assert "fetcher" in TOOL_SWITCH_LABELS

    def test_tool_labels_display_fetcher(self):
        from omnicrawl.ui.tool_labels import tool_display

        display = tool_display("fetcher")
        assert display.name == "fetcher"
        assert display.icon

    def test_build_agent_tools_contains_fetcher(self):
        from omnicrawl.agent.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        tools = build_agent_tools(
            mcp_manager=MCPClientManager(),
            memory_enabled=False,
            list=lambda a: "ok",
            read=lambda a: "ok",
            grep=lambda a: "ok",
            web_search=lambda a: "ok",
            fetcher=lambda a: "ok",
            replace_text=lambda a: "ok",
            write_file=lambda a: "ok",
            bash=lambda a: "ok",
            powershell=lambda a: "ok",
            monitor=lambda a: "ok",
            memory_search=lambda a: "ok",
            memory_read=lambda a: "ok",
            memory_expand_related=lambda a: "ok",
            memory_write=lambda a: "ok",
            mcp_call=lambda m, a: None,
            mcp_read_resource=lambda u: None,
            mcp_get_prompt=lambda n, a: None,
        )
        assert "fetcher" in tools
        assert tools["fetcher"].requires_confirmation is True
        assert "curl_cffi" in tools["fetcher"].description

    def test_build_agent_tools_omits_fetcher_when_none(self):
        from omnicrawl.agent.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        tools = build_agent_tools(
            mcp_manager=MCPClientManager(),
            memory_enabled=False,
            list=lambda a: "ok",
            read=lambda a: "ok",
            grep=lambda a: "ok",
            replace_text=lambda a: "ok",
            write_file=lambda a: "ok",
            bash=lambda a: "ok",
            powershell=lambda a: "ok",
            monitor=lambda a: "ok",
            memory_search=lambda a: "ok",
            memory_read=lambda a: "ok",
            memory_expand_related=lambda a: "ok",
            memory_write=lambda a: "ok",
            mcp_call=lambda m, a: None,
            mcp_read_resource=lambda u: None,
            mcp_get_prompt=lambda n, a: None,
        )
        assert "fetcher" not in tools
