#!/usr/bin/env python3
"""生成扩展层对照数据集，供 `omnicrawl-extensions` 的 parity 测试使用。

单一真相是 `omnicrawl/extensions/` 的 Python 真实现：本脚本把同一批输入喂给真实现，
把返回值（或错误文案）原样记下来。Rust 侧用同一份输入重放并逐字段比对。

用法：``python rust/tools/gen_extensions_fixture.py``
输出：``rust/crates/omnicrawl-extensions/tests/fixtures/extensions_parity.json``
"""

from __future__ import annotations

import json
import sys
from dataclasses import asdict, is_dataclass
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
OUTPUT_PATH = ROOT / "rust" / "crates" / "omnicrawl-extensions" / "tests" / "fixtures" / "extensions_parity.json"

sys.path.insert(0, str(ROOT))

import omnicrawl.extensions.plugin_models as models  # noqa: E402
import omnicrawl.extensions.plugin_registry as registry  # noqa: E402
import omnicrawl.extensions.skill as skill_mod  # noqa: E402
from omnicrawl.extensions.plugin_manager import PluginDispatchContext  # noqa: E402

for module in (models, registry, skill_mod):
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit(f"加载到的不是仓库源码：{module.__file__}")


def plain(value: Any) -> Any:
    """把 dataclass / 常量集合转成与 Rust 侧同构的普通 JSON 值。"""

    if is_dataclass(value):
        return {key: plain(item) for key, item in asdict(value).items()}
    if isinstance(value, dict):
        return {str(key): plain(item) for key, item in value.items()}
    if isinstance(value, (list, tuple, set, frozenset)):
        return [plain(item) for item in value]
    return value


def capture(call) -> dict[str, Any]:
    """执行一次调用，记录返回值或错误文案。"""

    try:
        return {"result": plain(call())}
    except Exception as exc:  # noqa: BLE001 - 错误文案本身就是要对照的值
        return {"error": str(exc)}


def handler_dict(item: models.ResolvedHandler) -> dict[str, Any]:
    return plain(item)


def manifest_dict(item: models.PluginManifest) -> dict[str, Any]:
    return plain(item)


def registration_dict(item: models.HandlerRegistration) -> dict[str, Any]:
    return {
        "id": item.id,
        "hook": item.hook,
        "mode": item.mode,
        "priority": item.priority,
        "replaces": list(item.replaces),
        "event_version": item.event_version,
        "timeout_ms": item.timeout_ms,
    }


def custom_event_dict(item: models.CustomEventDeclaration) -> dict[str, Any]:
    return {
        "name": item.name,
        "version": item.version,
        "visibility": item.visibility,
        "schema": plain(item.schema),
    }


def registry_document_dict(document: models.PluginRegistryDocument) -> dict[str, Any]:
    return plain(document.to_dict())


def build_semver_cases() -> list[dict[str, Any]]:
    samples = [
        "1.0.0",
        "0.0.1",
        "10.20.30",
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-0.3.7",
        "1.0.0+build.1",
        "1.0.0-beta+exp.sha.5114f85",
        " 1.2.3 ",
        "01.0.0",
        "1.0",
        "1.0.0-",
        "1.0.0-01",
        "1.0.0-0",
        "1.0.0-alpha..1",
        "v1.0.0",
        "",
        "1.0.0+",
    ]
    return [{"version": item, "valid": models.is_valid_semver(item)} for item in samples]


def build_npm_name_cases() -> list[dict[str, Any]]:
    samples = [
        "omnicrawl-plugin-demo",
        "@omnicrawl/plugin-demo",
        "a",
        "~demo",
        "@scope/name",
        "UPPER",
        "with space",
        "@scope",
        "@scope/",
        "",
        "name_with_underscore",
        "name.with.dot",
    ]
    return [{"name": item, "valid": models.is_valid_npm_name(item)} for item in samples]


def build_namespace_cases() -> list[dict[str, Any]]:
    samples = [
        "@omnicrawl/plugin-demo",
        "Plain-Name",
        "@Scope/Name",
        "a__b",
        "---",
        "!!!",
        "",
    ]
    return [
        {"package": item, "expect": models.normalize_plugin_namespace(item)} for item in samples
    ]


def build_config_cases() -> list[dict[str, Any]]:
    inputs: list[Any] = [
        None,
        {},
        {"enabled": True},
        {"enabled": "yes", "defaultTimeoutMs": "2500"},
        {"default_timeout_ms": 10},
        {"max_timeout_ms": 70000},
        {"failure_threshold": 0},
        {"max_message_bytes": 100},
        {"custom_event_max_depth": 99},
        {"audit_log_enabled": False, "allow_network_install": True},
        [1, 2],
        "text",
    ]
    cases: list[dict[str, Any]] = []
    for item in inputs:
        outcome = capture(lambda item=item: plain(models.parse_plugins_config(item)))
        cases.append({"input": item, **outcome})
    return cases


def build_registration_cases() -> list[dict[str, Any]]:
    inputs: list[Any] = [
        {"id": "h1", "hook": "turn.start", "mode": "transform"},
        {"id": "h1", "hook": "turn.start", "mode": "observe"},
        {"id": "h1", "hook": "plugin.demo.event", "mode": "notify"},
        {"id": "h1", "hook": "plugin.demo", "mode": "notify"},
        {"id": "h1", "hook": "unknown.hook", "mode": "notify"},
        {"id": "-bad", "hook": "turn.start", "mode": "guard"},
        {"id": "h1", "hook": "turn.start", "mode": "bogus"},
        {
            "id": "h1",
            "hook": "tool.execute.after",
            "mode": "transform",
            "priority": "3",
            "replaces": ["other/handler", "", "x"],
            "eventVersion": 2,
            "timeoutMs": "1500",
        },
        {"id": "h1", "hook": "turn.start", "mode": "transform", "replaces": "core/x"},
        {"id": "h1", "hook": "turn.start", "mode": "transform", "replaces": ["nope"]},
        {"id": "h1", "hook": "turn.start", "mode": "transform", "replaces": "single"},
        {"id": "h1", "hook": "turn.start", "mode": "transform", "replaces": ["core/sealed"]},
        "not-an-object",
    ]
    cases: list[dict[str, Any]] = []
    for item in inputs:
        outcome = capture(
            lambda item=item: registration_dict(
                models.parse_handler_registration(item, package_name="demo")
            )
        )
        cases.append({"input": item, **outcome})
    return cases


def build_custom_event_cases() -> list[dict[str, Any]]:
    inputs: list[Any] = [
        {"name": "plugin.demo.ping"},
        {"name": "plugin.demo.ping", "version": 2, "visibility": "public"},
        {"name": "plugin.other.ping"},
        {"name": "plugin.demo.Ping"},
        {"name": "plugin.demo.ping", "version": 0},
        {"name": "plugin.demo.ping", "visibility": "shared"},
        {"name": "plugin.demo.ping", "schema": []},
        "not-an-object",
    ]
    cases: list[dict[str, Any]] = []
    for item in inputs:
        outcome = capture(
            lambda item=item: custom_event_dict(
                models.parse_custom_event(item, package_name="@omnicrawl/demo")
            )
        )
        cases.append({"input": item, **outcome})
    return cases


def build_manifest_cases() -> list[dict[str, Any]]:
    base = {
        "name": "@omnicrawl/demo",
        "version": "1.2.3",
        "omnicrawl": {
            "apiVersion": "1",
            "engines": {"omnicrawl": ">=0.1", "node": ">=20"},
            "entry": "index.js",
            "permissions": ["hook:turn.start"],
            "hooks": [{"id": "h1", "hook": "turn.start", "mode": "transform"}],
        },
    }
    inputs: list[Any] = [
        base,
        {**base, "name": "Bad Name"},
        {**base, "version": "1.0"},
        {k: v for k, v in base.items() if k != "omnicrawl"},
        {
            **base,
            "omnicrawl": {**base["omnicrawl"], "apiVersion": "2"},
        },
        {
            **base,
            "omnicrawl": {**base["omnicrawl"], "engines": {"omnicrawl": ">=0.1"}},
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "entry": "../escape.js",
            },
        },
        {
            **base,
            "omnicrawl": {**base["omnicrawl"], "permissions": []},
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "hooks": [
                    {"id": "h1", "hook": "turn.start", "mode": "transform"},
                    {"id": "h1", "hook": "turn.end", "mode": "notify"},
                ],
            },
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "hooks": [{"id": "h1", "hook": "tool.call.before", "mode": "transform"}],
            },
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "agents": ["agents/a.md"],
                "permissions": ["hook:turn.start", "agent:definitions"],
            },
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "agents": ["agents/a.md", "agents/a.md"],
                "permissions": ["hook:turn.start", "agent:definitions"],
            },
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "agents": ["agents/b.txt"],
                "permissions": ["hook:turn.start", "agent:definitions"],
            },
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "customEvents": [{"name": "plugin.omnicrawl-demo.ping"}],
                "permissions": ["hook:turn.start", "hook:custom-emit"],
            },
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "customEvents": [{"name": "plugin.omnicrawl-demo.ping"}],
            },
        },
        {
            **base,
            "omnicrawl": {
                **base["omnicrawl"],
                "hooks": False,
            },
        },
        "not-an-object",
    ]
    cases: list[dict[str, Any]] = []
    for item in inputs:
        outcome = capture(
            lambda item=item: manifest_dict(
                models.parse_plugin_manifest(item, source_path="/tmp/demo/package.json")
            )
        )
        cases.append({"input": item, **outcome})
    return cases


def build_schema_cases() -> list[dict[str, Any]]:
    object_schema = {
        "type": "object",
        "properties": {"name": {"type": "string"}, "count": {"type": "integer"}},
        "required": ["name"],
        "additionalProperties": False,
    }
    inputs: list[tuple[Any, Any]] = [
        ({"name": "x"}, object_schema),
        ({"name": 1}, object_schema),
        ({"name": "x", "extra": 1}, object_schema),
        ({}, object_schema),
        ({"name": "x", "count": 2.5}, object_schema),
        ({"name": "x"}, {"type": "object", "additionalProperties": False, "properties": None}),
        ([], {"type": "array"}),
        ("text", {"type": "string"}),
        (1, {"type": "string"}),
        ([1], None),
        ({"a": 1}, None),
        ({"a": 1}, {}),
        (1, {"type": "unknown"}),
    ]
    cases: list[dict[str, Any]] = []
    for payload, schema in inputs:
        outcome = capture(lambda payload=payload, schema=schema: plain(models.validate_payload_against_schema(payload, schema)))
        cases.append({"payload": payload, "schema": schema, **outcome})
    return cases


def build_validate_patch_cases() -> list[dict[str, Any]]:
    inputs: list[tuple[str, Any]] = [
        ("turn.start", [{"op": "replace", "path": "/payload/tags", "value": ["a"]}]),
        ("turn.start", [{"op": "add", "path": "/payload/tags/-", "value": "b"}]),
        ("turn.start", [{"op": "remove", "path": "/payload/userText"}]),
        ("turn.start", [{"op": "add", "path": "/payload/other", "value": 1}]),
        ("turn.start", [{"op": "move", "path": "/payload/tags", "value": 1}]),
        ("turn.start", [{"op": "add", "path": "payload/tags", "value": 1}]),
        ("turn.start", [{"op": "add", "path": "/payload/tags"}]),
        ("turn.end", [{"op": "add", "path": "/payload/tags", "value": 1}]),
        ("model.request.before", [{"op": "replace", "path": "/payload/messages/0/content", "value": "x"}]),
        ("model.request.before", [{"op": "replace", "path": "/payload/temperature", "value": 0.5}]),
        ("turn.start", "not-a-list"),
        ("turn.start", ["not-an-object"]),
        ("turn.start", []),
    ]
    cases: list[dict[str, Any]] = []
    for hook, patch in inputs:
        outcome = capture(lambda hook=hook, patch=patch: plain(models.validate_json_patch(patch, hook=hook)))
        cases.append({"hook": hook, "patch": patch, **outcome})
    return cases


def build_apply_patch_cases() -> list[dict[str, Any]]:
    inputs: list[tuple[dict[str, Any], Any]] = [
        ({"payload": {"a": 1}}, [{"op": "replace", "path": "/payload/a", "value": 2}]),
        ({"payload": {"a": 1}}, [{"op": "add", "path": "/payload/b", "value": 2}]),
        ({"payload": {"a": 1}}, [{"op": "remove", "path": "/payload/a"}]),
        ({"payload": {"a": 1}}, [{"op": "remove", "path": "/payload/missing"}]),
        ({"payload": {"tags": ["a"]}}, [{"op": "add", "path": "/payload/tags/-", "value": "b"}]),
        ({"payload": {"tags": ["a"]}}, [{"op": "add", "path": "/payload/tags/5", "value": "b"}]),
        ({"payload": {"tags": ["a", "b"]}}, [{"op": "remove", "path": "/payload/tags/1"}]),
        ({"payload": {}}, [{"op": "replace", "path": "/payload/deep/child", "value": 1}]),
        ({"payload": {}}, [{"op": "replace", "path": "/payload/missing", "value": 1}]),
        ({"payload": {"a": 1}}, [{"op": "add", "path": "/", "value": 1}]),
        ({"payload": {"a~b": 1}}, [{"op": "replace", "path": "/payload/a~0b", "value": 2}]),
        ({"payload": {"a/b": 1}}, [{"op": "replace", "path": "/payload/a~1b", "value": 2}]),
    ]
    cases: list[dict[str, Any]] = []
    for document, patch in inputs:
        outcome = capture(lambda document=document, patch=patch: plain(models.apply_json_patch(document, patch)))
        cases.append({"document": document, "patch": patch, **outcome})
    return cases


def sample_handler(**overrides: Any) -> models.ResolvedHandler:
    base = {
        "key": "demo/h1",
        "plugin_name": "demo",
        "plugin_version": "1.0.0",
        "handler_id": "h1",
        "hook": "turn.start",
        "mode": "transform",
        "priority": 0,
        "scope": "user",
        "timeout_ms": 2000,
        "replaces": (),
        "integrity_prefix": "",
        "local_path": "",
        "permissions": (),
    }
    base.update(overrides)
    return models.ResolvedHandler(**base)


def build_sort_cases() -> list[dict[str, Any]]:
    handlers = [
        sample_handler(key="b/obs", plugin_name="b", handler_id="obs", mode="observe", priority=5),
        sample_handler(key="a/guard", plugin_name="a", handler_id="guard", mode="guard", priority=0),
        sample_handler(key="c/tr", plugin_name="c", handler_id="tr", mode="transform", priority=1),
        sample_handler(
            key="a/tr", plugin_name="a", handler_id="tr", mode="transform", priority=9, scope="project"
        ),
        sample_handler(key="a/notify", plugin_name="a", handler_id="notify", mode="notify", priority=1),
        sample_handler(key="zzz/tr", plugin_name="zzz", handler_id="tr", mode="transform", priority=9),
    ]
    sorted_items = models.sort_handlers(handlers)
    return [
        {
            "handlers": [handler_dict(item) for item in handlers],
            "expect": [handler_dict(item) for item in sorted_items],
        }
    ]


def build_hook_result_cases() -> list[dict[str, Any]]:
    inputs: list[Any] = [
        None,
        {},
        {"action": "continue"},
        {"action": "continue", "annotations": {"a": 1}},
        {"action": "approve"},
        {"action": "deny"},
        {"action": "deny", "reason": "  ", "code": "forbidden"},
        {"action": "patch", "patch": [{"op": "add", "path": "/payload/tags", "value": 1}]},
        {"action": "patch", "patch": "nope"},
        {"action": "bogus"},
        {"annotations": []},
        {"annotations": "text"},
        "not-an-object",
    ]
    cases: list[dict[str, Any]] = []
    for item in inputs:
        outcome = capture(
            lambda item=item: plain(
                models.parse_hook_result(item, handler_key="demo/h1", elapsed_ms=12.5)
            )
        )
        cases.append({"raw": item, **outcome})
    return cases


def build_stable_hash_cases() -> list[dict[str, Any]]:
    values: list[Any] = [
        {"b": 1, "a": 2},
        [1, 2, 3],
        "text",
        1,
        None,
        {"nested": {"z": 1, "a": [1, {"y": 2, "x": 3}]}},
        {"中文": "值"},
    ]
    return [{"value": item, "expect": models.stable_hash(item)} for item in values]


def build_timeout_cases() -> list[dict[str, Any]]:
    modes = ["observe", "transform", "guard", "notify", "bogus"]
    return [{"mode": item, "expect": models.default_timeout_for_mode(item)} for item in modes]


def build_merge_cases() -> list[dict[str, Any]]:
    user = models.PluginRegistryDocument(
        schema_version=1,
        plugins={
            "a": models.PluginRecord(name="a", enabled=True),
            "b": models.PluginRecord(name="b", enabled=False, dev_mode=True),
        },
        disabled_handlers=["a/h1"],
    )
    project = models.PluginRegistryDocument(
        schema_version=2,
        plugins={"b": models.PluginRecord(name="b", enabled=True, local_path="/tmp/b")},
        disabled_handlers=["a/h1", "b/h2"],
    )
    merged = registry.merge_registry_documents(user_doc=user, project_doc=project)
    return [
        {
            "user": registry_document_dict(user),
            "project": registry_document_dict(project),
            "expect": registry_document_dict(merged),
        }
    ]


def build_plan_cases() -> list[dict[str, Any]]:
    manifest = models.PluginManifest(
        name="demo",
        version="1.0.0",
        api_version="1",
        entry="index.js",
        permissions=("hook:turn.start",),
        hooks=(
            models.HandlerRegistration(id="h1", hook="turn.start", mode="transform"),
            models.HandlerRegistration(
                id="h2", hook="turn.end", mode="notify", timeout_ms=9000
            ),
        ),
        engines_omnicrawl=">=0.1",
        engines_node=">=20",
    )
    record = models.PluginRecord(
        name="demo",
        enabled=True,
        active=models.PluginVersionRef(
            version="1.0.0",
            integrity="sha512-abcdefghijklmnop",
            lockfile_hash="sha256-x",
            source="registry.npmjs.org",
            store_path="/store/demo",
        ),
        approved_permissions=["hook:turn.start", "hook:turn.end"],
        local_path="/store/demo",
    )
    disabled_record = models.PluginRecord(
        name="skipped",
        enabled=True,
        dev_mode=True,
        approved_permissions=["hook:turn.start"],
    )
    dev_manifest = models.PluginManifest(
        name="skipped",
        version="0.9.0",
        api_version="1",
        entry="index.js",
        permissions=("hook:turn.start",),
        hooks=(models.HandlerRegistration(id="d1", hook="turn.start", mode="guard"),),
        engines_omnicrawl=">=0.1",
        engines_node=">=20",
    )
    handlers = registry.build_execution_plan(
        manifests={
            "demo": (manifest, "user", record),
            "skipped": (dev_manifest, "project", disabled_record),
        },
        disabled_handlers=["skipped/d1"],
        max_timeout_ms=5000,
    )
    return [
        {
            "manifests": [
                {"manifest": manifest_dict(manifest), "scope": "user", "record": plain(record)},
                {
                    "manifest": manifest_dict(dev_manifest),
                    "scope": "project",
                    "record": plain(disabled_record),
                },
            ],
            "disabled": ["skipped/d1"],
            "max_timeout_ms": 5000,
            "expect": [handler_dict(item) for item in handlers],
        }
    ]


def build_replace_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    single = [
        sample_handler(key="a/h", plugin_name="a", handler_id="h", replaces=("b/h",)),
        sample_handler(key="b/h", plugin_name="b", handler_id="h"),
        sample_handler(key="c/h", plugin_name="c", handler_id="h"),
    ]
    cases.append(
        {
            "handlers": [handler_dict(item) for item in single],
            "expect": [item.key for item in registry.resolve_replacements(single)],
        }
    )

    conflict = [
        sample_handler(key="a/h", plugin_name="a", handler_id="h", replaces=("c/h",)),
        sample_handler(key="b/h", plugin_name="b", handler_id="h", replaces=("c/h",)),
        sample_handler(key="c/h", plugin_name="c", handler_id="h"),
    ]
    cases.append(
        {
            "handlers": [handler_dict(item) for item in conflict],
            "expect": [item.key for item in registry.resolve_replacements(conflict)],
        }
    )

    cross_hook = [
        sample_handler(key="a/h", plugin_name="a", handler_id="h", replaces=("b/other",)),
        sample_handler(key="b/other", plugin_name="b", handler_id="other", hook="turn.end"),
    ]
    cases.append(
        {
            "handlers": [handler_dict(item) for item in cross_hook],
            "expect": [item.key for item in registry.resolve_replacements(cross_hook)],
        }
    )

    sealed = [
        sample_handler(key="a/h", plugin_name="a", handler_id="h", replaces=("core/sealed",)),
        sample_handler(key="b/h", plugin_name="b", handler_id="h"),
    ]
    cases.append(
        {
            "handlers": [handler_dict(item) for item in sealed],
            "expect": [item.key for item in registry.resolve_replacements(sealed)],
        }
    )

    cycle = [
        sample_handler(key="a/h", plugin_name="a", handler_id="h", replaces=("b/h",)),
        sample_handler(key="b/h", plugin_name="b", handler_id="h", replaces=("a/h",)),
        sample_handler(key="c/h", plugin_name="c", handler_id="h"),
    ]
    cases.append(
        {
            "handlers": [handler_dict(item) for item in cycle],
            "expect": [item.key for item in registry.resolve_replacements(cycle)],
        }
    )

    missing_target = [
        sample_handler(key="a/h", plugin_name="a", handler_id="h", replaces=("ghost/h",)),
    ]
    cases.append(
        {
            "handlers": [handler_dict(item) for item in missing_target],
            "expect": [item.key for item in registry.resolve_replacements(missing_target)],
        }
    )
    return cases


def build_skill_name_cases() -> list[dict[str, Any]]:
    samples = [
        "ok-name",
        "a",
        "-bad",
        "bad-",
        "double--dash",
        "UPPER",
        "with space",
        "",
        "a" * 65,
        "中文",
    ]
    return [{"name": item, "errors": skill_mod.validate_skill_name(item)} for item in samples]


def build_skill_description_cases() -> list[dict[str, Any]]:
    samples: list[Any] = [None, "", "   ", "ok", "x" * 1025]
    return [
        {
            "description": item,
            "errors": skill_mod.validate_skill_description(item),
        }
        for item in samples
    ]


def build_frontmatter_cases() -> list[dict[str, Any]]:
    contents = [
        "---\nname: demo\ndescription: 说明\n---\n\n正文\n",
        "---\nname: demo\ndisable-model-invocation: true\n---\nbody\n",
        '---\nname: "demo"\ndescription: \'用引号\'\n---\nbody',
        "---\n# comment\nname: demo\n---\nbody",
        "no frontmatter\n",
        "---\nname: demo\n",
        "---\r\nname: demo\r\ndescription: x\r\n---\r\nbody\r\n",
        "---\nname: demo\nweird line\n---\nbody\n",
    ]
    cases: list[dict[str, Any]] = []
    for content in contents:
        frontmatter, body = skill_mod.SkillManager._parse_frontmatter(content)
        cases.append(
            {
                "content": content,
                "frontmatter": plain(frontmatter),
                "body": body,
            }
        )
    return cases


def build_infer_cases() -> list[dict[str, Any]]:
    samples = [
        ("# 标题\n\n正文", "# 标题\n\n正文"),
        ("raw text", ""),
        ("", "- 列表项"),
        ("---\nname: x\n---\n", ""),
        ("", "**加粗**"),
    ]
    return [
        {
            "raw": raw,
            "body": body,
            "expect": skill_mod.SkillManager._infer_description(raw, body),
        }
        for raw, body in samples
    ]


def build_skill_prompt_cases() -> list[dict[str, Any]]:
    metas = [
        skill_mod.SkillMeta(
            name="alpha",
            description="含 <特殊> & 字符",
            source_path=Path("/skills/alpha/SKILL.md"),
            base_dir=Path("/skills/alpha"),
            scope="user",
        ),
        skill_mod.SkillMeta(
            name="hidden",
            description="不展示",
            source_path=Path("/skills/hidden/SKILL.md"),
            base_dir=Path("/skills/hidden"),
            scope="user",
            disable_model_invocation=True,
        ),
    ]
    return [
        {
            "skills": [
                {
                    "name": item.name,
                    "description": item.description,
                    "source_path": str(item.source_path),
                    "base_dir": str(item.base_dir),
                    "scope": item.scope,
                    "disable_model_invocation": item.disable_model_invocation,
                }
                for item in metas
            ],
            "expect": skill_mod.SkillManager.format_skills_for_prompt(metas),
        }
    ]


def build_dispatch_context_cases() -> list[dict[str, Any]]:
    contexts = [
        PluginDispatchContext(),
        PluginDispatchContext(handlers=(sample_handler(),), source="parent-plan"),
    ]
    return [plain(item) for item in contexts]


def main() -> int:
    payload = {
        "semver": build_semver_cases(),
        "npm_name": build_npm_name_cases(),
        "namespace": build_namespace_cases(),
        "plugins_config": build_config_cases(),
        "handler_registration": build_registration_cases(),
        "custom_event": build_custom_event_cases(),
        "plugin_manifest": build_manifest_cases(),
        "payload_schema": build_schema_cases(),
        "validate_patch": build_validate_patch_cases(),
        "apply_patch": build_apply_patch_cases(),
        "sort_handlers": build_sort_cases(),
        "hook_result": build_hook_result_cases(),
        "stable_hash": build_stable_hash_cases(),
        "timeout_for_mode": build_timeout_cases(),
        "merge_registry": build_merge_cases(),
        "execution_plan": build_plan_cases(),
        "resolve_replacements": build_replace_cases(),
        "skill_name": build_skill_name_cases(),
        "skill_description": build_skill_description_cases(),
        "frontmatter": build_frontmatter_cases(),
        "infer_description": build_infer_cases(),
        "skill_prompt": build_skill_prompt_cases(),
        "dispatch_context": build_dispatch_context_cases(),
    }
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    # Windows 上 write_text 会把 \n 折成 \r\n；数据集要与仓库内其他 fixture 一致，直接写字节。
    OUTPUT_PATH.write_bytes(
        (json.dumps(payload, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    )
    print(f"已写入 {OUTPUT_PATH}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
