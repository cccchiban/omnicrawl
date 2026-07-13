"""Memory 分类、摘要与检索排序的纯逻辑。

本模块不读写磁盘；`MemoryStore` 负责 Markdown/index 持久化，并在需要时调用这里的策略。
"""

from __future__ import annotations

import difflib
import re
from datetime import datetime
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .memory import MemoryIndexEntry


DEFAULT_STORAGE_DIRECTORIES = (
    "user-preferences/general",
    "project-context/general",
    "task-history/general",
    "code-knowledge/general",
    "error-lessons/general",
    "external-context/general",
)

_CLASSIFICATION_RULES: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "user-preferences/communication-style",
        ("偏好", "喜欢", "不喜欢", "习惯", "沟通", "回答风格", "输出", "称呼"),
    ),
    (
        "project-context/general",
        ("项目", "仓库", "架构", "约束", "配置", "入口", "workspace", "repository"),
    ),
    (
        "code-knowledge/general",
        ("代码", "函数", "类", "模块", "接口", "实现", "源码", "class", "function"),
    ),
    (
        "error-lessons/general",
        ("错误", "失败", "异常", "修复", "调试", "踩坑", "bug", "error", "exception"),
    ),
    (
        "external-context/general",
        ("api", "外部服务", "环境变量", "域名", "权限", "token", "模型", "网关"),
    ),
    (
        "task-history/general",
        ("任务", "完成", "决策", "待办", "跟进", "历史", "计划"),
    ),
)


def classify_storage_directory(content: str) -> str:
    """按关键词启发式为记忆选择默认存储目录。"""

    text = content.lower()
    for directory, keywords in _CLASSIFICATION_RULES:
        if any(keyword in text for keyword in keywords):
            return directory
    return "task-history/general"


def make_summary(content: str, max_chars: int = 120) -> str:
    """从正文生成低成本摘要，供搜索结果和索引使用。"""

    text = re.sub(r"```.*?```", "", content, flags=re.DOTALL)
    text = re.sub(r"^\s{0,3}[-*+>#]+\s*", "", text, flags=re.MULTILINE)
    text = re.sub(r"\s+", " ", text).strip()
    if not text:
        return ""

    match = re.search(r"[。！？!?；;]", text)
    if match is not None and match.end() <= max_chars:
        text = text[: match.end()]
    if len(text) <= max_chars:
        return text
    return text[: max_chars - 1].rstrip() + "…"


def merge_memory_content(old_content: str, new_content: str) -> str:
    """合并近似重复记忆的正文，避免无意义的重复落盘。"""

    old = old_content.strip()
    new = new_content.strip()
    if not old:
        return new
    if normalize_for_compare(new) in normalize_for_compare(old):
        return old
    return f"{old}\n\n补充：{new}"


def directory_match_score(entry: "MemoryIndexEntry", directory: str) -> float:
    """计算记忆条目与候选目录的匹配强度。

    调用方应先完成目录归一化；本函数不做 I/O，也不反向依赖 MemoryStore。
    """

    if entry.storage_directory == directory:
        return 3.0
    if entry.storage_directory.startswith(f"{directory}/") or directory.startswith(
        f"{entry.storage_directory}/"
    ):
        return 2.0
    if directory in entry.related_directories:
        return 1.5
    if any(
        item.startswith(f"{directory}/") or directory.startswith(f"{item}/")
        for item in entry.related_directories
    ):
        return 1.0
    return 0.0


def directories_overlap(left: list[str], right: list[str]) -> bool:
    left_set = set(left)
    right_set = set(right)
    if left_set & right_set:
        return True
    return any(
        item.startswith(f"{other}/") or other.startswith(f"{item}/")
        for item in left_set
        for other in right_set
    )


def text_similarity(left: str, right: str) -> float:
    left_norm = normalize_for_compare(left)
    right_norm = normalize_for_compare(right)
    if not left_norm or not right_norm:
        return 0.0
    return difflib.SequenceMatcher(a=left_norm, b=right_norm).ratio()


def extract_search_tokens(text: str) -> set[str]:
    lowered = text.lower()
    tokens = set(re.findall(r"[a-z0-9_+-]{2,}", lowered))

    for chunk in re.findall(r"[\u4e00-\u9fff]{2,}", lowered):
        if len(chunk) <= 8:
            tokens.add(chunk)
        for index in range(len(chunk) - 1):
            tokens.add(chunk[index : index + 2])
        for index in range(len(chunk) - 2):
            tokens.add(chunk[index : index + 3])

    return tokens


def score_search_entry(
    entry: "MemoryIndexEntry",
    query: str,
    candidate_directories: list[str],
    *,
    now: datetime | None = None,
) -> float:
    """计算搜索候选分；可注入 now 便于单测。"""

    score = 0.0
    haystack = " ".join(
        [
            entry.summary,
            entry.storage_directory,
            " ".join(entry.related_directories),
        ]
    ).lower()

    query_tokens = extract_search_tokens(query)
    if query_tokens:
        token_hits = sum(1 for token in query_tokens if token in haystack)
        score += token_hits / len(query_tokens) * 10
        if query.lower() and query.lower() in haystack:
            score += 5

    for directory in candidate_directories:
        score += directory_match_score(entry, directory) * 3

    # 轻微倾向被反复使用或较新的记忆，但不让它盖过文本相关度。
    score += min(entry.touch_count, 10) * 0.05
    current = now or datetime.now().astimezone()
    age_days = max((current - entry.timestamp).total_seconds() / 86400, 0)
    score += max(0.0, 1.0 - min(age_days, 30) / 30) * 0.1
    return score


def score_related_entry(
    entry: "MemoryIndexEntry",
    directories: set[str],
    depth: int,
) -> float:
    best = max((directory_match_score(entry, directory) for directory in directories), default=0.0)
    if best <= 0:
        return 0.0
    return best / (depth + 1)


def normalize_for_compare(text: str) -> str:
    return re.sub(r"[\W_]+", "", text.lower(), flags=re.UNICODE)


# 兼容 MemoryStore 既有私有函数名。
_classify_storage_directory = classify_storage_directory
_make_summary = make_summary
_merge_memory_content = merge_memory_content
_directory_match_score = directory_match_score
_directories_overlap = directories_overlap
_text_similarity = text_similarity
_extract_search_tokens = extract_search_tokens
_normalize_for_compare = normalize_for_compare


__all__ = [
    "DEFAULT_STORAGE_DIRECTORIES",
    "classify_storage_directory",
    "directory_match_score",
    "directories_overlap",
    "extract_search_tokens",
    "make_summary",
    "merge_memory_content",
    "normalize_for_compare",
    "score_related_entry",
    "score_search_entry",
    "text_similarity",
]
