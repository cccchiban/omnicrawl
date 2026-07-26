# 任务：ocl 命令与首次启动配置初始化

状态：已完成
创建：2026-07-25
更新：2026-07-25

## 需求摘要

- 将主要 console script 命令改为 `ocl`，保留 `omnicrawl` 兼容入口。
- 首次启动时自动创建用户配置目录。
- 自动生成 `config.yaml` 和 `models.yaml`。
- 首次启动交互式收集 API Key，并写入用户配置目录。
- 缺少 API Key 时给出可操作提示并退出，不进入 TUI。
- 检查 Node.js、模型配置和插件状态，诊断失败不阻塞首次初始化。

## 关键决策

- Windows 使用 `%APPDATA%\\OmniCrawl`；Linux/macOS 使用 `~/.config/omnicrawl`。
- 配置文件默认从用户配置目录读取；`AI_CONFIG_FILE` 和 `AI_MODELS_FILE` 仍可覆盖。
- API Key 只写入用户配置目录，不写入项目目录、日志、测试夹具或发布包。
- `ocl` 是主命令，`omnicrawl` 作为兼容别名继续生成。
- 首次初始化后若 API Key 仍缺失，返回非零退出码；Node.js 或插件诊断问题只显示警告。

## 实现计划

- [x] 1. 补充命令入口、配置路径和首次初始化的失败测试
- [x] 2. 实现用户目录、模板生成和 API Key 交互写入
- [x] 3. 实现 Node.js、模型和插件诊断并接入应用入口
- [x] 4. 更新打包元数据和使用文档
- [x] 5. 运行定向测试、完整回归和发布包冒烟检查

## 已修改文件

- `.codex/tasks/ocl-first-run-config.md`
- `omnicrawl/config/runtime.py`
- `omnicrawl/config/bootstrap.py`
- `omnicrawl/config/templates/config.example.yaml`
- `omnicrawl/config/templates/models.example.yaml`
- `omnicrawl/entry.py`
- `pyproject.toml`
- `setup.cfg`
- `main.py`
- `README.md`
- `tests/test_startup_setup.py`
- `tests/test_packaging_entrypoints.py`
- `tests/test_runtime_config.py`

## 验证

- `python -m unittest discover -s tests`：764 tests OK
- `python -m compileall -q omnicrawl tests/test_startup_setup.py tests/test_packaging_entrypoints.py`：通过
- `git diff --check`：通过
- `python setup.py bdist_wheel`：成功；Wheel 包含 `ocl`、`omnicrawl` 和两个首次启动模板
- 隔离虚拟环境：两个命令的 `--help` 和首次配置初始化冒烟通过

## 残余风险

- 当前环境未安装 `build` 模块，因此 Wheel 验证使用已有的 `setuptools/wheel` 降级入口。
- 旧的用户插件注册表仍位于 `~/.omnicrawl/plugins`，本次只迁移了运行配置目录。
