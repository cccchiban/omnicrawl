#!/usr/bin/env python3
"""源码规模门禁（纯文本，不触发编译）。

口径：**生产行** = 剥离 `#[cfg(test)]` 块与 `#[test]` 函数之后的物理行数。
集成测试（`tests/`）不计生产口径，只按总行数检查，避免把测试规模当代码债。

用法：
    python rust/scripts/size_guard.py            # 报告并检查
    python rust/scripts/size_guard.py --report   # 只报告，不失败

退出码：0 = 通过；1 = 有新增超标项。
`rust/scripts/size_guard_allowlist.txt` 里登记的是已存在的超标文件，
**只允许删行，不允许新增**：拆到红线以下就从名单里移除。
"""
import argparse
import os
import re
import sys

MAX_PROD_LINES = 1000
MAX_TEST_FILE_LINES = 3000

HERE = os.path.dirname(os.path.abspath(__file__))
RUST_ROOT = os.path.dirname(HERE)
CRATES = os.path.join(RUST_ROOT, "crates")
ALLOWLIST = os.path.join(HERE, "size_guard_allowlist.txt")
SKIP_DIRS = {"target", "node_modules", "dist", ".git"}


def rust_files():
    for base, dirs, files in os.walk(CRATES):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            if name.endswith(".rs"):
                yield os.path.join(base, name)


def brace_deltas(lines):
    """逐行净花括号增量；跳过注释、字符串与原始字符串，避免计数被字面量污染。"""
    deltas = []
    block = False
    raw_hashes = None
    for line in lines:
        delta = 0
        i = 0
        n = len(line)
        while i < n:
            if block:
                end = line.find("*/", i)
                if end < 0:
                    break
                block = False
                i = end + 2
                continue
            if raw_hashes is not None:
                closer = '"' + "#" * raw_hashes
                end = line.find(closer, i)
                if end < 0:
                    break
                raw_hashes = None
                i = end + len(closer)
                continue
            ch = line[i]
            if ch == "/" and i + 1 < n:
                if line[i + 1] == "/":
                    break
                if line[i + 1] == "*":
                    block = True
                    i += 2
                    continue
            if ch == '"':
                i += 1
                while i < n:
                    if line[i] == "\\":
                        i += 2
                        continue
                    if line[i] == '"':
                        i += 1
                        break
                    i += 1
                continue
            if ch == "r" and i + 1 < n and line[i + 1] in '#"':
                j = i + 1
                hashes = 0
                while j < n and line[j] == "#":
                    hashes += 1
                    j += 1
                if j < n and line[j] == '"':
                    closer = '"' + "#" * hashes
                    end = line.find(closer, j + 1)
                    if end < 0:
                        raw_hashes = hashes
                        break
                    i = end + len(closer)
                    continue
            if ch == "'":
                m = re.match(r"'(?:\\.|[^\\'])'", line[i:])
                if m:
                    i += m.end()
                    continue
            if ch == "{":
                delta += 1
            elif ch == "}":
                delta -= 1
            i += 1
        deltas.append(delta)
    return deltas


def production_lines(lines, deltas):
    """剥离测试载体，返回生产行数。"""
    n = len(lines)
    keep = [True] * n
    i = 0
    while i < n:
        s = lines[i].strip()
        if s.startswith("#[cfg(test)]") or s.startswith("#[cfg(all(test") or s.startswith("#[test]"):
            depth = 0
            seen = False
            j = i
            while j < n:
                depth += deltas[j]
                if "{" in lines[j]:
                    seen = True
                j += 1
                if seen and depth <= 0:
                    break
            for k in range(i, j):
                keep[k] = False
            i = j
            continue
        i += 1
    return sum(1 for k in keep if k)


def load_allowlist():
    if not os.path.exists(ALLOWLIST):
        return set()
    entries = set()
    with open(ALLOWLIST, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            entries.add(line.split()[0])
    return entries


def main():
    # Windows runner 的 stdout 默认是 cp1252，打印中文会直接 UnicodeEncodeError；
    # 这里显式转 UTF-8，让门禁在两种 OS 上都能把报告打全。
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", action="store_true", help="只报告，不因超标失败")
    args = parser.parse_args()

    allow = load_allowlist()
    offenders = []
    test_offenders = []

    for path in rust_files():
        rel = os.path.relpath(path, RUST_ROOT).replace("\\", "/")
        with open(path, encoding="utf-8", errors="replace") as fh:
            lines = fh.readlines()
        is_test_dir = rel.startswith("crates/") and "/tests/" in rel
        if is_test_dir:
            if len(lines) > MAX_TEST_FILE_LINES:
                test_offenders.append((len(lines), rel))
            continue
        prod = production_lines(lines, brace_deltas(lines))
        if prod > MAX_PROD_LINES:
            offenders.append((prod, rel))

    offenders.sort(reverse=True)
    test_offenders.sort(reverse=True)

    print(f"生产行 > {MAX_PROD_LINES} 的文件：{len(offenders)} 个")
    for prod, rel in offenders:
        mark = "allow" if rel in allow else "NEW"
        print(f"  {prod:6d}  [{mark}]  {rel}")

    if test_offenders:
        print(f"\n集成测试文件 > {MAX_TEST_FILE_LINES} 行：{len(test_offenders)} 个")
        for total, rel in test_offenders:
            print(f"  {total:6d}  {rel}")

    stale = sorted(allow - {rel for _, rel in offenders})
    if stale:
        print("\n名单里已不再超标的条目（请删除）：")
        for rel in stale:
            print(f"  {rel}")

    if args.report:
        return 0

    new_ones = [rel for _, rel in offenders if rel not in allow]
    if new_ones:
        print(f"\n失败：{len(new_ones)} 个文件新越过 {MAX_PROD_LINES} 行红线，请拆分或登记说明。")
        return 1
    print("\n通过。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
