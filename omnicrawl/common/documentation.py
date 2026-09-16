from __future__ import annotations

from pathlib import Path, PurePosixPath


BUNDLED_DOC_URI_PREFIX = "omnicrawl://docs/"


class BundledDocumentationError(ValueError):
    """内置文档 URI 无效或目标文档不可用。"""


def bundled_docs_dir() -> Path:
    """返回随 OmniCrawl Python 包发布的只读文档目录。"""

    return Path(__file__).resolve().parent.parent / "docs"


def bundled_doc_names() -> list[str]:
    """列出安装包内可读取的 Markdown 文档文件名。"""

    docs_dir = bundled_docs_dir()
    if not docs_dir.is_dir():
        return []
    return sorted(
        (path.name for path in docs_dir.glob("*.md") if path.is_file()),
        key=str.lower,
    )


def bundled_doc_uri(name: str) -> str:
    """根据已验证的文档文件名构造稳定 URI。"""

    _validate_doc_name(name)
    return f"{BUNDLED_DOC_URI_PREFIX}{name}"


def resolve_bundled_doc_uri(uri: str) -> Path:
    """将内置文档 URI 解析为包内文件，并拒绝目录穿越。"""

    if not uri.startswith(BUNDLED_DOC_URI_PREFIX):
        raise BundledDocumentationError(f"不是有效的内置文档 URI：{uri}")
    name = uri[len(BUNDLED_DOC_URI_PREFIX) :]
    _validate_doc_name(name)
    path = bundled_docs_dir() / name
    if not path.is_file():
        raise BundledDocumentationError(f"内置文档不存在：{uri}")
    return path


def read_bundled_doc(uri: str) -> str:
    """读取 UTF-8 内置文档。"""

    try:
        return resolve_bundled_doc_uri(uri).read_text(encoding="utf-8")
    except OSError as exc:
        raise BundledDocumentationError(f"读取内置文档失败：{uri}，{exc}") from exc


def _validate_doc_name(name: str) -> None:
    candidate = PurePosixPath(name)
    if (
        not name
        or candidate.name != name
        or name in {".", ".."}
        or candidate.suffix.lower() != ".md"
    ):
        raise BundledDocumentationError(f"内置文档 URI 仅允许单个 Markdown 文件名：{name}")
