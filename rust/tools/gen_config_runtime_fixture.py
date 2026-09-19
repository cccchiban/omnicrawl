#!/usr/bin/env python3
"""生成配置仓库（`config/core/runtime.py` 的路径/读写面与 TOML 写回）的对照数据集。

期望值来自 Python 真实现：

- 路径解析（显式路径 / `AI_*` 环境变量 / 默认用户目录）与 `.toml` 后缀校验文案；
- `get_section` 的类型校验；
- `load_config_data` 的存在性、BOM、空文件与 config.json 遗留检测；
- `save_config_data` 的文本形状（`tomli_w.dumps` + `_strip_none`）；
- `migrate_legacy_user_config` 的搬迁、冲突备份命名与失败保留。

临时目录里的绝对路径在数据集里写成 `{root}` 占位符，由 Rust 测试替换成自己的临时根。

用法：``python rust/tools/gen_config_runtime_fixture.py``
输出：``rust/crates/omnicrawl-config/tests/fixtures/config_runtime_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-config/tests/fixtures/config_runtime_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.config.core import runtime as R  # noqa: E402

if not Path(R.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

HOME = str(Path.home())
# 默认用户目录由注入的 home 决定：Python 侧 patch `user_config_dir`，Rust 侧给同一个 home。
MANAGED_HOME = "C:\\oc-parity\\home"
MANAGED_DIR = MANAGED_HOME + "\\" + R.USER_CONFIG_DIRNAME


class _PatchedEnvironment:
    """统一注入：home 生效于默认路径解析，环境变量表完全由用例决定。"""

    def __init__(self, env: dict[str, str], managed_dir: str = MANAGED_DIR) -> None:
        # `~` 展开走 `os.path.expanduser`，所以进程环境里的 home 也要指向同一个可注入根。
        self._env = {"USERPROFILE": MANAGED_HOME, "HOME": MANAGED_HOME}
        self._env.update(env)
        self._patch_dir = mock.patch.object(
            R, "user_config_dir", lambda *args, **kwargs: Path(managed_dir)
        )

    def __enter__(self):
        self._stack = mock.patch.dict(os.environ, self._env, clear=True)
        self._stack.start()
        self._patch_dir.start()
        return self

    def __exit__(self, *exc_info):
        self._patch_dir.stop()
        self._stack.stop()
        return False


def _write_text(path: Path, content: str) -> None:
    """Python 3.9 的 write_text 没有 newline 参数，这里显式按 LF 写。"""

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(content)


def _read_text(path: Path) -> str:
    with path.open("r", encoding="utf-8", newline="") as handle:
        return handle.read()


def _placeholder(text: str, root: Path) -> str:
    return text.replace(str(root), "{root}")


def build_dirs() -> dict[str, str]:
    return {
        "home": HOME,
        "user_config_dir": str(R.user_config_dir()),
        "global_agents": str(R.global_agents_path()),
        "default_config": str(R.default_config_path()),
        "default_models": str(R.default_models_path()),
        "default_subagents": str(R.default_subagents_path()),
        "default_toml_config": str(R.default_toml_config_path()),
    }


def build_legacy() -> list[dict[str, object]]:
    cases = []
    for platform, env in (
        ("win32", {"APPDATA": "C:\\Users\\x\\AppData\\Roaming"}),
        ("win32", {}),
        ("linux", {"XDG_CONFIG_HOME": "/xdg/config"}),
        ("linux", {}),
        ("darwin", {}),
    ):
        paths = R.legacy_user_config_dirs(environ=env, platform_name=platform)
        cases.append(
            {
                "platform": platform,
                "env": env,
                "expected": [str(item) for item in paths],
            }
        )
    return cases


RESOLVE_CASES: list[tuple[str, str | None, dict[str, str], str]] = [
    ("config", None, {}, "默认用户目录"),
    ("config", None, {"AI_CONFIG_FILE": "C:\\custom\\my.toml"}, "环境变量优先"),
    ("config", None, {"AI_CONFIG_FILE": "   "}, "环境变量空白回退默认"),
    ("config", None, {"AI_CONFIG_FILE": "~/env.toml"}, "环境变量波浪号"),
    ("config", "D:\\explicit\\cfg.toml", {"AI_CONFIG_FILE": "C:\\custom\\my.toml"}, "显式优先"),
    ("config", "~/rel.toml", {}, "显式波浪号"),
    ("config", "./rel.toml", {}, "点前缀归一"),
    ("config", "a//b/./c.toml", {}, "分隔符归一"),
    ("config", "a/b/", {}, "尾随分隔符去掉后无后缀"),
    ("config", "cfg.TOML", {}, "大写后缀允许"),
    ("config", "x.json", {}, "JSON 后缀拒绝"),
    ("config", "noext", {}, "无后缀拒绝"),
    ("config", "cfg.tar.toml", {}, "双后缀允许"),
    ("models", None, {}, "默认"),
    ("models", None, {"AI_MODELS_FILE": "~/m.toml"}, "环境变量波浪号"),
    ("models", "m.yaml", {}, "yaml 拒绝"),
    ("subagents", None, {}, "默认"),
    ("subagents", None, {"AI_SUBAGENTS_FILE": "D:\\s.toml"}, "环境变量"),
    ("subagents", "s.JSON", {}, "大写 JSON 拒绝"),
    ("config_write", None, {}, "写路径默认"),
    ("config_write", "~/w.toml", {}, "写路径显式"),
    ("config_write", None, {"AI_CONFIG_FILE": "C:\\env\\w.toml"}, "写路径环境变量"),
    ("models_write", None, {"AI_MODELS_FILE": "m.toml"}, "写路径环境变量"),
    ("models_write", "bad.txt", {}, "写路径后缀拒绝"),
    ("subagents_write", "~/s.toml", {}, "写路径显式"),
    ("subagents_write", None, {}, "写路径默认"),
]

RESOLVE_FUNCTIONS = {
    "config": (R.resolve_config_path, "运行配置"),
    "models": (R.resolve_models_path, "模型配置"),
    "subagents": (R.resolve_subagents_path, "子代理设置"),
    "config_write": (R.resolve_config_write_path, "运行配置"),
    "models_write": (R.resolve_models_write_path, "模型配置"),
    "subagents_write": (R.resolve_subagents_write_path, "子代理设置"),
}


def build_resolves() -> list[dict[str, object]]:
    cases = []
    for func, explicit, env, name in RESOLVE_CASES:
        resolver, _source = RESOLVE_FUNCTIONS[func]
        with _PatchedEnvironment(env):
            try:
                value = str(resolver(explicit))
                record: dict[str, object] = {"expected_path": value}
            except R.RuntimeConfigError as exc:
                record = {"error": str(exc)}
        record.update(
            {
                "name": name,
                "func": func,
                "explicit": explicit,
                "env": env,
                "managed_home": MANAGED_HOME,
            }
        )
        cases.append(record)
    return cases


SECTION_CASES: list[tuple[str, dict[str, object], str]] = [
    ("缺失键", {"other": 1}, "mcp"),
    ("空字符串", {"mcp": ""}, "mcp"),
    ("空表", {"mcp": {}}, "mcp"),
    ("正常表", {"mcp": {"enabled": True, "servers": {"a": {"command": "x"}}}}, "mcp"),
    ("列表", {"mcp": [1, 2]}, "mcp"),
    ("字符串", {"mcp": "x"}, "mcp"),
    ("整数", {"mcp": 1}, "mcp"),
    ("布尔", {"mcp": False}, "mcp"),
    ("浮点", {"mcp": 1.5}, "mcp"),
    ("别的键是对象", {"llm": {"model": "x"}}, "mcp"),
]


def build_sections() -> list[dict[str, object]]:
    cases = []
    for name, data, key in SECTION_CASES:
        try:
            value = R.get_section(data, key)
            record: dict[str, object] = {"expected": value}
        except R.RuntimeConfigError as exc:
            record = {"error": str(exc)}
        record.update({"name": name, "data": data, "key": key})
        cases.append(record)
    return cases


LOAD_CASES: list[tuple[str, dict[str, str], bool, str | None, dict[str, str]]] = [
    (
        "正常 TOML",
        {
            "config.toml": (
                "# 注释\n"
                "[llm]\n"
                'model = "gpt-5.2"\n'
                "context_window_tokens = 128000\n"
                "temperature = 0.2\n"
                'tags = ["a", "b"]\n'
                "\n"
                "[ui]\n"
                "show_thinking = true\n"
            )
        },
        False,
        "config.toml",
        {},
    ),
    ("空文件", {"config.toml": ""}, False, "config.toml", {}),
    ("只有空白", {"config.toml": "  \n\t\n"}, False, "config.toml", {}),
    ("只有 BOM", {"config.toml": "\ufeff"}, False, "config.toml", {}),
    (
        "带 BOM 的正常文件",
        {"config.toml": "\ufeff[ui]\nshow_thinking = false\n"},
        False,
        "config.toml",
        {},
    ),
    ("文件不存在", {}, False, "config.toml", {}),
    ("解析失败", {"config.toml": "[llm\nmodel = 1\n"}, False, "config.toml", {}),
    ("顶层不是表", {"config.toml": "42\n"}, False, "config.toml", {}),
    ("显式 JSON 后缀", {"config.json": "{}"}, False, "config.json", {}),
    ("默认路径读取", {"config.toml": "[mcp]\nenabled = true\n"}, True, None, {}),
    ("默认路径不存在", {}, True, None, {}),
    ("遗留 config.json", {"config.json": "{}"}, True, None, {}),
    (
        "遗留 JSON 与 toml 并存",
        {"config.json": "{}", "config.toml": "[ui]\nshow_thinking = true\n"},
        True,
        None,
        {},
    ),
    ("环境变量指向的文件不存在", {}, True, None, {"AI_CONFIG_FILE": "C:\\nope\\x.toml"}),
    (
        "环境变量指向的文件存在",
        {"env.toml": "[llm]\nmodel = \"custom\"\n"},
        True,
        None,
        {"AI_CONFIG_FILE": "{root}\\env.toml"},
    ),
]


def build_loads() -> list[dict[str, object]]:
    cases = []
    with tempfile.TemporaryDirectory(prefix="oc-config-loads-") as tmp:
        root = Path(tmp).resolve()
        for index, (name, layout, use_default, explicit, env) in enumerate(LOAD_CASES):
            case_root = root / ("case-%d" % index)
            user_dir = case_root / R.USER_CONFIG_DIRNAME
            user_dir.mkdir(parents=True)
            for relative, content in layout.items():
                path = user_dir / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                _write_text(path, content)
            resolved_env = {key: value.replace("{root}", str(user_dir)) for key, value in env.items()}
            with _PatchedEnvironment(resolved_env, str(user_dir)):
                if explicit is None:
                    target = None
                else:
                    target = user_dir / explicit
                try:
                    value = R.load_config_data(target)
                    record: dict[str, object] = {"expected": value}
                except R.RuntimeConfigError as exc:
                    message = _placeholder(str(exc), user_dir)
                    if message.startswith("配置文件 TOML 解析失败："):
                        record = {"error_prefix": message.split("，", 2)[0] + "，"}
                    else:
                        record = {"error": message}
            record.update(
                {
                    "name": name,
                    "layout": layout,
                    "use_default": use_default,
                    "explicit": explicit,
                    "env": env,
                }
            )
            cases.append(record)
    return cases


DUMP_CASES: list[tuple[str, dict[str, object]]] = [
    (
        "真实 config.toml 形状",
        {
            "llm": {
                "provider": "openai",
                "protocol": "openai_chat",
                "model": "gpt-5.2",
                "context_window_tokens": 128000,
                "reasoning_effort": "high",
                "temperature": 0.2,
                "base_url": "https://api.openai.com/v1",
                "api_key_env": "OPENAI_API_KEY",
            },
            "ui": {"show_thinking": True},
            "approval": {"mode": "review", "review_model": ""},
            "context_compaction": {
                "enabled": True,
                "trigger_context_tokens": 96000,
                "trigger_context_percent": 75,
                "keep_recent_messages": 6,
            },
        },
    ),
    (
        "真实 models.toml 形状",
        {
            "version": 1,
            "models": {
                "local-qwen": {
                    "provider": "openai",
                    "protocol": "openai_chat",
                    "model_id": "qwen3-local",
                    "base_url": "http://127.0.0.1:8000/v1",
                    "api_key_env": "LOCAL_API_KEY",
                    "context_window_tokens": 32768,
                    "capabilities": {"streaming": True, "tools": True, "vision": False},
                    "aliases": ["qwen", "本地"],
                }
            },
        },
    ),
    (
        "真实 subagents.toml 形状",
        {
            "subagents": {
                "enabled": True,
                "max_concurrent": 2,
                "timeout_seconds": 1800,
                "model_overrides": {"review": "gpt-5.2"},
            }
        },
    ),
    ("空文档", {}),
    ("只有顶层标量", {"a": 1, "b": "x", "c": True, "d": 1.5}),
    ("标量在表之前", {"z": 1, "llm": {"model": "x"}, "a": 3}),
    ("空表", {"t": {}, "u": {"v": {}}}),
    ("深层空表", {"a": {"b": {"c": {}}}}),
    ("同名冲突备份字段", {"llm": {"model": "x"}, "ui": {"show_thinking": False, "nested": {"deep": True}}}),
    ("含 None 的写回", {"llm": {"model": "x", "temperature": None}, "extra": None, "list": [1, None, 2]}),
    ("空字符串与特殊键", {"": 1, "a b": 2, "a.b": 3, "中文键": 4, "a-b": 5}),
    ("控制字符", {"k": "\x00\x01\x07\x08\t\n\x0b\x0c\r\x1f\x7f"}),
    ("引号与反斜杠", {"k": 'quote"here \\ back', "p": "C:\\Users\\x"}),
    ("非 ASCII", {"k": "中文 é 😀 \u2028", "emoji": "🚀"}),
    ("数组", {"k": [], "one": ["a"], "many": [1, 2, 3], "mixed": [1, "a", True, 1.5]}),
    ("嵌套数组", {"k": [[1, 2], [3]], "deep": [[[1]]], "empty_in": [[], [[]]]}),
    ("数组里的内联表", {"k": [{"a": 1}, {"b": {"c": 2}}], "long": [{"a": "x" * 60}]}),
    ("浮点写法", {"a": 1.0, "b": 0.1, "c": 3.5, "d": 1e-7, "e": 1e16, "f": -0.0, "g": 1e100}),
    ("大整数", {"a": 9223372036854775807, "b": -9223372036854775808, "c": 0}),
    ("布尔", {"a": True, "b": False}),
    ("表内数组与子表", {"t": {"k": [1, 2], "inner": {"deep": "x"}, "scalar": True}}),
    (
        "多级表与标量混排",
        {"outer": {"scalar": 1, "inner": {"deep": "x"}}, "top": 2, "other": {"k": "v"}},
    ),
]


def build_dumps() -> list[dict[str, object]]:
    cases = []
    for name, data in DUMP_CASES:
        text = R.dump_toml_text(data)
        cases.append({"name": name, "data": data, "text": text})
    return cases


def _snapshot(root: Path) -> list[str]:
    entries = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_dir():
            entries.append(relative + "/")
        else:
            entries.append("%s = %s" % (relative, _read_text(path).replace("\r\n", "\n").strip()))
    return entries


MIGRATION_CASES: list[tuple[str, dict[str, str], dict[str, str], list[str]]] = [
    (
        "搬迁到空目标",
        {"legacy/config.toml": "[llm]\nmodel = \"x\"\n", "legacy/models.toml": "version = 1\n"},
        {},
        ["legacy"],
    ),
    (
        "目标有冲突备份",
        {"legacy/config.toml": "[llm]\nmodel = \"new\"\n"},
        {".OmniCrawl/config.toml": "[llm]\nmodel = \"old\"\n"},
        ["legacy"],
    ),
    (
        "旧目录不存在",
        {},
        {".OmniCrawl/config.toml": "x = 1\n"},
        ["legacy"],
    ),
    (
        "多个旧目录",
        {"legacy/config.toml": "a = 1\n", "old2/models.toml": "version = 1\n"},
        {},
        ["legacy", "old2"],
    ),
    (
        "旧目录里带子目录",
        {"legacy/nested/inner.toml": "x = 1\n"},
        {},
        ["legacy"],
    ),
]


def build_migrations() -> list[dict[str, object]]:
    cases = []
    for name, layout, target_layout, legacy_names in MIGRATION_CASES:
        with tempfile.TemporaryDirectory(prefix="oc-config-migrate-") as tmp:
            root = Path(tmp).resolve()
            for relative, content in layout.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                _write_text(path, content)
            for relative, content in target_layout.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                _write_text(path, content)
            target = root / R.USER_CONFIG_DIRNAME
            target.mkdir(parents=True, exist_ok=True)
            legacy_dirs = [root / item for item in legacy_names]
            with mock.patch.object(
                R, "user_config_dir", lambda environ=None, platform_name=None: target
            ):
                R.migrate_legacy_user_config(legacy_dirs=legacy_dirs)
            cases.append(
                {
                    "name": name,
                    "layout": layout,
                    "target_layout": target_layout,
                    "legacy_names": legacy_names,
                    "expected": _snapshot(root),
                }
            )
    return cases


def build_conflict_sequence() -> list[dict[str, object]]:
    """同名冲突按 `.migrated.bak`、`.migrated.1.bak` 递增。"""

    with tempfile.TemporaryDirectory(prefix="oc-config-conflict-") as tmp:
        root = Path(tmp).resolve()
        target = root / R.USER_CONFIG_DIRNAME
        target.mkdir(parents=True, exist_ok=True)
        _write_text(target / "config.toml", "a = 1\n")
        snapshots = []
        with mock.patch.object(
            R, "user_config_dir", lambda environ=None, platform_name=None: target
        ):
            for round_index in range(2):
                legacy = root / "legacy"
                legacy.mkdir(parents=True, exist_ok=True)
                _write_text(legacy / "config.toml", "a = %d\n" % (round_index + 2))
                R.migrate_legacy_user_config(legacy_dirs=[legacy])
                snapshots.append(_snapshot(target))
        return [{"name": "同名冲突递增", "expected": snapshots}]


def main() -> None:
    fixture = {
        "dirs": build_dirs(),
        "legacy_dirs": build_legacy(),
        "resolves": build_resolves(),
        "sections": build_sections(),
        "loads": build_loads(),
        "dumps": build_dumps(),
        "migrations": build_migrations(),
        "conflict_sequences": build_conflict_sequence(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1, sort_keys=False)
        handle.write("\n")
    print(
        "wrote %s: resolves=%d sections=%d loads=%d dumps=%d migrations=%d conflict=%d"
        % (
            FIXTURE_PATH,
            len(fixture["resolves"]),
            len(fixture["sections"]),
            len(fixture["loads"]),
            len(fixture["dumps"]),
            len(fixture["migrations"]),
            len(fixture["conflict_sequences"]),
        )
    )


if __name__ == "__main__":
    main()
