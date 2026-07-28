"""严格使用 YAML 的运行配置仓库，支持 UTF-8 读取与原子写回。"""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Iterable, Mapping


DEFAULT_CONFIG_FILENAME = "config.yaml"
DEFAULT_MODELS_FILENAME = "models.yaml"
GLOBAL_AGENTS_FILENAME = "AGENTS.md"
USER_CONFIG_DIRNAME = ".OmniCrawl"
CONFIG_PATH_ENV = "AI_CONFIG_FILE"
MODELS_PATH_ENV = "AI_MODELS_FILE"
_YAML_SUFFIXES = {".yaml", ".yml"}


class RuntimeConfigError(RuntimeError):
    """本地配置文件读取或校验失败时抛出。"""


def user_config_dir(
    environ: Mapping[str, str] | None = None,
    platform_name: str | None = None,
) -> Path:
    """返回统一的用户配置目录，不依赖当前工作目录或 site-packages。

    配置统一放在用户主目录下，便于安装版和源码版使用同一位置：
    ``~/.OmniCrawl``。``environ`` 和 ``platform_name`` 保留用于测试及兼容
    旧调用方，但新目录本身不再按操作系统区分。
    """

    return Path.home() / USER_CONFIG_DIRNAME


def legacy_user_config_dirs(
    environ: Mapping[str, str] | None = None,
    platform_name: str | None = None,
) -> tuple[Path, ...]:
    """返回升级前的用户配置目录，按兼容顺序排列。"""

    env = os.environ if environ is None else environ
    platform = platform_name or sys.platform
    if platform.startswith("win"):
        appdata = env.get("APPDATA", "").strip()
        base = Path(appdata) if appdata else Path.home() / "AppData" / "Roaming"
        return (base / "OmniCrawl", Path.home() / ".omnicrawl")

    config_home = env.get("XDG_CONFIG_HOME", "").strip()
    base = Path(config_home) if config_home else Path.home() / ".config"
    return (base / "omnicrawl", Path.home() / ".omnicrawl")


def global_agents_path() -> Path:
    """返回用户级全局 AGENTS.md 路径。"""

    return user_config_dir() / GLOBAL_AGENTS_FILENAME


def migrate_legacy_user_config(
    *,
    environ: Mapping[str, str] | None = None,
    platform_name: str | None = None,
    legacy_dirs: Iterable[Path] | None = None,
) -> Path:
    """把旧用户目录迁移到 ``~/.OmniCrawl`` 并删除旧目录。

    目标目录中的文件不会被覆盖；发生同名冲突时，旧文件保存为
    ``<name>.migrated.bak``（必要时追加序号），然后再删除旧目录。任何
    无法迁移的文件都会中止操作并保留旧目录，避免升级过程造成配置丢失。
    """

    target = user_config_dir(environ=environ, platform_name=platform_name)
    target.mkdir(parents=True, exist_ok=True)
    candidates = (
        tuple(legacy_dirs)
        if legacy_dirs is not None
        else legacy_user_config_dirs(environ, platform_name)
    )
    for legacy in candidates:
        legacy = Path(legacy).expanduser()
        try:
            if not legacy.is_dir() or legacy.resolve() == target.resolve():
                continue
        except OSError as exc:
            raise RuntimeConfigError(f"检查旧配置目录失败：{legacy}，{exc}") from exc

        try:
            for item in tuple(legacy.iterdir()):
                destination = target / item.name
                if destination.exists():
                    destination = _migration_backup_path(target, item.name)
                shutil.move(str(item), str(destination))
            legacy.rmdir()
        except OSError as exc:
            raise RuntimeConfigError(
                f"迁移旧配置目录失败：{legacy} -> {target}，{exc}。旧目录已保留，请修复权限后重试。"
            ) from exc
    return target


def _migration_backup_path(target: Path, name: str) -> Path:
    """为迁移冲突生成不覆盖既有文件的备份路径。"""

    candidate = target / f"{name}.migrated.bak"
    index = 1
    while candidate.exists():
        candidate = target / f"{name}.migrated.{index}.bak"
        index += 1
    return candidate


def project_root() -> Path:
    """返回源码项目根目录，保留旧调用名供开发工具使用。"""

    return Path(__file__).resolve().parent.parent.parent


def default_config_path() -> Path:
    """返回用户默认运行配置路径。"""

    return user_config_dir() / DEFAULT_CONFIG_FILENAME


def default_yaml_config_path() -> Path:
    """兼容既有调用名；默认配置本身就是 YAML。"""

    return default_config_path()


def default_models_path() -> Path:
    """返回用户默认模型目录配置路径。"""

    return user_config_dir() / DEFAULT_MODELS_FILENAME


def resolve_config_path(config_path: str | Path | None = None) -> Path:
    """按显式路径、环境变量、工作区、用户目录和开发源码回退解析。"""

    if config_path is not None:
        path = Path(config_path).expanduser()
    else:
        raw_env = os.getenv(CONFIG_PATH_ENV, "").strip()
        path = (
            Path(raw_env).expanduser()
            if raw_env
            else _resolve_default_path(DEFAULT_CONFIG_FILENAME)
        )
    _validate_yaml_path(path, source="运行配置")
    return path


def resolve_models_path(models_path: str | Path | None = None) -> Path:
    """按显式路径、环境变量、工作区、用户目录和开发源码回退解析。"""

    if models_path is not None:
        path = Path(models_path).expanduser()
    else:
        raw_env = os.getenv(MODELS_PATH_ENV, "").strip()
        path = (
            Path(raw_env).expanduser()
            if raw_env
            else _resolve_default_path(DEFAULT_MODELS_FILENAME)
        )
    _validate_yaml_path(path, source="模型配置")
    return path


def _resolve_default_path(filename: str) -> Path:
    """按用户目录、工作区、源码回退顺序查找默认配置。"""

    user_path = user_config_dir() / filename
    if user_path.is_file():
        return user_path

    for legacy_dir in legacy_user_config_dirs():
        legacy_path = legacy_dir / filename
        if legacy_path.is_file():
            return legacy_path

    if _is_development_environment():
        working_directory_path = Path.cwd() / filename
        if working_directory_path.is_file():
            return working_directory_path

        source_path = project_root() / filename
        if source_path.is_file():
            return source_path

    return user_path


def resolve_config_write_path(config_path: str | Path | None = None) -> Path:
    """解析配置写入路径；未显式指定时始终写入用户目录。"""

    if config_path is not None:
        path = Path(config_path).expanduser()
    else:
        raw_env = os.getenv(CONFIG_PATH_ENV, "").strip()
        path = Path(raw_env).expanduser() if raw_env else user_config_dir() / DEFAULT_CONFIG_FILENAME
    _validate_yaml_path(path, source="运行配置")
    return path


def resolve_models_write_path(models_path: str | Path | None = None) -> Path:
    """解析模型配置写入路径；未显式指定时始终写入用户目录。"""

    if models_path is not None:
        path = Path(models_path).expanduser()
    else:
        raw_env = os.getenv(MODELS_PATH_ENV, "").strip()
        path = Path(raw_env).expanduser() if raw_env else user_config_dir() / DEFAULT_MODELS_FILENAME
    _validate_yaml_path(path, source="模型配置")
    return path


def _is_development_environment() -> bool:
    """仅在源码项目中启用源码配置回退，避免 Wheel 误读安装目录文件。"""

    root = project_root()
    return (
        (root / "pyproject.toml").is_file()
        and (root / "setup.cfg").is_file()
        and (root / "omnicrawl").is_dir()
    )


def load_config_data(config_path: str | Path | None = None) -> dict[str, Any]:
    """读取 YAML 运行配置；不存在时返回空对象，不再读取或迁移 JSON。"""

    path = resolve_config_path(config_path)
    if path.exists():
        return _load_mapping_file(path)

    # 只检测默认位置的遗留文件以提供可操作错误，不解析、不迁移，也不把它
    # 当作配置源。显式路径和 AI_CONFIG_FILE 已在扩展名校验阶段直接拒绝 JSON。
    if config_path is None and not os.getenv(CONFIG_PATH_ENV, "").strip():
        legacy_path = path.with_suffix(".json")
        if legacy_path.exists():
            raise RuntimeConfigError(
                "检测到不再支持的 config.json，且 config.yaml 不存在。"
                "请根据 config.example.yaml 手工创建 config.yaml；程序不会读取或自动迁移 JSON。"
            )
    return {}


def save_config_data(data: Mapping[str, Any], config_path: str | Path | None = None) -> Path:
    """把运行配置以 YAML 原子写回；JSON 和未知扩展名会被明确拒绝。"""

    if not isinstance(data, Mapping):
        raise RuntimeConfigError("配置数据必须是对象。")

    path = resolve_config_write_path(config_path)
    _atomic_write_text(path, _dump_yaml(dict(data)))
    return path


def get_section(data: Mapping[str, Any], key: str) -> dict[str, Any]:
    """安全读取配置子对象。"""

    value = data.get(key, {})
    if value in (None, ""):
        return {}
    if not isinstance(value, dict):
        raise RuntimeConfigError(f"配置项 {key} 必须是对象。")
    return dict(value)


def _validate_yaml_path(path: Path, *, source: str) -> None:
    if path.suffix.lower() not in _YAML_SUFFIXES:
        raise RuntimeConfigError(
            f"{source}仅支持 .yaml 或 .yml 文件：{path}。JSON 配置已停止支持。"
        )


def _load_mapping_file(path: Path) -> dict[str, Any]:
    _validate_yaml_path(path, source="配置文件")
    try:
        text = path.read_text(encoding="utf-8-sig")
    except OSError as exc:
        raise RuntimeConfigError(f"读取配置文件失败：{path}，{exc}") from exc
    return _load_yaml(text, path)


def _load_yaml(text: str, path: Path) -> dict[str, Any]:
    if not text.strip():
        return {}
    try:
        import yaml
    except ImportError as exc:
        raise RuntimeConfigError("缺少 PyYAML 依赖，请先执行：pip install PyYAML") from exc
    try:
        data = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise RuntimeConfigError(f"配置文件 YAML 解析失败：{path}，{exc}") from exc
    if data is None:
        return {}
    if not isinstance(data, dict):
        raise RuntimeConfigError(f"配置文件顶层必须是对象：{path}")
    return data


def _dump_yaml(data: Mapping[str, Any]) -> str:
    try:
        import yaml
    except ImportError as exc:
        raise RuntimeConfigError("缺少 PyYAML 依赖，请先执行：pip install PyYAML") from exc

    class _Dumper(yaml.SafeDumper):
        pass

    def _str_representer(dumper: yaml.SafeDumper, value: str) -> Any:
        if "\n" in value:
            return dumper.represent_scalar("tag:yaml.org,2002:str", value, style="|")
        return dumper.represent_scalar("tag:yaml.org,2002:str", value)

    _Dumper.add_representer(str, _str_representer)
    return yaml.dump(
        dict(data),
        Dumper=_Dumper,
        allow_unicode=True,
        default_flow_style=False,
        sort_keys=False,
    )


# 与 session_locking 保持同量级：Windows 目标文件短暂占用时 os.replace 可能 WinError 5。
# config 层不依赖 state，因此在此内联同等短重试，避免循环导入。
_ATOMIC_REPLACE_MAX_ATTEMPTS = 8
_ATOMIC_REPLACE_RETRY_SECONDS = 0.05


def _is_transient_windows_access_denied(exc: BaseException) -> bool:
    """判断是否为 Windows 上可重试的目标文件占用错误。"""

    if sys.platform != "win32":
        return False
    if not isinstance(exc, OSError):
        return False
    if getattr(exc, "winerror", None) == 5:
        return True
    if isinstance(exc, PermissionError):
        return True
    return getattr(exc, "errno", None) in {getattr(os, "EACCES", 13), 13}


def _replace_with_retry(temp_path: Path, path: Path) -> None:
    """原子替换；Windows 短暂 Access Denied 时短退避重试。"""

    attempts = _ATOMIC_REPLACE_MAX_ATTEMPTS if sys.platform == "win32" else 1
    for attempt in range(1, attempts + 1):
        try:
            os.replace(temp_path, path)
            return
        except OSError as exc:
            if attempt >= attempts or not _is_transient_windows_access_denied(exc):
                raise
            time.sleep(_ATOMIC_REPLACE_RETRY_SECONDS * attempt)


def _atomic_write_text(path: Path, text: str) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp_name = tempfile.mkstemp(
            prefix=f".{path.name}.",
            suffix=".tmp",
            dir=str(path.parent),
        )
        tmp_path = Path(tmp_name)
        try:
            with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
                handle.write(text)
                handle.flush()
                try:
                    os.fsync(handle.fileno())
                except OSError:
                    pass
            _replace_with_retry(tmp_path, path)
        finally:
            if tmp_path.exists():
                try:
                    tmp_path.unlink()
                except OSError:
                    pass
    except OSError as exc:
        raise RuntimeConfigError(f"写入配置文件失败：{path}，{exc}") from exc


def load_raw_file(path: Path) -> dict[str, Any]:
    """供 model_store 等 YAML 配置模块复用的底层加载。"""

    return _load_mapping_file(path)


def atomic_write_text(path: Path, text: str) -> None:
    _atomic_write_text(path, text)


def dump_yaml_text(data: Mapping[str, Any]) -> str:
    return _dump_yaml(data)
