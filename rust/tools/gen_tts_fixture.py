#!/usr/bin/env python3
"""生成 `omnicrawl-tts` TTS 文本归一化的对照数据集。

期望值来自 Python 真实现 `omnicrawl/tts/normalize.py`：`normalize_tts_text`（稳健清洗管道）、
`prepare_tts_request_texts`（合成前预处理）、`resolve_text_normalization_language`（语言推断）与
`rewrite_hyphens_before_zh_wetext`（中文 WeText 连字符保护）。

用法（仓库根目录）：

    python rust/tools/gen_tts_fixture.py
    cd rust && cargo test -p omnicrawl-tts --test tts_parity
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tts/tests/fixtures/tts_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.tts.normalize import (  # noqa: E402
    _rewrite_hyphens_before_zh_wetext as rewrite_hyphens_before_zh_wetext,
    normalize_tts_text,
    prepare_tts_request_texts,
    resolve_text_normalization_language,
)

if not Path(sys.modules["omnicrawl.tts.normalize"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")

TEXTS = [
    "你好，世界。",
    "hello world",
    "启用/禁用",
    "A/B 测试",
    "路径 foo/bar-baz.py 与 app.js.map",
    "见 https://example.com/a/b?x=1 与 https://example.com",
    "邮箱 someone@example.com 与 @mention 与 r/rust 与 u/foo",
    "日期 2024-05-01 和 2024/05/01 还有 v2.3.1",
    "**加粗** 与 `inline code` 与 # 标题",
    "😀 表情 🚀 与 ★ 星号与 © 版权与 ■ 方块",
    "don't 与 can’t 缩写",
    '引号 "双" 与 \'单\' 与 “中文”',
    "# 一级标题\n> 引用\n- 列表项\n1. 有序项",
    "[链接文字](https://example.com/x) 尾部",
    "流程 A -> B -> C 与 A=>B 与 中文→中文",
    "破折号——结束 与 多连字符--继续",
    "感叹号！！！ 与 问号？？？ 与 混合?! 与 省略号......",
    "【结构括号】与 {花括号} 与 [方括号]",
    "《书名》后接。 与 嵌入式《书名》文本",
    "下划线 some_var_name 与 ___PROT0___ 占位",
    "第一行\n第二行\n\n第三行",
    "中文与 English 混排 sentence 结束",
    "数字 123 与中文之间",
    "spaces    多个     空格",
    "尾部没有标点",
    "",
    "   ",
    "emoji only 😀😀",
    "中文，标点 之间有 空格 。",
    "  两端有空白  ",
    "x=-2 与 -3 与 10-3 与 2024-05-01",
    "中文-中文 与 word-word 与 数字-数字",
]

LANGUAGES = [
    ("你好世界", "Junhao"),
    ("hello world", "Junhao"),
    ("12345", "Trump"),
    ("12345", "Junhao"),
    ("", "Ava"),
    ("", "Junhao"),
    ("中文 with English", "Junhao"),
]


def main() -> int:
    normalize_cases = [{"text": text, "expected": normalize_tts_text(text)} for text in TEXTS]

    hyphens = [
        "x=-2 与 -3 与 10-3 与 2024-05-01",
        "中文-中文 与 word-word 与 数字-数字",
        "没有连字符",
        "a - b 与 -- 与 ---",
    ]
    hyphen_cases = [
        {"text": text, "expected": rewrite_hyphens_before_zh_wetext(text)} for text in hyphens
    ]

    language_cases = [
        {"text": text, "voice": voice, "expected": resolve_text_normalization_language(text=text, voice=voice)}
        for text, voice in LANGUAGES
    ]

    pipeline_cases = []
    for text in ["你好，世界。", "hello world", ""]:
        for enable_normalize in (True, False):
            payload = prepare_tts_request_texts(
                text=text,
                prompt_text="",
                voice="Junhao",
                enable_wetext=False,
                enable_normalize_tts_text=enable_normalize,
            )
            pipeline_cases.append(
                {
                    "text": text,
                    "enable_normalize_tts_text": enable_normalize,
                    "expected": payload,
                }
            )
    # 参考文本分支
    pipeline_cases.append(
        {
            "text": "你好",
            "prompt_text": "参考/文本",
            "enable_normalize_tts_text": True,
            "expected": prepare_tts_request_texts(
                text="你好",
                prompt_text="参考/文本",
                voice="Junhao",
                enable_wetext=False,
                enable_normalize_tts_text=True,
            ),
        }
    )

    data = {
        "source": "omnicrawl/tts/normalize.py",
        "normalize": normalize_cases,
        "hyphens": hyphen_cases,
        "languages": language_cases,
        "pipeline": pipeline_cases,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH}（normalize {len(normalize_cases)} 例、"
        f"hyphens {len(hyphen_cases)}、languages {len(language_cases)}、pipeline {len(pipeline_cases)}）"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
