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


def run(cmd: list[str], timeout: int) -> int:
    proc = subprocess.run(
        cmd,
        text=True,
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
    )
    print(json.dumps({
        "success": proc.returncode == 0,
        "exit_code": proc.returncode,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
        "command": cmd,
    }, ensure_ascii=False, indent=2))
    return 0 if proc.returncode == 0 else proc.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description="Upload/download files through OpenSSH scp")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_upload = sub.add_parser("upload")
    p_upload.add_argument("alias")
    p_upload.add_argument("local_path")
    p_upload.add_argument("remote_path")
    p_upload.add_argument("--recursive", action="store_true")
    p_upload.add_argument("--timeout", type=int, default=300)
    p_upload.add_argument("--accept-new-host-key", action="store_true")
    p_upload.add_argument("--known-hosts-file")

    p_download = sub.add_parser("download")
    p_download.add_argument("alias")
    p_download.add_argument("remote_path")
    p_download.add_argument("local_path")
    p_download.add_argument("--recursive", action="store_true")
    p_download.add_argument("--timeout", type=int, default=300)
    p_download.add_argument("--accept-new-host-key", action="store_true")
    p_download.add_argument("--known-hosts-file")

    args = parser.parse_args()
    cmd = ["scp"]
    if args.recursive:
        cmd.append("-r")
    add_host_key_options(cmd, args)

    if args.cmd == "upload":
        cmd.extend([args.local_path, f"{args.alias}:{args.remote_path}"])
    else:
        cmd.extend([f"{args.alias}:{args.remote_path}", args.local_path])

    return run(cmd, args.timeout)


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
