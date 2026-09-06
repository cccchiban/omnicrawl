"""内置工具开关配置：默认值、覆盖、写回与 AgentConfig 集成回归。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

try:
    import tomllib
except ImportError:  # Python < 3.11
    import tomli as tomllib
from omnicrawl.config.core.runtime import dump_toml_text

from omnicrawl.agent import AgentConfig, AgentError
from omnicrawl.config.features.context_compaction import ContextCompactionConfig
from omnicrawl.config.features.tools import (
    TOOL_SWITCH_DEFAULTS,
    TOOL_SWITCH_KEYS,
    ToolSwitchConfigError,
    load_disabled_tools,
    load_tool_switches,
    save_tool_switch,
    validate_tool_switch_name,
)


class ToolSwitchConfigTest(unittest.TestCase):
    def test_default_switches_keep_only_powershell_disabled(self) -> None:
        # 默认除 powershell 关闭外，其余内置工具全部启用。
        self.assertEqual(TOOL_SWITCH_DEFAULTS["powershell"], False)
        for name in TOOL_SWITCH_KEYS:
            if name == "powershell":
                continue
            self.assertTrue(
                TOOL_SWITCH_DEFAULTS[name],
                f"工具 {name} 应默认启用",
            )

    def test_load_tool_switches_without_config_returns_defaults(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            switches = load_tool_switches(Path(temp_dir) / "missing.toml")

        self.assertEqual(switches["powershell"], False)
        self.assertEqual(switches["bash"], True)
        self.assertEqual(switches["windows_screenshot"], True)
        self.assertEqual(switches["project_memory_write"], True)

    def test_load_disabled_tools_default_only_powershell(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            disabled = load_disabled_tools(Path(temp_dir) / "missing.toml")

        self.assertEqual(disabled, frozenset({"powershell"}))

    def test_yaml_overrides_power_shell_switch(self) -> None:
        payload = {
            "tools": {
                "powershell": True,
                "bash": False,
                "windows_screenshot": False,
            }
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            yaml_path = Path(temp_dir) / "config.toml"
            yaml_path.write_text(dump_toml_text(payload), encoding="utf-8")
            switches = load_tool_switches(yaml_path)
            disabled = load_disabled_tools(yaml_path)

        self.assertEqual(switches["powershell"], True)
        self.assertEqual(switches["bash"], False)
        self.assertEqual(switches["windows_screenshot"], False)
        self.assertEqual(switches["read"], True)
        self.assertEqual(disabled, frozenset({"bash", "windows_screenshot"}))

    def test_unknown_tool_name_raises(self) -> None:
        payload = {"tools": {"not_a_tool": True}}
        with tempfile.TemporaryDirectory() as temp_dir:
            yaml_path = Path(temp_dir) / "config.toml"
            yaml_path.write_text(dump_toml_text(payload), encoding="utf-8")
            with self.assertRaises(ToolSwitchConfigError):
                load_tool_switches(yaml_path)

    def test_non_boolean_value_raises(self) -> None:
        payload = {"tools": {"bash": "yes"}}
        with tempfile.TemporaryDirectory() as temp_dir:
            yaml_path = Path(temp_dir) / "config.toml"
            yaml_path.write_text(dump_toml_text(payload), encoding="utf-8")
            with self.assertRaises(ToolSwitchConfigError):
                load_tool_switches(yaml_path)

    def test_validate_tool_switch_name_normalizes_whitespace(self) -> None:
        self.assertEqual(validate_tool_switch_name("  bash  "), "bash")
        with self.assertRaises(ToolSwitchConfigError):
            validate_tool_switch_name("unknown_tool")

    def test_save_tool_switch_preserves_other_sections(self) -> None:
        payload = {
            "version": 2,
            "memory": {"enabled": True},
            "tools": {"powershell": False},
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            yaml_path = Path(temp_dir) / "config.toml"
            yaml_path.write_text(dump_toml_text(payload), encoding="utf-8")

            saved = save_tool_switch("powershell", True, yaml_path)
            written = tomllib.loads(saved.read_text(encoding="utf-8"))

        self.assertEqual(written["version"], 2)
        self.assertEqual(written["memory"]["enabled"], True)
        self.assertEqual(written["tools"]["powershell"], True)

    def test_save_tool_switch_unknown_name_raises(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            yaml_path = Path(temp_dir) / "config.toml"
            yaml_path.write_text(dump_toml_text({}), encoding="utf-8")
            with self.assertRaises(ToolSwitchConfigError):
                save_tool_switch("unknown_tool", True, yaml_path)


class AgentConfigToolSwitchIntegrationTest(unittest.TestCase):
    def test_agent_config_default_disabled_tools_only_powershell(self) -> None:
        config = AgentConfig(
            llm=SimpleNamespace(api_key="test-key"),
            workspace_root=Path(tempfile.gettempdir()),
            memory_enabled=False,
                    )
        self.assertEqual(config.disabled_tools, frozenset({"powershell"}))

    def test_agent_config_rejects_single_string(self) -> None:
        with self.assertRaisesRegex(AgentError, "字符串集合"):
            AgentConfig(
                llm=SimpleNamespace(api_key="test-key"),
                workspace_root=Path(tempfile.gettempdir()),
                memory_enabled=False,
                        disabled_tools="powershell",
            )

    def test_agent_config_rejects_empty_element(self) -> None:
        with self.assertRaisesRegex(AgentError, "非空字符串"):
            AgentConfig(
                llm=SimpleNamespace(api_key="test-key"),
                workspace_root=Path(tempfile.gettempdir()),
                memory_enabled=False,
                        disabled_tools=frozenset({""}),
            )

    def test_agent_config_accepts_custom_disabled_set(self) -> None:
        config = AgentConfig(
            llm=SimpleNamespace(api_key="test-key"),
            workspace_root=Path(tempfile.gettempdir()),
            memory_enabled=False,
                    disabled_tools=frozenset({"bash", "windows_screenshot"}),
        )
        self.assertEqual(config.disabled_tools, frozenset({"bash", "windows_screenshot"}))


if __name__ == "__main__":
    unittest.main()
