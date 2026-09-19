#!/usr/bin/env python3
"""生成项目列表存储的对照数据集：期望值全部来自 Python 真实现。

两组：
- `pure`：纯函数（展示名清洗、路径键、隔离工作树判定、扫描排除、路径归一、git 根、条目解析）；
- `store`：在真实临时目录上跑一遍 ProjectStore 流程（创建 / 导入 / 改名 / 置顶 / 移除 / 扫描 / 总览），
  记录每步返回值与最终 `projects.json` 字节。

路径里的临时根一律替换成 `<ROOT>` / `<TEMP>` 占位，两侧各自用同一相对结构重放。
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/project_parity.json"

if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.state import project as project_module  # noqa: E402
from omnicrawl.state.project import ProjectEntry, ProjectStore, ProjectStoreError  # noqa: E402

ROOT_PLACEHOLDER = "<ROOT>"
TEMP_PLACEHOLDER = "<TEMP>"
# 用例统一用固定时刻：两侧各自跑一遍，落盘时间戳必须逐字相同。
NOW = datetime(2026, 1, 2, 3, 4, 5, 123456, tzinfo=timezone.utc)
TEMP_ROOT = Path(tempfile.gettempdir()).resolve()
WORK_ROOT = (
    Path(tempfile.gettempdir()) / f"omnicrawl-project-parity-{os.getpid()}"
).resolve()


def mask(text: object) -> str:
    # 顺序要紧：WORK_ROOT 在 TEMP_ROOT 之下，先长后短才不会被提前吃掉；
    # JSON 文本里的路径是转义形态（`\\`），要一并替换。
    raw = str(text)
    for root_path, placeholder in ((WORK_ROOT, ROOT_PLACEHOLDER), (TEMP_ROOT, TEMP_PLACEHOLDER)):
        base = str(root_path)
        raw = raw.replace(base.replace("\\", "\\\\"), placeholder)
        raw = raw.replace(base, placeholder)
    return raw


def outcome(call) -> dict:
    """把一次调用归一成 `{ok/error}`；路径按占位符掩名。"""
    try:
        value = call()
    except ProjectStoreError as exc:
        return {"ok": False, "error": mask(exc)}
    except Exception as exc:  # noqa: BLE001
        return {"ok": False, "error": f"{type(exc).__name__}: {mask(exc)}"}
    return {"ok": True, "value": value}


def mask_value(value):
    if isinstance(value, ProjectEntry):
        return {key: mask_value(item) for key, item in value.to_dict().items()}
    if isinstance(value, list):
        return [mask_value(item) for item in value]
    if isinstance(value, dict):
        return {key: mask_value(item) for key, item in value.items()}
    if isinstance(value, (str, Path)):
        return mask(value)
    return value


def pure_cases(probe: dict) -> dict:
    dates = {
        "created_at": "2026-01-02T03:04:05+00:00",
        "updated_at": "2026-01-02T03:04:05.123456+00:00",
    }
    payload = {
        "clean_name": [
            {"input": "  我的 项目 ", "expected": project_module._clean_project_name("  我的 项目 ")},
            {"input": "\t alpha \n", "expected": project_module._clean_project_name("\t alpha \n")},
            {"input": "a" * 80, "expected": project_module._clean_project_name("a" * 80)},
            {"input": "a" * 81, "expected": project_module._clean_project_name("a" * 81)},
            {"input": "中" * 82, "expected": project_module._clean_project_name("中" * 82)},
        ],
        "clean_name_errors": [
            {"input": text, "error": outcome(lambda text=text: project_module._clean_project_name(text))["error"]}
            for text in ["", "   ", "\t\n"]
        ],
        "path_key": [
            {"input": text, "expected": project_module._path_key(text)}
            for text in [
                "C:\\Users\\Someone\\Proj",
                "c:/users/someone/proj",
                "/tmp/MixedCase",
                "relative/Dir",
            ]
        ],
        "under_agent_worktrees": [
            {
                "input": text,
                "expected": project_module._under_agent_worktrees(text),
            }
            for text in [
                str(Path.home() / ".omnicrawl" / "agent-worktrees" / "aw-123"),
                str(Path.home() / ".omnicrawl" / "agent-worktrees"),
                str(Path.home() / ".omnicrawl" / "other"),
                str(Path.home()),
                probe["work_root"],
            ]
        ],
        "normalize_path": [
            {"input": text, "expected": mask(project_module._normalize_project_path(text))}
            for text in [
                probe["work_root"],
                str(Path(probe["work_root"]) / "data"),
                str(Path(probe["work_root"]) / "data" / ".." / "data" / "x"),
            ]
        ],
        "normalize_path_errors": [
            {"input": "   ", "error": outcome(lambda: project_module._normalize_project_path("   "))["error"]}
        ],
        "entry_from_dict": [],
        "entry_from_dict_errors": [],
        "scan_excluded": [
            {"input": text, "expected": project_module._is_scan_excluded(text)}
            for text in [
                str(WORK_ROOT / "data"),
                str(WORK_ROOT / "missing-dir"),
                str(TEMP_ROOT / f"tmp-project-parity-{os.getpid()}"),
                str(TEMP_ROOT),
                str(Path.home() / "site-packages" / "pkg"),
                probe["git_repo"],
                probe["git_broken"],
            ]
        ],
        "git_root": [
            {"input": text, "expected": mask_or_none(project_module._git_root(text))}
            for text in [
                probe["git_repo"],
                probe["git_repo_sub"],
                probe["git_broken"],
                probe["git_worktree"],
                probe["git_worktree_sub"],
                probe["plain_dir"],
            ]
        ],
        "expand_vars": [
            {"input": text, "expected": project_module.os.path.expandvars(text)}
            for text in [
                "plain/path",
                "$__OMNICRAWL_MISSING__/x",
                "${__OMNICRAWL_MISSING__}/x",
                "%__OMNICRAWL_MISSING__%/x",
            ]
        ],
    } | build_entry_cases(dates)
    mask_case_inputs(payload)
    return payload


def mask_case_inputs(payload: dict) -> None:
    """把用例输入与期望值里的临时根换成占位符，两侧才能用同一份脚本重放。"""
    for items in payload.values():
        for item in items:
            for key in ("input", "expected"):
                raw = item.get(key)
                if isinstance(raw, str):
                    item[key] = mask(raw)
                elif isinstance(raw, dict):
                    item[key] = {
                        field: (mask(value) if isinstance(value, str) else value)
                        for field, value in raw.items()
                    }


def mask_or_none(value):
    return None if value is None else mask(value)


def build_entry_cases(dates: dict) -> dict:
    valid = [
        {"name": "alpha", "path": WORK_ROOT / "data", **dates, "pinned": True, "source": "created"},
        {"name": "  spaced  name ", "path": WORK_ROOT / "data", **dates},
        {"name": "x" * 81, "path": WORK_ROOT / "data", **dates, "source": "imported"},
    ]
    invalid = [
        {},
        {"name": "a", "path": "", **dates},
        {"name": "", "path": str(WORK_ROOT), **dates},
        {"name": "a", "path": str(WORK_ROOT), **dates, "source": "  "},
        {"name": "a", "path": str(WORK_ROOT), "created_at": "", "updated_at": dates["updated_at"]},
        {"name": "a", "path": str(WORK_ROOT), "created_at": "not-a-date", "updated_at": dates["updated_at"]},
    ]

    cases = {"entry_from_dict": [], "entry_from_dict_errors": []}
    for payload in valid:
        encoded = {key: (str(value) if isinstance(value, Path) else value) for key, value in payload.items()}
        entry = ProjectEntry.from_dict(encoded)
        cases["entry_from_dict"].append(
            {"input": mask_value(encoded), "expected": mask_value(entry)}
        )
    for payload in invalid:
        encoded = {key: (str(value) if isinstance(value, Path) else value) for key, value in payload.items()}
        cases["entry_from_dict_errors"].append(
            {"input": mask_value(encoded), "error": outcome(lambda e=encoded: ProjectEntry.from_dict(e))["error"]}
        )
    return cases


def make_layout() -> dict:
    """准备真实目录：普通目录、git 仓库（含 HEAD）、断裂 .git、worktree 形态 .git 文件。"""
    if WORK_ROOT.exists():
        shutil.rmtree(WORK_ROOT, ignore_errors=True)
    layout = {
        "work_root": str(WORK_ROOT),
        "plain_dir": str(WORK_ROOT / "plain"),
        "git_repo": str(WORK_ROOT / "repo"),
        "git_repo_sub": str(WORK_ROOT / "repo" / "sub" / "deep"),
        "git_broken": str(WORK_ROOT / "broken"),
        "git_worktree": str(WORK_ROOT / "wt"),
        "git_worktree_sub": str(WORK_ROOT / "wt" / "sub"),
    }
    for key in ["plain_dir", "git_repo_sub", "git_broken", "git_worktree_sub"]:
        Path(layout[key]).mkdir(parents=True, exist_ok=True)
    (Path(layout["git_repo"]) / ".git").mkdir(exist_ok=True)
    (Path(layout["git_repo"]) / ".git" / "HEAD").write_text("ref: refs/heads/main\n", encoding="utf-8")
    (Path(layout["git_broken"]) / ".git").mkdir(exist_ok=True)
    (Path(layout["git_worktree"]) / ".git").write_text("gitdir: ../repo/.git/worktrees/wt\n", encoding="utf-8")
    temp_residue = TEMP_ROOT / f"tmp-project-parity-{os.getpid()}"
    temp_residue.mkdir(parents=True, exist_ok=True)
    return layout


def store_cases() -> list[dict]:
    cases: list[dict] = []

    # 一、CRUD 往返：创建 → 改名 → 置顶 → 切换 → 导入 → 移除
    cases.append({"label": "crud_cycle", "ops": [
        {"op": "create_project", "name": "  我的 项目 ", "path": "<ROOT>/data/alpha"},
        {"op": "list_projects"},
        {"op": "rename_project", "path": "<ROOT>/data/alpha", "name": "改过的名字"},
        {"op": "pin_project", "path": "<ROOT>/data/alpha", "pinned": True},
        {"op": "toggle_project_pin", "path": "<ROOT>/data/alpha"},
        {"op": "import_project", "name": "beta", "path": "<ROOT>/data/beta"},
        {"op": "create_project", "name": "已存在的文件", "path": "<ROOT>/data/plain.txt"},
        {"op": "rename_project", "path": "<ROOT>/data/missing", "name": "x"},
        {"op": "pin_project", "path": "<ROOT>/data/missing", "pinned": True},
        {"op": "toggle_project_pin", "path": "<ROOT>/data/missing"},
        {"op": "remove_project", "path": "<ROOT>/data/missing"},
        {"op": "import_project", "name": "nope", "path": "<ROOT>/data/missing"},
        {"op": "remove_project", "path": "<ROOT>/data/beta"},
        {"op": "list_projects"},
    ]})

    # 二、扫描入库：隔离工作树、临时残余、解释器目录与已删目录都要被忽略
    cases.append({"label": "scan_projects", "ops": [
        {"op": "create_project", "name": "显式项目", "path": "<ROOT>/data/explicit"},
        {"op": "scan_projects", "roots": ["<ROOT>/data/scanned-a"], "current": "<ROOT>/data/scanned-b"},
        {"op": "scan_projects", "roots": ["<ROOT>/data/scanned-a"], "current": None},
        {"op": "list_projects"},
    ]})

    # 三、总览聚合：子目录会话归并到 git 仓库根；显式项目保留；scanned 空目录不展示
    cases.append({"label": "project_overview", "ops": [
        {"op": "create_project", "name": "仓库项目", "path": "<ROOT>/repo"},
        {"op": "import_project", "name": "外部项目", "path": "<ROOT>/plain"},
        {"op": "scan_projects", "roots": ["<ROOT>/data/scanned-empty"], "current": None},
        {"op": "overview", "recent_limit": 2, "sessions": [
            {"workspace_root": "<ROOT>/repo/sub/deep", "session_id": "s1", "title": "一",
             "updated_at": "2026-01-02T03:04:05+00:00"},
            {"workspace_root": "<ROOT>/repo", "session_id": "s2", "title": "", "updated_at": None},
            {"workspace_root": "<ROOT>/repo/sub", "session_id": "s3", "title": "三",
             "updated_at": "2026-03-04T05:06:07.500000+00:00"},
            {"workspace_root": "<ROOT>/plain", "session_id": "s4", "title": "四",
             "updated_at": "2026-02-02T00:00:00+00:00"},
            {"workspace_root": "<ROOT>/data/scanned-empty", "session_id": "s5", "title": "五",
             "updated_at": "2026-02-02T00:00:00+00:00"},
            {"workspace_root": "", "session_id": "s6", "title": "空目录", "updated_at": None},
            {"workspace_root": "<ROOT>/missing-dir", "session_id": "s7", "title": "已删目录",
             "updated_at": None},
        ]},
    ]})

    # 四、损坏的 projects.json：非 JSON 与顶层形状错误
    cases.append({"label": "broken_file", "ops": [
        {"op": "write_file", "text": "{not json"},
        {"op": "list_projects"},
    ]})
    cases.append({"label": "wrong_top_level", "ops": [
        {"op": "write_file", "text": "{\"projects\": 3}"},
        {"op": "list_projects"},
    ]})
    return cases


def run_store_case(spec: dict) -> dict:
    root = WORK_ROOT / spec["label"]
    store = ProjectStore(root)
    results: list[dict] = []

    for op in spec["ops"]:
        kind = op["op"]
        if kind == "write_file":
            store.root.mkdir(parents=True, exist_ok=True)
            (store.root / "projects.json").write_text(op["text"], encoding="utf-8")
            results.append({"op": kind, "ok": True, "value": None})
            continue
        if kind == "list_projects":
            results.append({"op": kind, **normalize(outcome(store.list_projects))})
            continue
        if kind == "overview":
            sessions = [SessionStub(item) for item in op["sessions"]]
            value = store.project_overview(sessions, recent_limit=op.get("recent_limit"), sort_limit=0)
            results.append({"op": kind, "ok": True, "value": mask_value(value)})
            continue
        if kind == "scan_projects":
            roots = [expand(item) for item in op.get("roots", [])]
            current = expand(op["current"]) if op.get("current") else None
            results.append({
                "op": kind,
                **normalize(
                    outcome(
                        lambda: store.scan_projects(
                            roots, current_workspace=current, now=NOW
                        )
                    )
                ),
            })
            continue

        call = {
            "create_project": lambda: store.create_project(
                name=op["name"], path=expand(op["path"]), now=NOW
            ),
            "import_project": lambda: store.import_project(
                name=op["name"], path=expand(op["path"]), now=NOW
            ),
            "rename_project": lambda: store.rename_project(
                path=expand(op["path"]), name=op["name"], now=NOW
            ),
            "pin_project": lambda: store.pin_project(
                expand(op["path"]), pinned=op["pinned"], now=NOW
            ),
            "toggle_project_pin": lambda: store.toggle_project_pin(expand(op["path"]), now=NOW),
            "remove_project": lambda: store.remove_project(expand(op["path"])),
        }[kind]
        results.append({"op": kind, **normalize(outcome(call))})

    file_text = ""
    if store.path.exists():
        file_text = mask(store.path.read_text(encoding="utf-8"))
    return {"label": spec["label"], "ops": spec["ops"], "results": results, "file": file_text}


class SessionStub:
    """会话索引条目的最小替身：只补 project_overview 读到的那几个属性。"""

    def __init__(self, payload: dict) -> None:
        self.workspace_root = expand(payload["workspace_root"])
        self.session_id = payload["session_id"]
        self.title = payload["title"]
        raw = payload.get("updated_at")
        self.updated_at = None if raw is None else project_module._parse_datetime(raw)


def expand(text: str) -> str:
    if text is None:
        return None
    return text.replace(ROOT_PLACEHOLDER, str(WORK_ROOT)).replace(TEMP_PLACEHOLDER, str(TEMP_ROOT))


def normalize(result: dict) -> dict:
    if not result["ok"]:
        return {"ok": False, "error": result["error"]}
    return {"ok": True, "value": mask_value(result["value"])}


def main() -> None:
    module_path = Path(project_module.__file__).resolve()
    if ROOT not in module_path.parents:
        raise SystemExit(f"对照必须跑在仓库内的真实现上：{module_path}")

    layout = make_layout()
    root = WORK_ROOT / "data"
    root.mkdir(parents=True, exist_ok=True)
    (root / "plain.txt").write_text("x", encoding="utf-8")
    (root / "beta").mkdir(parents=True, exist_ok=True)
    (root / "alpha").mkdir(parents=True, exist_ok=True)
    (root / "scanned-a").mkdir(parents=True, exist_ok=True)
    (root / "scanned-b").mkdir(parents=True, exist_ok=True)
    (WORK_ROOT / "plain" / "keep").mkdir(parents=True, exist_ok=True)
    payload = {
        "source": "omnicrawl/state/project.py",
        "pure": pure_cases(layout),
        "store": [run_store_case(spec) for spec in store_cases()],
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
