#!/usr/bin/env python3
"""生成飞书 `[FILE:...]` 发文件的对照数据集：期望值全部来自 Python 真实现。

用最小探针驱动 `fsapp` 里的真方法：上传用桩返回固定 key（或 None），消息发送与提示
逐条记录，因此比对的是「分流、消息体、文案、调用顺序」而不是我对语义的理解。
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-connectors/tests/fixtures/feishu_file_send_parity.json"

if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.connectors import fsapp  # noqa: E402

WORK_ROOT = (
    Path(tempfile.gettempdir()) / f"omnicrawl-file-send-parity-{__import__('os').getpid()}"
).resolve()
RECEIVE_ID = "ou_receiver"
RECEIVE_ID_TYPE = "open_id"


def method(name: str):
    """按方法名在 fsapp 模块里定位真实现（类名不可硬编码）。"""
    for value in vars(fsapp).values():
        if isinstance(value, type):
            candidate = value.__dict__.get(name)
            if candidate is not None:
                return candidate
    raise SystemExit(f"未在 fsapp 里找到方法：{name}")


class Probe:
    """只补真方法真正读到的宿主能力，并把调用逐条记录下来。"""

    def __init__(self, image_key, file_key, calls: list) -> None:
        self._image_key = image_key
        self._file_key = file_key
        self._calls = calls

    def _upload_image(self, path: Path):
        self._calls.append({"call": "upload_image", "name": path.name})
        return self._image_key

    def _upload_file(self, path: Path):
        self._calls.append({"call": "upload_file", "name": path.name, "suffix": path.suffix.casefold()})
        return self._file_key

    def _send_raw(self, receive_id, body, *, msg_type, receive_id_type):
        self._calls.append({
            "call": "send_raw",
            "receive_id": receive_id,
            "body": body,
            "msg_type": msg_type,
            "receive_id_type": receive_id_type,
        })
        return True

    def _send_text(self, receive_id, text, *, receive_id_type):
        self._calls.append({
            "call": "send_text",
            "receive_id": receive_id,
            "text": text,
            "receive_id_type": receive_id_type,
        })
        return True


def mask(text: object) -> str:
    raw = str(text)
    base = str(WORK_ROOT)
    raw = raw.replace(base.replace("\\", "\\\\"), "<ROOT>")
    return raw.replace(base, "<ROOT>")


def expand(text: str) -> str:
    return text.replace("<ROOT>", str(WORK_ROOT))


def mask_calls(calls: list[dict]) -> list[dict]:
    """把调用记录里的路径换成占位符，两侧才能用同一份期望值。"""
    return [
        {key: (mask(value) if isinstance(value, str) else value) for key, value in call.items()}
        for call in calls
    ]


def local_file_cases() -> list[dict]:
    send = method("_send_local_file")
    cases = [
        ("image_ok", "<ROOT>/pic.png", "ik1", "fk1"),
        ("image_upload_failed", "<ROOT>/pic.png", None, "fk1"),
        ("audio_ok", "<ROOT>/clip.mp3", "ik1", "fk1"),
        ("video_ok", "<ROOT>/clip.mp4", "ik1", "fk1"),
        ("document_ok", "<ROOT>/notes.txt", "ik1", "fk1"),
        ("document_failed", "<ROOT>/notes.txt", "ik1", None),
        ("empty_key", "<ROOT>/notes.txt", "ik1", ""),
        ("no_extension", "<ROOT>/README", "ik1", "fk1"),
        ("missing_path", "<ROOT>/nope.txt", "ik1", "fk1"),
        ("directory_path", "<ROOT>", "ik1", "fk1"),
        ("empty_path", "", "ik1", "fk1"),
        ("dot_path", "<ROOT>/sub/..", "ik1", "fk1"),
    ]
    results = []
    for label, path_template, image_key, file_key in cases:
        calls: list[dict] = []
        probe = Probe(image_key, file_key, calls)
        result = send(probe, RECEIVE_ID, expand(path_template), receive_id_type=RECEIVE_ID_TYPE)
        results.append({
            "label": label,
            "path": path_template,
            "image_key": image_key,
            "file_key": file_key,
            "result": bool(result),
            "calls": mask_calls(calls),
        })
    return results


def generated_files_cases() -> list[dict]:
    send_all = method("_send_generated_files")
    # 探针上挂真实现：`_send_generated_files` 内部会 self._send_local_file(...)。
    Probe._send_local_file = method("_send_local_file")
    texts = [
        "结果见 [FILE:<ROOT>/pic.png] 与 [FILE:<ROOT>/notes.txt]",
        "没有标记",
        "[FILE:<ROOT>/missing.txt]",
        "[FILE:]",
        "[FILE: ]",
        "[FILE:<ROOT>/a.png]\n[FILE:<ROOT>/b.mp3]\n[FILE:<ROOT>/c.pdf]",
    ]
    results = []
    for index, template in enumerate(texts):
        calls: list[dict] = []
        probe = Probe("ik1", "fk1", calls)
        text = expand(template)
        send_all(probe, RECEIVE_ID, text, receive_id_type=RECEIVE_ID_TYPE)
        results.append({"label": f"text_{index}", "text": template, "calls": mask_calls(calls)})
    return results


def main() -> None:
    module_path = Path(fsapp.__file__).resolve()
    if ROOT not in module_path.parents:
        raise SystemExit(f"对照必须跑在仓库内的真实现上：{module_path}")

    if WORK_ROOT.exists():
        shutil.rmtree(WORK_ROOT, ignore_errors=True)
    (WORK_ROOT / "sub").mkdir(parents=True, exist_ok=True)
    for name, content in [
        ("pic.png", "png"),
        ("clip.mp3", "mp3"),
        ("clip.mp4", "mp4"),
        ("notes.txt", "text"),
        ("README", "readme"),
        ("a.png", "a"),
        ("b.mp3", "b"),
        ("c.pdf", "c"),
    ]:
        (WORK_ROOT / name).write_text(content, encoding="utf-8")

    payload = {
        "source": "omnicrawl/connectors/fsapp.py",
        "receive_id": RECEIVE_ID,
        "receive_id_type": RECEIVE_ID_TYPE,
        "local_files": local_file_cases(),
        "generated_files": generated_files_cases(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {FIXTURE_PATH}")
    shutil.rmtree(WORK_ROOT, ignore_errors=True)


if __name__ == "__main__":
    main()
