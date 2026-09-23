"""项目根启动入口（已弃用，仅作兼容垫片）。

产品入口是 npm 分发的 Rust 二进制：``npm install -g omnicrawl-cli && omnicrawl``。
保留 ``python main.py`` 只为兼容旧脚本：按同样的参数把命令行转发给 Rust 宿主并沿用其退出码，
找不到二进制时打印迁移提示（详见 :mod:`omnicrawl.compat`）。

开发对照 Textual UI：``python main.py --legacy-python`` 或 ``OMNICRAWL_LEGACY_PYTHON_UI=1``。
"""

from __future__ import annotations

from omnicrawl.compat import forward_to_rust


def main(argv: list[str] | None = None) -> int:
    """转发给 Rust 宿主并返回其退出码。"""

    return forward_to_rust(argv)


if __name__ == "__main__":
    raise SystemExit(main())
