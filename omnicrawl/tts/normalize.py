"""TTS 文本归一化（纯 Python，无 pynini/WeTextProcessing 硬依赖）。

- `normalize_tts_text`：鲁棒性文本清洗（来自 MOSS-TTS-Nano 的
  `tts_robust_normalizer_single_script.py`，Apache-2.0），只做清洗不做语义展开。
- `prepare_tts_request_texts`：合成前的文本预处理管道（稳健清洗 + 可选 WeText）。
- `WeTextNormalizer`：WeTextProcessing 的可选封装——安装了 pynini + WeTextProcessing
  时启用语义归一化（数字/单位/日期展开），未安装时自动降级为稳健清洗。
"""

from __future__ import annotations

import logging
import re
import unicodedata
from typing import Any

LOGGER = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# 鲁棒性文本归一化（vendored from MOSS-TTS-Nano tts_robust_normalizer_single_script.py）
# ---------------------------------------------------------------------------

# 不依赖空格分词的脚本：汉字 + 日文假名
_CJK_CHARS = r"\u3400-\u4dbf\u4e00-\u9fff\u3040-\u30ff"
_CJK = f"[{_CJK_CHARS}]"

# 保护占位符
_PROT = r"___PROT\d+___"

# 需要保护的高风险 token
_URL_RE = re.compile(r"https?://[^\s\u3000，。！？；、）】》〉」』]+")
_EMAIL_RE = re.compile(r"(?<![\\w.+-])[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}(?![\\w.-])")
_MENTION_RE = re.compile(r"(?<![A-Za-z0-9_])@[A-Za-z0-9_]{1,32}")
_REDDIT_RE = re.compile(r"(?<![A-Za-z0-9_])(?:u|r)/[A-Za-z0-9_]+")

# `.map` / `.env` / `.gitignore`
_DOT_TOKEN_RE = re.compile(r"(?<![A-Za-z0-9_])\.(?=[A-Za-z0-9._-]*[A-Za-z0-9])[A-Za-z0-9._-]+")

# 纯数字日期（2024/05/01、2024-05-01）：斜杠/连字符原样保留，便于读作日期
_DATE_TOKEN_RE = re.compile(
    r"(?<![A-Za-z0-9_])(?:\d{4}[/-]\d{1,2}[/-]\d{1,2}|\d{1,2}[/-]\d{1,2}[/-]\d{4})(?![A-Za-z0-9_])"
)

# `app.js.map` / `index.d.ts` / `v2.3.1` / `foo/bar-baz.py` 等。
# 要求必须含 `.` 或 `:`（否则像 `A/B` 这种口语列举会被误保护，斜杠无法清理；
# 裸词 `foo-bar` 不再当路径，但后续无清理规则会改动它，行为等价）。
_FILELIKE_RE = re.compile(
    r"(?<![A-Za-z0-9_])"
    r"(?=[A-Za-z0-9._/+:-]*[A-Za-z])"
    r"(?=[A-Za-z0-9._/+:-]*[.:])"
    r"[A-Za-z0-9][A-Za-z0-9._/+:-]*"
    r"(?![A-Za-z0-9_])"
)

# 参与"中英混排边界补空格"的 token：必须至少含 1 个拉丁字母，或本身就是受保护 token
_LATINISH = rf"(?:{_PROT}|(?=[A-Za-z0-9._/+:-]*[A-Za-z])[A-Za-z0-9][A-Za-z0-9._/+:-]*)"

# 零宽字符
_ZERO_WIDTH_RE = re.compile(r"[\u200b-\u200d\ufeff]")
_TRAILING_CLOSERS = set('"\')]}）】》〉」』”’')

# 表情符号（含肤色修饰、零宽连接序列、旗帜等变体）
_EMOJI_RE = re.compile(
    "["
    "\U0001f1e6-\U0001f1ff"  # 区域指示符（旗帜）
    "\U0001f300-\U0001f5ff"  # 杂项符号与象形文字
    "\U0001f600-\U0001f64f"  # 表情符号
    "\U0001f680-\U0001f6ff"  # 交通与地图符号
    "\U0001f700-\U0001f77f"  # 炼金符号
    "\U0001f780-\U0001f7ff"  # 几何图形扩展
    "\U0001f800-\U0001f8ff"  # 补充箭头
    "\U0001f900-\U0001f9ff"  # 补充符号与象形文字
    "\U0001fa00-\U0001fa6f"  # 扩展象形文字
    "\U0001fa70-\U0001faff"  # 扩展象形文字补充
    "\u2600-\u27bf"          # 杂项符号/装饰符号/箭头
    "\u2b00-\u2bff"          # 杂项符号与箭头
    "\ufe0f"                # 变体选择符（表情呈现）
    "\u200d"                # 零宽连接符（ZWJ 序列）
    "]+"
)
# 装饰性符号（键盘符号、几何、圈号/括号、特殊标点等，朗读无实义）
_DECORATIVE_SYMBOL_RE = re.compile(
    "["
    "\u2300-\u23ff"   # 杂项技术符号（⏰⌚⏳ 等）
    "\u25a0-\u25ff"   # 几何图形（■□▲△ 等）
    "\u2b00-\u2bff"   # 杂项符号与箭头
    "\u00a9\u00ae"    # © ®
    "]+"
)
# 需删除的朗读无实义装饰符（不含引号：引号由 _QUOTES_RE 单独处理，
# 结构括号转引号后还需二次清理）。不删 `_`（可见下划线已在
# _normalize_visible_underscores 转空格，剩余下划线只属于保护占位符
# ___PROTn___，必须保留）。
_STRIP_CHARS_RE = re.compile(
    r"[\*#~·•◦∙●○◎◇◆★☆※→←↑↓↔↕]"
)
# 引号与反引号（原输入直接删；结构括号转换生成的引号在管道尾部再删一次）
_QUOTES_RE = re.compile(r"[\"'`´‘’“”]")
# 反引号包裹的内联代码整体删除（连同反引号），避免朗读代码原文
_INLINE_CODE_RE = re.compile(r"`[^`\n]+`")
# 英文缩写撇号（字母之间）：朗读必须保留，先换占位符再恢复
_APOSTROPHE_RE = re.compile(r"(?<=[A-Za-z])['’](?=[A-Za-z])")
# 占位符刻意避开 `_` 与字母（_normalize_visible_underscores 会处理下划线，
# 且下划线归一化按“含拉丁字母才算词”分段，纯标点哨兵不会被误判）。
_APOSTROPHE_KEEP = "～ＡＰＯＳ～"
# 分隔符（斜杠/反斜杠/竖线）：两侧为汉字/假名时替换为顿号（启用/禁用→启用、禁用），
# 其余替换为空格（A/B→A B）；URL/日期/文件路径等受保护内容除外。
_SEPARATOR_CJK_RE = re.compile(
    rf"(?<=[{_CJK_CHARS}])[\\/|](?=[{_CJK_CHARS}])"
)
_SEPARATOR_SPACE_RE = re.compile(r"[\\/|]")


def normalize_tts_text(text: str) -> str:
    """对 TTS 输入做鲁棒性正则化（纯清洗，不做语义展开）。"""
    text = _base_cleanup(text)
    text = _normalize_markdown_and_lines(text)
    text = _normalize_flow_arrows(text)
    text, protected = _protect_spans(text)

    # 删除朗读无实义的符号（表情/装饰符/引号等）与斜杠等分隔符替换；
    # 受保护内容（URL/路径/版本号等）不受影响。
    text = _strip_speech_noise(text)
    # 可见下划线→空格（不能放在噪声清理前：撇号/日期等占位符含下划线，
    # 提前替换会破坏占位符，导致保护内容无法还原）。
    text = _normalize_visible_underscores(text)
    text = _normalize_spaces(text)
    text = _normalize_structural_punctuation(text)
    text = _normalize_repeated_punctuation(text)
    text = _normalize_spaces(text)
    # 结构括号（【】等）在上一阶段转成双引号，这里二次清理；随后恢复占位符。
    text = _restore_noise_placeholders(text)

    text = _restore_spans(text, protected)
    text = text.strip()
    return _ensure_terminal_punctuation_by_line(text)


def _strip_speech_noise(text: str) -> str:
    """删除朗读无实义的装饰符号，并把斜杠等分隔符替换成可读分隔。

    处理顺序（全部在受保护 span 替换为 ``___PROTn___`` 占位之后执行，
    因此 URL/邮箱/@提及/文件路径/日期等含斜杠或点号的内容不受影响）：
    1. 整体删除反引号包裹的内联代码（``code``），避免朗读代码原文；
    2. 删除表情符号（含 ZWJ/变体序列）与装饰性几何/技术符号；
    3. 保护英文缩写撇号（don't，占位符延迟到管道尾部恢复）；
    4. 删除引号（``"`` ``'`` ``“”`` ``‘’``）与强调符（``*``/``#``/``~``）、
       项目符号点、箭头等装饰符；删除后相邻中文字符直接相连；
    5. 斜杠/反斜杠/竖线：两侧均为中文字符时替换为顿号
       （``启用/禁用``→``启用、禁用``），其余替换为空格（``A/B``→``A B``）。
    """
    if not text:
        return text
    text = _INLINE_CODE_RE.sub("", text)
    text = _EMOJI_RE.sub("", text)
    text = _DECORATIVE_SYMBOL_RE.sub("", text)
    text = _APOSTROPHE_RE.sub(_APOSTROPHE_KEEP, text)
    text = _QUOTES_RE.sub("", text)
    text = _STRIP_CHARS_RE.sub("", text)
    text = _SEPARATOR_CJK_RE.sub("、", text)
    text = _SEPARATOR_SPACE_RE.sub(" ", text)
    return text


def _restore_noise_placeholders(text: str) -> str:
    """恢复撇号占位符（必须在结构括号转引号的二次清理之后调用）。"""
    if not text:
        return text
    text = _QUOTES_RE.sub("", text)
    text = text.replace(_APOSTROPHE_KEEP, "'")
    return text


def _base_cleanup(text: str) -> str:
    text = text.replace("\r\n", "\n").replace("\r", "\n").replace("\u3000", " ")
    text = _ZERO_WIDTH_RE.sub("", text)
    cleaned = []
    for ch in text:
        cat = unicodedata.category(ch)
        if ch in "\n\t " or not cat.startswith("C"):
            cleaned.append(ch)
    return "".join(cleaned)


def _normalize_markdown_and_lines(text: str) -> str:
    # Markdown 链接：[text](url) -> text url
    text = re.sub(r"\[([^\[\]]+?)\]\((https?://[^)\s]+)\)", r"\1 \2", text)
    lines = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        line = re.sub(r"^#{1,6}\s+", "", line)   # 标题
        line = re.sub(r"^>\s+", "", line)        # 引用
        line = re.sub(r"^[-*+]\s+", "", line)    # 无序列表
        line = re.sub(r"^\d+[.)]\s+", "", line)  # 有序列表
        lines.append(line)
    if not lines:
        return ""
    merged = [lines[0]]
    for line in lines[1:]:
        previous = merged[-1]
        merged[-1] = _ensure_terminal_punctuation(previous)
        merged.append(line)
    return "".join(merged)


def _protect_spans(text: str) -> tuple[str, list[str]]:
    protected: list[str] = []

    def repl(match: re.Match[str]) -> str:
        idx = len(protected)
        protected.append(match.group(0))
        return f"___PROT{idx}___"

    for pattern in (
        _URL_RE,
        _EMAIL_RE,
        _MENTION_RE,
        _REDDIT_RE,
        _DOT_TOKEN_RE,
        _DATE_TOKEN_RE,
        _FILELIKE_RE,
    ):
        text = pattern.sub(repl, text)
    return text, protected


def _restore_spans(text: str, protected: list[str]) -> str:
    for idx, original in enumerate(protected):
        text = text.replace(f"___PROT{idx}___", original)
    return text


def _normalize_visible_underscores(text: str) -> str:
    parts = re.split(rf"({_PROT})", text)
    return "".join(
        part if re.fullmatch(_PROT, part) else part.replace("_", " ")
        for part in parts
    )


def _normalize_flow_arrows(text: str) -> str:
    return re.sub(
        r"\s*(?:<[-=]+>|[-=]+>|<[-=]+|[→←↔⇒⇐⇔⟶⟵⟷⟹⟸⟺↦↤↪↩])\s*",
        "，",
        text,
    )


def _normalize_spaces(text: str) -> str:
    # 统一空白
    text = re.sub(r"[ \t\r\f\v]+", " ", text)
    # 汉字 / 日文片段内部：删除空格
    text = re.sub(rf"({_CJK})\s+(?={_CJK})", r"\1", text)
    # 汉字 / 日文 与纯数字之间：删除空格
    text = re.sub(rf"({_CJK})\s+(?=\d)", r"\1", text)
    text = re.sub(rf"(\d)\s+(?={_CJK})", r"\1", text)
    # 汉字 / 日文 与拉丁字母类 token / protected token 相邻：保留或补 1 个空格
    text = re.sub(rf"({_CJK})(?=({_LATINISH}))", r"\1 ", text)
    text = re.sub(rf"(({_LATINISH}))(?={_CJK})", r"\1 ", text)
    # 再压一遍连续空格
    text = re.sub(r" {2,}", " ", text)
    # 中文标点前后不保留空格
    text = re.sub(r"\s+([，。！？；：、”’」』】）》])", r"\1", text)
    text = re.sub(r"([（【「『《“‘])\s+", r"\1", text)
    text = re.sub(r"([，。！？；：、])\s*", r"\1", text)
    # ASCII 标点前不留空格
    text = re.sub(r"\s+([,.;!?])", r"\1", text)
    return re.sub(r" {2,}", " ", text).strip()


def _normalize_structural_punctuation(text: str) -> str:
    # 各类结构性括号：统一转成双引号包裹内容
    text = re.sub(r"\[\s*([^\[\]]+?)\s*\]", r'"\1"', text)
    text = re.sub(r"\{\s*([^{}]+?)\s*\}", r'"\1"', text)
    text = re.sub(r"[【〖『「]\s*([^】〗』」]+?)\s*[】〗』」]", r'"\1"', text)

    # 《》只处理独立标题，不处理嵌入式标题
    text = re.sub(
        r"(^|[。！？!?；;]\s*)《([^》]+)》(?=\s*(?:___PROT\d+___|[—–―-]{2,}|$|[。！？!?；;，,]))",
        r"\1\2",
        text,
    )

    # 流程 / 映射箭头：转成中文逗号
    text = _normalize_flow_arrows(text)

    # 长破折号 / 多连字符：转句边界
    text = re.sub(r"\s*(?:—|–|―|-){2,}\s*", "。", text)
    return text


def _normalize_repeated_punctuation(text: str) -> str:
    # 省略号 / 连续句点
    text = re.sub(r"(?:\.{3,}|…{2,}|……+)", "。", text)
    # 同类重复标点
    text = re.sub(r"[。．]{2,}", "。", text)
    text = re.sub(r"[，,]{2,}", "，", text)
    text = re.sub(r"[!！]{2,}", "！", text)
    text = re.sub(r"[?？]{2,}", "？", text)

    # 混合问叹号：收敛到 ？！
    def _mixed_qe(match: re.Match[str]) -> str:
        s = match.group(0)
        has_q = any(ch in s for ch in "?？")
        has_e = any(ch in s for ch in "!！")
        if has_q and has_e:
            return "？！"
        return "？" if has_q else "！"

    text = re.sub(r"[!?！？]{2,}", _mixed_qe, text)
    return text


def _ensure_terminal_punctuation(text: str) -> str:
    if not text:
        return text
    index = len(text) - 1
    while index >= 0 and text[index].isspace():
        index -= 1
    while index >= 0 and text[index] in _TRAILING_CLOSERS:
        index -= 1
    if index >= 0 and unicodedata.category(text[index]).startswith("P"):
        return text
    return text + "。"


def _ensure_terminal_punctuation_by_line(text: str) -> str:
    if not text:
        return text
    lines = text.split("\n")
    normalized_lines = [
        _ensure_terminal_punctuation(line.strip()) if line.strip() else ""
        for line in lines
    ]
    return "\n".join(normalized_lines).strip()


# ---------------------------------------------------------------------------
# WeTextProcessing 可选封装（需要 pynini，未安装时自动降级）
# ---------------------------------------------------------------------------

_ENGLISH_VOICES = frozenset({"Trump", "Ava", "Bella", "Adam", "Nathan"})
_ZH_WETEXT_KEEP_HYPHEN = "___KEEP_HYPHEN_BEFORE_ZH_WETEXT___"


class WeTextNormalizer:
    """WeTextProcessing 中文/英文语义归一化封装。

    仅当环境中可导入 `tn` 模块（pynini + WeTextProcessing 已安装）时可用；
    否则 `available` 为 False，调用方应回退到 `normalize_tts_text`。
    """

    def __init__(self) -> None:
        self._normalizers: dict[str, Any] | None = None
        self._available: bool | None = None

    @property
    def available(self) -> bool:
        if self._available is None:
            try:
                import tn  # noqa: F401  # pynini + WeTextProcessing 的顶层包
                self._available = True
            except Exception:
                self._available = False
        return self._available

    def _ensure_loaded(self) -> dict[str, Any]:
        if self._normalizers is not None:
            return self._normalizers
        from tn.chinese.normalizer import Normalizer as ZhNormalizer
        from tn.english.normalizer import Normalizer as EnNormalizer

        self._normalizers = {
            "zh": ZhNormalizer(overwrite_cache=False),
            "en": EnNormalizer(overwrite_cache=False),
        }
        return self._normalizers

    def normalize(self, *, text: str, language: str) -> str:
        """按语言执行语义归一化；language 需为 'zh' 或 'en'。"""
        if not self.available:
            raise RuntimeError("WeTextProcessing 未安装（需要 pynini），无法执行语义归一化。")
        normalizers = self._ensure_loaded()
        if language not in normalizers:
            raise ValueError(f"不支持的文本归一化语言：{language}")
        return normalizers[language].normalize(text) if text else ""


def resolve_text_normalization_language(*, text: str, voice: str) -> str:
    """按文本内容（及音色）推断归一化语言。"""
    if re.search(r"[\u3400-\u9fff]", text):
        return "zh"
    if re.search(r"[A-Za-z]", text):
        return "en"
    if voice in _ENGLISH_VOICES:
        return "en"
    return "zh"


def _rewrite_hyphens_before_zh_wetext(text: str) -> str:
    """避免中文 WeText 把非数字连字符读成"减"。"""
    rewritten = str(text or "")
    if "-" not in rewritten:
        return rewritten
    # 保留文本开头的负号，如 `-2`
    rewritten = re.sub(
        r"(^\s*)-\s*(?=\d)",
        rf"\1{_ZH_WETEXT_KEEP_HYPHEN}",
        rewritten,
    )
    # 保留常见分隔符后的负号，如 `x=-2` / `(-2)`
    rewritten = re.sub(
        r"([=:+*/,(，：:；;（【\[{])\s*-\s*(?=\d)",
        rf"\1{_ZH_WETEXT_KEEP_HYPHEN}",
        rewritten,
    )
    # 保留中文语境负号，如 `为-2` / `计算-2`
    rewritten = re.sub(
        r"([\u3400-\u9fff])\s*-\s*(?=\d)",
        rf"\1{_ZH_WETEXT_KEEP_HYPHEN}",
        rewritten,
    )
    # 保留数字范围/日期，如 `10-3` / `2024-05-01`
    rewritten = re.sub(
        r"(\d)\s*-\s*(?=\d)",
        rf"\1{_ZH_WETEXT_KEEP_HYPHEN}",
        rewritten,
    )
    # 中文复合词间的连字符转停顿边界
    rewritten = re.sub(
        r"([\u3400-\u9fff])\s*-\s*(?=[\u3400-\u9fff])",
        r"\1，",
        rewritten,
    )
    # 其余词内连字符压成空格
    rewritten = re.sub(
        r"([^\s-])\s*-\s*(?=[^\s-])",
        r"\1 ",
        rewritten,
    )
    rewritten = re.sub(r" {2,}", " ", rewritten).strip()
    return rewritten.replace(_ZH_WETEXT_KEEP_HYPHEN, "-")


# ---------------------------------------------------------------------------
# 合成前文本预处理管道
# ---------------------------------------------------------------------------


def prepare_tts_request_texts(
    *,
    text: str,
    prompt_text: str = "",
    voice: str = "",
    enable_wetext: bool,
    enable_normalize_tts_text: bool = True,
    wetext_normalizer: WeTextNormalizer | None = None,
) -> dict[str, Any]:
    """合成前的文本预处理：稳健清洗（+ 可选 WeText 语义归一化）。

    返回结构与官方 `text_normalization_pipeline.prepare_tts_request_texts` 一致，
    便于上层统一消费。
    """
    raw_text = str(text or "")
    raw_prompt_text = str(prompt_text or "")

    normalization_stages: list[str] = []
    normalization_language = ""
    intermediate_text = raw_text
    intermediate_prompt_text = raw_prompt_text

    if enable_normalize_tts_text and enable_wetext:
        pre_robust_text = normalize_tts_text(raw_text)
        pre_robust_prompt_text = normalize_tts_text(raw_prompt_text) if raw_prompt_text else ""
        if pre_robust_text != raw_text:
            LOGGER.info(
                "normalized text chars_before=%d chars_after=%d stage=robust_pre",
                len(raw_text), len(pre_robust_text),
            )
        intermediate_text = pre_robust_text
        intermediate_prompt_text = pre_robust_prompt_text
        normalization_stages.append("robust_pre")

    if enable_wetext:
        if wetext_normalizer is None or not wetext_normalizer.available:
            raise RuntimeError(
                "enable_wetext=True 但 WeTextProcessing 不可用；"
                "请安装 pynini 与 WeTextProcessing，或将 enable_wetext 设为 False。"
            )
        wetext_input_text = intermediate_text
        wetext_input_prompt_text = intermediate_prompt_text
        normalization_language = resolve_text_normalization_language(
            text=wetext_input_text, voice=voice
        )
        if normalization_language == "zh":
            rewritten = _rewrite_hyphens_before_zh_wetext(wetext_input_text)
            if rewritten != wetext_input_text:
                LOGGER.info("rewrote zh wetext text hyphens stage=zh_wetext_hyphen_guard")
            wetext_input_text = rewritten
            if wetext_input_prompt_text:
                wetext_input_prompt_text = _rewrite_hyphens_before_zh_wetext(
                    wetext_input_prompt_text
                )
        intermediate_text = wetext_normalizer.normalize(
            text=wetext_input_text, language=normalization_language
        )
        if wetext_input_prompt_text:
            intermediate_prompt_text = wetext_normalizer.normalize(
                text=wetext_input_prompt_text, language=normalization_language
            )
        normalization_stages.append(
            f"wetext:{normalization_language}" if normalization_language else "wetext"
        )

    final_text = intermediate_text
    final_prompt_text = intermediate_prompt_text
    if enable_normalize_tts_text:
        final_text = normalize_tts_text(intermediate_text)
        final_prompt_text = (
            normalize_tts_text(intermediate_prompt_text) if intermediate_prompt_text else ""
        )
        robust_stage_name = "robust_post" if enable_wetext else "robust"
        normalization_stages.append(robust_stage_name)

    return {
        "text": final_text,
        "prompt_text": final_prompt_text,
        "normalized_text": final_text,
        "normalized_prompt_text": final_prompt_text,
        "normalization_method": "+".join(normalization_stages) if normalization_stages else "none",
        "text_normalization_language": normalization_language,
        "text_normalization_enabled": bool(enable_wetext or enable_normalize_tts_text),
        "wetext_processing_enabled": bool(enable_wetext),
        "normalize_tts_text_enabled": bool(enable_normalize_tts_text),
    }
