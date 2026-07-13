"""config.json → config.yaml / models.yaml 迁移。"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from .runtime import (
    RuntimeConfigError,
    atomic_write_text,
    dump_yaml_text,
    load_raw_file,
)


MIGRATION_MARKER_KEY = "_migration"
MIGRATION_SOURCE_HASH_KEY = "source_config_json_sha256"


def maybe_migrate_config_json(
    *,
    json_path: Path,
    yaml_path: Path,
    models_path: Path,
) -> bool:
    """在默认路径下把旧 config.json 迁移为 YAML。

    返回 True 表示已写入或确认无需再迁移。
    显式 AI_CONFIG_FILE 指向外部 JSON 时，调用方不应调用本函数。
    """

    if yaml_path.exists():
        return True
    if not json_path.exists():
        return False

    try:
        text = json_path.read_text(encoding="utf-8-sig")
        data = json.loads(text)
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeConfigError(f"迁移读取 config.json 失败：{exc}") from exc
    if not isinstance(data, dict):
        raise RuntimeConfigError("config.json 顶层必须是对象，无法迁移。")

    source_hash = hashlib.sha256(text.encode("utf-8")).hexdigest()
    config_yaml, models_yaml = build_migrated_documents(data, source_hash=source_hash)

    # 先写 models.yaml，再写 config.yaml，避免 active_model 悬空。
    if models_yaml.get("models"):
        atomic_write_text(models_path, dump_yaml_text(models_yaml))
        # 回读校验
        loaded_models = load_raw_file(models_path)
        if not isinstance(loaded_models.get("models"), dict):
            raise RuntimeConfigError("迁移后 models.yaml 校验失败。")

    atomic_write_text(yaml_path, dump_yaml_text(config_yaml))
    loaded_config = load_raw_file(yaml_path)
    if not isinstance(loaded_config, dict):
        raise RuntimeConfigError("迁移后 config.yaml 校验失败。")

    # 备份旧 JSON，不删除。
    backup = json_path.with_suffix(json_path.suffix + ".migrated.bak")
    if not backup.exists():
        try:
            backup.write_text(text, encoding="utf-8")
        except OSError:
            pass
    return True


def build_migrated_documents(
    data: dict[str, Any],
    *,
    source_hash: str,
) -> tuple[dict[str, Any], dict[str, Any]]:
    """把旧 JSON 配置映射为 config.yaml + models.yaml 文档。"""

    llm = data.get("llm") if isinstance(data.get("llm"), dict) else {}
    api_key = str(llm.get("api_key") or "").strip()
    base_url = str(llm.get("base_url") or "").strip()
    model = str(llm.get("model") or "").strip() or "migrated-model"
    context_window = llm.get("context_window_tokens", 128_000)
    if not isinstance(context_window, int) or isinstance(context_window, bool) or context_window <= 0:
        context_window = 128_000
    reasoning_effort = str(llm.get("reasoning_effort") or "").strip()
    thinking_type = str(llm.get("thinking_type") or "disabled").strip() or "disabled"

    profile_id = "migrated-openai"
    model_key = "migrated-default"

    config_yaml: dict[str, Any] = {
        "version": 2,
        "llm": {
            "active_model": {
                "source": "custom",
                "key": model_key,
            },
            "defaults": {
                "request_timeout_seconds": 180,
                "request_retry_count": 5,
                "discovery_timeout_seconds": 10,
                "discovery_cache_ttl_seconds": 300,
                "reasoning_effort": reasoning_effort,
                "thinking_type": thinking_type,
            },
            "profiles": {
                profile_id: {
                    "provider": "openai",
                    "enabled": True,
                    "base_url": base_url,
                    "default_protocol": "openai_chat_completions",
                    "discovery": {"enabled": True},
                }
            },
        },
        MIGRATION_MARKER_KEY: {
            MIGRATION_SOURCE_HASH_KEY: source_hash,
            "from": "config.json",
        },
    }
    if api_key:
        config_yaml["llm"]["profiles"][profile_id]["api_key"] = api_key

    # 保留非 LLM section
    for section in ("approval", "api", "agent_temp", "plugins", "mcp", "voice"):
        if section in data and isinstance(data[section], dict):
            config_yaml[section] = data[section]

    models_yaml: dict[str, Any] = {
        "version": 1,
        "models": {
            model_key: {
                "display_name": model,
                "profile": profile_id,
                "model_id": model,
                "protocol": "openai_chat_completions",
                "enabled": True,
                "context_window_tokens": context_window,
                "capabilities": {
                    "streaming": True,
                    "tools": True,
                },
            }
        },
    }
    return config_yaml, models_yaml
