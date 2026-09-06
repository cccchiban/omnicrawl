"""tts 语音合成工具：配置、校验、读写与工具注册测试。"""

from __future__ import annotations

import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnicrawl.agent.controllers.tools.implementations import (  # noqa: E402
    ToolImplementationsMixin,
)
from omnicrawl.config.features.tts import (  # noqa: E402
    DEFAULT_TTS_VOICE,
    TTSConfigError,
    TTSConfiguration,
    load_tts_configuration,
    save_tts_configuration,
)


# ---------------------------------------------------------------------------
# 配置：默认值 / 校验 / 读写
# ---------------------------------------------------------------------------


class TestConfiguration:
    def test_defaults(self):
        conf = TTSConfiguration()
        assert conf.enabled is False
        assert conf.model_dir == ""
        assert conf.voice == DEFAULT_TTS_VOICE
        assert conf.auto_play is True
        assert conf.thread_count == 4
        assert conf.device == "auto"
        assert conf.streaming is True
        assert conf.output_dir == ".omnicrawl/.agent_tmp/tts"

    def test_rejects_invalid_values(self):
        with pytest.raises(TTSConfigError):
            TTSConfiguration(enabled="yes")
        with pytest.raises(TTSConfigError):
            TTSConfiguration(auto_play="yes")
        with pytest.raises(TTSConfigError):
            TTSConfiguration(streaming="yes")
        with pytest.raises(TTSConfigError):
            TTSConfiguration(voice="")
        with pytest.raises(TTSConfigError):
            TTSConfiguration(thread_count=3)
        with pytest.raises(TTSConfigError):
            TTSConfiguration(thread_count=0)
        with pytest.raises(TTSConfigError):
            TTSConfiguration(output_dir="")
        with pytest.raises(TTSConfigError):
            TTSConfiguration(device="gpu")

    def test_accepts_custom_values(self):
        conf = TTSConfiguration(
            enabled=True,
            model_dir="D:/models/tts",
            voice="Xiaoyu",
            auto_play=False,
            thread_count=8,
            device="cuda",
            streaming=True,
            output_dir="audio",
        )
        assert conf.enabled is True
        assert conf.voice == "Xiaoyu"
        assert conf.thread_count == 8
        assert conf.device == "cuda"
        assert conf.streaming is True
        assert conf.output_dir == "audio"

    def test_resolved_model_dir_prefers_configured(self):
        conf = TTSConfiguration(model_dir="D:/my/tts/models")
        assert str(conf.resolved_model_dir()).replace("\\", "/").endswith("my/tts/models")


class TestLoadSave:
    def test_load_and_save_roundtrip(self, tmp_path):
        from omnicrawl.config.core.runtime import load_config_data as _raw

        config_path = tmp_path / "config.toml"
        config_path.write_text("[memory]\nenabled = true\n", encoding="utf-8")
        conf = TTSConfiguration(
            enabled=True,
            model_dir="D:/models/tts",
            voice="Xiaoyu",
            auto_play=False,
            thread_count=8,
            device="cuda",
            streaming=True,
            output_dir="audio",
        )
        path = save_tts_configuration(conf, config_path)
        assert path == config_path
        loaded = load_tts_configuration(config_path)
        assert loaded == conf
        # 其他配置段保留
        data = _raw(config_path)
        assert data["memory"]["enabled"] is True

    def test_load_missing_section_returns_disabled_defaults(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[memory]\nenabled = true\n", encoding="utf-8")
        conf = load_tts_configuration(config_path)
        assert conf.enabled is False
        assert conf.voice == DEFAULT_TTS_VOICE
        assert conf.auto_play is True
        assert conf.device == "auto"
        assert conf.streaming is True

    def test_load_explicit_false_streaming_is_respected(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[tts]\nstreaming = false\n", encoding="utf-8")
        conf = load_tts_configuration(config_path)
        assert conf.streaming is False

    def test_load_invalid_section_raises(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[tts]\nthread_count = \"many\"\n", encoding="utf-8")
        with pytest.raises(TTSConfigError):
            load_tts_configuration(config_path)

    def test_load_empty_voice_falls_back_to_default(self, tmp_path):
        config_path = tmp_path / "config.toml"
        config_path.write_text("[tts]\nvoice = \"\"\n", encoding="utf-8")
        conf = load_tts_configuration(config_path)
        assert conf.voice == DEFAULT_TTS_VOICE


# ---------------------------------------------------------------------------
# 工具注册与开关
# ---------------------------------------------------------------------------


class TestRegistration:
    def test_config_switch_key_and_label(self):
        from omnicrawl.config.features.tools import (
            TOOL_SWITCH_KEYS,
            TOOL_SWITCH_LABELS,
        )

        assert "tts_synthesize" in TOOL_SWITCH_KEYS
        assert "TTS" in TOOL_SWITCH_LABELS["tts_synthesize"]

    def test_tool_labels(self):
        from omnicrawl.ui.tool_labels import tool_display

        display = tool_display("tts_synthesize")
        assert display.name == "tts_synthesize"
        assert display.icon

    def test_build_agent_tools_contains_tts(self):
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
            tts=lambda a: "ok",
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
        assert "tts_synthesize" in tools
        assert tools["tts_synthesize"].requires_confirmation is True
        assert "text" in tools["tts_synthesize"].argument_schema
        # 音色由设置配置决定，工具面不暴露 voice 参数（模型不可指定音色）
        assert "voice" not in tools["tts_synthesize"].argument_schema

    def test_build_agent_tools_omits_tts_when_none(self):
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
        assert "tts_synthesize" not in tools

    def test_agent_config_loads_tts(self):
        from omnicrawl.agent.core import AgentConfig

        config = AgentConfig()
        assert isinstance(config.tts, TTSConfiguration)
        config.tts = TTSConfiguration(enabled=True, voice="Xiaoyu", auto_play=False)
        assert config.tts.enabled is True
        assert config.tts.voice == "Xiaoyu"


# ---------------------------------------------------------------------------
# TUI 启动独立性：设置面板不能强依赖 TTS 引擎依赖（sentencepiece/numpy）
# ---------------------------------------------------------------------------


class TestUiImportIndependence:
    """TUI 启动不能因 TTS 可选依赖缺失而闪退。"""

    def test_fullscreen_ui_imports_without_sentencepiece(self):
        code = textwrap.dedent(
            """\
            import builtins
            import pathlib
            import sys
            import tempfile

            real_import = builtins.__import__

            def fake_import(name, *args, **kwargs):
                if name == "sentencepiece" or name.startswith("sentencepiece."):
                    raise ModuleNotFoundError(
                        "No module named 'sentencepiece'", name="sentencepiece"
                    )
                return real_import(name, *args, **kwargs)

            builtins.__import__ = fake_import

            # 模拟用户环境缺少 sentencepiece：整个 TUI 仍必须可导入。
            import omnicrawl.ui.fullscreen
            # 包本身轻量可导入，但推理引擎（依赖 numpy/sentencepiece/onnxruntime）
            # 绝不能随 UI 加载。
            assert "omnicrawl.tts.engine" not in sys.modules

            from omnicrawl.ui.fullscreen.screens import TTSSettingsScreen

            # 设置面板降级而不是崩溃：显式空 model_dir → 模型未就绪、音色走兜底。
            cfg = pathlib.Path(tempfile.mkdtemp()) / "config.toml"
            models_dir = pathlib.Path(tempfile.mkdtemp()) / "models"
            # 注意 \\n 是字面转义：嵌套字符串里的真换行会把 dedent 的最小缩进
            # 拉成 0，导致整段代码保留缩进无法执行。
            cfg.write_text(
                "[tts]\\nenabled = false\\n"
                "model_dir = '" + str(models_dir).replace("\\\\", "/") + "'\\n",
                encoding="utf-8",
            )
            screen = TTSSettingsScreen(cfg)
            assert screen._models_ready() is False
            assert "Junhao" in screen._voice_options()
            assert "模型未下载" in screen._model_status()
            print("UI-OK")
            """
        )
        result = subprocess.run(
            [sys.executable, "-c", code],
            capture_output=True,
            text=True,
            encoding="utf-8",
            cwd=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        )
        assert result.returncode == 0, result.stderr
        assert "UI-OK" in result.stdout


# ---------------------------------------------------------------------------
# 引擎辅助与端到端工具调用（需要已下载的 ONNX 模型）
# ---------------------------------------------------------------------------

_ENGINE_MODEL_DIR = Path.home() / ".omnicrawl" / "tts" / "models"
_ENGINE_MODELS_READY = any(
    (_ENGINE_MODEL_DIR / candidate).is_file()
    for candidate in (
        "browser_poc_manifest.json",
        "MOSS-TTS-Nano-100M-ONNX/browser_poc_manifest.json",
    )
)


class TestEngineHelpers:
    def test_models_ready_and_voices(self):
        from omnicrawl.tts import builtin_voice_names, models_ready

        assert models_ready() is _ENGINE_MODELS_READY
        voices = builtin_voice_names()
        if _ENGINE_MODELS_READY:
            assert "Junhao" in voices
            assert len(voices) >= 10
        else:
            assert voices == []


@pytest.mark.skipif(not _ENGINE_MODELS_READY, reason="ONNX 模型未下载（~/.omnicrawl/tts/models）")
class TestToolSynthesize:
    def test_tool_disabled_returns_clear_error(self):
        from omnicrawl.agent.controllers.tools.implementations import ToolImplementationsMixin

        agent = _FakeAgent(TTSConfiguration(enabled=False))
        result = agent._tool_tts_synthesize({"text": "你好"})
        assert result.ok is False
        assert "未启用" in result.output
        # 失败也返回 JSON，明确 ok=false，让模型不再重复调用
        import json

        payload = json.loads(result.output)
        assert payload["ok"] is False
        assert "未启用" in payload["error"]

    def test_tool_synthesizes_audio(self, tmp_path):
        from omnicrawl.agent.controllers.tools.implementations import ToolImplementationsMixin

        agent = _FakeAgent(
            TTSConfiguration(enabled=True, auto_play=False, thread_count=2)
        )
        output_path = str(tmp_path / "speech.wav")
        result = agent._tool_tts_synthesize(
            {"text": "语音合成测试。", "path": output_path}
        )
        assert result.ok is True, result.output
        assert Path(output_path).is_file()
        import json

        summary = json.loads(result.output)
        # 成功结果明确 ok=true，模型可据此停止重复调用
        assert summary["ok"] is True
        assert summary["sample_rate"] == 48000
        assert summary["duration_seconds"] > 0
        # 引擎被缓存复用
        assert agent._tts_engine is not None

    def test_tool_ignores_model_voice_arg_uses_config(self, tmp_path):
        """模型传的 voice 参数被忽略：音色固定使用配置（/settings → TTS）。"""
        from omnicrawl.agent.controllers.tools.implementations import ToolImplementationsMixin

        agent = _FakeAgent(
            TTSConfiguration(enabled=True, auto_play=False, thread_count=2, voice="Xiaoyu")
        )
        output_path = str(tmp_path / "speech.wav")
        # 模型传入不存在的音色 default：必须回退到配置音色 Xiaoyu 而不是报错。
        result = agent._tool_tts_synthesize(
            {"text": "语音合成测试。", "voice": "default", "path": output_path}
        )
        assert result.ok is True, result.output
        import json

        summary = json.loads(result.output)
        assert summary["ok"] is True
        assert summary["voice"] == "Xiaoyu"


class _FakeAgent(ToolImplementationsMixin):
    """仅携带 config 的最小工具宿主，供 _tool_tts_synthesize 单测。"""

    def __init__(self, tts_configuration: TTSConfiguration) -> None:
        self.config = type("Config", (), {"tts": tts_configuration})()
        self.workspace_root = Path.cwd()
