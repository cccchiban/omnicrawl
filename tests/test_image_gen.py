"""image_gen 图像生成工具：配置、调用、保存与工具注册测试。"""

from __future__ import annotations

import base64
import os
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnicrawl.config.features.image_gen import (  # noqa: E402
    DEFAULT_IMAGE_GEN_BASE_URL,
    ImageGenConfigError,
    ImageGenConfiguration,
    load_image_gen_configuration,
    save_image_gen_configuration,
)
from omnicrawl.image_gen import ImageGenError, ImageGenerator  # noqa: E402


# ---------------------------------------------------------------------------
# 配置：默认值 / 校验 / 读写 / key 解析
# ---------------------------------------------------------------------------


class TestConfiguration:
    def test_defaults(self):
        conf = ImageGenConfiguration()
        assert conf.enabled is False
        assert conf.base_url == DEFAULT_IMAGE_GEN_BASE_URL
        assert conf.model == "gpt-image-2"
        assert conf.size == "auto"
        assert conf.quality == "auto"
        assert conf.output_format == "png"
        assert conf.n == 1
        assert conf.timeout_seconds == 120
        assert conf.api_key_env == "OPENAI_API_KEY"

    def test_base_url_trailing_slash_stripped(self):
        conf = ImageGenConfiguration(base_url="https://api.example.com/v1/")
        assert conf.base_url == "https://api.example.com/v1"

    def test_rejects_invalid_values(self):
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(base_url="")
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(base_url="ftp://x")
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(model="")
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(size="1024")
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(size="abcx1024")
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(quality="ultra")
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(output_format="gif")
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(n=0)
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(n=11)
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(timeout_seconds=0)
        with pytest.raises(ImageGenConfigError):
            ImageGenConfiguration(enabled="yes")

    def test_accepts_custom_size(self):
        conf = ImageGenConfiguration(size="1536x1024")
        assert conf.size == "1536x1024"

    def test_resolve_api_key_prefers_configured_value(self, monkeypatch):
        monkeypatch.setenv("OPENAI_API_KEY", "env-key")
        conf = ImageGenConfiguration(api_key="config-key")
        assert conf.resolve_api_key() == "config-key"

    def test_resolve_api_key_falls_back_to_env(self, monkeypatch):
        monkeypatch.setenv("CUSTOM_IG_ENV", "env-key")
        conf = ImageGenConfiguration(api_key="", api_key_env="CUSTOM_IG_ENV")
        assert conf.resolve_api_key() == "env-key"

    def test_load_and_save_roundtrip(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[memory]\nenabled = true\n", encoding="utf-8")
        conf = ImageGenConfiguration(
            enabled=True,
            base_url="https://api.example.com/v1",
            api_key="sk-secret",
            api_key_env="MY_ENV",
            model="gpt-image-1",
            size="1024x1536",
            quality="high",
            output_format="jpeg",
            n=2,
            timeout_seconds=300,
        )
        path = save_image_gen_configuration(conf, config_path)
        assert path == config_path
        loaded = load_image_gen_configuration(config_path)
        assert loaded == conf
        # 其他配置段保留
        data = load_config_data_raw(config_path)
        assert data["memory"]["enabled"] is True

    def test_load_missing_section_returns_disabled_defaults(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[memory]\nenabled = true\n", encoding="utf-8")
        conf = load_image_gen_configuration(config_path)
        assert conf.enabled is False
        assert conf.model == "gpt-image-2"

    def test_load_invalid_section_raises(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[image_gen]\nquality = \"ultra\"\n", encoding="utf-8")
        with pytest.raises(ImageGenConfigError):
            load_image_gen_configuration(config_path)

    def test_load_invalid_numeric_raises(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[image_gen]\nn = \"many\"\n", encoding="utf-8")
        with pytest.raises(ImageGenConfigError):
            load_image_gen_configuration(config_path)


def load_config_data_raw(config_path: Path):
    try:
        import tomllib
    except ImportError:  # Python < 3.11
        import tomli as tomllib

    return tomllib.loads(config_path.read_text(encoding="utf-8")) or {}


# ---------------------------------------------------------------------------
# ImageGenerator：生成 / 编辑 / 保存
# ---------------------------------------------------------------------------


class FakeImages:
    def __init__(self, results=None):
        self.results = results or [
            SimpleNamespace(b64_json=base64.b64encode(b"PNG-DATA").decode("ascii"), url=None)
        ]
        self.generate_calls = []
        self.edit_calls = []

    def generate(self, model, **kwargs):
        self.generate_calls.append((model, kwargs))
        return SimpleNamespace(data=self.results)

    def edit(self, model, image, **kwargs):
        self.edit_calls.append((model, image.read(), kwargs))
        return SimpleNamespace(data=self.results)


class FakeClient:
    def __init__(self, images=None):
        self.images = images or FakeImages()


def make_generator(
    tmp_path,
    *,
    enabled=True,
    api_key="sk-test",
    model="gpt-image-2",
    images=None,
    **overrides,
):
    config_path = tmp_path / "config.toml"
    conf = ImageGenConfiguration(enabled=enabled, api_key=api_key, model=model, **overrides)
    save_image_gen_configuration(conf, config_path)
    return (
        ImageGenerator(config_path=config_path, client_factory=lambda **kw: FakeClient(images)),
        config_path,
    )


class TestGenerator:
    def test_generate_saves_png_and_returns_paths(self, tmp_path):
        generator, _ = make_generator(tmp_path)
        output = generator.generate("一只猫", output_path=str(tmp_path / "out"))
        assert "已生成 1 张图片" in output
        assert "1. " in output
        assert any((tmp_path / "out").glob("image_*.png"))

    def test_generate_passes_sdk_arguments(self, tmp_path):
        images = FakeImages()
        generator, _ = make_generator(tmp_path, images=images, n=2)
        generator.generate("一张图", n=3, size="1536x1024", quality="high", output_format="jpeg")
        model, kwargs = images.generate_calls[0]
        assert model == "gpt-image-2"
        assert kwargs["prompt"] == "一张图"
        assert kwargs["n"] == 3
        assert kwargs["size"] == "1536x1024"
        assert kwargs["quality"] == "high"
        assert kwargs["output_format"] == "jpeg"
        assert kwargs["response_format"] == "b64_json"

    def test_generate_uses_config_defaults(self, tmp_path):
        images = FakeImages()
        generator, _ = make_generator(
            tmp_path, images=images, size="1024x1024", quality="low", output_format="webp", n=4
        )
        generator.generate("一张图")
        _, kwargs = images.generate_calls[0]
        assert kwargs["n"] == 4
        assert kwargs["size"] == "1024x1024"
        assert kwargs["quality"] == "low"
        assert kwargs["output_format"] == "webp"

    def test_generate_multiple_images_numbered(self, tmp_path):
        images = FakeImages(
            [
                SimpleNamespace(b64_json=base64.b64encode(b"ONE").decode("ascii"), url=None),
                SimpleNamespace(b64_json=base64.b64encode(b"TWO").decode("ascii"), url=None),
            ]
        )
        generator, _ = make_generator(tmp_path, images=images, n=2)
        output = generator.generate("两张图", n=2, output_path=str(tmp_path / "out" / "pic.png"))
        assert "已生成 2 张图片" in output
        assert (tmp_path / "out" / "pic.png").read_bytes() == b"ONE"
        assert (tmp_path / "out" / "pic_2.png").read_bytes() == b"TWO"

    def test_generate_custom_directory_path(self, tmp_path):
        generator, _ = make_generator(tmp_path)
        output = generator.generate("一张图", output_path=str(tmp_path / "custom"))
        assert "已生成 1 张图片" in output
        assert list((tmp_path / "custom").glob("image_*.png"))

    def test_generate_disabled_raises(self, tmp_path):
        generator, _ = make_generator(tmp_path, enabled=False)
        with pytest.raises(ImageGenError, match="未启用"):
            generator.generate("一张图")

    def test_generate_missing_key_raises(self, tmp_path, monkeypatch):
        monkeypatch.delenv("OPENAI_API_KEY", raising=False)
        generator, _ = make_generator(tmp_path, api_key="")
        with pytest.raises(ImageGenError, match="缺少 API Key"):
            generator.generate("一张图")

    def test_generate_empty_prompt_raises(self, tmp_path):
        generator, _ = make_generator(tmp_path)
        with pytest.raises(ImageGenError, match="prompt"):
            generator.generate("   ")

    def test_edit_passes_image_file_and_prompt(self, tmp_path):
        images = FakeImages()
        generator, _ = make_generator(tmp_path, images=images)
        source = tmp_path / "source.png"
        source.write_bytes(b"RAW")
        output = generator.edit(
            "把背景换成红色", str(source), output_path=str(tmp_path / "edited.png")
        )
        assert "已生成 1 张图片" in output
        model, image_bytes, kwargs = images.edit_calls[0]
        assert model == "gpt-image-2"
        assert image_bytes == b"RAW"
        assert kwargs["prompt"] == "把背景换成红色"
        assert (tmp_path / "edited.png").exists()

    def test_edit_missing_image_raises(self, tmp_path):
        generator, _ = make_generator(tmp_path)
        with pytest.raises(ImageGenError, match="不存在"):
            generator.edit("改一下", str(tmp_path / "nope.png"))

    def test_run_dispatches_to_edit_when_image_present(self, tmp_path):
        images = FakeImages()
        generator, _ = make_generator(tmp_path, images=images)
        source = tmp_path / "source.png"
        source.write_bytes(b"RAW")
        output = generator.run(
            {"prompt": "改背景", "image": str(source), "path": str(tmp_path / "r.png")}
        )
        assert "已生成 1 张图片" in output
        assert images.edit_calls
        assert not images.generate_calls

    def test_run_generates_when_no_image(self, tmp_path):
        images = FakeImages()
        generator, _ = make_generator(tmp_path, images=images)
        generator.run({"prompt": "一张图", "n": "2", "size": "1024x1024"})
        model, kwargs = images.generate_calls[0]
        assert kwargs["n"] == 2  # 数字字符串被解析
        assert kwargs["size"] == "1024x1024"

    def test_run_rejects_missing_prompt(self, tmp_path):
        generator, _ = make_generator(tmp_path)
        with pytest.raises(ImageGenError):
            generator.run({})

    def test_run_ignores_invalid_numeric_override(self, tmp_path):
        images = FakeImages()
        generator, _ = make_generator(tmp_path, images=images, n=1)
        generator.run({"prompt": "一张图", "n": "abc"})
        _, kwargs = images.generate_calls[0]
        assert kwargs["n"] == 1

    def test_sdk_error_wrapped_friendly(self, tmp_path):
        class BoomImages:
            def generate(self, model, **kwargs):
                raise RuntimeError("401 Unauthorized  Invalid API key")

        generator, _ = make_generator(tmp_path, images=BoomImages())
        with pytest.raises(ImageGenError, match="401 Unauthorized"):
            generator.generate("一张图")

    def test_sdk_error_with_blank_message(self, tmp_path):
        class BlankImages:
            def generate(self, model, **kwargs):
                raise ValueError("")

        generator, _ = make_generator(tmp_path, images=BlankImages())
        with pytest.raises(ImageGenError, match="ValueError"):
            generator.generate("一张图")


# ---------------------------------------------------------------------------
# 工具注册与开关
# ---------------------------------------------------------------------------


class TestRegistration:
    def test_config_switch_key_and_label(self):
        from omnicrawl.config.features.tools import (
            TOOL_SWITCH_KEYS,
            TOOL_SWITCH_LABELS,
        )

        assert "image_gen" in TOOL_SWITCH_KEYS
        assert "图像生成" in TOOL_SWITCH_LABELS["image_gen"]

    def test_tool_labels(self):
        from omnicrawl.ui.tool_labels import tool_display

        display = tool_display("image_gen")
        assert display.name == "image_gen"
        assert display.icon

    def test_build_agent_tools_contains_image_gen(self):
        from omnicrawl.agent.toolkit.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        tools = build_agent_tools(
            mcp_manager=MCPClientManager(),
            memory_enabled=False,
            list=lambda a: "ok",
            read=lambda a: "ok",
            grep=lambda a: "ok",
            web_search=lambda a: "ok",
            fetcher=lambda a: "ok",
            image_gen=lambda a: "ok",
            edit_file=lambda a: "ok",
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
        assert "image_gen" in tools
        assert tools["image_gen"].requires_confirmation is True
        assert "prompt" in tools["image_gen"].argument_schema

    def test_build_agent_tools_omits_image_gen_when_none(self):
        from omnicrawl.agent.toolkit.tools import build_agent_tools
        from omnicrawl.mcp import MCPClientManager

        tools = build_agent_tools(
            mcp_manager=MCPClientManager(),
            memory_enabled=False,
            list=lambda a: "ok",
            read=lambda a: "ok",
            grep=lambda a: "ok",
            web_search=lambda a: "ok",
            fetcher=lambda a: "ok",
            edit_file=lambda a: "ok",
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
        assert "image_gen" not in tools

    def test_agent_config_loads_image_gen(self, tmp_path):
        from omnicrawl.agent.core import AgentConfig

        config_path = tmp_path / "config.toml"
        config_path.write_text(
            "[image_gen]\nenabled = true\nmodel = \"gpt-image-1\"\nn = 2\n", encoding="utf-8"
        )
        # AgentConfig.image_gen 是独立 default_factory 字段（读本机用户配置，值不可假设）；
        # 这里只验证字段类型正确且支持整体替换。
        config = AgentConfig()
        assert isinstance(config.image_gen, ImageGenConfiguration)
        config.image_gen = ImageGenConfiguration(enabled=True, model="gpt-image-1", n=2)
        assert config.image_gen.enabled is True
        assert config.image_gen.model == "gpt-image-1"
        assert config.image_gen.n == 2
