# 任务：随 OmniCrawl 安装包发布内置文档

状态：已完成
创建：2026-07-26
更新：2026-07-26

## 需求摘要

- 将根目录 `docs/` 中现有 6 份 Markdown 全部移动到 `omnicrawl/docs/`，不保留根目录副本。
- wheel 与 sdist 必须包含这些文档。
- PyPI 安装后的 Agent 必须能按需读取内置文档，不能依赖当前工作目录恰好是源码仓库。

## 关键决策

- 使用 `omnicrawl://docs/<文件名>` 作为稳定的内置文档 URI。
- 内置 `read_file` 仅对上述只读 URI 放行，不扩大工作区写入边界。
- Local MCP Server 同时暴露工作区文档和安装包内置文档资源。
- 完成实现与制品验证后再确认 GitHub 推送和 PyPI 0.1.3 发布。

## 实现计划

- [x] 1. 添加文档 URI、MCP Resource 和打包失败测试。
- [x] 2. 移动 6 份文档并实现只读 URI 解析。
- [x] 3. 更新提示词、README、打包清单和版本元数据。
- [x] 4. 运行定向与完整回归。
- [x] 5. 构建并隔离验证 wheel/sdist。

## 已修改文件

- `.codex/tasks/package-bundled-docs.md`
- `omnicrawl/documentation.py`
- `omnicrawl/workspace/tools.py`
- `omnicrawl/mcp/server.py`
- `omnicrawl/docs/*.md`
- `pyproject.toml`
- `setup.cfg`
- `MANIFEST.in`
- `README.md`
- `omnicrawl/agent/system_prompt.md`
- `tests/test_workspace_tools.py`
- `tests/test_mcp.py`
- `tests/test_packaging_entrypoints.py`

## 验证

- 定向测试：44 项通过。
- 完整测试：798 项通过。
- `compileall` 通过。
- 0.1.3 wheel/sdist 均通过 `twine check`。
- 干净隔离环境安装后，6 份内置文档均可通过 `omnicrawl://docs/...` 读取，`ocl --help` 正常。
- GitHub `main` 已推送至 `55c151c363546fdea2509ec73b987ddeb242e281`。
- PyPI `omnicrawl-agent 0.1.3` 已发布，线上 wheel/sdist 哈希与本地产物一致。
- 用户级 `TWINE_USERNAME`/`TWINE_PASSWORD` 已配置为项目级 PyPI 发布凭据。
