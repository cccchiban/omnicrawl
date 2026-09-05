"""NPM 插件安装、更新、卸载、回滚与开发模式本地注册。"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import re
import secrets
import shutil
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Mapping

from .plugin_models import (
    OMNICRAWL_VERSION,
    PluginInstallError,
    PluginRecord,
    PluginVersionRef,
    is_valid_npm_name,
    is_valid_semver,
    parse_plugin_manifest,
)
from .plugin_protocol import PluginWorkerClient, resolve_node_executable
from .plugin_registry import (
    PluginRegistryError,
    load_registry_document,
    project_registry_path,
    save_registry_document,
    user_registry_path,
    user_store_root,
    upsert_plugin_record,
    remove_plugin_record,
)


ConfirmCallback = Callable[[str], bool]


@dataclass(frozen=True)
class PackageSpec:
    name: str
    version: str | None = None  # None / tag / exact
    raw: str = ""


@dataclass(frozen=True)
class InstallResult:
    name: str
    version: str
    scope: str
    enabled: bool
    store_path: str
    integrity: str
    diagnostics: list[str]


def parse_package_spec(spec: str) -> PackageSpec:
    text = spec.strip()
    if not text:
        raise PluginInstallError("package-spec 不能为空。")
    # 仅支持 name / name@version / name@tag；不支持 git/http/file。
    if text.startswith("git+") or "://" in text or text.startswith(".") or text.startswith("/"):
        raise PluginInstallError("V1 仅支持 NPM registry 的 name / name@version / name@tag。")
    if text.startswith("@"):
        # @scope/name or @scope/name@version
        match = re.match(r"^(@[^/]+/[^@]+)(?:@(.+))?$", text)
        if not match:
            raise PluginInstallError(f"非法 package-spec：{spec}")
        name, version = match.group(1), match.group(2)
    else:
        if text.count("@") > 1:
            raise PluginInstallError(f"非法 package-spec：{spec}")
        if "@" in text:
            name, version = text.split("@", 1)
        else:
            name, version = text, None
    if not is_valid_npm_name(name):
        raise PluginInstallError(f"非法 NPM 包名：{name}")
    return PackageSpec(name=name, version=version, raw=text)


def detect_node_npm() -> tuple[str, str]:
    try:
        node = resolve_node_executable()
    except Exception as exc:  # noqa: BLE001
        raise PluginInstallError(str(exc)) from exc
    npm = shutil.which("npm")
    if not npm:
        raise PluginInstallError("未找到 npm，请安装 Node.js 20+ 自带的 npm。")
    # 版本检查
    try:
        node_version = subprocess.check_output([node, "--version"], text=True, timeout=10).strip()
    except Exception as exc:  # noqa: BLE001
        raise PluginInstallError(f"无法读取 node 版本：{exc}") from exc
    major = _node_major(node_version)
    if major < 20:
        raise PluginInstallError(f"需要 Node.js >= 20，当前：{node_version}")
    return node, npm


def _node_major(version_text: str) -> int:
    text = version_text.strip().lstrip("v")
    major = text.split(".", 1)[0]
    try:
        return int(major)
    except ValueError:
        return 0


def registry_url_for(name: str, version: str | None = None) -> str:
    encoded = urllib.parse.quote(name, safe="@")
    base = os.getenv("npm_config_registry") or os.getenv("NPM_CONFIG_REGISTRY") or "https://registry.npmjs.org"
    base = base.rstrip("/")
    if version:
        return f"{base}/{encoded}/{urllib.parse.quote(version)}"
    return f"{base}/{encoded}"


def fetch_json(url: str, *, timeout: float = 30.0) -> dict[str, Any]:
    request = urllib.request.Request(url, headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        raise PluginInstallError(f"NPM registry HTTP {exc.code}：{url}") from exc
    except Exception as exc:  # noqa: BLE001
        raise PluginInstallError(f"访问 NPM registry 失败：{exc}") from exc
    if not isinstance(payload, dict):
        raise PluginInstallError("NPM registry 返回值必须是对象。")
    return payload


def resolve_exact_version(spec: PackageSpec) -> tuple[str, dict[str, Any]]:
    """解析精确版本与 packument/version metadata。"""

    if spec.version and is_valid_semver(spec.version):
        meta = fetch_json(registry_url_for(spec.name, spec.version))
        return spec.version, meta

    packument = fetch_json(registry_url_for(spec.name))
    dist_tags = packument.get("dist-tags", {})
    versions = packument.get("versions", {})
    if not isinstance(dist_tags, dict) or not isinstance(versions, dict):
        raise PluginInstallError(f"NPM packument 结构无效：{spec.name}")

    tag = spec.version or "latest"
    exact = dist_tags.get(tag)
    if not exact:
        # 允许直接把 tag 字段当成版本键。
        if tag in versions:
            exact = tag
        else:
            raise PluginInstallError(f"无法解析 {spec.name}@{tag} 的精确版本。")
    version_meta = versions.get(exact)
    if not isinstance(version_meta, dict):
        # 回退拉 version endpoint
        version_meta = fetch_json(registry_url_for(spec.name, str(exact)))
    return str(exact), version_meta


def _sha512_integrity(data: bytes) -> str:
    digest = hashlib.sha512(data).digest()
    import base64

    return "sha512-" + base64.b64encode(digest).decode("ascii")


def _sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(chunk)
    return "sha256-" + hasher.hexdigest()


def _content_tree_hash(root: Path) -> str:
    """计算插件文件树的确定性哈希，排除 Host 自己写入的完整性标记。"""

    hasher = hashlib.sha256()
    for path in sorted(root.rglob("*"), key=lambda item: item.as_posix()):
        if path.name == ".omnicrawl-integrity":
            continue
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            raise PluginInstallError(f"插件 store 不允许符号链接：{relative}")
        if path.is_dir():
            continue
        if not path.is_file():
            raise PluginInstallError(f"插件 store 包含特殊文件：{relative}")
        hasher.update(relative.encode("utf-8"))
        hasher.update(b"\0")
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                hasher.update(chunk)
        hasher.update(b"\0")
    return "sha256-" + hasher.hexdigest()


def _escape_package_dir(name: str) -> str:
    return name.replace("/", "__").replace("@", "")


def download_tarball(url: str, dest: Path) -> bytes:
    try:
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read()
    except Exception as exc:  # noqa: BLE001
        raise PluginInstallError(f"下载 tarball 失败：{exc}") from exc
    dest.write_bytes(data)
    return data


def extract_tarball(tarball: Path, dest_dir: Path) -> Path:
    import tarfile

    dest_dir.mkdir(parents=True, exist_ok=True)
    resolved_dest = dest_dir.resolve()
    with tarfile.open(tarball, "r:gz") as archive:
        members = archive.getmembers()
        for member in members:
            normalized_name = member.name.replace("\\", "/")
            member_path = Path(normalized_name)
            target = (resolved_dest / member_path).resolve()
            if (
                member_path.is_absolute()
                or ".." in member_path.parts
                or not _is_relative_to(target, resolved_dest)
            ):
                raise PluginInstallError(f"tarball 包含不安全路径：{member.name}")
            # V1 插件包只需要普通文件和目录。拒绝链接、设备、FIFO 等特殊
            # member，避免后续文件经链接写出解压根目录。
            if not (member.isdir() or member.isfile()):
                raise PluginInstallError(f"tarball 包含不安全特殊文件：{member.name}")
        archive.extractall(dest_dir, members=members)
    # npm pack 通常顶层是 package/
    package_dir = dest_dir / "package"
    if package_dir.is_dir():
        return package_dir
    # 否则取唯一子目录
    children = [path for path in dest_dir.iterdir() if path.is_dir()]
    if len(children) == 1:
        return children[0]
    raise PluginInstallError("无法定位 tarball 解压后的包根目录。")


def _is_relative_to(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def _package_has_production_deps(package_dir: Path) -> bool:
    """判断是否需要 npm install（无 dependencies 时可离线跳过）。"""

    package_json = package_dir / "package.json"
    if not package_json.is_file():
        return False
    try:
        data = json.loads(package_json.read_text(encoding="utf-8-sig"))
    except Exception:
        return True
    deps = data.get("dependencies") if isinstance(data, dict) else None
    return bool(isinstance(deps, dict) and deps)


def npm_install_production(package_dir: Path, npm: str, *, allow_offline_skip: bool = True) -> Path:
    """在包目录安装 production 依赖，强制 --ignore-scripts。

    无 dependencies 时默认跳过联网 install，写入最小 lock，便于离线 fixture / 本地包安装。
    """

    lock_path = package_dir / "package-lock.json"
    if allow_offline_skip and not _package_has_production_deps(package_dir):
        if not lock_path.is_file():
            lock_path.write_text(
                json.dumps(
                    {
                        "name": "omnicrawl-plugin",
                        "lockfileVersion": 3,
                        "requires": True,
                        "packages": {"": {}},
                    },
                    ensure_ascii=False,
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )
        return lock_path

    command = [
        npm,
        "install",
        "--ignore-scripts",
        "--omit=dev",
        "--no-audit",
        "--no-fund",
        "--package-lock-only=false",
    ]
    try:
        completed = subprocess.run(
            command,
            cwd=str(package_dir),
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
    except Exception as exc:  # noqa: BLE001
        raise PluginInstallError(f"npm install 失败：{exc}") from exc
    if completed.returncode != 0:
        stderr = (completed.stderr or completed.stdout or "").strip()
        raise PluginInstallError(f"npm install 退出码 {completed.returncode}：{stderr[:1000]}")
    if not lock_path.is_file():
        # npm 可能未生成 lock；写一个最小占位并基于 node_modules 扫描不在 V1 强依赖。
        lock_path.write_text("{}\n", encoding="utf-8")
    return lock_path


def collect_lockfile_integrity_report(lock_path: Path) -> dict[str, Any]:
    """从 package-lock.json 提取依赖 integrity 摘要（V1 加强校验用）。

    返回：
    - packages: 带 integrity 的包数量
    - missingIntegrity: 缺少 integrity 的路径列表（最多 20 条）
    - lockfileHash: lock 文件 sha256
    """

    report: dict[str, Any] = {
        "packages": 0,
        "missingIntegrity": [],
        "lockfileHash": _sha256_file(lock_path) if lock_path.is_file() else "",
    }
    if not lock_path.is_file():
        return report
    try:
        data = json.loads(lock_path.read_text(encoding="utf-8-sig"))
    except Exception:
        report["missingIntegrity"].append("<unreadable-lockfile>")
        return report

    missing: list[str] = []
    counted = 0
    packages = data.get("packages")
    if isinstance(packages, dict):
        for pkg_path, meta in packages.items():
            if not pkg_path or pkg_path == "":
                continue  # 根包
            if not isinstance(meta, dict):
                continue
            # 仅统计有 resolved 的远程依赖。
            if not meta.get("resolved") and not meta.get("version"):
                continue
            counted += 1
            if not str(meta.get("integrity", "")).strip():
                if len(missing) < 20:
                    missing.append(str(pkg_path))
    else:
        # lockfileVersion 1 风格 dependencies 树。
        def walk(node: Mapping[str, Any], prefix: str = "") -> None:
            nonlocal counted
            deps = node.get("dependencies")
            if not isinstance(deps, dict):
                return
            for name, meta in deps.items():
                if not isinstance(meta, dict):
                    continue
                counted += 1
                path = f"{prefix}{name}"
                if not str(meta.get("integrity", "")).strip():
                    if len(missing) < 20:
                        missing.append(path)
                walk(meta, prefix=f"{path}>")

        walk(data)

    report["packages"] = counted
    report["missingIntegrity"] = missing
    return report


def verify_store_integrity(store_path: Path, expected_integrity: str) -> None:
    """校验 store 内 package.json 仍在，且可选地重读 tarball 标记文件。

    V1 不重新下载 tarball；若 expected_integrity 非空，要求 registry 记录与
    安装时写入的 .omnicrawl-integrity 一致。
    """

    root = Path(store_path)
    if not root.is_dir():
        raise PluginInstallError(f"store 路径不存在：{root}")
    if not (root / "package.json").is_file():
        raise PluginInstallError(f"store 缺少 package.json：{root}")
    marker = root / ".omnicrawl-integrity"
    if expected_integrity and marker.is_file():
        actual = marker.read_text(encoding="utf-8").strip()
        if actual and actual != expected_integrity:
            raise PluginInstallError(
                f"store integrity 标记不匹配：期望 {expected_integrity[:32]}... 实际 {actual[:32]}..."
            )


def verify_content_tree_hash(store_path: Path, expected_content_hash: str) -> None:
    if not expected_content_hash:
        return
    actual = _content_tree_hash(Path(store_path))
    if actual != expected_content_hash:
        raise PluginInstallError(
            f"插件内容哈希不匹配：期望 {expected_content_hash[:20]}... 实际 {actual[:20]}..."
        )


def verify_lockfile_hash(store_path: Path, expected_lockfile_hash: str) -> None:
    """校验 store 内 package-lock.json 与注册表记录的 lockfileHash 一致。"""

    if not expected_lockfile_hash:
        return
    lock_path = Path(store_path) / "package-lock.json"
    if not lock_path.is_file():
        raise PluginInstallError(f"store 缺少 package-lock.json：{store_path}")
    actual = _sha256_file(lock_path)
    if actual != expected_lockfile_hash:
        raise PluginInstallError(
            f"lockfileHash 不匹配：期望 {expected_lockfile_hash[:16]}... 实际 {actual[:16]}..."
        )


def install_from_local_package(
    local_path: str | Path,
    *,
    scope: str = "user",
    workspace_root: Path | None = None,
    enable: bool = True,
    yes: bool = True,
    confirm: ConfirmCallback | None = None,
    store_root: Path | None = None,
) -> InstallResult:
    """离线/可测安装：把本地插件目录复制进 store，走冒烟与 registry。

    与 register_local_dev_plugin 不同：会写入 active/store_path/integrity/lockfileHash，
    可用于验收 install/list/doctor 的 store 语义，而不访问 NPM 网络。
    """

    if scope not in {"user", "project"}:
        raise PluginInstallError(f"非法 scope：{scope}")
    src = Path(local_path).expanduser().resolve()
    if not src.is_dir():
        raise PluginInstallError(f"本地插件目录不存在：{src}")
    package_json = src / "package.json"
    if not package_json.is_file():
        raise PluginInstallError(f"缺少 package.json：{package_json}")

    node, npm = detect_node_npm()
    _ = node
    diagnostics: list[str] = ["offline local package install"]

    with tempfile.TemporaryDirectory(prefix="omnicrawl-plugin-local-") as tmp:
        package_dir = Path(tmp) / "package"
        shutil.copytree(src, package_dir)
        marker_file = package_dir / ".omnicrawl-integrity"
        if marker_file.exists():
            marker_file.unlink()

        digest = hashlib.sha512()
        digest.update(package_json.read_bytes())
        digest.update(str(src).encode("utf-8"))
        integrity = "sha512-" + base64.b64encode(digest.digest()).decode("ascii")

        manifest = parse_plugin_manifest(
            json.loads((package_dir / "package.json").read_text(encoding="utf-8-sig")),
            source_path=str(package_dir / "package.json"),
        )
        exact_version = manifest.version
        summary_lines = [
            f"将安装插件 {manifest.name}@{exact_version}",
            f"  source: local:{src}",
            f"  integrity: {integrity[:40]}...",
            f"  hooks: {', '.join(h.hook for h in manifest.hooks)}",
            f"  permissions: {', '.join(manifest.permissions)}",
            f"  scope: {scope}",
            "",
        ]
        summary = "\n".join(summary_lines)
        if not yes:
            ok = True if confirm is None else confirm(summary)
            if not ok:
                raise PluginInstallError("用户取消安装。")

        lock_path = npm_install_production(package_dir, npm)
        lockfile_hash = _sha256_file(lock_path)
        content_hash = _content_tree_hash(package_dir)
        integrity_report = collect_lockfile_integrity_report(lock_path)
        if integrity_report["missingIntegrity"]:
            diagnostics.append(
                "package-lock 中有 "
                f"{len(integrity_report['missingIntegrity'])} 个依赖缺少 integrity："
                + ", ".join(integrity_report["missingIntegrity"][:5])
            )
        diagnostics.append(
            f"lock 依赖条目 {integrity_report['packages']} 个，lockfileHash={lockfile_hash[:12]}"
        )

        root = Path(store_root) if store_root is not None else user_store_root()
        root.mkdir(parents=True, exist_ok=True)
        integrity_prefix = integrity.split("-", 1)[-1][:12] or "local"
        target = root / _escape_package_dir(manifest.name) / f"{exact_version}-{integrity_prefix}"
        target.parent.mkdir(parents=True, exist_ok=True)
        if target.exists():
            # 内容寻址目标不可原地覆盖。先验证现有对象；一致时幂等复用，
            # 不一致时拒绝，避免冒烟失败破坏当前 active store。
            verify_store_integrity(target, integrity)
            verify_lockfile_hash(target, lockfile_hash)
            verify_content_tree_hash(target, content_hash)
            smoke_test_worker(target, manifest.name)
        else:
            staging = target.parent / f".{target.name}.{secrets.token_hex(6)}.staging"
            shutil.copytree(package_dir, staging)
            (staging / ".omnicrawl-integrity").write_text(integrity + "\n", encoding="utf-8")
            try:
                smoke_test_worker(staging, manifest.name)
                os.replace(staging, target)
            finally:
                if staging.exists():
                    shutil.rmtree(staging, ignore_errors=True)

        registry_path, document = _load_scope_registry(scope, workspace_root)
        existing = document.plugins.get(manifest.name)
        version_ref = PluginVersionRef(
            version=exact_version,
            integrity=integrity,
            lockfile_hash=lockfile_hash,
            source=f"local:{src}",
            store_path=str(target),
            content_hash=content_hash,
        )
        if existing is None:
            record = PluginRecord(
                name=manifest.name,
                enabled=bool(enable),
                active=version_ref if enable else None,
                candidate=None if enable else version_ref,
                previous=None,
                approved_permissions=list(manifest.permissions),
                dev_mode=False,
            )
        else:
            previous = existing.active
            if enable or existing.enabled:
                record = PluginRecord(
                    name=manifest.name,
                    enabled=True if enable else existing.enabled,
                    active=version_ref,
                    candidate=None,
                    previous=previous,
                    approved_permissions=sorted(
                        set(existing.approved_permissions) | set(manifest.permissions)
                    ),
                    local_path="",
                    dev_mode=False,
                )
            else:
                record = PluginRecord(
                    name=manifest.name,
                    enabled=False,
                    active=existing.active,
                    candidate=version_ref,
                    previous=existing.previous,
                    approved_permissions=sorted(
                        set(existing.approved_permissions) | set(manifest.permissions)
                    ),
                    local_path="",
                    dev_mode=False,
                )
        upsert_plugin_record(document, record)
        save_registry_document(registry_path, document)

        return InstallResult(
            name=manifest.name,
            version=exact_version,
            scope=scope,
            enabled=bool(enable) or bool(existing.enabled if existing else False),
            store_path=str(target),
            integrity=integrity,
            diagnostics=diagnostics,
        )


def smoke_test_worker(plugin_root: Path, plugin_name: str) -> None:
    client = PluginWorkerClient(plugin_root=plugin_root, plugin_name=plugin_name, timeout_ms=5000)
    try:
        client.start()
        client.initialize(
            {
                "apiVersion": "1",
                "omnicrawlVersion": OMNICRAWL_VERSION,
                "permissions": [],
            },
            timeout_ms=5000,
        )
        client.shutdown(timeout_ms=2000)
    except Exception as exc:  # noqa: BLE001
        try:
            client.close(force=True)
        except Exception:
            pass
        raise PluginInstallError(f"Worker 握手冒烟失败：{exc}") from exc


def _load_scope_registry(scope: str, workspace_root: Path | None) -> tuple[Path, Any]:
    if scope == "project":
        if workspace_root is None:
            raise PluginInstallError("project scope 需要工作区路径。")
        path = project_registry_path(workspace_root)
    else:
        path = user_registry_path()
    return path, load_registry_document(path)


def install_from_npm(
    package_spec: str,
    *,
    scope: str = "user",
    workspace_root: Path | None = None,
    enable: bool = False,
    yes: bool = False,
    confirm: ConfirmCallback | None = None,
    allow_network: bool = True,
) -> InstallResult:
    if scope not in {"user", "project"}:
        raise PluginInstallError(f"非法 scope：{scope}")
    if not allow_network:
        raise PluginInstallError("当前配置不允许网络安装（plugins.allow_network_install=false）。")

    node, npm = detect_node_npm()
    _ = node
    spec = parse_package_spec(package_spec)
    exact_version, version_meta = resolve_exact_version(spec)
    dist = version_meta.get("dist", {})
    if not isinstance(dist, dict):
        raise PluginInstallError("版本 metadata 缺少 dist。")
    tarball_url = str(dist.get("tarball", "")).strip()
    integrity = str(dist.get("integrity", "")).strip()
    if not tarball_url:
        raise PluginInstallError("版本 metadata 缺少 tarball URL。")

    diagnostics: list[str] = []
    store_root = user_store_root()
    store_root.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="omnicrawl-plugin-") as tmp:
        tmp_path = Path(tmp)
        tarball_path = tmp_path / "package.tgz"
        data = download_tarball(tarball_url, tarball_path)
        actual_integrity = _sha512_integrity(data)
        if integrity and actual_integrity != integrity:
            # 某些 registry 可能只给 shasum；有 integrity 时严格匹配。
            raise PluginInstallError("tarball integrity 与 registry 不一致。")
        if not integrity:
            integrity = actual_integrity
            diagnostics.append("registry 未提供 integrity，已使用本地 sha512。")

        extract_root = tmp_path / "extract"
        package_dir = extract_tarball(tarball_path, extract_root)
        manifest = parse_plugin_manifest(
            json.loads((package_dir / "package.json").read_text(encoding="utf-8-sig")),
            source_path=str(package_dir / "package.json"),
        )
        if manifest.name != spec.name:
            raise PluginInstallError(f"包名不匹配：期望 {spec.name}，实际 {manifest.name}")

        # 展示确认信息
        summary = (
            f"将安装插件 {manifest.name}@{exact_version}\n"
            f"  source: {tarball_url}\n"
            f"  integrity: {integrity[:40]}...\n"
            f"  hooks: {', '.join(h.hook for h in manifest.hooks)}\n"
            f"  permissions: {', '.join(manifest.permissions)}\n"
            f"  scope: {scope}\n"
        )
        if not yes:
            ok = True if confirm is None else confirm(summary)
            if not ok:
                raise PluginInstallError("用户取消安装。")

        lock_path = npm_install_production(package_dir, npm)
        lockfile_hash = _sha256_file(lock_path)
        content_hash = _content_tree_hash(package_dir)
        integrity_report = collect_lockfile_integrity_report(lock_path)
        if integrity_report["missingIntegrity"]:
            diagnostics.append(
                "package-lock 中有 "
                f"{len(integrity_report['missingIntegrity'])} 个依赖缺少 integrity："
                + ", ".join(integrity_report["missingIntegrity"][:5])
            )
        diagnostics.append(
            f"lock 依赖条目 {integrity_report['packages']} 个，lockfileHash={lockfile_hash[:12]}"
        )

        integrity_prefix = integrity.split("-", 1)[-1][:12]
        target = store_root / _escape_package_dir(manifest.name) / f"{exact_version}-{integrity_prefix}"
        target.parent.mkdir(parents=True, exist_ok=True)
        if target.exists():
            verify_store_integrity(target, integrity)
            verify_lockfile_hash(target, lockfile_hash)
            verify_content_tree_hash(target, content_hash)
            smoke_test_worker(target, manifest.name)
        else:
            staging = target.parent / f".{target.name}.{secrets.token_hex(6)}.staging"
            shutil.move(str(package_dir), str(staging))
            (staging / ".omnicrawl-integrity").write_text(integrity + "\n", encoding="utf-8")
            try:
                smoke_test_worker(staging, manifest.name)
                os.replace(staging, target)
            finally:
                if staging.exists():
                    shutil.rmtree(staging, ignore_errors=True)

        registry_path, document = _load_scope_registry(scope, workspace_root)
        existing = document.plugins.get(manifest.name)
        version_ref = PluginVersionRef(
            version=exact_version,
            integrity=integrity,
            lockfile_hash=lockfile_hash,
            source=urllib.parse.urlparse(tarball_url).netloc or "registry.npmjs.org",
            store_path=str(target),
            content_hash=content_hash,
        )
        if existing is None:
            record = PluginRecord(
                name=manifest.name,
                enabled=bool(enable),
                active=version_ref if enable else None,
                candidate=None if enable else version_ref,
                previous=None,
                approved_permissions=list(manifest.permissions),
            )
        else:
            previous = existing.active
            # 权限增量必须单独确认；--yes 只跳过普通安装确认，不能批准新权限。
            new_perms = set(manifest.permissions) - set(existing.approved_permissions)
            if new_perms:
                msg = f"插件 {manifest.name} 新增权限：{', '.join(sorted(new_perms))}。是否批准？"
                if confirm is None:
                    raise PluginInstallError(
                        "检测到新增权限；必须交互确认或提供独立的预批准权限策略。"
                    )
                if not confirm(msg):
                    raise PluginInstallError("用户拒绝新增权限。")
            if enable or existing.enabled:
                record = PluginRecord(
                    name=manifest.name,
                    enabled=True if enable else existing.enabled,
                    active=version_ref,
                    candidate=None,
                    previous=previous,
                    approved_permissions=sorted(set(existing.approved_permissions) | set(manifest.permissions)),
                    local_path=existing.local_path,
                    dev_mode=False,
                )
            else:
                record = PluginRecord(
                    name=manifest.name,
                    enabled=False,
                    active=existing.active,
                    candidate=version_ref,
                    previous=existing.previous,
                    approved_permissions=sorted(set(existing.approved_permissions) | set(manifest.permissions)),
                    local_path=existing.local_path,
                    dev_mode=False,
                )
        upsert_plugin_record(document, record)
        save_registry_document(registry_path, document)

    return InstallResult(
        name=manifest.name,
        version=exact_version,
        scope=scope,
        enabled=bool(enable) or bool(existing.enabled if existing else False),
        store_path=str(target),
        integrity=integrity,
        diagnostics=diagnostics,
    )


def register_local_dev_plugin(
    local_path: str | Path,
    *,
    scope: str = "user",
    workspace_root: Path | None = None,
    enable: bool = True,
) -> InstallResult:
    """Phase 1 开发模式：本地路径插件，不进入可回滚 store。"""

    root = Path(local_path).expanduser().resolve()
    if not root.is_dir():
        raise PluginInstallError(f"本地插件目录不存在：{root}")
    package_path = root / "package.json"
    if not package_path.is_file():
        raise PluginInstallError(f"缺少 package.json：{package_path}")
    manifest = parse_plugin_manifest(
        json.loads(package_path.read_text(encoding="utf-8-sig")),
        source_path=str(package_path),
    )
    # 可选冒烟
    try:
        smoke_test_worker(root, manifest.name)
    except PluginInstallError:
        # 本地开发允许先注册，doctor 再报；但仍建议成功。
        pass

    registry_path, document = _load_scope_registry(scope, workspace_root)
    record = PluginRecord(
        name=manifest.name,
        enabled=enable,
        active=None,
        candidate=None,
        previous=None,
        approved_permissions=list(manifest.permissions),
        local_path=str(root),
        dev_mode=True,
    )
    upsert_plugin_record(document, record)
    save_registry_document(registry_path, document)
    return InstallResult(
        name=manifest.name,
        version=manifest.version,
        scope=scope,
        enabled=enable,
        store_path=str(root),
        integrity="dev",
        diagnostics=["dev-mode local path registered"],
    )


def set_enabled(
    name: str,
    enabled: bool,
    *,
    scope: str = "user",
    workspace_root: Path | None = None,
) -> None:
    registry_path, document = _load_scope_registry(scope, workspace_root)
    record = document.plugins.get(name)
    if record is None:
        raise PluginInstallError(f"未找到插件：{name}")
    record.enabled = enabled
    document.plugins[name] = record
    save_registry_document(registry_path, document)


def uninstall_plugin(
    name: str,
    *,
    scope: str = "user",
    workspace_root: Path | None = None,
    purge: bool = False,
) -> None:
    registry_path, document = _load_scope_registry(scope, workspace_root)
    record = document.plugins.get(name)
    if record is None:
        raise PluginInstallError(f"未找到插件：{name}")
    store_paths = []
    for ref in (record.active, record.candidate, record.previous):
        if ref and ref.store_path:
            store_paths.append(Path(ref.store_path))
    remove_plugin_record(document, name)
    save_registry_document(registry_path, document)
    if purge and not record.dev_mode:
        # 项目注册表分散在任意工作区，当前进程无法证明 store 未被其他项目
        # 引用。安全默认是不做物理删除；user scope 仍可在受信任目录内清理。
        if scope == "project":
            return
        store_root = user_store_root().resolve()
        referenced = _registry_store_references(document)
        # 当前工作区可见的另一作用域也必须计入引用。其他未知项目无法被
        # 可靠枚举，因此只有受信任内容寻址 store 且当前已知零引用时才删除。
        other_paths = [user_registry_path()]
        if workspace_root is not None:
            other_paths.append(project_registry_path(workspace_root))
        for other_path in other_paths:
            if Path(other_path) == Path(registry_path):
                continue
            try:
                referenced.update(_registry_store_references(load_registry_document(Path(other_path))))
            except PluginRegistryError:
                continue
        for raw_path in store_paths:
            path = raw_path.expanduser()
            if path.is_symlink():
                raise PluginInstallError(f"拒绝清理符号链接 store：{path}")
            resolved = path.resolve()
            if resolved == store_root or not _is_relative_to(resolved, store_root):
                raise PluginInstallError(f"拒绝清理 store 根目录之外的路径：{path}")
            if str(resolved) in referenced:
                continue
            if resolved.is_dir():
                shutil.rmtree(resolved)


def _registry_store_references(document: Any) -> set[str]:
    references: set[str] = set()
    for item in document.plugins.values():
        for ref in (item.active, item.candidate, item.previous):
            if ref and ref.store_path:
                references.add(str(Path(ref.store_path).expanduser().resolve()))
    return references


def rollback_plugin(
    name: str,
    *,
    scope: str = "user",
    workspace_root: Path | None = None,
) -> PluginVersionRef:
    registry_path, document = _load_scope_registry(scope, workspace_root)
    record = document.plugins.get(name)
    if record is None or record.previous is None:
        raise PluginInstallError(f"插件 {name} 没有可回滚的 previous 版本。")
    previous = record.previous
    if previous.store_path and not Path(previous.store_path).is_dir():
        raise PluginInstallError(f"previous 版本 store 不存在：{previous.store_path}")
    if previous.store_path:
        verify_store_integrity(Path(previous.store_path), previous.integrity)
        verify_lockfile_hash(Path(previous.store_path), previous.lockfile_hash)
        verify_content_tree_hash(Path(previous.store_path), previous.content_hash)
        # 先冒烟 previous，失败则不改 active 指针，保证旧版本（当前 active）仍可用。
        try:
            smoke_test_worker(Path(previous.store_path), name)
        except Exception as exc:  # noqa: BLE001
            raise PluginInstallError(f"回滚冒烟失败，active 保持不变：{exc}") from exc
    failed_active = record.active
    record.active = previous
    record.previous = failed_active
    record.candidate = None
    document.plugins[name] = record
    save_registry_document(registry_path, document)
    return previous


def list_plugins(
    *,
    scope: str = "all",
    workspace_root: Path | None = None,
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    docs: list[tuple[str, Path]] = []
    if scope in {"user", "all"}:
        docs.append(("user", user_registry_path()))
    if scope in {"project", "all"} and workspace_root is not None:
        docs.append(("project", project_registry_path(workspace_root)))
    for scope_name, path in docs:
        try:
            document = load_registry_document(path)
        except PluginRegistryError as exc:
            rows.append({"scope": scope_name, "error": str(exc)})
            continue
        for name, record in sorted(document.plugins.items()):
            rows.append(
                {
                    "name": name,
                    "scope": scope_name,
                    "enabled": record.enabled,
                    "devMode": record.dev_mode,
                    "active": record.active.to_dict() if record.active else None,
                    "candidate": record.candidate.to_dict() if record.candidate else None,
                    "previous": record.previous.to_dict() if record.previous else None,
                    "localPath": record.local_path,
                    "approvedPermissions": list(record.approved_permissions),
                    "registryPath": str(path),
                }
            )
    return rows


def doctor(
    name: str | None = None,
    *,
    workspace_root: Path | None = None,
) -> dict[str, Any]:
    report: dict[str, Any] = {"ok": True, "node": None, "npm": None, "issues": [], "plugins": []}
    try:
        node, npm = detect_node_npm()
        report["node"] = subprocess.check_output([node, "--version"], text=True).strip()
        report["npm"] = subprocess.check_output([npm, "--version"], text=True).strip()
    except Exception as exc:  # noqa: BLE001
        report["ok"] = False
        report["issues"].append(str(exc))

    for row in list_plugins(scope="all", workspace_root=workspace_root):
        if name and row.get("name") != name:
            continue
        issues: list[str] = []
        if row.get("error"):
            issues.append(str(row["error"]))
            report["ok"] = False
        local = row.get("localPath") or ""
        active = row.get("active") or {}
        store = active.get("storePath") if isinstance(active, dict) else ""
        root = local or store
        if root and not Path(str(root)).exists():
            issues.append(f"路径不存在：{root}")
            report["ok"] = False
        elif root:
            try:
                package_path = Path(str(root)) / "package.json"
                parse_plugin_manifest(json.loads(package_path.read_text(encoding="utf-8-sig")))
            except Exception as exc:  # noqa: BLE001
                issues.append(f"manifest 无效：{exc}")
                report["ok"] = False
            if isinstance(active, dict) and active.get("storePath"):
                try:
                    verify_store_integrity(Path(str(active["storePath"])), str(active.get("integrity") or ""))
                    verify_lockfile_hash(
                        Path(str(active["storePath"])),
                        str(active.get("lockfileHash") or ""),
                    )
                except Exception as exc:  # noqa: BLE001
                    issues.append(str(exc))
                    report["ok"] = False
        report["plugins"].append({**row, "issues": issues})
    return report
