#!/usr/bin/env python3
"""生成工作区隔离层的对照数据集，供 `omnicrawl-workspace` 的 parity 测试使用。

单一真相是 `omnicrawl/workspace/` 的 Python 真实现：本脚本把同一批输入喂给真实现
（含合成出来的 gitdir 目录树、隔离区元数据、清扫条目），把返回值或错误文案原样记下来，
并把「目录树」本身一并写进数据集——Rust 侧照原样重建后重放，逐字段比对。

两边都有的手写期望（比如「缺失 ref 应该解析成 None」）会先跟真实现断言一致，再写进数据集，
避免数据集里混进作者以为的行为而不是真实行为。

不进数据集的输入：实例 ID 哈希与进程启动时刻。Python 的 `hash(str(path))` 每进程随机
加盐，两侧数值本来就不同（Rust 侧对应 `process_instance_id` 的语义一致性，由真 git
集成测试覆盖），写进数据集只会制造假失败。

用法：``python rust/tools/gen_workspace_isolation_fixture.py``
输出：``rust/crates/omnicrawl-workspace/tests/fixtures/workspace_isolation_parity.json``
"""

from __future__ import annotations

import importlib
import json
import shutil
import sys
import tempfile
import types
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
OUTPUT_PATH = (
    ROOT
    / "rust"
    / "crates"
    / "omnicrawl-workspace"
    / "tests"
    / "fixtures"
    / "workspace_isolation_parity.json"
)

sys.path.insert(0, str(ROOT))


def import_workspace_modules():
    """导入工作区的真实现（`agent_isolation` + `slug`）。

    `omnicrawl/__init__.py` 会急切导入一批兼容模块，其中 `omnicrawl.commands.slash` 会拉起
    `agent` → `llm.desensitization`；仓库工作区里那份文件当前带着未解决的合并冲突标记
    （`<<<<<<< ours`），语法就不合法，于是任何 `import omnicrawl.*` 都会连带失败。这条链路
    与工作区层无关，两个目标模块也不依赖它，因此先用空模块把 `omnicrawl.commands.slash`
    挡住；导入完成后逐个断言模块文件确实位于仓库内，避免「挡住了冲突」变成换了真相源。
    """

    sys.modules.setdefault("omnicrawl.commands.slash", types.ModuleType("omnicrawl.commands.slash"))
    agent_isolation = importlib.import_module("omnicrawl.workspace.agent_isolation")
    slug = importlib.import_module("omnicrawl.workspace.slug")
    modules = (agent_isolation, slug)
    for module in modules:
        if not Path(module.__file__).resolve().is_relative_to(ROOT):
            raise SystemExit(f"加载到的不是仓库源码：{module.__file__}")
    return modules


A, S = import_workspace_modules()

ROOT_TOKEN = "<ROOT>"

# 长度 40（SHA-1）与 64（SHA-256）的完整提交哈希。
SHA_MAIN = "a" * 40
SHA_OTHER = "b" * 40
SHA_PACKED = "c" * 40
SHA_FEATURE = "d" * 40
SHA_LEGACY = "e" * 40
SHA_SHA256 = "0123456789abcdef" * 4


def norm(value: Any, root: Path) -> str:
    """把路径折算成与具体临时目录无关的稳定形状（两侧各自替换自己的根）。"""

    path = Path(str(value)).resolve()
    try:
        relative = path.relative_to(root.resolve())
    except ValueError:
        return "<OUTSIDE>"
    text = relative.as_posix()
    return ROOT_TOKEN if text == "." else f"{ROOT_TOKEN}/{text}"


def materialize(root: Path, tree: dict[str, str]) -> None:
    """按数据集里的 `tree` 描述建出目录与文件（`<ROOT>` 替换为本次根目录）。"""

    for relative, content in tree.items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        # 写字节而不是 write_text：Windows 上文本模式会把 \n 折成 \r\n，gitdir / ref 文件
        # 里的换行位置会影响解析结果，两侧必须写出一模一样的字节。
        text = content.replace(ROOT_TOKEN, root.as_posix())
        target.write_bytes(text.encode("utf-8"))


def build_slug_cases() -> list[dict[str, Any]]:
    samples = [
        "abc",
        "a_b-c",
        "w1a2b3c",
        "aw-0123456789ab",
        "ABC_123-xyz",
        "a" * 64,
        "a" * 65,
        "",
        "   ",
        "  abc  ",
        "a b",
        "..",
        "../etc",
        "a/b",
        "a\\b",
        "a.b",
        "中文",
        "a" * 8,
        "abcdefgh",
        "abcdefghi",
        "abc\n",
        "a\tb",
        "a*b",
        "-",
        "_",
    ]
    cases: list[dict[str, Any]] = []
    for index, item in enumerate(samples):
        # 部分样本额外跑一次 max_length=8，覆盖长度边界与超长文案。
        lengths = (64, 8) if index % 3 == 0 else (64,)
        for max_length in lengths:
            entry: dict[str, Any] = {
                "name": item,
                "max_length": max_length,
                "safe": S.is_safe_slug(item, max_length=max_length),
            }
            try:
                entry["validated"] = S.validate_slug(
                    item, field="隔离区实例 ID", max_length=max_length
                )
            except S.SlugSafetyError as exc:
                entry["error"] = str(exc)
            cases.append(entry)
    return cases


DIFF_MULTI = (
    "diff --git a/one.txt b/one.txt\n"
    "index 0000000..1111111 100644\n"
    "--- a/one.txt\n"
    "+++ b/one.txt\n"
    "@@ -0,0 +1 @@\n"
    "+hello\n"
    "diff --git a/dir/two.txt b/dir/two.txt\n"
    "new file mode 100644\n"
    "--- /dev/null\n"
    "+++ b/dir/two.txt\n"
    "@@ -0,0 +1,2 @@\n"
    "+a\n"
    "+b\n"
)
DIFF_RENAME = (
    "diff --git a/old.txt b/new.txt\n"
    "similarity index 100%\n"
    "rename from old.txt\n"
    "rename to new.txt\n"
)
DIFF_QUOTED = 'diff --git "a/with space.txt" "b/with space.txt"\n'
DIFF_HEADER_ONLY = "diff --git a/lonely\n"


def build_diff_cases() -> list[dict[str, Any]]:
    samples = [DIFF_MULTI, DIFF_RENAME, DIFF_QUOTED, DIFF_HEADER_ONLY, "", "\n", "  \n"]
    cases = []
    for item in samples:
        cases.append(
            {
                "diff": item,
                "changed": A._count_changed(item),
                "files": A._patch_files(item),
            }
        )
    return cases


def build_worktree_head_cases() -> list[dict[str, Any]]:
    """`resolve_worktree_head` 的纯文件系统用例：合成每种 gitdir 布局。

    `expect` 先按文档手写，稍后与真实现断言一致后再落库。
    """

    cases: list[dict[str, Any]] = []

    def add(
        name: str,
        files: dict[str, str],
        expect: str | None,
    ) -> None:
        """每个用例一个独立仓库目录，避免 packed-refs 之类的文件互相覆盖。

        用例内的相对路径：`wt/...` 是 worktree 目录（即 gitdir），其余相对仓库根。
        """

        prefix = f"repo-{name}"
        tree: dict[str, str] = {}
        for relative, content in files.items():
            if relative.startswith("wt/"):
                target = f"{prefix}/.git/worktrees/{name}/{relative[3:]}"
            else:
                target = f"{prefix}/{relative}"
            tree[target] = content
        cases.append(
            {
                "name": name,
                "gitdir": f"{ROOT_TOKEN}/{prefix}/.git/worktrees/{name}",
                "prefix": prefix,
                "tree": tree,
                "expect": expect,
                "documented": expect,
            }
        )

    add(
        "detached",
        {
            "wt/HEAD": f"{SHA_MAIN}\n",
            "wt/commondir": "../..\n",
        },
        SHA_MAIN,
    )
    add(
        "detached-sha256",
        {"wt/HEAD": SHA_SHA256, "wt/commondir": "../.."},
        SHA_SHA256,
    )
    add(
        "symref-loose",
        {
            "wt/HEAD": "ref: refs/heads/main\n",
            "wt/commondir": "../..",
            ".git/refs/heads/main": f"{SHA_MAIN}\n",
        },
        SHA_MAIN,
    )
    add(
        "symref-nested-ref",
        {
            "wt/HEAD": "ref: refs/heads/feature/x",
            "wt/commondir": "../..",
            ".git/refs/heads/feature/x": SHA_FEATURE,
        },
        SHA_FEATURE,
    )
    add(
        "symref-packed",
        {
            "wt/HEAD": "ref: refs/heads/packed\n",
            "wt/commondir": "../..",
            ".git/packed-refs": (
                "# pack-refs with: peeled fully-peeled sorted \n"
                f"{SHA_OTHER} refs/heads/other\n"
                "^0000000000000000000000000000000000000000\n"
                f"{SHA_PACKED} refs/heads/packed\n"
            ),
        },
        SHA_PACKED,
    )
    add(
        "loose-preferred-over-packed",
        {
            "wt/HEAD": "ref: refs/heads/main",
            "wt/commondir": "../..",
            ".git/refs/heads/main": SHA_MAIN,
            ".git/packed-refs": f"{SHA_OTHER} refs/heads/main\n",
        },
        SHA_MAIN,
    )
    add(
        "absolute-commondir",
        {
            "wt/HEAD": "ref: refs/heads/main",
            "wt/commondir": f"{ROOT_TOKEN}/repo-absolute-commondir/.git\n",
            ".git/refs/heads/main": SHA_MAIN,
        },
        SHA_MAIN,
    )
    add(
        "missing-ref",
        {
            "wt/HEAD": "ref: refs/heads/gone",
            "wt/commondir": "../..",
        },
        None,
    )
    add("no-commondir", {"wt/HEAD": "ref: refs/heads/main"}, None)
    add(
        "garbage-head",
        {"wt/HEAD": "not-a-sha\n", "wt/commondir": "../.."},
        None,
    )
    add("short-sha", {"wt/HEAD": "a" * 39, "wt/commondir": "../.."}, None)
    add("uppercase-sha", {"wt/HEAD": "A" * 40, "wt/commondir": "../.."}, None)
    add("empty-head", {"wt/HEAD": "\n", "wt/commondir": "../.."}, None)
    add("no-head", {"wt/commondir": "../.."}, None)
    return cases


def metadata_payload(worktree_path: str, *, mode: str = "worktree") -> dict[str, Any]:
    return {
        "instance_id": worktree_path.rsplit("/", 1)[-1],
        "mode": mode,
        "repo_root": f"{ROOT_TOKEN}/repo",
        "main_workspace": f"{ROOT_TOKEN}/repo",
        "worktree_path": worktree_path,
        "base_ref": SHA_MAIN,
        "branch_name": "",
        "created_at": 1700000000.0,
        "apply_on_exit": True,
        "cleanup_on_exit": "auto",
    }


def session_expect(
    instance_id: str,
    *,
    mode: str,
    worktree: str,
    branch_name: str = "",
    base_ref: str = SHA_MAIN,
    created_at: Any = 1700000000.0,
    repo: str = "repo",
) -> dict[str, Any]:
    return {
        "instance_id": instance_id,
        "mode": mode,
        "repo_root": f"{ROOT_TOKEN}/{repo}",
        "worktree_path": f"{ROOT_TOKEN}/{worktree}",
        "base_ref": base_ref,
        "main_workspace": f"{ROOT_TOKEN}/{repo}",
        "created_at": created_at,
        "branch_name": branch_name,
    }


def metadata_tree(entry: str, payload: Any) -> dict[str, str]:
    text = payload if isinstance(payload, str) else json.dumps(payload, ensure_ascii=False)
    return {f"{entry}.json": text}


def legacy_tree(entry: str) -> dict[str, str]:
    """遗留目录：没有元数据，只有 `.git` 指针文件。"""

    prefix = f"repo-{entry}"
    return {
        f"{entry}/.git": f"gitdir: {ROOT_TOKEN}/{prefix}/.git/worktrees/{entry}",
        f"{prefix}/.git/worktrees/{entry}/HEAD": SHA_LEGACY,
        f"{prefix}/.git/worktrees/{entry}/commondir": "../..",
    }


def build_sweep_entry_cases() -> list[dict[str, Any]]:
    """`_session_from_sweep_entry` 的接受 / 拒绝矩阵（含被篡改的元数据）。"""

    cases: list[dict[str, Any]] = []

    def add(entry: dict[str, Any]) -> None:
        cases.append(entry)

    add(
        {
            "name": "aw-abc",
            "instance_id": "abc",
            "tree": metadata_tree("aw-abc", metadata_payload(f"{ROOT_TOKEN}/aw-abc")),
            # 元数据里的 instance_id 优先于目录名推断出的那份。
            "expect": session_expect("aw-abc", mode="worktree", worktree="aw-abc"),
        }
    )
    # 相对路径的 worktree_path 按隔离区根目录解析，同样指向条目自身。
    relative = metadata_payload("aw-rel")
    relative["instance_id"] = "rel"
    add(
        {
            "name": "aw-rel",
            "instance_id": "rel",
            "tree": metadata_tree("aw-rel", relative),
            "expect": session_expect("rel", mode="worktree", worktree="aw-rel"),
        }
    )
    # sw- 条目只要元数据可解析就走元数据分支（不额外要求前缀）。
    subagent = metadata_payload(f"{ROOT_TOKEN}/sw-task", mode="subagent")
    subagent["instance_id"] = "task"
    subagent["branch_name"] = "omnicrawl/subagent/task"
    add(
        {
            "name": "sw-task",
            "instance_id": "task",
            "tree": metadata_tree("sw-task", subagent),
            "expect": session_expect(
                "task",
                mode="subagent",
                worktree="sw-task",
                branch_name="omnicrawl/subagent/task",
            ),
        }
    )
    # 拒绝：worktree_path 指向隔离区根目录内的其他目录。
    add(
        {
            "name": "aw-outside",
            "instance_id": "outside",
            "tree": metadata_tree("aw-outside", metadata_payload(f"{ROOT_TOKEN}/other")),
            "expect": None,
        }
    )
    # 拒绝：worktree_path 指向隔离区根目录之外（路径穿越）。
    add(
        {
            "name": "aw-escape",
            "instance_id": "escape",
            "tree": metadata_tree("aw-escape", metadata_payload(f"{ROOT_TOKEN}/../escape")),
            "expect": None,
        }
    )
    # 拒绝：元数据缺少 worktree_path。
    no_key = metadata_payload(f"{ROOT_TOKEN}/aw-nokey")
    no_key.pop("worktree_path")
    add(
        {
            "name": "aw-nokey",
            "instance_id": "nokey",
            "tree": metadata_tree("aw-nokey", no_key),
            "expect": None,
        }
    )
    # 拒绝：元数据不是合法 JSON，且目录里没有 .git 可推断。
    add(
        {
            "name": "aw-badjson",
            "instance_id": "badjson",
            "tree": metadata_tree("aw-badjson", "{not json"),
            "expect": None,
        }
    )
    # 拒绝：实例 ID 不是安全 slug。
    add(
        {
            "name": "aw-bad name",
            "instance_id": "bad name",
            "tree": metadata_tree("aw-bad name", metadata_payload(f"{ROOT_TOKEN}/aw-bad name")),
            "expect": None,
        }
    )
    # 拒绝：无元数据、没有 .git 指针文件，不是 worktree 布局。
    add(
        {
            "name": "aw-plain",
            "instance_id": "plain",
            "tree": {"aw-plain/keep.txt": "x"},
            "expect": None,
        }
    )
    # 接受（遗留目录）：无元数据，但 .git 指针能解析出 common git dir，创建时间取 mtime。
    add(
        {
            "name": "aw-legacy",
            "instance_id": "legacy",
            "tree": legacy_tree("aw-legacy"),
            "expect": session_expect(
                "legacy",
                mode="worktree",
                worktree="aw-legacy",
                base_ref="",
                created_at="<MTIME>",
                repo="repo-aw-legacy",
            ),
        }
    )
    # 拒绝：sw- 条目没有元数据时不做 .git 推断（分支名无从得知，不能猜着清理）。
    add(
        {
            "name": "sw-legacy",
            "instance_id": "legacy",
            "tree": legacy_tree("sw-legacy"),
            "expect": None,
        }
    )
    return cases


def build_metadata_read_cases() -> list[dict[str, Any]]:
    """`_read_isolation_metadata` 的容错面：缺失 / 损坏 / 非对象一律 None。"""

    samples: list[tuple[str, str | None]] = [
        ("aw-ok", json.dumps({"a": 1, "中文": "值"})),
        ("aw-array", "[]"),
        ("aw-null", "null"),
        ("aw-broken", "{oops"),
        ("aw-empty", ""),
        ("aw-missing", None),
    ]
    cases: list[dict[str, Any]] = []
    for name, content in samples:
        cases.append(
            {
                "name": name,
                "entry": name,
                "tree": {} if content is None else {f"{name}.json": content},
                "content": content,
            }
        )
    return cases


def build_cleanup_gate_cases(work: Path, now: float) -> list[dict[str, Any]]:
    """四层门禁的前两层（纯判定，不依赖 git）：命名、使用中、保留期。"""

    def session(
        name: str,
        *,
        mode: str = "local",
        created_at: float = 0.0,
    ) -> A.IsolationSession:
        return A.IsolationSession(
            instance_id=name[3:],
            mode=mode,
            repo_root=work / "repo",
            worktree_path=work / name,
            base_ref="",
            main_workspace=work / "repo",
            created_at=created_at,
        )

    gates: list[tuple[A.IsolationSession, tuple[str, ...], tuple[bool, str]]] = [
        (
            session("aw-old", created_at=now - 4000),
            (),
            (True, "可安全清理"),
        ),
        (
            session("aw-fresh", created_at=now - 10),
            (),
            (False, "隔离区未过保留期（还需 3590s）"),
        ),
        (
            session("not-temp", created_at=now - 4000),
            (),
            (False, "非临时隔离区（目录名不以 aw-/sw- 开头）"),
        ),
        (
            session("sw-old", mode="subagent", created_at=now - 4000),
            (),
            (True, "可安全清理"),
        ),
        (
            session("aw-used", created_at=now - 4000),
            ("used",),
            (False, "隔离区当前正在使用中"),
        ),
        # created_at 为 0（元数据缺失）：跳过保留期判定，直接进入后续层。
        (session("aw-noclock"), (), (True, "可安全清理")),
        # worktree 模式的目录不存在：跳过变更 / 未推送层的 git 调用。
        (
            session("aw-gone", mode="worktree", created_at=now - 4000),
            (),
            (True, "可安全清理"),
        ),
    ]

    cases: list[dict[str, Any]] = []
    for entry, in_use, expect in gates:
        actual = A.cleanup_eligible(
            entry,
            in_use=set(in_use),
            now=now,
            remote_ref="origin",
        )
        assert actual == expect, f"{entry.worktree_path}：手写期望 {expect}，真实现 {actual}"
        cases.append(
            {
                "session": {
                    "instance_id": entry.instance_id,
                    "mode": entry.mode,
                    "repo_root": norm(entry.repo_root, work),
                    "worktree_path": norm(entry.worktree_path, work),
                    "base_ref": entry.base_ref,
                    "main_workspace": norm(entry.main_workspace, work),
                    "created_at": entry.created_at,
                    "branch_name": entry.branch_name,
                },
                "in_use": list(in_use),
                "now": now,
                "expect": [actual[0], actual[1]],
            }
        )
    return cases


def build_metadata_path_cases(work: Path) -> list[dict[str, Any]]:
    return [
        {
            "root": norm(work, work),
            "entry": item,
            "expect": norm(A._isolation_metadata_path(work, item), work),
        }
        for item in ("aw-abc", "sw-task", "plain")
    ]


def main() -> int:
    work = Path(tempfile.mkdtemp(prefix="ociso-")).resolve()
    now = 1_700_000_000.0
    try:
        head_cases = build_worktree_head_cases()
        for case in head_cases:
            materialize(work, case["tree"])
        sweep_cases = build_sweep_entry_cases()
        for case in sweep_cases:
            materialize(work, case["tree"])
        metadata_cases = build_metadata_read_cases()
        for case in metadata_cases:
            materialize(work, case["tree"])

        # 用真实现跑一遍 worktree_head：手写期望必须先对上，再落库真实现结果。
        for case in head_cases:
            gitdir = work / case["gitdir"].replace(ROOT_TOKEN, "").lstrip("/")
            actual = A.resolve_worktree_head(gitdir)
            assert actual == case["documented"], (
                f"{case['name']}：手写期望 {case['documented']}，真实现 {actual}"
            )
            case["expect"] = actual
            case.pop("documented")

        # 用真实现跑一遍清扫条目重建。
        for case in sweep_cases:
            entry = work / case["name"]
            session = A._session_from_sweep_entry(work, entry, case["instance_id"])
            if session is None:
                assert case["expect"] is None, f"{case['name']}：手写期望应被接受"
                continue
            expected = session_expect(
                session.instance_id,
                mode=session.mode,
                worktree=norm(session.worktree_path, work)[len(ROOT_TOKEN) + 1 :],
                branch_name=session.branch_name,
                base_ref=session.base_ref,
                created_at=session.created_at,
                repo=norm(session.repo_root, work)[len(ROOT_TOKEN) + 1 :],
            )
            if case["name"] == "aw-legacy":
                # 遗留目录的创建时间取目录 mtime，两侧都不确定性，只对照语义。
                assert abs(session.created_at - entry.stat().st_mtime) < 1e-6
                expected["created_at"] = "<MTIME>"
            assert expected == case["expect"], f"{case['name']}：{expected} != {case['expect']}"
            assert norm(session.main_workspace, work) == norm(session.repo_root, work)

        for case in metadata_cases:
            actual = A._read_isolation_metadata(work, case["entry"])
            case["expect"] = actual
            # 落库时不需要 content（tree 已经表达了它）。
            case.pop("content")

        payload = {
            "slug": build_slug_cases(),
            "diff_stats": build_diff_cases(),
            "worktree_head": head_cases,
            "sweep_entries": sweep_cases,
            "metadata_read": metadata_cases,
            "metadata_path": build_metadata_path_cases(work),
            "cleanup_gates": build_cleanup_gate_cases(work, now),
        }
        OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
        # Windows 上 write_text 会把 \n 折成 \r\n；数据集要与仓库内其他 fixture 一致，
        # 直接写字节。
        OUTPUT_PATH.write_bytes(
            (json.dumps(payload, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
        )
        print(f"已写入 {OUTPUT_PATH}")
        print("用例统计：" + json.dumps({key: len(value) for key, value in payload.items()}))
        return 0
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
