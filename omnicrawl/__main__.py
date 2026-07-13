"""支持 ``python -m omnicrawl`` 与 console script 等价入口。"""

from __future__ import annotations

import sys

from omnicrawl.entry import run_application


def main() -> None:
    raise SystemExit(run_application())


if __name__ == "__main__":
    main()
