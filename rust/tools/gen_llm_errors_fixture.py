#!/usr/bin/env python3
"""生成 `omnicrawl/llm/errors.py` 错误分类（`map_openai_exception`）的对照数据集。

内核没有 SDK 异常对象，所以数据集记录的是分类函数真正读到的字段：
`str(exc).strip()`、`type(exc).__name__`、`exc.body`、`exc.response.json()`、状态码，
以及 Python 真实现给出的分类结果（码 / 文案 / 可重试标记 / 状态码）。

用法：``python rust/tools/gen_llm_errors_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/llm_errors_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/llm_errors_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm import errors as E  # noqa: E402

if not Path(E.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

KNOWN_MODELS = ("gpt-4o", "gpt-4o-mini")


def jsonable(value):
    """只保留能进 JSON 的载荷；其余按「响应体不是 JSON」处理。"""

    try:
        json.dumps(value)
    except (TypeError, ValueError):
        return None
    return value


class FakeResponse:
    def __init__(self, payload=None, *, status_code=None, not_json=False):
        self._payload = payload
        self._not_json = not_json
        if status_code is not None:
            self.status_code = status_code

    def json(self):
        if self._not_json:
            raise ValueError("响应体不是 JSON")
        return self._payload


class FakeException(Exception):
    def __init__(self, message, *, body=None, response=None, status_code=None):
        super().__init__(message)
        if body is not None:
            self.body = body
        if response is not None:
            self.response = response
        if status_code is not None:
            self.status_code = status_code


def case(label, exc, models=()):
    response = getattr(exc, "response", None)
    response_json = None
    if response is not None:
        try:
            response_json = jsonable(response.json())
        except Exception:  # noqa: BLE001 - 与 Python 同口径：响应体不是 JSON 时跳过
            response_json = None
    return {
        "label": label,
        "message": str(exc).strip(),
        "type_name": type(exc).__name__,
        "body": jsonable(getattr(exc, "body", None)),
        "response_json": response_json,
        "status_code": getattr(exc, "status_code", None),
        "response_status_code": getattr(response, "status_code", None),
        "known_models": list(models),
    }, exc


def build_cases():
    cases = [
        case("模型不存在（文案）", FakeException("Error code: 404 - model not found: gpt-x")),
        case(
            "模型不存在（结构化错误体）",
            FakeException(
                "Error code: 422 - Unprocessable Entity",
                body={"error": {"message": "model not found: gpt-x", "code": "invalid_model"}},
            ),
        ),
        case(
            "模型不存在（带可用模型列表）",
            FakeException("invalid_model", status_code=404),
            models=KNOWN_MODELS,
        ),
        case("HTML 错误页（无状态码）", FakeException("<html><body>502 Bad Gateway</body></html>")),
        case("HTML 错误页（有状态码）", FakeException("<h1>Bad Gateway</h1>", status_code=502)),
        case("HTML 错误页（doctype）", FakeException("<!DOCTYPE html>\n<html>bad</html>")),
        case("HTML 错误页（仅闭合标签）", FakeException("oops</html>")),
        case("伪 HTML（无词边界）", FakeException("the <htmlx> tag is not html")),
        case("限流（rate limit）", FakeException("Rate limit reached for gpt-x")),
        case("限流（大写）", FakeException("TOO MANY REQUESTS")),
        case("限流（额度）", FakeException("insufficient_quota: check your plan")),
        case("限流（结构化错误体）", FakeException("upstream said no", body={"message": "quota exceeded"})),
        case("上下文超限（英文）", FakeException("This model's maximum context length is 8192 tokens")),
        case("上下文超限（中文）", FakeException("请求过长：上下文长度超过上限")),
        case("上下文超限（token limit）", FakeException("token 超限", status_code=400)),
        case("状态码 400", FakeException("bad request", status_code=400)),
        case("状态码 401", FakeException("unauthorized", status_code=401)),
        case("状态码 403", FakeException("forbidden", status_code=403)),
        case("状态码 404", FakeException("not found", status_code=404)),
        case("状态码 408", FakeException("slow", status_code=408)),
        case("状态码 409", FakeException("conflict", status_code=409)),
        case("状态码 422", FakeException("unprocessable", status_code=422)),
        case("状态码 429", FakeException("slow down", status_code=429)),
        case("状态码 500", FakeException("boom", status_code=500)),
        case("状态码 503", FakeException("gateway", status_code=503)),
        case("状态码 599", FakeException("weird", status_code=599)),
        case("状态码 418", FakeException("teapot", status_code=418)),
        case("状态码越界（42）", FakeException("weird", status_code=42)),
        case(
            "响应对象上的状态码",
            FakeException("upstream failed", response=FakeResponse({"detail": "x"}, status_code=503)),
        ),
        case(
            "响应体不是 JSON",
            FakeException("upstream failed", response=FakeResponse(status_code=None, not_json=True)),
        ),
        case("文案里的状态码", FakeException("Error code: 503 - Service Unavailable")),
        case("文案里的四位数字", FakeException("request id 12345 failed")),
        case("连接提前断开", FakeException("peer closed connection without sending complete message body")),
        case("分块读取不完整", FakeException("incomplete chunked read")),
        case("远端协议错误", FakeException("RemoteProtocolError: remote protocol error")),
        case("服务端断开", FakeException("server disconnected")),
        case("连接被重置", FakeException("ConnectionResetError: connection reset by peer")),
        case("管道断开", FakeException("BrokenPipeError: broken pipe")),
        case("请求超时", FakeException("Request timed out.")),
        case("读超时", FakeException("ReadTimeout: 60s")),
        case("连接超时（无状态码）", FakeException("ConnectTimeout while connecting")),
        case("鉴权关键词（无状态码）", FakeException("invalid_api_key provided")),
        case("鉴权关键词 authentication", FakeException("authentication failed")),
        case("权限关键词", FakeException("permission denied for model gpt-x")),
        case("连接关键词", FakeException("Connection error.")),
        case("DNS 关键词", FakeException("Temporary failure in name resolution")),
        case("域名解析", FakeException("nodename nor servname provided")),
        case("TLS 关键词", FakeException("SSL: CERTIFICATE_VERIFY_FAILED")),
        case("TLS handshake", FakeException("tls handshake failed")),
        case("认不出的失败", FakeException("something odd happened")),
        case(
            "认不出的失败（带结构化错误体）",
            FakeException("upstream said no", body={"error": {"type": "server_error"}}),
        ),
        case(
            "结构化错误体：列表只取前四条",
            FakeException(
                "unclassified",
                body={"detail": [{"message": "first"}, {"message": "second"}, {"message": "third"}, {"message": "fourth"}, {"message": "fifth"}]},
            ),
        ),
        case(
            "结构化错误体：深度截断",
            FakeException("deep", body={"error": {"error": {"error": {"error": {"error": {"message": "too deep"}}}}}}),
        ),
        case("结构化错误体：字符串载荷", FakeException("raw", body="rate limit exceeded in raw body")),
        case("结构化错误体：长文本截断", FakeException("long", body={"message": "x" * 600})),
    ]
    return cases


def main() -> None:
    entries = []
    for item, exc in build_cases():
        mapped = E.map_openai_exception(exc, known_models=tuple(item["known_models"]))
        entries.append(
            {
                **item,
                "expected": {
                    "code": mapped.code.value,
                    "message": mapped.message,
                    "retryable": mapped.retryable,
                    "status_code": mapped.status_code,
                },
            }
        )

    fixture = {
        "source": "omnicrawl/llm/errors.py",
        "cases": entries,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    codes = sorted({entry["expected"]["code"] for entry in entries})
    print(
        "已写入 %s：用例 %d 个，覆盖 %d 个分类码"
        % (FIXTURE_PATH.relative_to(ROOT), len(entries), len(codes))
    )


if __name__ == "__main__":
    main()
