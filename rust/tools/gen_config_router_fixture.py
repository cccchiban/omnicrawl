#!/usr/bin/env python3
"""把配置对话 checkpoint 转成 Rust 可读的紧凑权重，并生成对照数据集。

权重来源是 Python 侧的 `omnicrawl/config_chat/assets/router.pt`（zip + pickle 的 torch 存档）；
内核侧不实现 pickle 解析，改由本脚本转成「魔数 + 头部 JSON + f32 数据块」的自描述二进制。
同时把 `labels.json` / `aliases.json` 原样复制到内核 crate 的 `data/`，并记录三份文件的 sha256
（对照测试据此确认内核读到的就是这一份快照）。

用法：``python rust/tools/gen_config_router_fixture.py``
输出：
- ``rust/crates/omnicrawl-config-chat/data/config_router.bin``
- ``rust/crates/omnicrawl-config-chat/data/labels.json``
- ``rust/crates/omnicrawl-config-chat/data/aliases.json``
- ``rust/crates/omnicrawl-config-chat/tests/fixtures/config_chat_parity.json``
"""

from __future__ import annotations

import hashlib
import importlib
import json
import os
import re
import struct
import sys
import tempfile
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / "omnicrawl/config_chat/assets"
CRATE = ROOT / "rust/crates/omnicrawl-config-chat"
DATA = CRATE / "data"
FIXTURE_PATH = CRATE / "tests/fixtures/config_chat_parity.json"

sys.path.insert(0, str(ROOT))

import torch  # noqa: E402

MAGIC = b"OCCFG1\x00\x00"

# `router.py` 的从句切分模式（对照数据集里的 clauses 就是它的输出）。
CLAUSE_PATTERN = re.compile(r"[，,；;。！？!?]|然后|接着|顺便|另外|还有")

ROUTER_TEXTS = [
    "打开记忆功能",
    "把记忆关掉",
    "开启子代理",
    "打开 TTS 语音合成，然后关闭思考显示",
    "把请求超时设为 60 秒",
    "修改模型名称为 gpt-5",
    "显示思考过程",
    "关闭消息脱敏",
    "打开插件功能；顺便把上下文压缩阈值改成 70",
    "把子代理的 max_turns 改成 12",
    "disable plugins and set temperature to 0.25",
    "关掉记忆，另外把重试次数改成 3，还有开启 MCP",
    "打开…接着关闭…顺便重启…另外重置…还有退出",
    "把 " + "很长的描述" * 30 + " 的超时改成 30",
    "",
    "   ",
    "，；。！？",
    "你好",
]

SERVICE_CASES = [
    ("打开记忆功能", None),
    ("把记忆关掉", None),
    ("开启子代理", None),
    ("把请求超时设为 60 秒", "llm.defaults.request_timeout_seconds = 5\n"),
    ("显示思考过程", None),
    ("修改模型名称为 gpt-5", None),
    ("关闭消息脱敏", None),
    ("打开 TTS 语音合成，然后关闭思考显示", None),
    ("你好", None),
    ("把子代理的 max_turns 改成 12", None),
]


def load_python_modules():
    """按包路径导入 Python 侧实现。

    `omnicrawl/__init__.py` 会把每个子模块整体重命名并导入整棵依赖树（含脱敏层），
    本脚本只需要配置对话与配置仓库两块，因此先把顶层包换成只有 ``__path__`` 的壳。
    """

    if "omnicrawl" not in sys.modules:
        shell = types.ModuleType("omnicrawl")
        shell.__path__ = [str(ROOT / "omnicrawl")]
        sys.modules["omnicrawl"] = shell
    router = importlib.import_module("omnicrawl.config_chat.router")
    service = importlib.import_module("omnicrawl.config_chat.service")
    if not Path(router.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit("加载到的不是仓库源码")
    return router, service


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_weights(checkpoint: dict, path: Path) -> dict:
    state_dict = checkpoint["model_state"]
    tensors: list[dict] = []
    blob = bytearray()
    for name, tensor in state_dict.items():
        data = tensor.detach().cpu()
        if data.dtype == torch.bool:
            payload = bytes(1 if item else 0 for item in data.reshape(-1).tolist())
            dtype = "bool"
        else:
            payload = data.to(torch.float32).contiguous().numpy().tobytes()
            dtype = "f32"
        tensors.append(
            {
                "name": name,
                "dtype": dtype,
                "shape": list(data.shape),
                "offset": len(blob),
                "length": len(payload),
            }
        )
        blob.extend(payload)

    model_config = dict(checkpoint.get("model_config", {}))
    # `vocab_size` 计入 `<pad>` / `<unk>` 这类多字符键（内核侧按字符建表，两者不同）：
    # 它必须与 `embedding.weight` 的行数一致，内核靠它做形状校验。
    model_config["vocab_size"] = len(checkpoint["vocab"])
    header = {
        "vocab": {str(key): int(value) for key, value in checkpoint["vocab"].items()},
        "actions": list(checkpoint["actions"]),
        "configs": list(checkpoint["configs"]),
        "model_config": model_config,
        "tensors": tensors,
    }
    header_bytes = json.dumps(header, ensure_ascii=False).encode("utf-8")
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as handle:
        handle.write(MAGIC)
        handle.write(struct.pack("<I", len(header_bytes)))
        handle.write(header_bytes)
        handle.write(bytes(blob))
    return header


def clauses_of(text: str) -> list[str]:
    return [part.strip() for part in CLAUSE_PATTERN.split(text) if part.strip()]


def build_router_cases(router) -> list[dict]:
    cases: list[dict] = []
    for text in ROUTER_TEXTS:
        clauses: list[dict] = []
        for clause in clauses_of(text):
            characters = list(clause)[:96]
            ids = torch.tensor(
                [[router.vocab.get(ch, 1) for ch in characters]], dtype=torch.long
            )
            mask = torch.ones_like(ids)
            with torch.no_grad():
                output = router.model(ids, mask)
            config_tags = output["config_logits"].argmax(-1)[0].tolist()
            value_tags = output["value_logits"].argmax(-1)[0].tolist()
            action_ids = output["action_logits"].argmax(-1)[0].tolist()
            spans = [list(item) for item in router._spans(config_tags)]

            reps = output["token_reps"][0]
            config_indices: list[int] = []
            for start, end in spans[:12]:
                pooled = reps[start:end].mean(0)
                pooled = pooled / pooled.norm().clamp(min=1e-6)
                similarity = router.alias_vectors @ pooled
                scores = torch.full((len(router.configs),), -2.0)
                scores.scatter_reduce_(0, router.alias_owner, similarity, reduce="amax")
                config_indices.append(int(scores.argmax()))

            clauses.append(
                {
                    "text": clause,
                    "token_ids": [router.vocab.get(ch, 1) for ch in characters],
                    "config_tags": config_tags,
                    "value_tags": value_tags,
                    "action_ids": action_ids,
                    "spans": spans,
                    "config_indices": config_indices,
                }
            )
        cases.append(
            {
                "text": text,
                "clauses": [item["text"] for item in clauses],
                "detail": clauses,
                "commands": router.predict(text),
            }
        )
    return cases


def build_prepare_cases(service, labels: dict[str, dict]) -> list[dict]:
    instance = service.ConfigChatService(agent=None)
    by_type: dict[str, str] = {}
    for path, item in labels.items():
        by_type.setdefault(item.get("type", "str"), path)
    bool_path = "memory.enabled" if "memory.enabled" in labels else by_type["bool"]
    int_path = by_type["int"]
    float_path = by_type["float"]
    str_path = by_type["str"]
    list_path = by_type["list"]
    dict_path = by_type["dict"]

    cases = [
        ("OPEN", "memory", ""),
        ("TOGGLE", bool_path, ""),
        ("RESET", bool_path, ""),
        ("ENABLE", "memory", ""),
        ("ENABLE", bool_path, ""),
        ("DISABLE", bool_path, ""),
        ("SET", bool_path, "开"),
        ("SET", bool_path, "关闭"),
        ("SET", bool_path, "maybe"),
        ("SET", bool_path, " True "),
        ("SET", int_path, "60"),
        ("SET", int_path, "-3"),
        ("SET", int_path, "abc"),
        ("SET", float_path, "0.25"),
        ("SET", str_path, "  value  "),
        ("SET", list_path, "a,b"),
        ("SET", dict_path, "a=1"),
        ("SET", "not.a.real.key", "1"),
    ]
    prepared = []
    for action, config, value in cases:
        command = service.ConfigChatCommand(action=action, config=config, value=value)
        entry = {"action": action, "config": config, "value": value}
        try:
            entry["coerced"] = instance._prepare_command(command)
        except service.ConfigChatError as exc:
            entry["error"] = str(exc)
        prepared.append(entry)
    return prepared


def build_service_cases(service) -> list[dict]:
    cases: list[dict] = []
    for text, initial in SERVICE_CASES:
        with tempfile.TemporaryDirectory(prefix="oc-config-chat-") as directory:
            root = Path(directory)
            config_path = root / "config.toml"
            subagents_path = root / "subagents.toml"
            if initial is not None:
                config_path.write_text(initial, encoding="utf-8")
            os.environ["AI_CONFIG_FILE"] = str(config_path)
            os.environ["AI_SUBAGENTS_FILE"] = str(subagents_path)
            instance = service.ConfigChatService(agent=None)
            entry: dict = {"text": text, "initial": initial}
            try:
                changes = instance.apply_text(text)
                entry["changes"] = [
                    {"path": change.path, "value": change.value} for change in changes
                ]
                entry["commands"] = [
                    {
                        "action": command.action,
                        "config": command.config,
                        "value": command.value,
                    }
                    for command in instance.predict(text)
                ]
            except service.ConfigChatError as exc:
                entry["error"] = str(exc)
            entry["config_file"] = (
                config_path.read_text(encoding="utf-8") if config_path.exists() else None
            )
            entry["subagents_file"] = (
                subagents_path.read_text(encoding="utf-8")
                if subagents_path.exists()
                else None
            )
            cases.append(entry)
    for key in ("AI_CONFIG_FILE", "AI_SUBAGENTS_FILE"):
        os.environ.pop(key, None)
    return cases


def main() -> None:
    os.environ.pop("AI_CONFIG_FILE", None)
    os.environ.pop("AI_SUBAGENTS_FILE", None)
    router_module, service_module = load_python_modules()

    checkpoint = torch.load(
        str(ASSETS / "router.pt"), map_location="cpu", weights_only=False
    )
    header = write_weights(checkpoint, DATA / "config_router.bin")

    for name in ("labels.json", "aliases.json"):
        source = ASSETS / name
        if not source.exists():
            raise SystemExit(f"缺少 Python 侧资源：{source}")
        (DATA / name).write_bytes(source.read_bytes())

    router = router_module.ConfigRouter()
    alias_map = router.alias_map
    labels = {
        item["path"]: item
        for item in json.loads((DATA / "labels.json").read_text(encoding="utf-8"))["keys"]
    }

    fixture = {
        "weights": {
            "path": "data/config_router.bin",
            "magic": MAGIC.decode("latin-1"),
            "size": (DATA / "config_router.bin").stat().st_size,
            "sha256": sha256(DATA / "config_router.bin"),
            "labels_sha256": sha256(DATA / "labels.json"),
            "aliases_sha256": sha256(DATA / "aliases.json"),
            "vocab_size": len(checkpoint["vocab"]),
            "configs": len(checkpoint["configs"]),
            # 别名索引条数：**不**直接数 aliases.json 的条目（那是 907，比真实索引多 1）。
            # 索引按 `alias_map.get(config, [config])` 逐条配置展开，因此 aliases.json 里
            # 多出的键（例如不在 configs 里的 `version`）不进索引——按真实现口径统计。
            "aliases": sum(
                len(alias_map[config]) if config in alias_map else 1
                for config in checkpoint["configs"]
            ),
            "tensors": len(header["tensors"]),
            "model_config": header["model_config"],
            "actions": header["actions"],
        },
        "router": build_router_cases(router),
        "prepare": build_prepare_cases(service_module, labels),
        "service": build_service_cases(service_module),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print(
        "wrote %s (%d bytes) and %s"
        % (
            DATA / "config_router.bin",
            (DATA / "config_router.bin").stat().st_size,
            FIXTURE_PATH,
        )
    )


if __name__ == "__main__":
    main()
