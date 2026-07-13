"""兼容旧版 pip/setuptools 的 editable 安装入口。

现代元数据以 pyproject.toml 为准；本文件仅调用 setuptools.setup()。
"""

from setuptools import setup

setup()
