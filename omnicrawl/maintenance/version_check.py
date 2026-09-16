"""通过 PyPI RSS 检测 OmniCrawl 最新版本，并维护短期本地缓存。"""

from __future__ import annotations

import json
import os
import re
import time
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import httpx

from ..config.core.runtime import user_config_dir
from ..extensions.plugin_models import OMNICRAWL_VERSION


PYPI_RELEASES_RSS_URL = (
    "https://pypi.org/rss/project/omnicrawl-agent/releases.xml"
)
VERSION_CHECK_TIMEOUT_SECONDS = 3.0
VERSION_CHECK_CACHE_TTL_SECONDS = 24 * 60 * 60
VERSION_CHECK_CACHE_FILENAME = "version-check.json"
_MAX_RSS_BYTES = 256 * 1024
_VERSION_PATTERN = re.compile(
    r"^v?(?P<release>\d+(?:\.\d+)*)"
    r"(?:(?P<pre>a|b|rc)(?P<pre_number>\d+))?"
    r"(?:\.?post(?P<post_number>\d+))?"
    r"(?:\.?dev(?P<dev_number>\d+))?"
    r"(?:\+[a-z0-9.]+)?$",
    re.IGNORECASE,
)
VersionFetcher = Callable[[str, float], bytes]


@dataclass(frozen=True)
class VersionCheckResult:
    """一次版本检查的稳定结果；失败时 ``latest_version`` 为空。"""

    current_version: str
    latest_version: str | None

    @property
    def update_available(self) -> bool:
        return bool(
            self.latest_version
            and is_newer_version(self.latest_version, self.current_version)
        )


def current_version() -> str:
    """返回随源码和 Wheel 一同发布的当前 OmniCrawl 版本。"""

    return OMNICRAWL_VERSION


def parse_latest_version(payload: bytes) -> str:
    """按 PyPI RSS 顺序返回首个有效发布版本。"""

    if len(payload) > _MAX_RSS_BYTES:
        raise ValueError("PyPI RSS 响应超过允许大小。")
    root = ET.fromstring(payload)
    for item in root.findall("./channel/item"):
        title = str(item.findtext("title") or "").strip()
        if _parse_version(title) is not None:
            return title.lstrip("vV")
    raise ValueError("PyPI RSS 中没有有效版本。")


def is_newer_version(candidate: str, current: str) -> bool:
    """比较常见 PEP 440 数字版本，异常值按不可升级处理。"""

    candidate_key = _parse_version(candidate)
    current_key = _parse_version(current)
    if candidate_key is None or current_key is None:
        return False

    candidate_release, candidate_stage = candidate_key
    current_release, current_stage = current_key
    width = max(len(candidate_release), len(current_release))
    candidate_release = candidate_release + (0,) * (width - len(candidate_release))
    current_release = current_release + (0,) * (width - len(current_release))
    return (candidate_release, candidate_stage) > (current_release, current_stage)


def check_latest_version(
    installed_version: str | None = None,
    *,
    cache_path: Path | None = None,
    now: float | None = None,
    fetcher: VersionFetcher | None = None,
) -> VersionCheckResult:
    """读取 24 小时缓存或请求 PyPI；任何 I/O 失败都静默降级。"""

    installed = str(installed_version or current_version()).strip()
    cache = cache_path or user_config_dir() / VERSION_CHECK_CACHE_FILENAME
    checked_at = time.time() if now is None else float(now)
    cached = _read_fresh_cache(cache, checked_at)
    if cached is not None:
        return VersionCheckResult(installed, cached)

    try:
        payload = (fetcher or _fetch_pypi_rss)(
            PYPI_RELEASES_RSS_URL,
            VERSION_CHECK_TIMEOUT_SECONDS,
        )
        latest = parse_latest_version(payload)
        _write_cache(cache, checked_at, latest)
        return VersionCheckResult(installed, latest)
    except Exception:  # noqa: BLE001
        # 版本检查不能改变应用可用性；离线、代理和缓存损坏均只隐藏更新提示。
        return VersionCheckResult(installed, None)


def _parse_version(value: str) -> tuple[tuple[int, ...], tuple[int, int]] | None:
    text = str(value or "").strip()
    match = _VERSION_PATTERN.fullmatch(text)
    if match is None:
        return None

    release = tuple(int(part) for part in match.group("release").split("."))
    if match.group("post_number") is not None:
        stage = (4, int(match.group("post_number")))
    elif match.group("pre") is not None:
        stage_order = {"a": 0, "b": 1, "rc": 2}
        stage = (
            stage_order[match.group("pre").lower()],
            int(match.group("pre_number")),
        )
    elif match.group("dev_number") is not None:
        stage = (-1, int(match.group("dev_number")))
    else:
        stage = (3, 0)
    return release, stage


def _fetch_pypi_rss(url: str, timeout: float) -> bytes:
    """直连公开 PyPI，避免损坏的全局代理配置耗尽启动检测超时。"""

    response = httpx.get(
        url,
        headers={
            "Accept": "application/rss+xml, application/xml;q=0.9",
            "User-Agent": f"OmniCrawl/{current_version()} version-check",
        },
        timeout=timeout,
        follow_redirects=True,
        trust_env=False,
    )
    response.raise_for_status()
    payload = response.content
    if len(payload) > _MAX_RSS_BYTES:
        raise ValueError("PyPI RSS 响应超过允许大小。")
    return payload


def _read_fresh_cache(path: Path, now: float) -> str | None:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        checked_at = float(data["checked_at"])
        latest = str(data["latest_version"]).strip()
        age = max(0.0, now - checked_at)
        if age < VERSION_CHECK_CACHE_TTL_SECONDS and _parse_version(latest):
            return latest
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError):
        return None
    return None


def _write_cache(path: Path, checked_at: float, latest_version: str) -> None:
    temp_path = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        temp_path.write_text(
            json.dumps(
                {
                    "checked_at": checked_at,
                    "latest_version": latest_version,
                },
                ensure_ascii=True,
                separators=(",", ":"),
            ),
            encoding="utf-8",
        )
        os.replace(temp_path, path)
    except OSError:
        # 缓存失败不应抹掉已经取得的在线检查结果。
        pass
    finally:
        try:
            temp_path.unlink(missing_ok=True)
        except OSError:
            pass


__all__ = [
    "PYPI_RELEASES_RSS_URL",
    "VERSION_CHECK_CACHE_TTL_SECONDS",
    "VersionCheckResult",
    "check_latest_version",
    "current_version",
    "is_newer_version",
    "parse_latest_version",
]
