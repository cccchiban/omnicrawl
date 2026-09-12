"""`python -m omnicrawl.api` 启动入口。"""

from __future__ import annotations

import uvicorn

from . import create_app, load_api_config


def main() -> None:
    config = load_api_config()
    if config.workers > 1:
        # 多 worker 必须传 import string + factory：uvicorn 需要在每个子进程里
        # 重新导入模块并调用工厂构造 App，直接传 App 实例时 workers 参数会被
        # 忽略（静默退化成单进程）。每个 worker 会各自建 Agent 与隔离 worktree。
        uvicorn.run(
            "omnicrawl.api.app:create_app_from_env",
            factory=True,
            host=config.host,
            port=config.port,
            log_level="info",
            workers=config.workers,
        )
        return
    uvicorn.run(
        create_app(config=config),
        host=config.host,
        port=config.port,
        log_level="info",
    )


if __name__ == "__main__":
    main()
