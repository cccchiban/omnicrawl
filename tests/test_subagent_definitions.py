from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from omnicrawl.agent.subagents.definitions import (
    AgentDefinitionError,
    AgentDefinitionRegistry,
    parse_agent_definition,
)


def _definition_text(name: str, description: str, body: str, **fields) -> str:
    rows = ["---", f"name: {name}", f"description: {description}"]
    for key, value in fields.items():
        if isinstance(value, list):
            rows.append(f"{key}:")
            rows.extend(f"  - {item}" for item in value)
        elif isinstance(value, bool):
            rows.append(f"{key}: {'true' if value else 'false'}")
        else:
            rows.append(f"{key}: {value}")
    rows.extend(["---", "", body, ""])
    return "\n".join(rows)


class AgentDefinitionParsingTest(unittest.TestCase):
    def test_parses_yaml_lists_and_role_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "reviewer.md"
            path.write_text(
                _definition_text(
                    "security-reviewer",
                    "只读审查",
                    "只读取并返回证据。",
                    tools=["read_file", "search_text"],
                    disallowedTools=["write_file", "subagent"],
                    model="inherit",
                    permissionMode="delegated-read-only",
                    background=False,
                    isolation="shared",
                    skills=[],
                    mcpServers=[],
                ),
                encoding="utf-8",
            )

            definition = parse_agent_definition(path, source="project")

        self.assertEqual(definition.name, "security-reviewer")
        self.assertEqual(definition.tools, ("read_file", "search_text"))
        self.assertEqual(definition.disallowed_tools, ("write_file", "subagent"))
        self.assertEqual(definition.system_prompt, "只读取并返回证据。")
        self.assertEqual(definition.source, "project")

    def test_ignores_removed_execution_budget_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "reviewer.md"
            path.write_text(
                _definition_text(
                    "security-reviewer",
                    "只读审查",
                    "只读取并返回证据。",
                    maxTurns=12,
                ),
                encoding="utf-8",
            )

            definition = parse_agent_definition(path, source="project")

        self.assertEqual(definition.name, "security-reviewer")

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            oversized = root / "oversized.md"
            oversized.write_text("x" * 300_000, encoding="utf-8")
            with self.assertRaisesRegex(AgentDefinitionError, "大小"):
                parse_agent_definition(oversized, source="project")

            long_body = root / "long-body.md"
            long_body.write_text(
                _definition_text("long-body", "long", "x" * 70_000),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AgentDefinitionError, "正文"):
                parse_agent_definition(long_body, source="project")

    def test_rejects_oversized_list_or_list_item(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            too_many = root / "too-many.md"
            too_many.write_text(
                _definition_text(
                    "too-many",
                    "too many",
                    "body",
                    tools=[f"tool-{index}" for index in range(65)],
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AgentDefinitionError, "最多"):
                parse_agent_definition(too_many, source="project")

            too_long = root / "too-long.md"
            too_long.write_text(
                _definition_text(
                    "too-long",
                    "too long",
                    "body",
                    tools=["x" * 129],
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AgentDefinitionError, "单项"):
                parse_agent_definition(too_long, source="project")

    def test_rejects_missing_frontmatter_invalid_yaml_and_unknown_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            missing = root / "missing.md"
            missing.write_text("没有 frontmatter", encoding="utf-8")
            with self.assertRaisesRegex(AgentDefinitionError, "frontmatter"):
                parse_agent_definition(missing, source="project")

            invalid = root / "invalid.md"
            invalid.write_text("---\nname: [\n---\nbody", encoding="utf-8")
            with self.assertRaisesRegex(AgentDefinitionError, "YAML"):
                parse_agent_definition(invalid, source="project")

            unknown = root / "unknown.md"
            unknown.write_text(
                _definition_text("demo", "demo", "body", arbitraryPermission="write-all"),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AgentDefinitionError, "未知字段"):
                parse_agent_definition(unknown, source="project")


class AgentDefinitionRegistryTest(unittest.TestCase):
    def test_priority_is_project_compat_user_builtin_plugin(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            workspace = root / "workspace"
            home = root / "home"
            builtin = root / "builtin"
            plugin_file = root / "plugin" / "agents" / "same.md"
            project_file = workspace / ".omnicrawl" / "agents" / "same.md"
            compat_file = workspace / ".agents" / "agents" / "same.md"
            user_file = home / ".omnicrawl" / "agents" / "same.md"
            builtin_file = builtin / "same.md"
            for path, body in (
                (plugin_file, "plugin"),
                (builtin_file, "builtin"),
                (user_file, "user"),
                (compat_file, "compat"),
                (project_file, "project"),
            ):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(_definition_text("same", body, body), encoding="utf-8")

            registry = AgentDefinitionRegistry(
                builtin_directory=builtin,
                home_directory=home,
            )
            registry.discover(
                workspace,
                plugin_definitions=[("@demo/plugin", plugin_file)],
            )

        winner = registry.get("same")
        self.assertIsNotNone(winner)
        self.assertEqual(winner.system_prompt, "project")
        self.assertEqual(winner.source, "project")
        collisions = [item for item in registry.diagnostics if item.kind == "collision"]
        self.assertEqual(len(collisions), 4)
        self.assertTrue(all(item.winner_path == str(project_file.resolve()) for item in collisions))

    def test_project_definition_symlink_cannot_escape_source_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            workspace = root / "workspace"
            definitions = workspace / ".omnicrawl" / "agents"
            definitions.mkdir(parents=True)
            outside = root / "outside.md"
            outside.write_text(
                _definition_text("outside", "outside", "secret"),
                encoding="utf-8",
            )
            link = definitions / "outside.md"
            try:
                link.symlink_to(outside)
            except OSError as exc:
                self.skipTest(f"当前环境不能创建文件符号链接：{exc}")
            registry = AgentDefinitionRegistry(
                builtin_directory=root / "no-builtins",
                home_directory=root / "home",
            )

            registry.discover(workspace)

        self.assertIsNone(registry.get("outside"))
        self.assertTrue(
            any("来源目录" in item.message for item in registry.diagnostics),
            registry.diagnostics,
        )

    def test_invalid_definition_is_diagnostic_and_does_not_block_valid_files(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            definitions = workspace / ".omnicrawl" / "agents"
            definitions.mkdir(parents=True)
            (definitions / "bad.md").write_text("not frontmatter", encoding="utf-8")
            (definitions / "good.md").write_text(
                _definition_text("good", "good", "valid"),
                encoding="utf-8",
            )
            registry = AgentDefinitionRegistry(
                builtin_directory=workspace / "no-builtins",
                home_directory=workspace / "home",
            )

            registry.discover(workspace)

        self.assertEqual(registry.get("good").system_prompt, "valid")
        self.assertTrue(any(item.kind == "invalid" for item in registry.diagnostics))

    def test_default_registry_loads_packaged_explore_and_plan(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            registry = AgentDefinitionRegistry(home_directory=Path(temp_dir) / "home")
            registry.discover(Path(temp_dir))

        names = {item.name for item in registry.list_all()}
        self.assertIn("explore", names)
        self.assertIn("plan", names)
        self.assertEqual(registry.get("explore").source, "builtin")


if __name__ == "__main__":
    unittest.main()
