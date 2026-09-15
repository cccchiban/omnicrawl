"""脱敏（Agent 网关消息脱敏）配置：默认关闭，仅显式启用后对出网 AI 消息做可逆脱敏。

对应设计稿 ``omnicrawl/docs/agent_gateway_desensitization_design.md``，落到
config.toml 的 ``[desensitization]`` 段：

.. code-block:: yaml

    [desensitization]
    enabled = false                # opt-in；未启用零成本
    fail_closed = true             # 屏蔽异常时中止请求，不静默发送原文
    strict_restore = false         # 还原缺失时中止并报错（默认保留 + 告警）
    extra_sensitive_keys = []      # 追加敏感键名
    exempt_keys = []               # 豁免键名（优先于命中）
    entropy_enabled = true         # 熵兜底开关（关闭则仅键名 / 结构匹配）
    entropy_min_length = 20        # 熵兜底：长度下限
    entropy_min_bits = 3.5         # 熵兜底：熵阈值
    entropy_pure_letters = false   # 纯字母令牌：长度达标即脱敏（默认关闭）
    entropy_pure_digits = false    # 纯数字令牌：长度达标即脱敏（默认关闭）
    # 值类型规则层（正则识别「无键名的敏感值类型」）：高危类型默认开启，
    # 泛化类型（邮箱 / 外网 IP / 网址）误报较高，默认关闭、按需开启。
    detect_pem_private_key = true         # PEM 私钥（BEGIN/END … PRIVATE KEY）
    detect_db_connection_string = true    # 数据库连接串（URI / ADO 键值）
    detect_email = false                  # 邮箱地址
    detect_bank_card = true               # 银行卡号（Luhn 校验）
    detect_internal_ip = true             # 内网 IP（私有 / 链路本地）
    detect_external_ip = false            # 外网 IP（公网可路由）
    detect_url = false                    # 网址（http / https / ftp）
    detect_mac_address = true             # MAC 地址
    detect_license_plate = true           # 中国大陆车牌
    gitleaks_enabled = true               # gitleaks 开源规则（内置离线快照）
    gitleaks_config_path = ""             # 自定义 gitleaks.toml（按 id 覆盖 / 追加）
    # NER 兜底层（BiLSTM-CRF：人名 / 地名 / 机构名）：结构 / 规则 / 熵之外的最后一道
    # 语义兜底，输入是前几层处理后的文本且只对中文片段兜底（非中文字符已等长隔离）；可选依赖 torch，缺失时自动跳过。
    ner_enabled = false                   # 默认关闭；开启后作为语义兜底
    ner_model_path = ""                   # checkpoint 路径；留空用随包权重 / 环境变量
    ner_device = "auto"                   # auto 优先 CUDA、不可用回退 CPU
    ner_entity_types = ["PER", "ORG", "LOC"]  # 需要识别的实体类型
    ner_min_entity_chars = 2              # 最小实体长度（2 规避单字地名歧义）
    ner_cache_size = 2048                 # 推理结果缓存容量（单位为「块」，0 关闭）
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

from ..core.runtime import (
    RuntimeConfigError,
    get_section,
    load_config_data,
    save_config_data,
)

DEFAULT_ENTROPY_MIN_LENGTH = 20
DEFAULT_ENTROPY_MIN_BITS = 3.5

#: NER 兜底层的固定取值。
NER_ENTITY_TYPES = ("PER", "ORG", "LOC")
NER_DEVICES = ("auto", "cpu", "cuda")
DEFAULT_NER_DEVICE = "auto"
DEFAULT_NER_MIN_ENTITY_CHARS = 2
#: NER 推理结果缓存容量（单位为「块」，每块 ≤ 200 字符）：默认值约覆盖 400KB 文本，
#: 足以让「历史重发」的整段对话常驻；与 ``llm.desensitization.ner.DEFAULT_CACHE_SIZE``
#: 保持一致（配置层不反向依赖 llm 层，故分别声明，由测试守护）。
DEFAULT_NER_CACHE_SIZE = 2048


class DesensitizationConfigError(RuntimeError):
    """脱敏配置无效或无法写回。"""


@dataclass(frozen=True)
class DesensitizationConfig:
    """脱敏开关、失败策略与匹配参数。

    ``enabled`` 为 False 时运行时不做任何包装（零成本）；``fail_closed`` 为
    True 时屏蔽阶段异常会中止本次请求，而不是退化为发送原文；默认值面向
    opt-in 的保守语义（设计稿 §10.1）。
    """

    enabled: bool = False
    fail_closed: bool = True
    strict_restore: bool = False
    extra_sensitive_keys: tuple[str, ...] = ()
    exempt_keys: tuple[str, ...] = ()
    entropy_enabled: bool = True
    entropy_min_length: int = DEFAULT_ENTROPY_MIN_LENGTH
    entropy_min_bits: float = DEFAULT_ENTROPY_MIN_BITS
    entropy_pure_letters: bool = False
    entropy_pure_digits: bool = False
    # 值类型规则层：高危类型默认开启，泛化类型（邮箱 / 外网 IP / 网址）默认关闭。
    detect_pem_private_key: bool = True
    detect_db_connection_string: bool = True
    detect_email: bool = False
    detect_bank_card: bool = True
    detect_internal_ip: bool = True
    detect_external_ip: bool = False
    detect_url: bool = False
    detect_mac_address: bool = True
    detect_license_plate: bool = True
    gitleaks_enabled: bool = True
    gitleaks_config_path: str = ""
    # NER 兜底层：可选依赖 torch；未安装 / 模型缺失时运行时静默跳过。
    ner_enabled: bool = False
    ner_model_path: str = ""
    ner_device: str = DEFAULT_NER_DEVICE
    ner_entity_types: tuple[str, ...] = NER_ENTITY_TYPES
    ner_min_entity_chars: int = DEFAULT_NER_MIN_ENTITY_CHARS
    #: 结果缓存容量（单位为「块」，非整段文本）；0 表示关闭缓存。
    ner_cache_size: int = DEFAULT_NER_CACHE_SIZE

    def __post_init__(self) -> None:
        for name in (
            "enabled",
            "fail_closed",
            "strict_restore",
            "entropy_enabled",
            "entropy_pure_letters",
            "entropy_pure_digits",
            "detect_pem_private_key",
            "detect_db_connection_string",
            "detect_email",
            "detect_bank_card",
            "detect_internal_ip",
            "detect_external_ip",
            "detect_url",
            "detect_mac_address",
            "detect_license_plate",
            "gitleaks_enabled",
            "ner_enabled",
        ):
            if not isinstance(getattr(self, name), bool):
                raise DesensitizationConfigError(f"desensitization.{name} 必须是布尔值。")
        for name in ("extra_sensitive_keys", "exempt_keys"):
            value = getattr(self, name)
            if not isinstance(value, (tuple, list)):
                raise DesensitizationConfigError(f"desensitization.{name} 必须是字符串数组。")
            object.__setattr__(
                self,
                name,
                tuple(str(item).strip() for item in value if str(item).strip()),
            )
        length = self.entropy_min_length
        if isinstance(length, bool) or not isinstance(length, int) or length < 0:
            raise DesensitizationConfigError(
                "desensitization.entropy_min_length 必须是非负整数。"
            )
        bits = self.entropy_min_bits
        if (
            isinstance(bits, bool)
            or not isinstance(bits, (int, float))
            or not 0 <= float(bits) <= 8
        ):
            raise DesensitizationConfigError(
                "desensitization.entropy_min_bits 必须是 0–8 之间的数。"
            )
        object.__setattr__(self, "entropy_min_bits", float(bits))
        path = self.gitleaks_config_path
        if not isinstance(path, str):
            raise DesensitizationConfigError(
                "desensitization.gitleaks_config_path 必须是字符串路径。"
            )
        object.__setattr__(self, "gitleaks_config_path", path.strip())
        # ── NER 兜底层 ──────────────────────────────────────────────
        model_path = self.ner_model_path
        if not isinstance(model_path, str):
            raise DesensitizationConfigError(
                "desensitization.ner_model_path 必须是字符串路径。"
            )
        object.__setattr__(self, "ner_model_path", model_path.strip())
        device = self.ner_device
        if not isinstance(device, str) or device.strip().lower() not in NER_DEVICES:
            raise DesensitizationConfigError(
                "desensitization.ner_device 必须是 auto / cpu / cuda 之一。"
            )
        object.__setattr__(self, "ner_device", device.strip().lower())
        raw_types = self.ner_entity_types
        if not isinstance(raw_types, (tuple, list)):
            raise DesensitizationConfigError(
                "desensitization.ner_entity_types 必须是字符串数组。"
            )
        entity_types = tuple(
            str(item).strip().upper() for item in raw_types if str(item).strip()
        )
        if not entity_types or any(item not in NER_ENTITY_TYPES for item in entity_types):
            raise DesensitizationConfigError(
                f"desensitization.ner_entity_types 只能包含 {list(NER_ENTITY_TYPES)}。"
            )
        object.__setattr__(self, "ner_entity_types", entity_types)
        min_chars = self.ner_min_entity_chars
        if isinstance(min_chars, bool) or not isinstance(min_chars, int) or min_chars < 1:
            raise DesensitizationConfigError(
                "desensitization.ner_min_entity_chars 必须是正整数。"
            )
        cache_size = self.ner_cache_size
        if (
            isinstance(cache_size, bool)
            or not isinstance(cache_size, int)
            or cache_size < 0
        ):
            raise DesensitizationConfigError(
                "desensitization.ner_cache_size 必须是非负整数。"
            )


def load_desensitization_config(
    config_path: str | Path | None = None,
) -> DesensitizationConfig:
    """读取 config.toml 的 ``[desensitization]`` 段；缺失或为空时返回默认关闭配置。"""

    try:
        data = load_config_data(config_path)
        section = get_section(data, "desensitization")
    except RuntimeConfigError as exc:
        raise DesensitizationConfigError(str(exc)) from exc
    return _parse_desensitization_section(section)


def _parse_desensitization_section(section: Mapping[str, Any]) -> DesensitizationConfig:
    return DesensitizationConfig(
        enabled=_bool_field(section, "enabled", False),
        fail_closed=_bool_field(section, "fail_closed", True),
        strict_restore=_bool_field(section, "strict_restore", False),
        extra_sensitive_keys=_key_list_field(section, "extra_sensitive_keys"),
        exempt_keys=_key_list_field(section, "exempt_keys"),
        entropy_enabled=_bool_field(section, "entropy_enabled", True),
        entropy_min_length=_int_field(
            section, "entropy_min_length", DEFAULT_ENTROPY_MIN_LENGTH
        ),
        entropy_min_bits=_float_field(
            section, "entropy_min_bits", DEFAULT_ENTROPY_MIN_BITS
        ),
        entropy_pure_letters=_bool_field(section, "entropy_pure_letters", False),
        entropy_pure_digits=_bool_field(section, "entropy_pure_digits", False),
        detect_pem_private_key=_bool_field(section, "detect_pem_private_key", True),
        detect_db_connection_string=_bool_field(
            section, "detect_db_connection_string", True
        ),
        detect_email=_bool_field(section, "detect_email", False),
        detect_bank_card=_bool_field(section, "detect_bank_card", True),
        detect_internal_ip=_bool_field(section, "detect_internal_ip", True),
        detect_external_ip=_bool_field(section, "detect_external_ip", False),
        detect_url=_bool_field(section, "detect_url", False),
        detect_mac_address=_bool_field(section, "detect_mac_address", True),
        detect_license_plate=_bool_field(section, "detect_license_plate", True),
        gitleaks_enabled=_bool_field(section, "gitleaks_enabled", True),
        gitleaks_config_path=_str_field(section, "gitleaks_config_path", ""),
        ner_enabled=_bool_field(section, "ner_enabled", False),
        ner_model_path=_str_field(section, "ner_model_path", ""),
        ner_device=_device_field(section, "ner_device", DEFAULT_NER_DEVICE),
        ner_entity_types=_entity_types_field(section, "ner_entity_types"),
        ner_min_entity_chars=_int_field(
            section, "ner_min_entity_chars", DEFAULT_NER_MIN_ENTITY_CHARS
        ),
        ner_cache_size=_int_field(section, "ner_cache_size", DEFAULT_NER_CACHE_SIZE),
    )


def save_desensitization_config(
    config: DesensitizationConfig,
    config_path: str | Path | None = None,
) -> Path:
    """把脱敏配置写回 config.toml 的 ``[desensitization]`` 段（保留其他段）。"""

    if not isinstance(config, DesensitizationConfig):
        raise DesensitizationConfigError("脱敏配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["desensitization"] = {
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
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise DesensitizationConfigError(str(exc)) from exc


def _bool_field(section: Mapping[str, Any], name: str, default: bool) -> bool:
    value = section.get(name, default)
    if not isinstance(value, bool):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是布尔值。")
    return value


def _key_list_field(section: Mapping[str, Any], name: str) -> tuple[str, ...]:
    raw = section.get(name, ())
    if raw in (None, ""):
        raw = ()
    if not isinstance(raw, (list, tuple)):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是字符串数组。")
    return tuple(str(item).strip() for item in raw if str(item).strip())


def _str_field(section: Mapping[str, Any], name: str, default: str) -> str:
    value = section.get(name, default)
    if value is None:
        return default
    if not isinstance(value, str):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是字符串路径。")
    return value.strip()


def _int_field(section: Mapping[str, Any], name: str, default: int) -> int:
    value = section.get(name, default)
    if isinstance(value, bool) or not isinstance(value, int):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是非负整数。")
    return value


def _device_field(section: Mapping[str, Any], name: str, default: str) -> str:
    value = section.get(name, default)
    if not isinstance(value, str) or value.strip().lower() not in NER_DEVICES:
        raise DesensitizationConfigError(
            f"desensitization.{name} 必须是 auto / cpu / cuda 之一。"
        )
    return value.strip().lower()


def _entity_types_field(section: Mapping[str, Any], name: str) -> tuple[str, ...]:
    raw = section.get(name, NER_ENTITY_TYPES)
    if raw in (None, ""):
        raw = NER_ENTITY_TYPES
    if not isinstance(raw, (list, tuple)):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是字符串数组。")
    return tuple(str(item).strip().upper() for item in raw if str(item).strip())


def _float_field(section: Mapping[str, Any], name: str, default: float) -> float:
    value = section.get(name, default)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise DesensitizationConfigError(f"desensitization.{name} 必须是 0–8 之间的数。")
    return float(value)


__all__ = [
    "DEFAULT_ENTROPY_MIN_BITS",
    "DEFAULT_ENTROPY_MIN_LENGTH",
    "DEFAULT_NER_DEVICE",
    "DEFAULT_NER_CACHE_SIZE",
    "DEFAULT_NER_MIN_ENTITY_CHARS",
    "DesensitizationConfig",
    "DesensitizationConfigError",
    "NER_DEVICES",
    "NER_ENTITY_TYPES",
    "load_desensitization_config",
    "save_desensitization_config",
]
