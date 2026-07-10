"""`python -m omnicrawl.api` 启动入口。"""

from __future__ import annotations

import uvicorn

from . import create_app, load_api_config


def main() -> None:
    config = load_api_config()
    uvicorn.run(
        create_app(config=config),
        host=config.host,
        port=config.port,
        log_level="info",
    )


if __name__ == "__main__":
    main()
