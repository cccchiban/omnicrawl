# 任务：运行配置改为严格 YAML-only

状态：已完成
创建：2026-07-18
更新：2026-07-18

## 已确认范围

- 所有运行配置入口（默认、显式路径、`AI_CONFIG_FILE`）只接受 `.yaml` / `.yml`。
- 核对并补齐 `config.yaml` 后删除根目录 `config.json`。
- 删除 `omnicrawl/config/migration.py` 和 `config.example.json`。
- 保留 Session、API、Tool、MCP 协议等非运行配置用途的 JSON/JSONL。
- 保留 `.gitignore` 对历史 `config.json` 与 `*.migrated.bak` 的保护，避免旧凭据误提交。

## 计划

- [x] 盘点 JSON 配置兼容与迁移链路
- [x] 用户确认严格 YAML-only、删除本地 JSON、删除迁移模块
- [x] 安全合并旧 JSON 中 YAML 缺失的非 LLM 配置并验证 LLM 迁移语义
- [x] 重写运行配置仓库并删除迁移资产
- [x] 更新调用方文案、保护规则、测试和文档
- [x] 引用扫描、编译检查和全量测试

## 验证结果

- `config.yaml`、`models.yaml`、示例 YAML 均可安全解析。
- `config.json`、`config.example.json`、`omnicrawl/config/migration.py` 均已删除。
- 显式 JSON 配置路径和 `AI_CONFIG_FILE` JSON 路径均直接拒绝。
- 默认位置残留 JSON 时只给出手工创建 YAML 的提示，不读取、不迁移。
- `python -m compileall -q omnicrawl tests`：通过。
- `git diff --check`：通过。
- `python -m unittest discover -s tests`：634 项测试通过。

## 保留边界

- Session、API、Tool、MCP、插件协议的 JSON/JSONL 序列化保持不变。
- `.gitignore` 继续保护历史 `config.json` 与 `*.migrated.bak`，防止旧凭据误提交。
