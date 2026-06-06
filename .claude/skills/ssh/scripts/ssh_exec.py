#!/usr/bin/env python3
import argparse
import json
import subprocess
import sys


def add_host_key_options(cmd: list[str], args: argparse.Namespace) -> None:
    if args.accept_new_host_key:
        cmd.extend(["-o", "StrictHostKeyChecking=accept-new"])
    if args.known_hosts_file:
        cmd.extend(["-o", f"UserKnownHostsFile={args.known_hosts_file}"])


def main() -> int:
    parser = argparse.ArgumentParser(description="Execute command through OpenSSH host alias")
    parser.add_argument("alias")
    parser.add_argument("command")
    parser.add_argument("--timeout", type=int, default=30)
    parser.add_argument("--accept-new-host-key", action="store_true")
    parser.add_argument("--known-hosts-file")
    args = parser.parse_args()

    cmd = ["ssh"]
    add_host_key_options(cmd, args)
    cmd.extend([args.alias, args.command])

    proc = subprocess.run(
        cmd,
        text=True,
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=args.timeout,
    )
    result = {
        "success": proc.returncode == 0,
        "exit_code": proc.returncode,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
    }
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result["success"] else proc.returncode


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.TimeoutExpired as exc:
        print(json.dumps({
            "success": False,
            "exit_code": -1,
            "stdout": exc.stdout or "",
            "stderr": f"timeout after {exc.timeout}s",
        }, ensure_ascii=False, indent=2), file=sys.stderr)
        raise SystemExit(124)
