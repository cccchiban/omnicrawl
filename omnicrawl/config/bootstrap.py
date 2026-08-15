"""首次启动时创建用户配置并输出运行环境诊断。"""

from __future__ import annotations

import getpass
import os
import shutil
import subprocess
from dataclasses import dataclass
from importlib.resources import files
from pathlib import Path
from typing import Callable, Mapping

from .model_store import ModelStore, ModelStoreError, load_model_store
from .runtime import (
    DEFAULT_CONFIG_FILENAME,
    DEFAULT_MODELS_FILENAME,
    DEFAULT_SUBAGENTS_FILENAME,
    RuntimeConfigError,
    load_config_data,
    migrate_legacy_user_config,
    resolve_config_write_path,
    resolve_models_write_path,
    resolve_subagents_write_path,
    save_config_data,
    user_config_dir as runtime_user_config_dir,
)


@dataclass(frozen=True)
class StartupCheck:
    """一次启动检查的可展示结果。"""

    name: str
    status: str
    message: str


@dataclass(frozen=True)
class StartupSetup:
    """首次启动初始化和诊断的完整结果。"""

    config_dir: Path
    config_path: Path
    models_path: Path
    subagents_path: Path
    config_created: bool
    models_created: bool
    subagents_created: bool
    api_key_prompted: bool
    api_key_configured: bool
    checks: tuple[StartupCheck, ...]
    errors: tuple[str, ...] = ()

    @property
    def first_run(self) -> bool:
        return (
            self.config_created
            or self.models_created
            or self.subagents_created
            or self.api_key_prompted
        )


def user_config_dir(
    environ: Mapping[str, str] | None = None,
    platform_name: str | None = None,
) -> Path:
    """暴露运行时使用的用户配置目录，便于启动层和测试复用。"""

    return runtime_user_config_dir(environ=environ, platform_name=platform_name)


def initialize_user_configuration(
    config_dir: Path | None = None,
    *,
    prompt: Callable[[str], str] | None = None,
    channel_setup: Callable[[Path, Path], bool] | None = None,
) -> StartupSetup:
    """创建配置模板、收集 API Key，并完成首次启动诊断。

    ``config_dir`` 只用于测试或显式的内部调用；正常启动会遵循
    ``AI_CONFIG_FILE`` / ``AI_MODELS_FILE`` 覆盖，否则使用系统用户配置目录。
    """

    migration_error: str | None = None
    if config_dir is None:
        try:
            migrate_legacy_user_config()
        except RuntimeConfigError as exc:
            migration_error = str(exc)
        config_path = resolve_config_write_path()
        models_path = resolve_models_write_path()
        subagents_path = resolve_subagents_write_path()
        resolved_dir = config_path.parent
    else:
        resolved_dir = Path(config_dir).expanduser().resolve()
        config_path = resolved_dir / DEFAULT_CONFIG_FILENAME
        models_path = resolved_dir / DEFAULT_MODELS_FILENAME
        subagents_path = resolved_dir / DEFAULT_SUBAGENTS_FILENAME

    config_path.parent.mkdir(parents=True, exist_ok=True)
    models_path.parent.mkdir(parents=True, exist_ok=True)
    subagents_path.parent.mkdir(parents=True, exist_ok=True)

    config_created = _ensure_template(config_path, "config.example.toml")
    models_created = _ensure_template(models_path, "models.example.toml")
    subagents_created = _ensure_template(subagents_path, "subagents.example.toml")

    errors: list[str] = []
    if migration_error:
        errors.append(migration_error)
    config_data: dict[str, object] = {}
    model_store = ModelStore(version=1, models=(), path=models_path)
    try:
        config_data = load_config_data(config_path)
    except RuntimeConfigError as exc:
        errors.append(f"运行配置读取失败：{exc}")

    try:
        model_store = load_model_store(models_path)
    except ModelStoreError as exc:
        errors.append(f"模型配置读取失败：{exc}")

    api_key_configured = False
    api_key_prompted = False
    if not errors:
        api_key_configured = _active_api_key_configured(config_data, model_store)
        if not api_key_configured and channel_setup is not None and prompt is None:
            api_key_prompted = True
            try:
                completed = bool(channel_setup(config_path, models_path))
            except Exception as exc:  # noqa: BLE001 - 转换为可读启动错误
                completed = False
                errors.append(f"模型渠道配置失败：{exc}")
            if completed and not errors:
                try:
                    config_data = load_config_data(config_path)
                    model_store = load_model_store(models_path)
                    api_key_configured = _active_api_key_configured(
                        config_data,
                        model_store,
                    )
                except (RuntimeConfigError, ModelStoreError) as exc:
                    errors.append(f"模型渠道配置读取失败：{exc}")
        elif not api_key_configured:
            api_key_configured, api_key_prompted, key_error = _ensure_api_key(
                config_data,
                model_store,
                config_path,
                prompt=prompt,
            )
            if key_error:
                errors.append(key_error)

    checks = (
        _check_model_config(config_data, model_store, errors),
        _check_node(),
        _check_plugin_state(config_data),
    )
    return StartupSetup(
        config_dir=resolved_dir,
        config_path=config_path,
        models_path=models_path,
        subagents_path=subagents_path,
        config_created=config_created,
        models_created=models_created,
        subagents_created=subagents_created,
        api_key_prompted=api_key_prompted,
        api_key_configured=api_key_configured,
        checks=checks,
        errors=tuple(errors),
    )


def format_startup_report(setup: StartupSetup) -> tuple[str, ...]:
    """把初始化结果转换为用户可直接执行的启动提示。"""

    if not setup.first_run and setup.api_key_configured and not setup.errors:
        return ()

    lines: list[str] = []
    if setup.config_created or setup.models_created or setup.subagents_created:
        lines.append(f"已准备用户配置目录：{setup.config_dir}")
    if setup.config_created:
        lines.append(f"已生成运行配置：{setup.config_path}")
    if setup.models_created:
        lines.append(f"已生成模型配置：{setup.models_path}")
    if setup.subagents_created:
        lines.append(f"已生成子代理设置：{setup.subagents_path}")

    for error in setup.errors:
        lines.append(f"[错误] {error}")
    for check in setup.checks:
        prefix = "通过" if check.status == "ok" else "警告"
        lines.append(f"[{prefix}] {check.name}：{check.message}")

    if not setup.api_key_configured:
        lines.append("未完成模型渠道配置，本次不启动 TUI。请重新运行并保存至少一个可用渠道。")
    elif setup.api_key_prompted:
        lines.append("模型渠道和 API Key 已保存到本机配置目录。")
    return tuple(lines)


def _ensure_template(path: Path, resource_name: str) -> bool:
    if path.exists():
        return False
    resource = files("omnicrawl").joinpath("config", "templates", resource_name)
    path.write_text(resource.read_text(encoding="utf-8"), encoding="utf-8")
    _restrict_config_permissions(path)
    return True


def _active_api_key_configured(
    config_data: dict[str, object],
    model_store: ModelStore,
) -> bool:
    """检查当前模型 Profile 是否已有直接 Key 或环境变量 Key。"""

    llm = config_data.get("llm")
    if not isinstance(llm, dict):
        return False
    profiles = llm.get("profiles")
    if not isinstance(profiles, dict):
        return False
    profile = profiles.get(_active_profile_id(llm, model_store))
    if not isinstance(profile, dict):
        return False
    provider = str(profile.get("provider") or "openai").strip().lower()
    env_name = str(profile.get("api_key_env") or _default_api_key_env(provider)).strip()
    direct_key = str(profile.get("api_key") or "").strip()
    return bool(direct_key or (env_name and os.getenv(env_name, "").strip()))


def _ensure_api_key(
    config_data: dict[str, object],
    model_store: ModelStore,
    config_path: Path,
    *,
    prompt: Callable[[str], str] | None,
) -> tuple[bool, bool, str | None]:
    llm = config_data.get("llm")
    if not isinstance(llm, dict):
        return False, False, "配置缺少 llm 对象，无法确定 API Key 所属 Profile。"
    profiles = llm.get("profiles")
    if not isinstance(profiles, dict):
        return False, False, "配置缺少 llm.profiles，无法确定 API Key 所属 Profile。"

    profile_id = _active_profile_id(llm, model_store)
    profile = profiles.get(profile_id)
    if not isinstance(profile, dict):
        return False, False, f"当前模型引用的 Profile 不存在：{profile_id or '(空)'}。"

    provider = str(profile.get("provider") or "openai").strip().lower()
    env_name = str(profile.get("api_key_env") or _default_api_key_env(provider)).strip()
    direct_key = str(profile.get("api_key") or "").strip()
    if direct_key or (env_name and os.getenv(env_name, "").strip()):
        return True, False, None

    get_key = prompt or getpass.getpass
    prompted = True
    try:
        value = get_key(
            f"请输入 {env_name or '当前模型 API Key'}（输入不会回显，直接回车跳过）："
        ).strip()
    except (EOFError, KeyboardInterrupt):
        value = ""
    if not value:
        return False, prompted, None

    profile["api_key"] = value
    try:
        save_config_data(config_data, config_path)
    except RuntimeConfigError as exc:
        return False, prompted, f"API Key 保存失败：{exc}"
    _restrict_config_permissions(config_path)
    return True, prompted, None


def _active_profile_id(llm: dict[str, object], model_store: ModelStore) -> str:
    active = llm.get("active_model")
    if isinstance(active, dict):
        explicit_profile = str(active.get("profile") or "").strip()
        if explicit_profile:
            return explicit_profile
        key = str(active.get("key") or "").strip()
        if key:
            try:
                record = model_store.resolve_alias(key)
            except ModelStoreError:
                record = None
            if record is not None:
                return record.profile

    profiles = llm.get("profiles")
    if isinstance(profiles, dict):
        for profile_id, profile in profiles.items():
            if isinstance(profile, dict) and profile.get("enabled", True) is not False:
                return str(profile_id)
    return ""


def _default_api_key_env(provider: str) -> str:
    return {
        "anthropic": "ANTHROPIC_API_KEY",
        "gemini": "GEMINI_API_KEY",
        "openai": "OPENAI_API_KEY",
    }.get(provider, "OPENAI_API_KEY")


def _check_model_config(
    config_data: dict[str, object],
    model_store: ModelStore,
    errors: list[str],
) -> StartupCheck:
    if errors:
        return StartupCheck("模型配置", "warning", "配置文件存在错误，请修复后重试。")
    if not model_store.models:
        return StartupCheck("模型配置", "warning", "models.toml 中没有可用模型。")

    llm = config_data.get("llm")
    active = llm.get("active_model") if isinstance(llm, dict) else None
    key = active.get("key") if isinstance(active, dict) else ""
    try:
        record = model_store.resolve_alias(str(key or ""))
    except ModelStoreError as exc:
        return StartupCheck("模型配置", "warning", str(exc))
    if record is None:
        return StartupCheck("模型配置", "warning", f"找不到默认模型：{key or '(空)'}。")
    return StartupCheck("模型配置", "ok", f"默认模型：{record.display_name} ({record.model_id})")


def _check_node() -> StartupCheck:
    node = shutil.which("node")
    npm = shutil.which("npm")
    if not node:
        return StartupCheck("Node.js", "warning", "未检测到 Node.js；插件功能暂不可用。")
    try:
        result = subprocess.run(
            [node, "--version"],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        version = (result.stdout or result.stderr).strip()
        major = int(version.lstrip("v").split(".", 1)[0])
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        return StartupCheck("Node.js", "warning", f"版本检查失败：{exc}")
    if major < 20:
        return StartupCheck("Node.js", "warning", f"需要 Node.js >=20，当前为 {version}。")
    if not npm:
        return StartupCheck("Node.js", "warning", f"Node.js {version} 可用，但未找到 npm。")
    return StartupCheck("Node.js", "ok", f"Node.js {version} / npm 可用。")


def _check_plugin_state(config_data: dict[str, object]) -> StartupCheck:
    plugins = config_data.get("plugins")
    enabled = isinstance(plugins, dict) and bool(plugins.get("enabled", False))
    try:
        from ..extensions.plugin_install import list_plugins

        rows = list_plugins(scope="all", workspace_root=Path.cwd())
    except Exception as exc:  # noqa: BLE001 - 启动诊断不能阻塞主流程
        return StartupCheck("插件状态", "warning", f"读取插件注册表失败：{exc}")

    errors = [str(row["error"]) for row in rows if row.get("error")]
    enabled_count = sum(1 for row in rows if row.get("enabled"))
    state = "已启用" if enabled else "已禁用"
    if errors:
        return StartupCheck("插件状态", "warning", "; ".join(errors))
    return StartupCheck(
        "插件状态",
        "ok",
        f"插件系统{state}，已注册 {len(rows)} 个插件，当前启用 {enabled_count} 个。",
    )


def _restrict_config_permissions(path: Path) -> None:
    """在支持 POSIX 权限的系统上限制配置文件读取权限。"""

    try:
        if os.name != "nt":
            path.chmod(0o600)
    except OSError:
        # Windows 或受限文件系统不支持时不影响启动；文件仍不会进入包或 Git。
        pass
