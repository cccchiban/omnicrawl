from __future__ import annotations

import json
import os
import socket
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Mapping

from .llm import LLMConfig
from .runtime_config import RuntimeConfigError, get_section, load_config_data, save_config_data


MODEL_LIST_TIMEOUT_SECONDS = 10
MAX_MODEL_LIST_BYTES = 2 * 1024 * 1024


class ModelCatalogError(RuntimeError):
    """模型列表检测或模型配置写回失败。"""


@dataclass(frozen=True)
class ModelOption:
    """可供 TUI / Qt UI 展示和切换的模型项。"""

    id: str
    name: str
    provider: str

    def to_ui_dict(self) -> dict[str, str]:
        return {"id": self.id, "name": self.name, "provider": self.provider}


def detect_model_options(
    config: LLMConfig,
    *,
    timeout_seconds: float = MODEL_LIST_TIMEOUT_SECONDS,
) -> list[ModelOption]:
    """从当前 `llm.base_url` 的 OpenAI 兼容 `/models` 接口检测模型列表。"""

    endpoint = _models_endpoint(config.base_url)
    request = urllib.request.Request(
        endpoint,
        headers={
            "Accept": "application/json",
            "Authorization": f"Bearer {config.api_key}",
            "User-Agent": "ai-voice-agent/1.0",
        },
        method="GET",
    )

    try:
        with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
            raw_body = response.read(MAX_MODEL_LIST_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise ModelCatalogError(_format_http_error(exc)) from exc
    except (urllib.error.URLError, TimeoutError, socket.timeout) as exc:
        raise ModelCatalogError(f"无法连接模型列表接口：{exc}") from exc
    except OSError as exc:
        raise ModelCatalogError(f"读取模型列表失败：{exc}") from exc

    if len(raw_body) > MAX_MODEL_LIST_BYTES:
        raise ModelCatalogError("模型列表响应过大，已拒绝解析。")

    try:
        payload = json.loads(raw_body.decode("utf-8-sig"))
    except UnicodeDecodeError as exc:
        raise ModelCatalogError("模型列表接口返回的内容不是 UTF-8 JSON。") from exc
    except json.JSONDecodeError as exc:
        raise ModelCatalogError(f"模型列表接口返回的内容不是合法 JSON：第 {exc.lineno} 行。") from exc

    model_ids = list(_extract_model_ids(payload))
    if not model_ids:
        error_message = _extract_error_message(payload)
        if error_message:
            raise ModelCatalogError(f"模型列表接口返回错误：{error_message}")
        raise ModelCatalogError("模型列表接口没有返回可用模型。")

    return [ModelOption(id=model_id, name=model_id, provider=detect_model_provider(model_id)) for model_id in model_ids]


def detect_model_provider(model_id: str) -> str:
    """按常见模型名前缀给 UI 一个轻量 provider 分类，用于图标和颜色。"""

    normalized = model_id.strip().lower()
    if normalized.startswith(("gpt-", "chatgpt-", "o1", "o3", "o4")):
        return "gpt"
    if normalized.startswith(("claude-", "anthropic/claude")):
        return "claude"
    if normalized.startswith(("deepseek", "deepseek/")):
        return "deepseek"
    if normalized.startswith(("qwen", "qwen/", "qwq")):
        return "qwen"
    if normalized.startswith(("glm", "chatglm", "zhipu")):
        return "glm"
    return "other"


def ensure_current_model_option(
    options: Iterable[ModelOption],
    current_model: str,
) -> list[ModelOption]:
    """确保当前模型即使未出现在远端列表里，也能在 UI 中被看见和高亮。"""

    current = current_model.strip()
    result = list(options)
    if not current:
        return result
    if any(option.id == current for option in result):
        return result
    return [
        ModelOption(id=current, name=current, provider=detect_model_provider(current)),
        *result,
    ]


def model_options_to_ui(options: Iterable[ModelOption]) -> list[dict[str, str]]:
    return [option.to_ui_dict() for option in options]


def format_model_options(options: Iterable[ModelOption], *, current_model: str = "", limit: int = 40) -> str:
    """把模型列表格式化为 TUI 友好的短清单。"""

    rows = list(options)
    lines: list[str] = []
    for index, option in enumerate(rows[:limit], start=1):
        marker = " *" if option.id == current_model else ""
        lines.append(f"{index:>2}. {option.id}{marker}")
    remaining = len(rows) - limit
    if remaining > 0:
        lines.append(f"... 还有 {remaining} 个模型未显示。")
    return "\n".join(lines)


def save_llm_model(model_id: str, config_path: str | Path | None = None) -> Path:
    """把当前模型写回 `config.json` 的 `llm.model`，并保留其他配置项。"""

    model = model_id.strip()
    if not model:
        raise ModelCatalogError("模型 ID 不能为空。")

    try:
        data = load_config_data(config_path)
        llm_section = get_section(data, "llm")
        llm_section["model"] = model
        data["llm"] = llm_section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise ModelCatalogError(str(exc)) from exc


def model_env_override_active() -> bool:
    return bool(os.getenv("OPENAI_MODEL", "").strip())


def _models_endpoint(base_url: str) -> str:
    base = base_url.strip()
    if not base:
        raise ModelCatalogError("缺少 llm.base_url，无法检测模型列表。")
    return f"{base.rstrip('/')}/models"


def _extract_model_ids(payload: Any) -> Iterable[str]:
    data = payload.get("data") if isinstance(payload, Mapping) else payload
    if not isinstance(data, list):
        return []

    seen: set[str] = set()
    result: list[str] = []
    for item in data:
        model_id = _read_model_id(item)
        if not model_id or model_id in seen:
            continue
        seen.add(model_id)
        result.append(model_id)
    return result


def _read_model_id(item: Any) -> str:
    if isinstance(item, str):
        return item.strip()
    if not isinstance(item, Mapping):
        return ""
    for key in ("id", "model", "name"):
        value = item.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ""


def _extract_error_message(payload: Any) -> str:
    if not isinstance(payload, Mapping):
        return ""
    error = payload.get("error")
    if isinstance(error, Mapping):
        message = error.get("message")
        return message.strip() if isinstance(message, str) else ""
    return error.strip() if isinstance(error, str) else ""


def _format_http_error(exc: urllib.error.HTTPError) -> str:
    status = getattr(exc, "code", None)
    if status == 401:
        return "模型列表接口鉴权失败（HTTP 401），请检查 API Key。"
    if status == 403:
        return "当前 API Key 没有读取模型列表的权限（HTTP 403）。"
    if status == 404:
        return "模型列表接口不存在（HTTP 404），请检查 llm.base_url 是否指向 OpenAI 兼容 /v1 地址。"
    if status == 429:
        return "模型列表接口触发限流（HTTP 429），请稍后重试。"
    if isinstance(status, int) and 500 <= status <= 599:
        return f"模型列表服务暂时不可用（HTTP {status}）。"
    if isinstance(status, int):
        return f"模型列表接口返回错误（HTTP {status}）。"
    return "模型列表接口返回错误。"
