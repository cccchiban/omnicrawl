"""Qt 对话导出保存工具。"""

from __future__ import annotations

from datetime import datetime
from pathlib import Path


def save_chat_export(
    markdown_text: str,
    *,
    workspace_root: Path,
    now: datetime | None = None,
) -> Path:
    """将前端导出的 Markdown 对话记录保存到工作区临时文件目录。

    导出内容属于用户主动产生的一次性文件，默认放入 `.agent_tmp/files/`，
    既不会污染项目根目录，也会沿用项目已有的临时目录清理约定。
    """

    root = workspace_root.expanduser().resolve()
    export_dir = root / ".agent_tmp" / "files"
    export_dir.mkdir(parents=True, exist_ok=True)

    timestamp = (now or datetime.now()).strftime("%Y%m%d_%H%M%S")
    path = export_dir / f"chat_export_{timestamp}.md"
    path.write_text(markdown_text, encoding="utf-8")
    return path
