#!/usr/bin/env python3
"""生成本地 API 配置面（`api/models.py::APIConfig` 与 `api/app.py::load_api_config`）的对照数据集。

期望值来自 Python 真实现：

- `APIConfig` 的校验顺序与文案（令牌、回环地址、端口、确认超时、worker 上限、CORS 通配符）；
- 归一化结果（令牌 strip、来源列表去空白与空串、端口与 worker 取整）；
- `load_api_config` 的环境变量优先级、`api` 段类型校验与解析错误文案。

环境变量用 `mock.patch.dict` 注入（`USERPROFILE`/`HOME` 也一并固定），`load_config_data`
被替换成返回 `{"api": <用例段>}`，因此 `get_section` 的类型校验同样走真实现。

用法：``python rust/tools/gen_api_config_fixture.py``
输出：``rust/crates/omnicrawl-api/tests/fixtures/api_config_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-api/tests/fixtures/api_config_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.api import app as APP  # noqa: E402
from omnicrawl.api.models import APIConfig  # noqa: E402

if not Path(APP.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

# 注入的环境表：与 Rust 侧 `ConfigEnvironment::new(home, "win32")` 同义的固定 home。
BASE_ENV = {"USERPROFILE": "C:\\oc-api-parity\\home", "HOME": "C:\\oc-api-parity\\home"}
TOKEN = "parity-token"


def _payload(config: APIConfig) -> dict:
    return {
        "bearer_token": config.bearer_token,
        "host": config.host,
        "port": config.port,
        "allowed_origins": list(config.allowed_origins),
        "confirmation_timeout_seconds": config.confirmation_timeout_seconds,
        "workers": config.workers,
    }


def construct_case(name: str, **kwargs) -> dict:
    """`APIConfig(...)` 的构造结果：归一化字段或异常文案。"""

    try:
        return {"name": name, "input": kwargs, "outcome": {"ok": _payload(APIConfig(**kwargs))}}
    except (ValueError, TypeError) as error:
        return {"name": name, "input": kwargs, "outcome": {"error": str(error)}}


def load_case(name: str, *, env: dict | None = None, section: object = None) -> dict:
    """`load_api_config()` 的结果：环境变量与 `api` 段一起喂给真实现。"""

    environ = dict(BASE_ENV)
    environ.update(env or {})
    with mock.patch.dict(os.environ, environ, clear=True):
        with mock.patch.object(APP, "load_config_data", lambda *args, **kwargs: {"api": section}):
            try:
                outcome = {"ok": _payload(APP.load_api_config())}
            except Exception as error:  # noqa: BLE001 - 用例要的就是异常文案
                outcome = {"error": str(error)}
    return {"name": name, "env": env or {}, "section": section, "outcome": outcome}


def build_constructs() -> list[dict]:
    return [
        construct_case("缺省字段与令牌去空白", bearer_token=" parity-token "),
        construct_case("空令牌", bearer_token=""),
        construct_case("纯空白令牌", bearer_token="   "),
        construct_case("回环地址大小写与空白", bearer_token=TOKEN, host=" LocalHost "),
        construct_case("IPv6 回环", bearer_token=TOKEN, host="::1"),
        construct_case("非回环地址", bearer_token=TOKEN, host="0.0.0.0"),
        construct_case("端口下界越界", bearer_token=TOKEN, port=0),
        construct_case("端口上界越界", bearer_token=TOKEN, port=65536),
        construct_case("端口为整数值字符串", bearer_token=TOKEN, port="8765"),
        construct_case("确认超时为 0", bearer_token=TOKEN, confirmation_timeout_seconds=0),
        construct_case("确认超时为负", bearer_token=TOKEN, confirmation_timeout_seconds=-1),
        construct_case("worker 为 0", bearer_token=TOKEN, workers=0),
        construct_case("worker 超上限", bearer_token=TOKEN, workers=33),
        construct_case(
            "来源列表去空白与空串",
            bearer_token=TOKEN,
            allowed_origins=("  http://a  ", "", "   ", "http://b"),
        ),
        construct_case("来源通配符", bearer_token=TOKEN, allowed_origins=(" * ",)),
    ]


def build_loads() -> list[dict]:
    return [
        load_case("段内令牌", section={"bearer_token": TOKEN}),
        load_case("环境变量令牌优先", env={"OMNICRAWL_API_TOKEN": " env-token "}, section={"bearer_token": "file-token"}),
        load_case(
            "环境变量令牌为空回退段内",
            env={"OMNICRAWL_API_TOKEN": "   "},
            section={"bearer_token": "file-token"},
        ),
        load_case("缺少令牌", section={}),
        load_case("令牌字段非字符串", section={"bearer_token": 123}),
        load_case("api 段为字符串", section="text"),
        load_case("api 段为空字符串", section=""),
        load_case(
            "监听地址字段非字符串",
            section={"bearer_token": TOKEN, "host": 8765},
        ),
        load_case(
            "监听地址字段为列表",
            section={"bearer_token": TOKEN, "host": ["127.0.0.1"]},
        ),
        load_case(
            "环境变量覆盖地址与端口",
            env={"OMNICRAWL_API_HOST": "localhost", "OMNICRAWL_API_PORT": "9000"},
            section={"bearer_token": TOKEN, "host": "127.0.0.1", "port": 8765},
        ),
        load_case(
            "环境变量 worker",
            env={"OMNICRAWL_API_WORKERS": "2"},
            section={"bearer_token": TOKEN},
        ),
        load_case("端口为字面量浮点", section={"bearer_token": TOKEN, "port": 9000.5}),
        load_case("端口为布尔", section={"bearer_token": TOKEN, "port": True}),
        load_case("端口为布尔假", section={"bearer_token": TOKEN, "port": False}),
        load_case("端口为数字字符串", section={"bearer_token": TOKEN, "port": "8765"}),
        load_case("端口为小数字符串", section={"bearer_token": TOKEN, "port": "9000.0"}),
        load_case("端口为字母", section={"bearer_token": TOKEN, "port": "abc"}),
        load_case("端口越界", section={"bearer_token": TOKEN, "port": 70000}),
        load_case(
            "来源为逗号分隔字符串",
            section={"bearer_token": TOKEN, "allowed_origins": "http://a, ,http://b"},
        ),
        load_case(
            "来源为字符串列表",
            section={"bearer_token": TOKEN, "allowed_origins": [" http://a ", "", "http://b"]},
        ),
        load_case(
            "来源为非法列表",
            section={"bearer_token": TOKEN, "allowed_origins": [1, 2]},
        ),
        load_case(
            "来源为表",
            section={"bearer_token": TOKEN, "allowed_origins": {"a": "http://a"}},
        ),
        load_case(
            "确认超时为数字字符串",
            section={"bearer_token": TOKEN, "confirmation_timeout_seconds": "60"},
        ),
        load_case(
            "确认超时非数字",
            section={"bearer_token": TOKEN, "confirmation_timeout_seconds": "soon"},
        ),
        load_case("worker 非整数", section={"bearer_token": TOKEN, "workers": "many"}),
        load_case("worker 为布尔", section={"bearer_token": TOKEN, "workers": True}),
    ]


def main() -> None:
    fixture = {
        "constructs": build_constructs(),
        "loads": build_loads(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1, sort_keys=False)
        handle.write("\n")
    print(
        "wrote %s: constructs=%d loads=%d"
        % (FIXTURE_PATH, len(fixture["constructs"]), len(fixture["loads"]))
    )


if __name__ == "__main__":
    main()
