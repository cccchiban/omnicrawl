"""运行配置仓库：JSON 兼容 + YAML 优先，原子写回。"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any, Mapping


# 兼容既有 README / 测试：默认路径仍指向 config.json。
# 加载时若同目录存在 config.yaml，则以 YAML 为权威。
DEFAULT_CONFIG_FILENAME = "config.json"
DEFAULT_YAML_CONFIG_FILENAME = "config.yaml"
DEFAULT_MODELS_FILENAME = "models.yaml"
CONFIG_PATH_ENV = "AI_CONFIG_FILE"
MODELS_PATH_ENV = "AI_MODELS_FILE"


class RuntimeConfigError(RuntimeError):
    """本地配置文件读取或校验失败时抛出。"""


def project_root() -> Path:
    return Path(__file__).resolve().parent.parent.parent


def default_config_path() -> Path:
    """返回项目默认配置文件路径（兼容既有 config.json 约定）。"""

    return project_root() / DEFAULT_CONFIG_FILENAME


def default_yaml_config_path() -> Path:
    return project_root() / DEFAULT_YAML_CONFIG_FILENAME


def default_models_path() -> Path:
    return project_root() / DEFAULT_MODELS_FILENAME


def resolve_config_path(config_path: str | Path | None = None) -> Path:
    """解析配置文件路径。

    解析顺序为：显式参数 > `AI_CONFIG_FILE` > 项目根目录 `config.json`。
    注意：无显式路径时，`load_config_data` 还会额外检查同目录 `config.yaml`。
    """

    if config_path is not None:
        return Path(config_path).expanduser()

    raw_env = os.getenv(CONFIG_PATH_ENV, "").strip()
    if raw_env:
        return Path(raw_env).expanduser()

    return default_config_path()


def resolve_models_path(models_path: str | Path | None = None) -> Path:
    if models_path is not None:
        return Path(models_path).expanduser()
    raw_env = os.getenv(MODELS_PATH_ENV, "").strip()
    if raw_env:
        return Path(raw_env).expanduser()
    return default_models_path()


def load_config_data(config_path: str | Path | None = None) -> dict[str, Any]:
    """读取并解析运行配置。

    规则：
    1. 显式/环境路径存在 → 按扩展名解析 yaml/json。
    2. 默认路径：config.yaml 存在则以 YAML 为权威。
    3. 仅有 config.json → 尝试迁移为 YAML；迁移失败则兼容读取 JSON。
    4. 都不存在 → 返回空字典。
    """

    # 显式参数或 AI_CONFIG_FILE
    if config_path is not None or os.getenv(CONFIG_PATH_ENV, "").strip():
        path = resolve_config_path(config_path)
        if not path.exists():
            return {}
        return _load_mapping_file(path)

    yaml_path = default_yaml_config_path()
    json_path = default_config_path()

    if yaml_path.exists():
        return _load_mapping_file(yaml_path)

    if json_path.exists():
        try:
            from .migration import maybe_migrate_config_json

            migrated = maybe_migrate_config_json(
                json_path=json_path,
                yaml_path=yaml_path,
                models_path=default_models_path(),
            )
            if migrated and yaml_path.exists():
                return _load_mapping_file(yaml_path)
        except Exception:
            # 迁移失败不阻塞启动，回退读 JSON。
            pass
        return _load_mapping_file(json_path)

    return {}


def save_config_data(data: Mapping[str, Any], config_path: str | Path | None = None) -> Path:
    """把运行配置原子写回文件。

    - 显式/环境目标：按扩展名写 JSON 或 YAML。
    - 默认路径：若已有 config.yaml 则写 YAML，否则写 config.json（兼容旧流程）。
    """

    if not isinstance(data, Mapping):
        raise RuntimeConfigError("配置数据必须是对象。")

    payload = dict(data)

    if config_path is not None or os.getenv(CONFIG_PATH_ENV, "").strip():
        path = resolve_config_path(config_path)
    else:
        yaml_path = default_yaml_config_path()
        path = yaml_path if yaml_path.exists() else default_config_path()

    suffix = path.suffix.lower()
    if suffix in {".yaml", ".yml"}:
        text = _dump_yaml(payload)
    else:
        text = json.dumps(payload, ensure_ascii=False, indent=2) + "\n"
    _atomic_write_text(path, text)
    return path


def get_section(data: Mapping[str, Any], key: str) -> dict[str, Any]:
    """安全读取配置子对象。"""

    value = data.get(key, {})
    if value in (None, ""):
        return {}
    if not isinstance(value, dict):
        raise RuntimeConfigError(f"配置项 {key} 必须是对象。")
    return dict(value)


def _load_mapping_file(path: Path) -> dict[str, Any]:
    try:
        text = path.read_text(encoding="utf-8-sig")
    except OSError as exc:
        raise RuntimeConfigError(f"读取配置文件失败：{path}，{exc}") from exc

    suffix = path.suffix.lower()
    if suffix in {".yaml", ".yml"}:
        data = _load_yaml(text, path)
    elif suffix == ".json":
        try:
            data = json.loads(text) if text.strip() else {}
        except json.JSONDecodeError as exc:
            raise RuntimeConfigError(
                f"配置文件 JSON 解析失败：{path}，第 {exc.lineno} 行第 {exc.colno} 列：{exc.msg}"
            ) from exc
    else:
        try:
            data = json.loads(text) if text.strip() else {}
        except json.JSONDecodeError:
            data = _load_yaml(text, path)

    if not isinstance(data, dict):
        raise RuntimeConfigError(f"配置文件顶层必须是对象：{path}")
    return data


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
    """供 migration / model_store 复用的底层加载。"""

    return _load_mapping_file(path)


def atomic_write_text(path: Path, text: str) -> None:
    _atomic_write_text(path, text)


def dump_yaml_text(data: Mapping[str, Any]) -> str:
    return _dump_yaml(data)
