"""OmniCrawl CLI 入口：plugin 子命令与 TUI 启动路由。"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Sequence

from .extensions.plugin_install import (
    doctor,
    install_from_npm,
    list_plugins,
    parse_package_spec,
    register_local_dev_plugin,
    rollback_plugin,
    set_enabled,
    uninstall_plugin,
)
from .extensions.plugin_models import PluginError, PluginInstallError, parse_plugins_config
from .extensions.plugin_registry import project_registry_path, user_registry_path
from .config.runtime import load_config_data, save_config_data, get_section, RuntimeConfigError


EXIT_OK = 0
EXIT_USAGE = 2
EXIT_NODE = 3
EXIT_REGISTRY_NET = 4
EXIT_MANIFEST = 5
EXIT_USER_CANCEL = 6
EXIT_ATOMIC = 7
EXIT_SMOKE = 8


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="omnicrawl", description="OmniCrawl CLI")
    parser.add_argument(
        "--resume",
        metavar="SESSION_ID",
        default="",
        help="启动 TUI 时恢复指定会话 ID",
    )
    sub = parser.add_subparsers(dest="command")

    plugin = sub.add_parser("plugin", help="管理 Hook NPM 插件")
    plugin_sub = plugin.add_subparsers(dest="plugin_command", required=True)

    p_install = plugin_sub.add_parser("install", help="安装 NPM 插件或注册本地开发插件")
    p_install.add_argument("package_spec", help="name / name@version / name@tag / 本地路径(dev)")
    p_install.add_argument("--project", action="store_true", help="写入项目注册表")
    p_install.add_argument("--user", action="store_true", help="写入用户注册表")
    p_install.add_argument("--enable", action="store_true", help="安装后启用")
    p_install.add_argument("--yes", action="store_true", help="跳过交互确认（不跳过安全校验）")
    p_install.add_argument("--dev", action="store_true", help="将 package_spec 视为本地开发路径")

    p_system = plugin_sub.add_parser("system", help="全局插件系统开关")
    p_system.add_argument("action", choices=["enable", "disable"])

    p_list = plugin_sub.add_parser("list", help="列出已安装插件")
    p_list.add_argument("--project", action="store_true")
    p_list.add_argument("--user", action="store_true")
    p_list.add_argument("--all", action="store_true")
    p_list.add_argument("--json", action="store_true")

    p_info = plugin_sub.add_parser("info", help="查看插件详情")
    p_info.add_argument("name")
    p_info.add_argument("--json", action="store_true")

    p_enable = plugin_sub.add_parser("enable", help="启用插件")
    p_enable.add_argument("name")
    p_enable.add_argument("--project", action="store_true")
    p_enable.add_argument("--user", action="store_true")

    p_disable = plugin_sub.add_parser("disable", help="禁用插件")
    p_disable.add_argument("name")
    p_disable.add_argument("--project", action="store_true")
    p_disable.add_argument("--user", action="store_true")

    p_update = plugin_sub.add_parser("update", help="更新插件")
    p_update.add_argument("name")
    p_update.add_argument("--to", dest="to_version", default="")
    p_update.add_argument("--project", action="store_true")
    p_update.add_argument("--user", action="store_true")
    p_update.add_argument("--yes", action="store_true")
    p_update.add_argument("--no-activate", action="store_true")

    p_rollback = plugin_sub.add_parser("rollback", help="回滚到 previous 版本")
    p_rollback.add_argument("name")
    p_rollback.add_argument("--project", action="store_true")
    p_rollback.add_argument("--user", action="store_true")

    p_uninstall = plugin_sub.add_parser("uninstall", help="卸载插件")
    p_uninstall.add_argument("name")
    p_uninstall.add_argument("--project", action="store_true")
    p_uninstall.add_argument("--user", action="store_true")
    p_uninstall.add_argument("--purge", action="store_true")
    p_uninstall.add_argument("--yes", action="store_true")

    p_doctor = plugin_sub.add_parser("doctor", help="诊断插件环境")
    p_doctor.add_argument("name", nargs="?", default="")
    p_doctor.add_argument("--json", action="store_true")

    return parser


def _resolve_scope(args: argparse.Namespace, workspace_root: Path) -> str:
    if getattr(args, "project", False) and getattr(args, "user", False):
        raise SystemExit(EXIT_USAGE)
    if getattr(args, "project", False):
        return "project"
    if getattr(args, "user", False):
        return "user"
    # 工作区内默认 project
    if (workspace_root / "AGENTS.md").exists() or (workspace_root / "package.json").exists():
        return "project"
    return "user"


def _print_json(data: Any) -> None:
    print(json.dumps(data, ensure_ascii=False, indent=2))


def _confirm(message: str) -> bool:
    print(message, file=sys.stderr)
    try:
        answer = input("继续？[y/N] ").strip().lower()
    except EOFError:
        return False
    return answer in {"y", "yes"}


def run_plugin_command(args: argparse.Namespace, *, workspace_root: Path | None = None) -> int:
    root = Path(workspace_root or Path.cwd()).resolve()
    command = args.plugin_command

    try:
        if command == "system":
            return _cmd_system(args)
        if command == "install":
            return _cmd_install(args, root)
        if command == "list":
            return _cmd_list(args, root)
        if command == "info":
            return _cmd_info(args, root)
        if command == "enable":
            return _cmd_enable(args, root, True)
        if command == "disable":
            return _cmd_enable(args, root, False)
        if command == "update":
            return _cmd_update(args, root)
        if command == "rollback":
            return _cmd_rollback(args, root)
        if command == "uninstall":
            return _cmd_uninstall(args, root)
        if command == "doctor":
            return _cmd_doctor(args, root)
        print(f"未知 plugin 命令：{command}", file=sys.stderr)
        return EXIT_USAGE
    except PluginInstallError as exc:
        text = str(exc)
        print(text, file=sys.stderr)
        if "Node" in text or "npm" in text:
            return EXIT_NODE
        if "用户" in text and ("取消" in text or "拒绝" in text):
            return EXIT_USER_CANCEL
        if "integrity" in text or "manifest" in text.lower() or "权限" in text:
            return EXIT_MANIFEST
        if "registry" in text.lower() or "下载" in text or "NPM" in text:
            return EXIT_REGISTRY_NET
        if "冒烟" in text or "握手" in text:
            return EXIT_SMOKE
        return EXIT_MANIFEST
    except PluginError as exc:
        print(str(exc), file=sys.stderr)
        return EXIT_MANIFEST
    except RuntimeConfigError as exc:
        print(str(exc), file=sys.stderr)
        return EXIT_ATOMIC


def _cmd_system(args: argparse.Namespace) -> int:
    data = load_config_data()
    plugins = get_section(data, "plugins")
    plugins["enabled"] = args.action == "enable"
    data["plugins"] = plugins
    path = save_config_data(data)
    state = "启用" if plugins["enabled"] else "禁用"
    print(f"已{state}全局插件系统：{path}")
    return EXIT_OK


def _cmd_install(args: argparse.Namespace, root: Path) -> int:
    scope = _resolve_scope(args, root)
    registry = project_registry_path(root) if scope == "project" else user_registry_path()
    print(f"目标注册表：{registry}", file=sys.stderr)

    config = parse_plugins_config(get_section(load_config_data(), "plugins"))
    package_spec = args.package_spec
    is_dev = bool(args.dev) or package_spec.startswith(".") or package_spec.startswith("/") or "\\" in package_spec
    # Windows 盘符路径
    if len(package_spec) >= 2 and package_spec[1] == ":":
        is_dev = True

    if is_dev:
        result = register_local_dev_plugin(
            package_spec,
            scope=scope,
            workspace_root=root,
            # 本地开发插件默认启用，便于联调；可用后续 disable 关掉。
            enable=True,
        )
    else:
        # 预解析帮助给出更好错误
        parse_package_spec(package_spec)
        result = install_from_npm(
            package_spec,
            scope=scope,
            workspace_root=root,
            enable=bool(args.enable),
            yes=bool(args.yes),
            confirm=_confirm,
            allow_network=config.allow_network_install,
        )
    print(
        f"已安装 {result.name}@{result.version} scope={result.scope} enabled={result.enabled}"
    )
    for item in result.diagnostics:
        print(f"- {item}", file=sys.stderr)
    if not config.enabled:
        print(
            "提示：全局 plugins.enabled=false。需要时执行：python main.py plugin system enable",
            file=sys.stderr,
        )
    return EXIT_OK


def _cmd_list(args: argparse.Namespace, root: Path) -> int:
    if args.project:
        scope = "project"
    elif args.user:
        scope = "user"
    else:
        scope = "all"
    rows = list_plugins(scope=scope, workspace_root=root)
    if args.json:
        _print_json(rows)
        return EXIT_OK
    if not rows:
        print("未安装插件。")
        return EXIT_OK
    for row in rows:
        if "error" in row:
            print(f"[{row.get('scope')}] ERROR {row['error']}")
            continue
        active = row.get("active") or {}
        version = active.get("version") if isinstance(active, dict) else None
        if not version and row.get("devMode"):
            version = "dev"
        print(
            f"{row['name']}\tscope={row['scope']}\tenabled={row['enabled']}\tversion={version or '-'}"
        )
    return EXIT_OK


def _cmd_info(args: argparse.Namespace, root: Path) -> int:
    rows = [row for row in list_plugins(scope="all", workspace_root=root) if row.get("name") == args.name]
    if not rows:
        print(f"未找到插件：{args.name}", file=sys.stderr)
        return EXIT_MANIFEST
    if args.json:
        _print_json(rows)
    else:
        for row in rows:
            print(json.dumps(row, ensure_ascii=False, indent=2))
    return EXIT_OK


def _cmd_enable(args: argparse.Namespace, root: Path, enabled: bool) -> int:
    scope = _resolve_scope(args, root)
    registry = project_registry_path(root) if scope == "project" else user_registry_path()
    print(f"目标注册表：{registry}", file=sys.stderr)
    set_enabled(args.name, enabled, scope=scope, workspace_root=root)
    print(f"已{'启用' if enabled else '禁用'} {args.name} ({scope})")
    config = parse_plugins_config(get_section(load_config_data(), "plugins"))
    if enabled and not config.enabled:
        print(
            "提示：插件已启用，但全局 plugins.enabled=false。请执行 plugin system enable。",
            file=sys.stderr,
        )
    return EXIT_OK


def _cmd_update(args: argparse.Namespace, root: Path) -> int:
    scope = _resolve_scope(args, root)
    name = args.name
    version = args.to_version.strip()
    spec = f"{name}@{version}" if version else name
    config = parse_plugins_config(get_section(load_config_data(), "plugins"))
    # 已启用插件默认 activate；--no-activate 时只写 candidate（install_from_npm enable=False 且保留 enabled）
    result = install_from_npm(
        spec,
        scope=scope,
        workspace_root=root,
        enable=not bool(args.no_activate),
        yes=bool(args.yes),
        confirm=_confirm,
        allow_network=config.allow_network_install,
    )
    print(f"已更新 {result.name}@{result.version}")
    return EXIT_OK


def _cmd_rollback(args: argparse.Namespace, root: Path) -> int:
    scope = _resolve_scope(args, root)
    ref = rollback_plugin(args.name, scope=scope, workspace_root=root)
    print(f"已回滚 {args.name} -> {ref.version}")
    return EXIT_OK


def _cmd_uninstall(args: argparse.Namespace, root: Path) -> int:
    scope = _resolve_scope(args, root)
    if not args.yes:
        if not _confirm(f"确认卸载插件 {args.name}（scope={scope}）？"):
            print("已取消。", file=sys.stderr)
            return EXIT_USER_CANCEL
    uninstall_plugin(args.name, scope=scope, workspace_root=root, purge=bool(args.purge))
    print(f"已卸载 {args.name}")
    return EXIT_OK


def _cmd_doctor(args: argparse.Namespace, root: Path) -> int:
    report = doctor(args.name or None, workspace_root=root)
    if args.json:
        _print_json(report)
    else:
        print(f"node: {report.get('node')}")
        print(f"npm: {report.get('npm')}")
        for issue in report.get("issues", []):
            print(f"ISSUE: {issue}")
        for plugin in report.get("plugins", []):
            issues = plugin.get("issues") or []
            mark = "OK" if not issues else "WARN"
            print(f"[{mark}] {plugin.get('name')} scope={plugin.get('scope')}")
            for issue in issues:
                print(f"  - {issue}")
    return EXIT_OK if report.get("ok") else EXIT_MANIFEST


def main(argv: Sequence[str] | None = None) -> int:
    """CLI / console script 主入口。返回进程退出码。

    - ``omnicrawl plugin ...``：插件管理
    - ``omnicrawl`` / ``omnicrawl --resume <id>``：启动 TUI（当前控制台）
    """

    from omnicrawl.entry import run_application

    return run_application(argv)


if __name__ == "__main__":
    raise SystemExit(main())
