#!/usr/bin/env python3
import argparse
import datetime as _dt
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path


def ssh_config_path() -> Path:
    return Path.home() / ".ssh" / "config"


def read_lines(path: Path) -> list[str]:
    if not path.exists():
        return []
    return path.read_text(encoding="utf-8").splitlines()


def parse_hosts(lines: list[str]) -> list[dict]:
    hosts: list[dict] = []
    comments: list[str] = []
    current: dict | None = None

    def finish() -> None:
        nonlocal current
        if current:
            hosts.append(current)
            current = None

    for line in lines:
        stripped = line.strip()
        if stripped.startswith("#") and current is None:
            comments.append(line)
            continue
        if not stripped and current is None:
            comments.append(line)
            continue

        if stripped.lower().startswith("host ") and not stripped.lower().startswith("host *"):
            finish()
            aliases = stripped.split(None, 1)[1].strip()
            current = {
                "alias": aliases,
                "options": {},
                "metadata": parse_metadata(comments),
                "raw_comments": comments,
            }
            comments = []
            continue

        if current and (line.startswith(" ") or line.startswith("\t")) and stripped:
            parts = stripped.split(None, 1)
            if len(parts) == 2:
                current["options"][parts[0].lower()] = parts[1]
            continue

        if current and not stripped:
            finish()
            comments = [line]
        else:
            comments = []

    finish()
    return hosts


def parse_metadata(comments: list[str]) -> dict:
    metadata: dict = {}
    for line in comments:
        text = line.strip()
        if not text.startswith("#"):
            continue
        text = text[1:].strip()
        if ":" not in text:
            continue
        key, value = text.split(":", 1)
        key = key.strip().lower()
        value = value.strip()
        if key in {"description", "tags", "location"}:
            metadata[key] = value
    return metadata


def backup_config(path: Path) -> Path | None:
    if not path.exists():
        return None
    stamp = _dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    backup = path.with_name(f"{path.name}.bak-{stamp}")
    shutil.copy2(path, backup)
    return backup


def run_ssh_g(alias: str) -> dict:
    proc = subprocess.run(
        ["ssh", "-G", alias],
        text=True,
        capture_output=True,
        encoding="utf-8",
        errors="replace",
    )
    if proc.returncode != 0:
        return {"success": False, "stderr": proc.stderr.strip(), "config": {}}

    config: dict[str, str] = {}
    for line in proc.stdout.splitlines():
        if " " not in line:
            continue
        key, value = line.split(" ", 1)
        if key in {"hostname", "user", "port", "identityfile", "proxyjump"}:
            config[key] = value
    return {"success": True, "stderr": "", "config": config}


def cmd_list(_args: argparse.Namespace) -> int:
    hosts = parse_hosts(read_lines(ssh_config_path()))
    print(json.dumps({"success": True, "hosts": hosts}, ensure_ascii=False, indent=2))
    return 0


def cmd_find(args: argparse.Namespace) -> int:
    query = args.query.lower()
    matches = []
    for host in parse_hosts(read_lines(ssh_config_path())):
        haystack = " ".join([
            host.get("alias", ""),
            json.dumps(host.get("metadata", {}), ensure_ascii=False),
            json.dumps(host.get("options", {}), ensure_ascii=False),
        ]).lower()
        if query in haystack:
            matches.append(host)
    print(json.dumps({"success": True, "hosts": matches}, ensure_ascii=False, indent=2))
    return 0


def cmd_show(args: argparse.Namespace) -> int:
    resolved = run_ssh_g(args.alias)
    hosts = parse_hosts(read_lines(ssh_config_path()))
    local = next((h for h in hosts if h["alias"] == args.alias), None)
    result = {
        "success": bool(resolved["success"] and local),
        "alias": args.alias,
        "defined": local is not None,
        "metadata": local.get("metadata", {}) if local else {},
        "options": local.get("options", {}) if local else {},
        "resolved": resolved["config"],
        "stderr": resolved["stderr"],
    }
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result["success"] else 1


def cmd_add(args: argparse.Namespace) -> int:
    path = ssh_config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    hosts = parse_hosts(read_lines(path))
    if any(h["alias"] == args.alias for h in hosts):
        print(json.dumps({
            "success": False,
            "error": f"Host alias already exists: {args.alias}",
        }, ensure_ascii=False, indent=2), file=sys.stderr)
        return 1

    backup = backup_config(path)
    tags = args.tags or ""
    block: list[str] = []
    if path.exists() and path.read_text(encoding="utf-8").strip():
        block.append("")
    if args.description:
        block.append(f"# description: {args.description}")
    if tags:
        block.append(f"# tags: {tags}")
    if args.location:
        block.append(f"# location: {args.location}")
    block.extend([
        f"Host {args.alias}",
        f"    HostName {args.host}",
        f"    User {args.user}",
        f"    Port {args.port}",
    ])
    if args.key:
        block.append(f"    IdentityFile {args.key}")
    if args.proxy_jump:
        block.append(f"    ProxyJump {args.proxy_jump}")

    with path.open("a", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(block))
        f.write("\n")

    print(json.dumps({
        "success": True,
        "alias": args.alias,
        "config_path": str(path),
        "backup_path": str(backup) if backup else None,
    }, ensure_ascii=False, indent=2))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Manage OpenSSH config hosts")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_list = sub.add_parser("list")
    p_list.set_defaults(func=cmd_list)

    p_find = sub.add_parser("find")
    p_find.add_argument("query")
    p_find.set_defaults(func=cmd_find)

    p_show = sub.add_parser("show")
    p_show.add_argument("alias")
    p_show.set_defaults(func=cmd_show)

    p_add = sub.add_parser("add")
    p_add.add_argument("alias")
    p_add.add_argument("--host", required=True)
    p_add.add_argument("--user", required=True)
    p_add.add_argument("--port", default="22")
    p_add.add_argument("--key")
    p_add.add_argument("--proxy-jump")
    p_add.add_argument("--description")
    p_add.add_argument("--tags")
    p_add.add_argument("--location")
    p_add.set_defaults(func=cmd_add)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
