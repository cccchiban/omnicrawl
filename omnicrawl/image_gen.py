"""图像生成与编辑（OpenAI 兼容 Image API）。

实现基于 OpenAI Python SDK 的 ``images.generate`` / ``images.edit`` 端点
（``POST /v1/images/generations`` 与 ``POST /v1/images/edits``），兼容任何
OpenAI 兼容接口地址（官方 API 或中转站），支持 gpt-image-1 系列模型
（gpt-image-2 / gpt-image-1.5 / gpt-image-1 / gpt-image-1-mini）的
``size`` / ``quality`` / ``output_format`` 参数。

接口参数来自 config.yaml 的 ``image_gen`` 段（见 config/image_gen.py）；
调用时可用参数覆盖 size / quality / output_format / n。生成的图片默认保存到
工作区 ``.agent_tmp/images/`` 目录，也可通过 ``path`` 指定保存位置。
"""

from __future__ import annotations

import base64
from datetime import datetime
from pathlib import Path
from typing import Any, Mapping

from .config.image_gen import (
    ImageGenConfiguration,
    load_image_gen_configuration,
)

DEFAULT_OUTPUT_DIR = Path(".agent_tmp") / "images"
_DEFAULT_OUTPUT_FORMAT = "png"


class ImageGenError(RuntimeError):
    """图像生成失败（配置缺失、网络错误、API 拒绝等）。"""


class ImageGenerator:
    """封装 OpenAI 兼容 Image API 的生成与编辑调用。"""

    def __init__(
        self,
        configuration: ImageGenConfiguration | None = None,
        config_path: str | Path | None = None,
        *,
        client_factory: Any = None,
    ) -> None:
        """``client_factory`` 用于测试注入；接收 ``api_key``/``base_url``/``timeout`` 关键字。"""

        if configuration is None:
            configuration = load_image_gen_configuration(config_path)
        self._configuration = configuration
        self._client_factory = client_factory

    def run(self, arguments: Mapping[str, Any]) -> str:
        """从工具参数字典解析并执行生成或编辑，返回结果文本。

        ``image`` 非空时走编辑流程，否则走生成流程；``n`` 支持整数或数字字符串。
        """

        prompt = str(arguments.get("prompt") or "").strip()
        if not prompt:
            raise ImageGenError("需要提供 prompt。")
        image = str(arguments.get("image") or "").strip()
        n = _optional_int(arguments.get("n"))
        size = _optional_str(arguments.get("size"))
        quality = _optional_str(arguments.get("quality"))
        output_format = _optional_str(arguments.get("output_format"))
        output_path = _optional_str(arguments.get("path"))
        if image:
            return self.edit(
                prompt,
                image,
                n=n,
                size=size,
                quality=quality,
                output_format=output_format,
                output_path=output_path,
            )
        return self.generate(
            prompt,
            n=n,
            size=size,
            quality=quality,
            output_format=output_format,
            output_path=output_path,
        )

    def generate(
        self,
        prompt: str,
        *,
        n: int | None = None,
        size: str | None = None,
        quality: str | None = None,
        output_format: str | None = None,
        output_path: str | None = None,
    ) -> str:
        """根据文本提示生成一张或多张图片，保存后返回结果文本。"""

        prompt = (prompt or "").strip()
        if not prompt:
            raise ImageGenError("生成图片需要提供 prompt。")
        conf = self._configuration
        self._ensure_ready()
        kwargs = self._request_kwargs(
            n=n,
            size=size,
            quality=quality,
            output_format=output_format,
        )
        kwargs["prompt"] = prompt
        try:
            result = self._client().images.generate(
                model=conf.model,
                **kwargs,
            )
        except Exception as exc:  # openai/httpx 网络与 API 错误统一包装
            raise ImageGenError(f"图像生成请求失败：{_friendly_error(exc)}") from exc
        return self._save_results(result, output_path=output_path, prefix="gen")

    def edit(
        self,
        prompt: str,
        image: str,
        *,
        n: int | None = None,
        size: str | None = None,
        quality: str | None = None,
        output_format: str | None = None,
        output_path: str | None = None,
    ) -> str:
        """编辑（或基于参考图生成）已有本地图片，保存后返回结果文本。"""

        prompt = (prompt or "").strip()
        if not prompt:
            raise ImageGenError("编辑图片需要提供 prompt。")
        image_path = Path(image).expanduser()
        if not image_path.is_file():
            raise ImageGenError(f"图片文件不存在：{image_path}")
        conf = self._configuration
        self._ensure_ready()
        kwargs = self._request_kwargs(
            n=n,
            size=size,
            quality=quality,
            output_format=output_format,
        )
        kwargs["prompt"] = prompt
        try:
            with image_path.open("rb") as image_file:
                result = self._client().images.edit(
                    model=conf.model,
                    image=image_file,
                    **kwargs,
                )
        except Exception as exc:  # openai/httpx 网络与 API 错误统一包装
            raise ImageGenError(f"图像编辑请求失败：{_friendly_error(exc)}") from exc
        return self._save_results(result, output_path=output_path, prefix="edit")

    def _ensure_ready(self) -> None:
        """校验配置可用性，缺失时给出前往设置面板的明确指引。"""

        conf = self._configuration
        if not conf.enabled:
            raise ImageGenError(
                "图像生成未启用：请在 TUI 设置面板（/settings → 图像生成）中启用并配置。"
            )
        api_key = conf.resolve_api_key()
        if not api_key:
            raise ImageGenError(
                "缺少 API Key：请在设置面板填写 image_gen.api_key，"
                f"或设置环境变量 {conf.api_key_env}。"
            )

    def _request_kwargs(
        self,
        *,
        n: int | None,
        size: str | None,
        quality: str | None,
        output_format: str | None,
    ) -> dict[str, Any]:
        """合并配置默认值与本次调用覆盖值，构造请求参数。"""

        conf = self._configuration
        return {
            "n": n if n is not None else conf.n,
            "size": size or conf.size,
            "quality": quality or conf.quality,
            "output_format": output_format or conf.output_format,
            "response_format": "b64_json",
        }

    def _client(self) -> Any:
        """构造（或按测试注入）openai 客户端。"""

        conf = self._configuration
        if self._client_factory is not None:
            return self._client_factory(
                api_key=conf.resolve_api_key(),
                base_url=conf.base_url,
                timeout=conf.timeout_seconds,
            )
        import httpx
        from openai import OpenAI

        # 与项目其他 OpenAI 调用保持一致（见 llm/providers/openai_common.py）：
        # OpenAI SDK 默认 trust_env=True，会在 Windows 上读取系统代理注册表，
        # 对可直连的中转站会导致 TLS 握手失败（EOF occurred in violation of protocol），
        # 因此显式禁用系统代理，直连目标服务。
        return OpenAI(
            api_key=conf.resolve_api_key(),
            base_url=conf.base_url,
            timeout=conf.timeout_seconds,
            http_client=httpx.Client(trust_env=False, follow_redirects=True),
        )

    def _save_results(self, result: Any, *, output_path: str | None, prefix: str) -> str:
        """把返回的 base64/URL 图片数据落盘，生成结果文本。"""

        data = list(getattr(result, "data", []) or [])
        if not data:
            raise ImageGenError("接口未返回任何图片数据。")
        fmt = str(getattr(data[0], "output_format", "") or self._configuration.output_format)
        extension = _extension_for_format(fmt)
        lines: list[str] = []
        for index, item in enumerate(data):
            target = _resolve_target_path(
                output_path,
                default_dir=DEFAULT_OUTPUT_DIR,
                prefix=prefix,
                index=index,
                count=len(data),
                extension=extension,
            )
            raw = getattr(item, "b64_json", None)
            if raw:
                target.write_bytes(base64.b64decode(raw))
            else:
                url = getattr(item, "url", None)
                if not url:
                    raise ImageGenError(f"第 {index + 1} 张图片既无 base64 也无 URL。")
                _download(url, target, timeout=self._configuration.timeout_seconds)
            lines.append(f"{index + 1}. {target}（{target.stat().st_size} 字节）")
        header = f"已生成 {len(data)} 张图片（模型 {self._configuration.model}）："
        return "\n".join([header, *lines])


def _optional_int(value: Any) -> int | None:
    """参数里的整数可能是 int 或数字字符串；非法值忽略（回退默认）。"""

    if value in (None, ""):
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _optional_str(value: Any) -> str | None:
    """空字符串参数视为未提供。"""

    if value in (None, ""):
        return None
    return str(value)


def _friendly_error(exc: Exception) -> str:
    """把 openai/httpx 异常压缩成一行可读描述。"""

    message = str(exc).strip()
    if not message:
        return type(exc).__name__
    return " ".join(message.split())[:300]


def _extension_for_format(fmt: str) -> str:
    fmt = (fmt or "").strip().lower()
    if fmt == "jpeg":
        return "jpg"
    if fmt in {"png", "webp"}:
        return fmt
    return _DEFAULT_OUTPUT_FORMAT


def _resolve_target_path(
    output_path: str | None,
    *,
    default_dir: Path,
    prefix: str,
    index: int,
    count: int,
    extension: str,
) -> Path:
    """解析保存路径：无 path 时用时间戳命名存默认目录；path 是目录或文件均可。"""

    stamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    if output_path:
        candidate = Path(output_path).expanduser()
        if candidate.suffix:  # 带扩展名视为完整文件名
            target = candidate if index == 0 else _numbered_sibling(candidate, index)
        else:
            target = candidate / f"image_{stamp}_{prefix}_{index + 1}.{extension}"
    else:
        target = default_dir / f"image_{stamp}_{prefix}_{index + 1}.{extension}"
    target.parent.mkdir(parents=True, exist_ok=True)
    return target


def _numbered_sibling(path: Path, index: int) -> Path:
    """多张图片共用同一文件名时追加序号（name_2.ext）。"""

    return path.with_name(f"{path.stem}_{index + 1}{path.suffix}")


def _download(url: str, target: Path, *, timeout: int) -> None:
    """接口返回 URL 形式时下载图片（复用 httpx）。"""

    try:
        import httpx

        # trust_env=False：与 _client() 一致，避免 Windows 系统代理干扰图片下载
        response = httpx.get(
            url, timeout=timeout, follow_redirects=True, trust_env=False
        )
        response.raise_for_status()
        target.write_bytes(response.content)
    except Exception as exc:
        raise ImageGenError(f"下载图片失败：{_friendly_error(exc)}") from exc


__all__ = ["DEFAULT_OUTPUT_DIR", "ImageGenError", "ImageGenerator"]
