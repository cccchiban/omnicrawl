"""严格使用 YAML 的运行配置仓库，支持 UTF-8 读取与原子写回。"""

from __future__ import annotations

import os
import sys
import tempfile
from pathlib import Path
from typing import Any, Mapping


DEFAULT_CONFIG_FILENAME = "config.yaml"
DEFAULT_MODELS_FILENAME = "models.yaml"
CONFIG_PATH_ENV = "AI_CONFIG_FILE"
MODELS_PATH_ENV = "AI_MODELS_FILE"
_YAML_SUFFIXES = {".yaml", ".yml"}


class RuntimeConfigError(RuntimeError):
    """本地配置文件读取或校验失败时抛出。"""


def user_config_dir(
    environ: Mapping[str, str] | None = None,
    platform_name: str | None = None,
) -> Path:
    """返回跨平台的用户配置目录，不依赖当前工作目录或 site-packages。"""

    env = os.environ if environ is None else environ
    platform = platform_name or sys.platform
    if platform.startswith("win"):
        appdata = env.get("APPDATA", "").strip()
        base = Path(appdata) if appdata else Path.home() / "AppData" / "Roaming"
        return base / "OmniCrawl"

    config_home = env.get("XDG_CONFIG_HOME", "").strip()
    base = Path(config_home) if config_home else Path.home() / ".config"
    return base / "omnicrawl"


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
    """查找默认配置；找不到现有文件时返回用户目录作为创建目标。"""

    working_directory_path = Path.cwd() / filename
    if working_directory_path.is_file():
        return working_directory_path

    user_path = user_config_dir() / filename
    if user_path.is_file():
        return user_path

    if _is_development_environment():
        source_path = project_root() / filename
        if source_path.is_file():
            return source_path

    return user_path


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

    path = resolve_config_path(config_path)
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
            os.replace(tmp_path, path)
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
