"""``python -m omnicrawl`` 与控制台脚本（``ocl`` / ``omnicrawl``）入口。

已弃用，仅作兼容垫片：转发给 Rust 宿主并沿用其退出码（详见 :mod:`omnicrawl.compat`）。
"""

from __future__ import annotations

from .compat import forward_to_rust


def main() -> None:
    raise SystemExit(forward_to_rust())


if __name__ == "__main__":
    main()
