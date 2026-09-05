"""图像生成（OpenAI 兼容 Image API）配置的读取、校验与写回。

配置段示例（config.toml）：

.. code-block:: yaml

    image_gen:
      enabled: true
      base_url: "https://api.openai.com/v1"
      api_key: ""
      api_key_env: "OPENAI_API_KEY"
      model: "gpt-image-2"
      size: "auto"
      quality: "auto"
      output_format: "png"
      n: 1
      timeout_seconds: 120

说明：
- ``base_url`` 支持任何 OpenAI 兼容接口地址（官方或中转站），默认官方地址。
- ``api_key`` 直接填写时写入本地 config.toml（本地文件，不提交到仓库）；
  留空时运行时按 ``api_key_env``（默认 ``OPENAI_API_KEY``）读取环境变量。
- ``size`` 支持 ``auto`` 或 ``宽x高``（如 1024x1024、1536x1024），最大边长 3840、16 的倍数。
- ``quality`` 支持 low / medium / high / auto。
- ``output_format`` 支持 png / jpeg / webp。
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

from ..core.runtime import RuntimeConfigError, load_config_data, save_config_data

DEFAULT_IMAGE_GEN_BASE_URL = "https://api.openai.com/v1"
DEFAULT_IMAGE_GEN_MODEL = "gpt-image-2"
DEFAULT_IMAGE_GEN_API_KEY_ENV = "OPENAI_API_KEY"
_IMAGE_GEN_QUALITIES = ("auto", "low", "medium", "high")
_IMAGE_GEN_FORMATS = ("png", "jpeg", "webp")
_SIZE_RE = re.compile(r"^\d{2,4}x\d{2,4}$")


class ImageGenConfigError(RuntimeError):
    """图像生成配置无效或无法写回。"""


@dataclass(frozen=True)
class ImageGenConfiguration:
    """图像生成服务的开关与接口参数。"""

    enabled: bool = False
    base_url: str = DEFAULT_IMAGE_GEN_BASE_URL
    api_key: str = ""
    api_key_env: str = DEFAULT_IMAGE_GEN_API_KEY_ENV
    model: str = DEFAULT_IMAGE_GEN_MODEL
    size: str = "auto"
    quality: str = "auto"
    output_format: str = "png"
    n: int = 1
    timeout_seconds: int = 120

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise ImageGenConfigError("image_gen.enabled 必须是布尔值。")
        base_url = self.base_url.strip().rstrip("/")
        if not base_url:
            raise ImageGenConfigError("image_gen.base_url 不能为空。")
        if not base_url.startswith(("http://", "https://")):
            raise ImageGenConfigError("image_gen.base_url 必须以 http:// 或 https:// 开头。")
        model = self.model.strip()
        if not model:
            raise ImageGenConfigError("image_gen.model 不能为空。")
        if self.size != "auto" and not _SIZE_RE.match(self.size):
            raise ImageGenConfigError(
                "image_gen.size 必须是 auto 或 宽x高 形式（例如 1024x1024、1536x1024）。"
            )
        if self.quality not in _IMAGE_GEN_QUALITIES:
            raise ImageGenConfigError(
                f"image_gen.quality 仅支持 {'、'.join(_IMAGE_GEN_QUALITIES)}。"
            )
        if self.output_format not in _IMAGE_GEN_FORMATS:
            raise ImageGenConfigError(
                f"image_gen.output_format 仅支持 {'、'.join(_IMAGE_GEN_FORMATS)}。"
            )
        if isinstance(self.n, bool) or not isinstance(self.n, int) or not 1 <= self.n <= 10:
            raise ImageGenConfigError("image_gen.n 必须是 1~10 的整数。")
        if (
            isinstance(self.timeout_seconds, bool)
            or not isinstance(self.timeout_seconds, int)
            or not 1 <= self.timeout_seconds <= 600
        ):
            raise ImageGenConfigError("image_gen.timeout_seconds 必须是 1~600 的整数。")
        object.__setattr__(self, "base_url", base_url)
        object.__setattr__(self, "model", model)
        object.__setattr__(self, "api_key", self.api_key.strip())
        object.__setattr__(self, "api_key_env", (self.api_key_env.strip() or DEFAULT_IMAGE_GEN_API_KEY_ENV))

    def resolve_api_key(self) -> str:
        """优先使用配置里的 api_key，否则按 api_key_env 读取环境变量。"""

        if self.api_key:
            return self.api_key
        return os.getenv(self.api_key_env, "")


def load_image_gen_configuration(
    config_path: str | Path | None = None,
) -> ImageGenConfiguration:
    """读取图像生成配置；缺少 ``image_gen`` 段时返回关闭的默认配置。"""

    try:
        data = load_config_data(config_path)
        raw_section = data.get("image_gen", {})
        if raw_section in (None, ""):
            raw_section = {}
        if not isinstance(raw_section, Mapping):
            raise ImageGenConfigError("配置段 image_gen 必须是对象。")
        return ImageGenConfiguration(
            enabled=bool(raw_section.get("enabled", False)),
            base_url=str(raw_section.get("base_url") or DEFAULT_IMAGE_GEN_BASE_URL),
            api_key=str(raw_section.get("api_key") or ""),
            api_key_env=str(raw_section.get("api_key_env") or DEFAULT_IMAGE_GEN_API_KEY_ENV),
            model=str(raw_section.get("model") or DEFAULT_IMAGE_GEN_MODEL),
            size=str(raw_section.get("size") or "auto"),
            quality=str(raw_section.get("quality") or "auto"),
            output_format=str(raw_section.get("output_format") or "png"),
            n=int(raw_section.get("n") or 1),
            timeout_seconds=int(raw_section.get("timeout_seconds") or 120),
        )
    except ImageGenConfigError:
        raise
    except (ValueError, TypeError) as exc:
        raise ImageGenConfigError(f"配置段 image_gen 数值字段无效：{exc}") from exc
    except RuntimeConfigError as exc:
        raise ImageGenConfigError(str(exc)) from exc


def save_image_gen_configuration(
    configuration: ImageGenConfiguration,
    config_path: str | Path | None = None,
) -> Path:
    """保留其他配置段，只更新完整的图像生成配置。"""

    if not isinstance(configuration, ImageGenConfiguration):
        raise ImageGenConfigError("图像生成配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["image_gen"] = {
            "enabled": configuration.enabled,
            "base_url": configuration.base_url,
            "api_key": configuration.api_key,
            "api_key_env": configuration.api_key_env,
            "model": configuration.model,
            "size": configuration.size,
            "quality": configuration.quality,
            "output_format": configuration.output_format,
            "n": configuration.n,
            "timeout_seconds": configuration.timeout_seconds,
        }
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise ImageGenConfigError(str(exc)) from exc


__all__ = [
    "DEFAULT_IMAGE_GEN_BASE_URL",
    "DEFAULT_IMAGE_GEN_MODEL",
    "ImageGenConfigError",
    "ImageGenConfiguration",
    "load_image_gen_configuration",
    "save_image_gen_configuration",
]
