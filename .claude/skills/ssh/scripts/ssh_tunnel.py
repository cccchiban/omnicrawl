#!/usr/bin/env python3
import argparse
import subprocess


def add_host_key_options(cmd: list[str], args: argparse.Namespace) -> None:
    if args.accept_new_host_key:
        cmd.extend(["-o", "StrictHostKeyChecking=accept-new"])
    if args.known_hosts_file:
        cmd.extend(["-o", f"UserKnownHostsFile={args.known_hosts_file}"])


def main() -> int:
    parser = argparse.ArgumentParser(description="Start foreground OpenSSH local port forwarding")
    parser.add_argument("alias")
    parser.add_argument("--local-port", required=True)
    parser.add_argument("--remote-host", default="127.0.0.1")
    parser.add_argument("--remote-port", required=True)
    parser.add_argument("--accept-new-host-key", action="store_true")
    parser.add_argument("--known-hosts-file")
    args = parser.parse_args()

    target = f"127.0.0.1:{args.local_port}:{args.remote_host}:{args.remote_port}"
    cmd = ["ssh"]
    add_host_key_options(cmd, args)
    cmd.extend(["-N", "-L", target, args.alias])
    return subprocess.call(cmd)


if __name__ == "__main__":
    raise SystemExit(main())
