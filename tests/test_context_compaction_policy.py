from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent
from omnicrawl.agent.context_compaction import (
    ContextBudgetManager,
    SourceEvent,
    TokenUsageSample,
)
from omnicrawl.config.context_compaction import (
    ContextCompactionConfig,
    ContextCompactionConfigError,
    load_context_compaction_config,
)
from omnicrawl.llm import LLMConfig


class ContextCompactionConfigTest(unittest.TestCase):
    def test_defaults_keep_model_compaction_disabled_and_recent_window_at_six(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"

            config = load_context_compaction_config(config_path)

        self.assertFalse(config.enabled)
        self.assertEqual(config.trigger_context_tokens, 70_000)
        self.assertEqual(config.next_user_reserve_tokens, 4_096)
        self.assertEqual(config.minimum_turns_between_model_compactions, 4)
        self.assertEqual(config.emergency_context_ratio, 0.85)
        self.assertEqual(config.recent_turns, 6)
        self.assertEqual(config.target_summary_tokens, 6_000)
        self.assertFalse(config.allow_cross_provider)

    def test_example_configuration_uses_only_supported_fields(self) -> None:
        config_path = Path(__file__).resolve().parents[1] / "config.example.yaml"

        config = load_context_compaction_config(config_path)

        self.assertFalse(config.enabled)
        self.assertEqual(config.recent_turns, 6)

    def test_enabled_config_rejects_model_window_without_output_reserve(self) -> None:
        with self.assertRaises(AgentError):
            AgentConfig(
                llm=LLMConfig(
                    api_key="test",
                    base_url="https://example.test/v1",
                    model="test-model",
                    context_window_tokens=80_000,
                ),
                context_compaction=ContextCompactionConfig(enabled=True),
            )

    def test_should_reject_small_context_model_when_compaction_is_enabled(self) -> None:
        parent = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="main",
            context_window_tokens=128_000,
        )
        small = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="small",
            context_window_tokens=32_000,
        )
        manager = SimpleNamespace(switch=Mock())
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=parent,
            context_compaction=ContextCompactionConfig(enabled=True),
        )
        agent._runtime_manager = manager

        with patch("omnicrawl.agent.core.apply_model_selection", return_value=small):
            with self.assertRaises(AgentError):
                agent.set_model("small")

        manager.switch.assert_not_called()
        self.assertIs(agent.config.llm, parent)

    def test_should_refresh_summary_service_when_main_model_changes(self) -> None:
        parent = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="main",
            context_window_tokens=128_000,
        )
        selected = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="next",
            context_window_tokens=128_000,
        )
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=parent,
            context_compaction=ContextCompactionConfig(enabled=True),
        )
        agent._runtime_manager = SimpleNamespace(switch=Mock())
        agent._context_compaction_service_instance = object()

        with (
            patch("omnicrawl.agent.core.apply_model_selection", return_value=selected),
            patch(
                "omnicrawl.agent.core.llm_config_to_profile_and_descriptor",
                return_value=(object(), object()),
            ),
        ):
            agent.set_model("next")

        self.assertIs(agent.config.llm, selected)
        self.assertNotIn("_context_compaction_service_instance", agent.__dict__)

    def test_should_reject_small_runtime_context_window_when_compaction_is_enabled(self) -> None:
        llm = LLMConfig(
            api_key="test",
            base_url="https://example.test/v1",
            model="main",
            context_window_tokens=128_000,
        )
        manager = SimpleNamespace(set_context_window_tokens=Mock())
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=llm,
            context_compaction=ContextCompactionConfig(enabled=True),
        )
        agent._runtime_manager = manager

        with self.assertRaises(AgentError):
            agent.set_context_window_tokens(32_000)

        manager.set_context_window_tokens.assert_not_called()
        self.assertEqual(agent.config.llm.context_window_tokens, 128_000)

    def test_loader_rejects_invalid_threshold_and_ratio(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                "context_compaction:\n"
                "  trigger_context_tokens: 0\n"
                "  emergency_context_ratio: 1.0\n",
                encoding="utf-8",
            )

            with self.assertRaises(ContextCompactionConfigError):
                load_context_compaction_config(config_path)


class ContextBudgetManagerTest(unittest.TestCase):
    def test_snapshot_partitions_summary_cold_history_and_recent_turns(self) -> None:
        manager = ContextBudgetManager()
        history = [
            {"role": "assistant", "content": "会话压缩摘要：\n旧摘要"},
            {"role": "user", "content": "冷问题" * 100},
            {"role": "assistant", "content": "冷回答" * 100},
            {"role": "user", "content": "最近问题"},
            {"role": "assistant", "content": "最近回答"},
        ]

        snapshot = manager.measure(
            system_prompt="system",
            context_messages=[{"role": "user", "content": "project context"}],
            history_messages=history,
            tool_schemas=[{"type": "function", "function": {"name": "read"}}],
            recent_turns=1,
            target_summary_tokens=20,
            next_user_reserve_tokens=100,
            trigger_context_tokens=70_000,
            context_window_tokens=128_000,
            usage=TokenUsageSample(input_tokens=1_000, cached_input_tokens=250),
        )

        self.assertGreater(snapshot.existing_summary_tokens, 0)
        self.assertGreater(snapshot.cold_history_tokens, 0)
        self.assertGreater(snapshot.recent_history_tokens, 0)
        self.assertEqual(snapshot.cache_hit_ratio, 0.25)
        self.assertEqual(snapshot.next_user_reserve_tokens, 100)
        self.assertGreater(snapshot.potential_retired_tokens, 0)
        self.assertFalse(snapshot.trigger_reached)

    def test_trigger_boundary_is_exactly_seventy_thousand_tokens(self) -> None:
        manager = ContextBudgetManager()

        below = manager.measure_from_token_counts(
            stable_context_tokens=10_000,
            existing_summary_tokens=0,
            cold_history_tokens=20_000,
            recent_history_tokens=35_903,
            next_user_reserve_tokens=4_096,
            target_summary_tokens=6_000,
            trigger_context_tokens=70_000,
            context_window_tokens=128_000,
            usage=TokenUsageSample(),
        )
        at_boundary = manager.measure_from_token_counts(
            stable_context_tokens=10_000,
            existing_summary_tokens=0,
            cold_history_tokens=20_000,
            recent_history_tokens=35_904,
            next_user_reserve_tokens=4_096,
            target_summary_tokens=6_000,
            trigger_context_tokens=70_000,
            context_window_tokens=128_000,
            usage=TokenUsageSample(),
        )

        self.assertEqual(below.estimated_next_input_tokens, 69_999)
        self.assertFalse(below.trigger_reached)
        self.assertEqual(at_boundary.estimated_next_input_tokens, 70_000)
        self.assertTrue(at_boundary.trigger_reached)

    def test_batch_keeps_recent_turn_and_never_splits_tool_chain(self) -> None:
        events = [
            SourceEvent("u1", "user_message", {"content": "old"}),
            SourceEvent("c1", "tool_call_requested", {"tool_call_id": "call-1"}),
            SourceEvent("r1", "tool_result", {"tool_call_id": "call-1"}),
            SourceEvent("a1", "assistant_message", {"content": "done"}),
            SourceEvent("u2", "user_message", {"content": "recent"}),
            SourceEvent("a2", "assistant_message", {"content": "recent done"}),
        ]

        batch = ContextBudgetManager().select_batch(events, recent_turns=1)

        self.assertIsNotNone(batch)
        assert batch is not None
        self.assertEqual([event.event_id for event in batch.events], ["u1", "c1", "r1", "a1"])
        self.assertEqual([event.event_id for event in batch.recent_events], ["u2", "a2"])

    def test_rolling_batch_reuses_previous_remaining_event_ids(self) -> None:
        events = [
            SourceEvent("u1", "user_message", {"content": "covered"}),
            SourceEvent("a1", "assistant_message", {"content": "covered done"}),
            SourceEvent("u2", "user_message", {"content": "carried"}),
            SourceEvent("a2", "assistant_message", {"content": "carried done"}),
            SourceEvent(
                "s1",
                "compact_summary",
                {
                    "content": "summary",
                    "covered_event_ids": ["u1", "a1"],
                    "remaining_message_count": 2,
                    "remaining_event_ids": ["u2", "a2"],
                    "model_generated": True,
                },
            ),
            SourceEvent("u3", "user_message", {"content": "new"}),
            SourceEvent("a3", "assistant_message", {"content": "new done"}),
        ]

        batch = ContextBudgetManager().select_batch(events, recent_turns=1)

        self.assertIsNotNone(batch)
        assert batch is not None
        self.assertEqual([event.event_id for event in batch.events], ["u2", "a2"])
        self.assertEqual([event.event_id for event in batch.recent_events], ["u3", "a3"])
        self.assertEqual(batch.previous_covered_event_ids, ("u1", "a1"))

    def test_rolling_batch_infers_remaining_events_from_legacy_summary(self) -> None:
        events = [
            SourceEvent("u1", "user_message", {"content": "covered"}),
            SourceEvent("a1", "assistant_message", {"content": "covered done"}),
            SourceEvent("u2", "user_message", {"content": "carried"}),
            SourceEvent("a2", "assistant_message", {"content": "carried done"}),
            SourceEvent(
                "s1",
                "compact_summary",
                {"content": "legacy", "remaining_message_count": 2},
            ),
            SourceEvent("u3", "user_message", {"content": "new"}),
            SourceEvent("a3", "assistant_message", {"content": "new done"}),
        ]

        batch = ContextBudgetManager().select_batch(events, recent_turns=1)

        self.assertIsNotNone(batch)
        assert batch is not None
        self.assertEqual([event.event_id for event in batch.events], ["u2", "a2"])

    def test_recent_token_budget_is_stricter_than_turn_count(self) -> None:
        events = [
            SourceEvent("u1", "user_message", {"content": "old" * 200}),
            SourceEvent("a1", "assistant_message", {"content": "done" * 200}),
            SourceEvent("u2", "user_message", {"content": "recent"}),
            SourceEvent("a2", "assistant_message", {"content": "recent done"}),
        ]

        batch = ContextBudgetManager().select_batch(
            events,
            recent_turns=2,
            recent_token_budget=100,
        )

        self.assertIsNotNone(batch)
        assert batch is not None
        self.assertEqual([event.event_id for event in batch.events], ["u1", "a1"])
        self.assertEqual([event.event_id for event in batch.recent_events], ["u2", "a2"])

    def test_single_large_turn_is_selected_only_when_explicitly_allowed(self) -> None:
        events = [
            SourceEvent("u1", "user_message", {"content": "large payload"}),
            SourceEvent("a1", "assistant_message", {"content": "result"}),
        ]
        manager = ContextBudgetManager()

        normal = manager.select_batch(events, recent_turns=6)
        large = manager.select_batch(
            events,
            recent_turns=6,
            allow_single_large_turn=True,
        )

        self.assertIsNone(normal)
        self.assertIsNotNone(large)
        assert large is not None
        self.assertTrue(large.single_large_turn)
        self.assertEqual(large.recent_events, ())

    def test_automatic_model_failure_starts_cooldown(self) -> None:
        events = [
            SourceEvent(
                "f1",
                "context_compaction_failed",
                {"mode": "automatic_model", "reason": "temporary failure"},
            ),
            SourceEvent(
                "s1",
                "compact_summary",
                {"content": "deterministic fallback", "model_generated": False},
            ),
            SourceEvent("u1", "user_message", {"content": "next"}),
            SourceEvent("a1", "assistant_message", {"content": "next done"}),
        ]

        turns = ContextBudgetManager().turns_since_last_model_compaction(events)

        self.assertEqual(turns, 1)

    def test_cooldown_is_bypassed_only_at_emergency_ratio(self) -> None:
        manager = ContextBudgetManager()
        batch = manager.select_batch(
            [
                SourceEvent("u1", "user_message", {"content": "old"}),
                SourceEvent("a1", "assistant_message", {"content": "done"}),
                SourceEvent("u2", "user_message", {"content": "recent"}),
                SourceEvent("a2", "assistant_message", {"content": "recent done"}),
            ],
            recent_turns=1,
        )
        normal = manager.measure_from_token_counts(
            stable_context_tokens=10_000,
            existing_summary_tokens=0,
            cold_history_tokens=30_000,
            recent_history_tokens=26_000,
            next_user_reserve_tokens=4_096,
            target_summary_tokens=6_000,
            trigger_context_tokens=70_000,
            context_window_tokens=128_000,
            usage=TokenUsageSample(),
        )
        emergency = manager.measure_from_token_counts(
            stable_context_tokens=20_000,
            existing_summary_tokens=0,
            cold_history_tokens=50_000,
            recent_history_tokens=40_000,
            next_user_reserve_tokens=4_096,
            target_summary_tokens=6_000,
            trigger_context_tokens=70_000,
            context_window_tokens=128_000,
            usage=TokenUsageSample(),
        )

        blocked = manager.decide_auto_compaction(
            normal,
            batch,
            turns_since_last_model_compaction=2,
            minimum_turns_between_model_compactions=4,
        )
        bypassed = manager.decide_auto_compaction(
            emergency,
            batch,
            turns_since_last_model_compaction=2,
            minimum_turns_between_model_compactions=4,
        )

        self.assertFalse(blocked.should_compact)
        self.assertEqual(blocked.reason, "cooldown_active")
        self.assertTrue(bypassed.should_compact)
        self.assertTrue(bypassed.bypassed_cooldown)


if __name__ == "__main__":
    unittest.main()
