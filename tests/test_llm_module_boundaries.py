from __future__ import annotations

import importlib
import unittest
from pathlib import Path

from omnicrawl.config.models import llm as llm_config
from omnicrawl.config.models import llm_client


class LLMModuleBoundaryTests(unittest.TestCase):
    """锁定 LLM 配置与网络客户端的真实模块边界。"""

    def test_llm_client_is_real_module(self) -> None:
        package_path = Path(llm_config.__file__).resolve().parent
        module = importlib.import_module("omnicrawl.config.models.llm_client")
        self.assertEqual(module.__name__, "omnicrawl.config.models.llm_client")
        self.assertEqual(Path(module.__file__).resolve(), package_path / "llm_client.py")

    def test_config_module_keeps_public_exports(self) -> None:
        for export_name in (
            "LLMConfig",
            "LLMError",
            "OpenAIResponseLLM",
            "load_llm_config",
            "normalize_reasoning_effort",
            "save_reasoning_effort",
        ):
            with self.subTest(export=export_name):
                self.assertTrue(hasattr(llm_config, export_name))

    def test_openai_client_lives_in_client_module(self) -> None:
        self.assertIs(llm_config.OpenAIResponseLLM, llm_client.OpenAIResponseLLM)
        source = Path(llm_config.__file__).read_text(encoding="utf-8")
        self.assertNotIn("class OpenAIResponseLLM", source)
        self.assertNotIn("from openai import OpenAI", source)
        self.assertLess(len(source.splitlines()), 280)

    def test_compat_alias_still_exports_client(self) -> None:
        compat = importlib.import_module("omnicrawl.llm")
        self.assertIs(compat.OpenAIResponseLLM, llm_client.OpenAIResponseLLM)
        self.assertIs(compat.LLMConfig, llm_config.LLMConfig)
