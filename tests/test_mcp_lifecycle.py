from __future__ import annotations

import unittest

from ai_voice_agent.agent import LocalToolAgent


class AgentLifecycleTest(unittest.TestCase):
    def test_agent_close_closes_mcp_manager(self) -> None:
        class FakeMCPManager:
            closed = False

            def close(self) -> None:
                self.closed = True

        agent = object.__new__(LocalToolAgent)
        manager = FakeMCPManager()
        agent._mcp_manager = manager

        LocalToolAgent.close(agent)

        self.assertTrue(manager.closed)


if __name__ == "__main__":
    unittest.main()
