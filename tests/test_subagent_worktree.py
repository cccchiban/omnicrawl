"""Phase 3 worktree / standard SubAgent regression."""
from __future__ import annotations
import json, subprocess, tempfile, unittest
from pathlib import Path
from omnicrawl.agent.subagents.approval import SubAgentApprovalOrigin, subagent_approval_risk_summary
from omnicrawl.agent.subagents.definitions import parse_agent_definition
from omnicrawl.agent.subagents.worktree import WorktreeError, apply_worktree_to_main, cleanup_worktree_session, collect_worktree_artifacts, create_worktree_session, is_git_repository
from omnicrawl.agent.tools import ToolDefinition
from omnicrawl.config.subagents import SubAgentConfig, load_subagent_config

def _run_git(args, cwd):
    c=subprocess.run(["git", *args], cwd=str(cwd), capture_output=True, text=True, check=False)
    if c.returncode!=0: raise RuntimeError(c.stderr or c.stdout or "git failed")

def _init_repo(path):
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    _run_git(["init"], path)
    _run_git(["config","user.email","test@example.com"], path)
    _run_git(["config","user.name","Test"], path)
    (path/"README.md").write_text("hello" + chr(10), encoding="utf-8")
    _run_git(["add","README.md"], path)
    _run_git(["commit","-m","init"], path)

class WorktreeLifecycleTests(unittest.TestCase):
    def test_create_collect_apply_cleanup(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo=Path(tmp)/"repo"; repo.mkdir(); _init_repo(repo)
            self.assertTrue(is_git_repository(repo))
            session=create_worktree_session(workspace_root=repo, task_id="t-worktree-1", worktree_parent=Path(tmp)/"wts")
            self.addCleanup(lambda: cleanup_worktree_session(session, remove_branch=True))
            (session.worktree_path/"feature.txt").write_text("from-worktree" + chr(10), encoding="utf-8")
            artifacts=collect_worktree_artifacts(session)
            self.assertTrue(artifacts.has_changes)
            self.assertIn("feature.txt", artifacts.changed_files)
            self.assertTrue(artifacts.branch_name.startswith("omnicrawl/subagent/"))
            self.assertFalse((repo/"feature.txt").exists())
            message=apply_worktree_to_main(session, strategy="checkout")
            self.assertIn("检出", message)
            self.assertTrue((repo/"feature.txt").exists())
            self.assertEqual((repo/"feature.txt").read_text(encoding="utf-8"), "from-worktree" + chr(10))
    def test_non_git_workspace_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(WorktreeError):
                create_worktree_session(workspace_root=Path(tmp), task_id="no-git")

class StandardApprovalPolicyTests(unittest.TestCase):
    def _tool(self, name):
        return ToolDefinition(name=name, description=name, argument_schema="{}", requires_confirmation=False, run=lambda _a: None)
    def test_standard_write_requires_approval(self):
        self.assertEqual(subagent_approval_risk_summary(self._tool("write_file"), {"path":"a.py","content":"x"}, permission_mode="standard"), "工作区写入")
        self.assertEqual(subagent_approval_risk_summary(self._tool("bash"), {"command":"echo hi"}, permission_mode="standard"), "命令执行")
    def test_read_only_write_does_not_use_standard_rule(self):
        self.assertEqual(subagent_approval_risk_summary(self._tool("write_file"), {"path":"a.py","content":"x"}, permission_mode="delegated-read-only"), "")
    def test_origin_carries_permission_mode(self):
        origin=SubAgentApprovalOrigin(batch_id="b1", task_id="t1", agent_label="general-purpose", description="impl", permission_mode="standard")
        self.assertEqual(origin.as_public_dict()["permission_mode"], "standard")

class GeneralPurposeDefinitionTests(unittest.TestCase):
    def test_builtin_general_purpose_definition(self):
        path=Path(__file__).resolve().parents[1]/"omnicrawl"/"agent"/"subagents"/"builtin"/"general-purpose.md"
        d=parse_agent_definition(path, source="builtin")
        self.assertEqual(d.name,"general-purpose")
        self.assertEqual(d.permission_mode,"standard")
        self.assertEqual(d.isolation,"worktree")
        self.assertIn("write_file", d.tools)
        self.assertIn("replace_text", d.tools)

class ConfigFlagsTests(unittest.TestCase):
    def test_default_flags_closed(self):
        c=SubAgentConfig()
        self.assertFalse(c.allow_worktree); self.assertFalse(c.allow_standard_agent); self.assertFalse(c.allow_shared_workspace_writes)
    def test_load_config_allows_worktree_flags(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"config.yaml"
            payload={"subagents":{"enabled":True,"allow_worktree":True,"allow_standard_agent":True,"allow_shared_workspace_writes":True}}
            path.write_text(json.dumps(payload), encoding="utf-8")
            c=load_subagent_config(path)
            self.assertTrue(c.allow_worktree); self.assertTrue(c.allow_standard_agent); self.assertTrue(c.allow_shared_workspace_writes)



class DirtyMainTreeGateTests(WorktreeLifecycleTests):
    def test_create_rejects_dirty_main_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            _init_repo(repo)
            (repo / "dirty.txt").write_text("x", encoding="utf-8")
            with self.assertRaises(WorktreeError) as ctx:
                create_worktree_session(
                    workspace_root=repo,
                    task_id="dirty-create",
                    worktree_parent=Path(tmp) / "wts",
                )
            self.assertIn("未提交变更", str(ctx.exception))

    def test_apply_rejects_dirty_main_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            _init_repo(repo)
            session = create_worktree_session(
                workspace_root=repo,
                task_id="dirty-apply",
                worktree_parent=Path(tmp) / "wts",
            )
            try:
                (session.worktree_path / "child.txt").write_text(
                    "from-child", encoding="utf-8"
                )
                (repo / "main-dirty.txt").write_text("main", encoding="utf-8")
                with self.assertRaises(WorktreeError) as ctx:
                    apply_worktree_to_main(session, strategy="checkout")
                self.assertIn("未提交变更", str(ctx.exception))
            finally:
                cleanup_worktree_session(session)


class HostWorktreeControlSurfaceTests(unittest.TestCase):
    def test_list_apply_discard_callbacks_roundtrip(self) -> None:
        import threading

        from omnicrawl.agent.core import LocalToolAgent
        from omnicrawl.agent.subagents.coordinator import SubAgentCoordinator
        from omnicrawl.agent.subagents.definitions import AgentDefinitionRegistry
        from omnicrawl.config.subagents import SubAgentConfig

        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            _init_repo(repo)
            agent = object.__new__(LocalToolAgent)
            agent._subagent_worktree_sessions = {}
            agent._subagent_worktree_lock = threading.Lock()

            session = create_worktree_session(
                workspace_root=repo,
                task_id="ctrl-1",
                worktree_parent=Path(tmp) / "wts",
            )
            LocalToolAgent._register_subagent_worktree_session(agent, session)
            (session.worktree_path / "feature.txt").write_text("ok", encoding="utf-8")

            listed = LocalToolAgent.list_subagent_worktrees(agent)
            self.assertEqual(len(listed), 1)
            self.assertEqual(listed[0]["branch"], session.branch_name)

            empty_builtin = Path(tmp) / "empty-builtin"
            empty_builtin.mkdir(parents=True, exist_ok=True)
            coordinator = SubAgentCoordinator(
                config=SubAgentConfig(enabled=True, allow_worktree=True),
                registry=AgentDefinitionRegistry(builtin_directory=empty_builtin),
                tools_provider=lambda: {},
                execute_task=lambda *args, **kwargs: None,
                apply_worktree=agent.apply_subagent_worktree,
                discard_worktree=agent.discard_subagent_worktree,
                list_worktrees=agent.list_subagent_worktrees,
            )
            listed_result = coordinator.run({"action": "list_worktrees"})
            self.assertTrue(listed_result.ok)
            apply_result = coordinator.run(
                {
                    "action": "apply_worktree",
                    "task_id": session.task_id,
                    "strategy": "checkout",
                    "cleanup": False,
                }
            )
            self.assertTrue(apply_result.ok, apply_result.output)
            self.assertTrue((repo / "feature.txt").exists())
            discard_result = coordinator.run(
                {
                    "action": "discard_worktree",
                    "branch": session.branch_name,
                    "remove_branch": True,
                }
            )
            self.assertTrue(discard_result.ok, discard_result.output)
            self.assertEqual(LocalToolAgent.list_subagent_worktrees(agent), [])


class HostWorktreePreparationSafetyTests(unittest.TestCase):
    def test_failed_plugin_context_freeze_does_not_create_or_register_worktree(self) -> None:
        """所有可失败的上下文冻结必须发生在 Git Worktree 创建之前。"""

        import threading
        from types import SimpleNamespace
        from unittest.mock import Mock, patch

        from omnicrawl.agent.core import LocalToolAgent
        from omnicrawl.agent.subagents.definitions import AgentDefinition

        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(llm=None)
            agent._subagent_worktree_sessions = {}
            agent._subagent_worktree_lock = threading.Lock()
            agent._freeze_subagent_plugin_dispatch_context = Mock(
                side_effect=RuntimeError("plugin context failed")
            )
            definition = AgentDefinition(
                name="writer",
                description="write",
                system_prompt="write",
                permission_mode="standard",
                isolation="worktree",
            )

            with patch("omnicrawl.agent.core.create_worktree_session") as create:
                with self.assertRaisesRegex(RuntimeError, "plugin context failed"):
                    agent._prepare_subagent_execution(definition, "fresh", "")

            create.assert_not_called()
            self.assertEqual(agent.list_subagent_worktrees(), [])

    def test_registration_failure_rolls_back_created_worktree(self) -> None:
        """Git 资源创建后若登记失败，必须立即清理目录和临时分支。"""

        from types import SimpleNamespace
        from unittest.mock import Mock, patch

        from omnicrawl.agent.core import LocalToolAgent
        from omnicrawl.agent.subagents.definitions import AgentDefinition

        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            session = SimpleNamespace(
                task_id="writer-register-failure",
                branch_name="omnicrawl/subagent/writer-register-failure",
                worktree_path=workspace / "worktree",
            )
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(llm=None)
            agent._freeze_subagent_plugin_dispatch_context = Mock(return_value=None)
            agent._register_subagent_worktree_session = Mock(
                side_effect=RuntimeError("registration failed")
            )
            definition = AgentDefinition(
                name="writer",
                description="write",
                system_prompt="write",
                permission_mode="standard",
                isolation="worktree",
            )

            with patch(
                "omnicrawl.agent.core.create_worktree_session",
                return_value=session,
            ):
                with patch(
                    "omnicrawl.agent.core.cleanup_worktree_session"
                ) as cleanup:
                    with self.assertRaisesRegex(RuntimeError, "registration failed"):
                        agent._prepare_subagent_execution(definition, "fresh", "")

                    cleanup.assert_called_once_with(session, remove_branch=True)


class SharedWriterLockTests(unittest.TestCase):
    def test_requires_shared_writer_lock_only_for_shared_standard(self) -> None:
        from omnicrawl.agent.subagents.coordinator import (
            STANDARD_WRITE_TOOL_NAMES,
            SubAgentCoordinator,
            _PreparedTask,
        )
        from omnicrawl.agent.subagents.definitions import (
            AgentDefinition,
            AgentDefinitionRegistry,
        )
        from omnicrawl.agent.subagents.execution import SubAgentExecutionContext
        from omnicrawl.agent.tools import ToolDefinition
        from omnicrawl.config.subagents import SubAgentConfig

        def _def(isolation: str, mode: str) -> AgentDefinition:
            return AgentDefinition(
                name="writer",
                description="w",
                system_prompt="x",
                tools=tuple(sorted(STANDARD_WRITE_TOOL_NAMES | {"read_file"})),
                disallowed_tools=(),
                model="inherit",
                max_turns=5,
                max_tool_calls=10,
                permission_mode=mode,
                background=False,
                isolation=isolation,
            )

        tools = {
            name: ToolDefinition(
                name=name,
                description=name,
                argument_schema="{}",
                requires_confirmation=False,
                run=lambda arguments: None,
            )
            for name in sorted(STANDARD_WRITE_TOOL_NAMES | {"read_file"})
        }
        empty_builtin = Path(tempfile.mkdtemp())
        coordinator = SubAgentCoordinator(
            config=SubAgentConfig(
                enabled=True,
                allow_standard_agent=True,
                allow_shared_workspace_writes=True,
            ),
            registry=AgentDefinitionRegistry(builtin_directory=empty_builtin),
            tools_provider=lambda: tools,
            execute_task=lambda *args, **kwargs: None,
        )
        shared_task = _PreparedTask(
            batch_id="b",
            task_id="t1",
            description="d",
            prompt="p",
            agent_type="writer",
            definition=_def("shared", "standard"),
            tools=tools,
            execution_context=SubAgentExecutionContext(
                context="fresh", isolation="shared"
            ),
        )
        worktree_task = _PreparedTask(
            batch_id="b",
            task_id="t2",
            description="d",
            prompt="p",
            agent_type="writer",
            definition=_def("worktree", "standard"),
            tools=tools,
            execution_context=SubAgentExecutionContext(
                context="fresh", isolation="worktree"
            ),
        )
        self.assertTrue(coordinator._requires_shared_writer_lock(shared_task))
        self.assertFalse(coordinator._requires_shared_writer_lock(worktree_task))

if __name__=="__main__":
    unittest.main()
