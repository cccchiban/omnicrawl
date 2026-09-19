#!/usr/bin/env python3
"""把 NER checkpoint 转成 Rust 可读的紧凑权重，并生成 NER 对照数据集。

权重来源是 Python 侧的 `omnicrawl/llm/desensitization/models/bilstm_crf_best.pt`
（zip + pickle 的 torch 存档）；内核侧不实现 pickle 解析，改由本脚本转成
「头部 JSON + f32 数据块」的自描述二进制。

用法：``python rust/tools/gen_ner_fixture.py``
输出：
- ``rust/crates/omnicrawl-llm/data/ner_bilstm_crf.bin``
- ``rust/crates/omnicrawl-llm/tests/fixtures/ner_parity.json``
"""

from __future__ import annotations

import json
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CHECKPOINT = ROOT / "omnicrawl/llm/desensitization/models/bilstm_crf_best.pt"
WEIGHTS_PATH = ROOT / "rust/crates/omnicrawl-llm/data/ner_bilstm_crf.bin"
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/ner_parity.json"

sys.path.insert(0, str(ROOT))

import torch  # noqa: E402

from omnicrawl.llm.desensitization import ner as N  # noqa: E402

if not Path(N.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

MAGIC = b"OCNER1\x00\x00"

TEXTS = [
    "张三在北京大学读书，李四在上海工作。",
    "联系人为王小明，邮箱 a@b.com，电话 13800000000。",
    "the quick brown fox jumps over the lazy dog",
    "会议由中国科学院主办，地点在杭州。",
    "客户编号 A-1024，服务器 10.0.0.1 已重启。",
    "赵六",
    "订单详情：收货人 陈七，地址 深圳市南山区。",
    "｛Desensitized:1｝由 周八 提交。",
    "报告人：欧阳修、司马迁与诸葛亮。",
    "第一句。第二句里出现 张三丰。",
]

TAG_CASES = [
    ["O", "B-PER", "I-PER", "O"],
    ["B-ORG", "I-ORG", "I-LOC", "O"],
    ["I-PER", "I-PER", "O"],
    ["B-LOC"],
    ["O", "O"],
    ["B-PER", "I-PER", "B-LOC", "I-LOC"],
]

CHUNK_CASES = [
    ("第一句。第二句！超长句子需要硬切", 5),
    ("没有标点的长文本", 3),
    ("", 4),
    ("a。b。c", 10),
    ("句末标点结尾。", 4),
]


def checkpoint_payload() -> dict[str, object]:
    return torch.load(str(CHECKPOINT), map_location="cpu", weights_only=False)


def write_weights(checkpoint: dict[str, object]) -> dict[str, object]:
    state_dict = checkpoint["state_dict"]
    tensors: list[dict[str, object]] = []
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

    header = {
        "tags": list(checkpoint["tags"]),
        "char2idx": {str(key): int(value) for key, value in checkpoint["char2idx"].items()},
        "config": dict(checkpoint["model_config"]),
        "tensors": tensors,
    }
    header_bytes = json.dumps(header, ensure_ascii=False).encode("utf-8")
    WEIGHTS_PATH.parent.mkdir(parents=True, exist_ok=True)
    with WEIGHTS_PATH.open("wb") as handle:
        handle.write(MAGIC)
        handle.write(struct.pack("<I", len(header_bytes)))
        handle.write(header_bytes)
        handle.write(bytes(blob))
    return header


def build_fixture(checkpoint: dict[str, object]) -> dict[str, object]:
    extractor = N.NerExtractor()
    layer = N.NerLayer(extractor, entity_types=N.NER_ENTITY_TYPES, min_entity_chars=2)

    spans = []
    for text in TEXTS:
        spans.append(
            {
                "text": text,
                "isolated": N._isolate_chinese(text),
                "chunks": list(N.iter_chunks(text, N.DEFAULT_MAX_SEQ_LEN)),
                "entities": [
                    [start, end, entity_type]
                    for start, end, entity_type in extractor.find_entities(text)
                ],
                "layer_spans": [[start, end] for start, end in layer.find_spans(text)],
            }
        )

    entities = [
        {
            "tags": tags,
            "characters_len": 8,
            "expected": [[s, e, t] for s, e, t in N.extract_entities("x" * 8, tags)],
        }
        for tags in TAG_CASES
    ]

    chunks = [
        {"text": text, "max_len": max_len, "expected": list(N.iter_chunks(text, max_len))}
        for text, max_len in CHUNK_CASES
    ]

    batches = []
    for lengths, budget in [
        ([10, 20, 30], 12000),
        ([100, 100, 100], 250),
        ([1], 1),
        ([5, 5, 5, 5], 10),
    ]:
        probe = N.NerExtractor.__new__(N.NerExtractor)
        probe.batch_tokens = budget
        batches.append(
            {
                "lengths": lengths,
                "budget": budget,
                "expected": [list(item) for item in probe._iter_batches(lengths)],
            }
        )

    misc = {
        "chinese_isolation": [
            {"text": text, "isolated": N._isolate_chinese(text)}
            for text in ["张三 abc 李四", "", "hello", "王·小明的・笔记"]
        ],
        "chinese_spans": [
            {"text": text, "is_chinese_span": N._is_chinese_span(text), "has_cjk": N._has_cjk(text)}
            for text in ["张三", "张", "张a", "", "·", "王·五"]
        ],
        "model_paths": [
            {"configured": None, "env": None, "expected_suffix": N.PACKAGED_MODEL_FILENAME},
            {"configured": "D:/x/custom.pt", "env": None, "expected_suffix": "custom.pt"},
            {"configured": "", "env": " D:/y/env.pt ", "expected_suffix": "env.pt"},
            {"configured": None, "env": "   ", "expected_suffix": N.PACKAGED_MODEL_FILENAME},
        ],
        "devices": [
            {"requested": None, "expected": N.resolve_device(None)},
            {"requested": "cpu", "expected": N.resolve_device("cpu")},
            {"requested": "cuda", "expected": N.resolve_device("cuda")},
            {"requested": " ATO ", "expected": N.resolve_device(" ATO ")},
        ],
        "tags": list(checkpoint["tags"]),
        "char2idx_size": len(checkpoint["char2idx"]),
        "config": dict(checkpoint["model_config"]),
    }
    return {"spans": spans, "entities": entities, "chunks": chunks, "batches": batches, "misc": misc}


def main() -> None:
    checkpoint = checkpoint_payload()
    header = write_weights(checkpoint)
    fixture = build_fixture(checkpoint)
    fixture["weights"] = {
        "path": "data/ner_bilstm_crf.bin",
        "vocab_size": int(header["config"]["vocab_size"]),
        "tags": header["tags"],
        "texts": len(fixture["spans"]),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print(
        "wrote %s (%d bytes) and %s"
        % (WEIGHTS_PATH, WEIGHTS_PATH.stat().st_size, FIXTURE_PATH)
    )


if __name__ == "__main__":
    main()
