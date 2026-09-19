#!/usr/bin/env python3
"""生成 features 其余模块（advisor / agent_workspace / desensitization / image_gen /
tool_output_compression / tts）的对照数据集。

期望值来自 Python 真实现：各配置段的读取校验、默认值与写回文本。

用法：``python rust/tools/gen_config_features_extra_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_features_extra_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path
from unittest import mock

import tomli_w

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_features_extra_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.config.core import runtime as R  # noqa: E402
from omnicrawl.config.features import advisor as AD  # noqa: E402
from omnicrawl.config.features import agent_workspace as AW  # noqa: E402
from omnicrawl.config.features import desensitization as DE  # noqa: E402
from omnicrawl.config.features import image_gen as IG  # noqa: E402
from omnicrawl.config.features import tool_output_compression as TC  # noqa: E402
from omnicrawl.config.features import tts as TT  # noqa: E402

for module in (R, AD, AW, DE, IG, TC, TT):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")

MANAGED_HOME = "C:\\oc-features\\home"
MANAGED_DIR = MANAGED_HOME + "\\" + R.USER_CONFIG_DIRNAME


class Patched:
    def __init__(self, managed_dir: str = MANAGED_DIR) -> None:
        self._env = {"USERPROFILE": MANAGED_HOME, "HOME": MANAGED_HOME}
        self._patch = mock.patch.object(
            R, "user_config_dir", lambda *args, **kwargs: Path(managed_dir)
        )

    def __enter__(self):
        self._stack = mock.patch.dict(os.environ, self._env, clear=True)
        self._stack.start()
        self._patch.start()
        return self

    def __exit__(self, *exc_info):
        self._patch.stop()
        self._stack.stop()
        return False


def write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(content)


def load_cases(loader, cases) -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-features-extra-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, config) in enumerate(cases):
            user_dir = root / ("case-%d" % index) / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            write_text(user_dir / "config.toml", tomli_w.dumps(config))
            record: dict[str, object] = {"name": name, "config": config}
            with Patched(str(user_dir)):
                try:
                    record["result"] = loader()
                except Exception as exc:  # noqa: BLE001 - 期望值就是这段文案
                    record["error"] = str(exc)
            records.append(record)
    return records


def save_case(saver, configuration) -> list[dict[str, object]]:
    records = []
    with tempfile.TemporaryDirectory(prefix="oc-features-extra-save-") as tmp:
        user_dir = Path(tmp).resolve() / R.USER_CONFIG_DIRNAME
        user_dir.mkdir(parents=True)
        target = user_dir / "config.toml"
        write_text(target, tomli_w.dumps({"llm": {"model": "keep-me"}, "tts": {"voice": "old"}}))
        with Patched(str(user_dir)):
            saver(configuration)
            text = target.read_text(encoding="utf-8")
        records.append({"text": text})
    return records


# ── advisor ────────────────────────────────────────────────────────────────

def advisor_result(config: AD.AdvisorConfig) -> dict[str, object]:
    return {
        "enabled": config.enabled,
        "model_key": config.model_key,
        "effort": config.effort,
        "disabled_for_models": list(config.disabled_for_models),
        "active": config.active,
        "display_effort": config.display_effort,
    }


ADVISOR_CASES = [
    ("缺段", {}),
    (
        "全字段",
        {
            "advisor": {
                "enabled": True,
                "model_key": " deepseek-v4-flash ",
                "effort": "xhigh",
                "disabled_for_models": ["gpt-5", " ", "claude"],
            }
        },
    ),
    ("推理档位为空", {"advisor": {"effort": ""}}),
    ("推理档位非法", {"advisor": {"effort": "ultra"}}),
    ("启用但未选模型", {"advisor": {"enabled": True}}),
    ("enabled 非布尔", {"advisor": {"enabled": "yes"}}),
    ("黑名单是字符串", {"advisor": {"disabled_for_models": "gpt"}}),
    ("黑名单是空串", {"advisor": {"disabled_for_models": ""}}),
    ("model_key 非字符串", {"advisor": {"model_key": 5}}),
    ("段不是对象", {"advisor": 5}),
]


# ── tts ────────────────────────────────────────────────────────────────────

def tts_result(config: TT.TTSConfiguration) -> dict[str, object]:
    return {
        "enabled": config.enabled,
        "model_dir": config.model_dir,
        "voice": config.voice,
        "auto_play": config.auto_play,
        "thread_count": config.thread_count,
        "device": config.device,
        "streaming": config.streaming,
        "output_dir": config.output_dir,
    }


TTS_CASES = [
    ("缺段", {}),
    (
        "全字段",
        {
            "tts": {
                "enabled": True,
                "model_dir": " D:\\tts\\models ",
                "voice": " Xiaoyu ",
                "auto_play": False,
                "thread_count": 8,
                "device": " CUDA ",
                "streaming": False,
                "output_dir": " out\\tts ",
            }
        },
    ),
    ("线程数非法", {"tts": {"thread_count": 3}}),
    ("线程数是字符串", {"tts": {"thread_count": "4"}}),
    ("线程数不可解析", {"tts": {"thread_count": "abc"}}),
    ("设备非法", {"tts": {"device": "tpu"}}),
    ("音色为空", {"tts": {"voice": "   "}}),
    ("输出目录为空", {"tts": {"output_dir": ""}}),
    ("段不是对象", {"tts": 5}),
    ("空串段", {"tts": ""}),
]


# ── agent_workspace ────────────────────────────────────────────────────────

def workspace_result(config: AW.AgentWorkspaceConfig) -> dict[str, object]:
    return {
        "enabled": config.enabled,
        "mode": config.mode,
        "base_branch": config.base_branch,
        "base_ref": config.base_ref,
        "detached": config.detached,
        "apply_on_exit": config.apply_on_exit,
        "cleanup_on_exit": config.cleanup_on_exit,
        "sync_uncommitted": config.sync_uncommitted,
        "copy_dirs": list(config.copy_dirs),
        "env_scripts": list(config.env_scripts),
    }


WORKSPACE_CASES = [
    ("缺段", {}),
    (
        "全字段",
        {
            "agent_workspace": {
                "enabled": False,
                "mode": "local",
                "base_branch": "main",
                "base_ref": "origin/main",
                "detached": False,
                "apply_on_exit": False,
                "cleanup_on_exit": "never",
                "sync_uncommitted": False,
                "copy_dirs": [".env", "data"],
                "env_scripts": ["setup.ps1"],
            }
        },
    ),
    ("模式非法", {"agent_workspace": {"mode": "docker"}}),
    ("清理策略非法", {"agent_workspace": {"cleanup_on_exit": "sometimes"}}),
    ("开关非布尔", {"agent_workspace": {"enabled": 1}}),
    ("复制目录非数组", {"agent_workspace": {"copy_dirs": ".env"}}),
    ("复制目录含非字符串", {"agent_workspace": {"copy_dirs": [".env", 3]}}),
    ("段不是对象", {"agent_workspace": 5}),
]


# ── image_gen ──────────────────────────────────────────────────────────────

def image_gen_result(config: IG.ImageGenConfiguration) -> dict[str, object]:
    return {
        "enabled": config.enabled,
        "base_url": config.base_url,
        "api_key": config.api_key,
        "api_key_env": config.api_key_env,
        "model": config.model,
        "size": config.size,
        "quality": config.quality,
        "output_format": config.output_format,
        "n": config.n,
        "timeout_seconds": config.timeout_seconds,
    }


IMAGE_GEN_CASES = [
    ("缺段", {}),
    (
        "全字段",
        {
            "image_gen": {
                "enabled": True,
                "base_url": " https://api.example.com/v1/ ",
                "api_key": " sk-test ",
                "api_key_env": " MY_KEY ",
                "model": " gpt-image-3 ",
                "size": "1536x1024",
                "quality": "high",
                "output_format": "webp",
                "n": 4,
                "timeout_seconds": 300,
            }
        },
    ),
    ("地址为空", {"image_gen": {"base_url": ""}}),
    ("地址缺协议", {"image_gen": {"base_url": "api.example.com"}}),
    ("模型为空", {"image_gen": {"model": " "}}),
    ("尺寸非法", {"image_gen": {"size": "1x1"}}),
    ("尺寸 auto", {"image_gen": {"size": "auto"}}),
    ("质量非法", {"image_gen": {"quality": "ultra"}}),
    ("格式非法", {"image_gen": {"output_format": "gif"}}),
    ("张数为零", {"image_gen": {"n": 0}}),
    ("张数超限", {"image_gen": {"n": 11}}),
    ("超时超限", {"image_gen": {"timeout_seconds": 601}}),
    ("凭据环境变量为空", {"image_gen": {"api_key_env": " "}}),
    ("段不是对象", {"image_gen": 5}),
]


# ── tool_output_compression ────────────────────────────────────────────────

def compression_result(config: TC.ToolOutputCompressionConfig) -> dict[str, object]:
    return {
        "enabled": config.enabled,
        "model_key": config.model_key,
        "thinking_enabled": config.thinking_enabled,
        "reasoning_effort": config.reasoning_effort,
        "min_chars": config.min_chars,
        "max_input_chars": config.max_input_chars,
        "max_output_chars": config.max_output_chars,
        "timeout_seconds": config.timeout_seconds,
        "active": config.active,
    }


COMPRESSION_CASES = [
    ("缺段", {}),
    (
        "全字段",
        {
            "tool_output_compression": {
                "enabled": True,
                "model_key": " qwen2.5-3b-instruct ",
                "thinking_enabled": True,
                "reasoning_effort": " xhigh ",
                "min_chars": 800,
                "max_input_chars": 12000,
                "max_output_chars": 900,
                "timeout_seconds": 30,
            }
        },
    ),
    ("思考档位为空", {"tool_output_compression": {"reasoning_effort": ""}}),
    ("思考档位为 none", {"tool_output_compression": {"reasoning_effort": "none"}}),
    ("下限为零", {"tool_output_compression": {"min_chars": 0}}),
    ("超时为负", {"tool_output_compression": {"timeout_seconds": -1}}),
    ("启用但未选模型", {"tool_output_compression": {"enabled": True}}),
    ("开关非布尔", {"tool_output_compression": {"enabled": 1}}),
    ("段不是对象", {"tool_output_compression": 5}),
]


# ── desensitization ────────────────────────────────────────────────────────

def desensitization_result(config: DE.DesensitizationConfig) -> dict[str, object]:
    return {
        "enabled": config.enabled,
        "fail_closed": config.fail_closed,
        "strict_restore": config.strict_restore,
        "extra_sensitive_keys": list(config.extra_sensitive_keys),
        "exempt_keys": list(config.exempt_keys),
        "entropy_enabled": config.entropy_enabled,
        "entropy_min_length": config.entropy_min_length,
        "entropy_min_bits": config.entropy_min_bits,
        "entropy_pure_letters": config.entropy_pure_letters,
        "entropy_pure_digits": config.entropy_pure_digits,
        "detect_pem_private_key": config.detect_pem_private_key,
        "detect_db_connection_string": config.detect_db_connection_string,
        "detect_email": config.detect_email,
        "detect_bank_card": config.detect_bank_card,
        "detect_internal_ip": config.detect_internal_ip,
        "detect_external_ip": config.detect_external_ip,
        "detect_url": config.detect_url,
        "detect_mac_address": config.detect_mac_address,
        "detect_license_plate": config.detect_license_plate,
        "gitleaks_enabled": config.gitleaks_enabled,
        "gitleaks_config_path": config.gitleaks_config_path,
        "ner_enabled": config.ner_enabled,
        "ner_model_path": config.ner_model_path,
        "ner_device": config.ner_device,
        "ner_entity_types": list(config.ner_entity_types),
        "ner_min_entity_chars": config.ner_min_entity_chars,
        "ner_cache_size": config.ner_cache_size,
    }


DESENSITIZATION_CASES = [
    ("缺段", {}),
    (
        "全字段",
        {
            "desensitization": {
                "enabled": True,
                "fail_closed": False,
                "strict_restore": True,
                "extra_sensitive_keys": [" secret ", "", "token"],
                "exempt_keys": ["public_key"],
                "entropy_enabled": False,
                "entropy_min_length": 12,
                "entropy_min_bits": 4,
                "entropy_pure_letters": True,
                "entropy_pure_digits": True,
                "detect_pem_private_key": False,
                "detect_db_connection_string": False,
                "detect_email": True,
                "detect_bank_card": False,
                "detect_internal_ip": False,
                "detect_external_ip": True,
                "detect_url": True,
                "detect_mac_address": False,
                "detect_license_plate": False,
                "gitleaks_enabled": False,
                "gitleaks_config_path": " D:\\gitleaks.toml ",
                "ner_enabled": True,
                "ner_model_path": " D:\\ner.pt ",
                "ner_device": " CPU ",
                "ner_entity_types": ["per", " loc "],
                "ner_min_entity_chars": 3,
                "ner_cache_size": 16,
            }
        },
    ),
    ("熵阈值超界", {"desensitization": {"entropy_min_bits": 9}}),
    ("熵阈值为负", {"desensitization": {"entropy_min_bits": -1}}),
    ("熵长度下限为负", {"desensitization": {"entropy_min_length": -1}}),
    ("熵长度非整数", {"desensitization": {"entropy_min_length": 1.5}}),
    ("设备非法", {"desensitization": {"ner_device": "GPU"}}),
    ("实体类型非法", {"desensitization": {"ner_entity_types": ["XYZ"]}}),
    ("实体类型为空", {"desensitization": {"ner_entity_types": []}}),
    ("最小实体长度为零", {"desensitization": {"ner_min_entity_chars": 0}}),
    ("缓存容量为负", {"desensitization": {"ner_cache_size": -1}}),
    ("缓存容量为零", {"desensitization": {"ner_cache_size": 0}}),
    ("敏感键非数组", {"desensitization": {"extra_sensitive_keys": "secret"}}),
    ("开关非布尔", {"desensitization": {"enabled": 1}}),
    ("段不是对象", {"desensitization": 5}),
]


def main() -> None:
    fixture = {
        "advisor_load": load_cases(
            lambda: advisor_result(AD.load_advisor_config()), ADVISOR_CASES
        ),
        "advisor_save": save_case(
            lambda config: AD.save_advisor_config(config),
            AD.AdvisorConfig(enabled=True, model_key="gpt-5", effort="max", disabled_for_models=("x",)),
        ),
        "advisor_clear": save_case(lambda config=None: AD.clear_advisor_config(), None),
        "tts_load": load_cases(lambda: tts_result(TT.load_tts_configuration()), TTS_CASES),
        "tts_save": save_case(
            lambda config: TT.save_tts_configuration(config),
            TT.TTSConfiguration(enabled=True, voice="Junhao", thread_count=4),
        ),
        "agent_workspace_load": load_cases(
            lambda: workspace_result(AW.load_agent_workspace_config()), WORKSPACE_CASES
        ),
        "agent_workspace_save": save_case(
            lambda config: AW.save_agent_workspace_config(config),
            AW.AgentWorkspaceConfig(mode="local", copy_dirs=(".env",)),
        ),
        "image_gen_load": load_cases(
            lambda: image_gen_result(IG.load_image_gen_configuration()), IMAGE_GEN_CASES
        ),
        "image_gen_save": save_case(
            lambda config: IG.save_image_gen_configuration(config),
            IG.ImageGenConfiguration(enabled=True, size="1024x1024", n=2),
        ),
        "tool_output_compression_load": load_cases(
            lambda: compression_result(TC.load_tool_output_compression_config()),
            COMPRESSION_CASES,
        ),
        "tool_output_compression_save": save_case(
            lambda config: TC.save_tool_output_compression_config(config),
            TC.ToolOutputCompressionConfig(enabled=True, model_key="qwen2.5-3b-instruct"),
        ),
        "tool_output_compression_clear": save_case(
            lambda config=None: TC.clear_tool_output_compression_config(), None
        ),
        "desensitization_load": load_cases(
            lambda: desensitization_result(DE.load_desensitization_config()),
            DESENSITIZATION_CASES,
        ),
        "desensitization_save": save_case(
            lambda config: DE.save_desensitization_config(config),
            DE.DesensitizationConfig(
                enabled=True,
                extra_sensitive_keys=("secret",),
                ner_entity_types=("PER", "LOC"),
            ),
        ),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print("wrote %s: %s" % (FIXTURE_PATH, {key: len(value) for key, value in fixture.items()}))


if __name__ == "__main__":
    main()
