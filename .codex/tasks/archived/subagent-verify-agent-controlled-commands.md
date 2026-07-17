# 任务：SubAgent verify Agent 与受控命令执行

状态：已完成
创建：2026-07-15
更新：2026-07-15

## 需求摘要

- 为 OmniCrawl 新增默认关闭的内置 `verify` SubAgent。
- 允许该角色执行有限、固定的项目验证检查；不得向模型开放任意 Bash、PowerShell、文件写入、依赖安装、网络访问或 Git 变更能力。
- 以当前未提交的 SubAgent 工作区为基线追加本阶段改动，不重置、拆分或覆盖既有改动。

## 关键决策

- 新增仅对子 Agent 可见的 `verify_command` 工具；模型只能传入固定的检查标识和受限超时，不能传入可解释的命令字符串、路径、Shell 或环境变量。
- 首期固定检查为：全量 Python unittest、Python compileall、`git diff --check`；Python 与 Git 均通过参数数组直接启动，不经 Bash/PowerShell 解析。
- `verify` profile 保留现有只读文件/Memory 工具，但禁止 `bash`、`powershell`、`monitor`、写文件、MCP、浏览器、递归 SubAgent 与 Memory 写入。
- `subagents.enable_verify_agent` 默认 `false`；显式启用后才接受 `explicit-command-allowlist` 定义。环境变量只能关闭该能力或收紧验证命令超时。
- 固定检查均不包含删除或变更性 Git 操作，因此不会绕过 ApprovalBroker；后续若新增高风险检查，仍必须走已有 Broker 策略。

## 实现计划

- [x] 1. 阅读设计稿、审批/命令/协调器实现与现有测试，完成改动前定向回归。
- [x] 2. 新增受控命令执行底座、`verify_command` 工具和内置 `verify` 定义。
- [x] 3. 扩展 SubAgent 配置与 Coordinator profile 过滤，保持默认关闭与最小权限。
- [x] 4. 补充配置、工具、协调器、执行隔离与工作区命令的回归测试。
- [x] 5. 同步 README、设计稿和配置示例，执行定向、全量、编译与差异检查。

## 已修改文件

- `omnicrawl/agent/subagents/verify.py`：固定检查标识、参数校验和 child-only `verify_command`。
- `omnicrawl/agent/subagents/builtin/verify.md`：默认关闭的内置验证角色定义。
- `omnicrawl/workspace/tools.py`：不经 Shell 解析的受控 argv 执行与复用的超时回收。
- `omnicrawl/config/subagents.py`：verify 启用开关、命令超时与只收紧的环境变量。
- `omnicrawl/agent/subagents/coordinator.py`、`omnicrawl/agent/core.py`、`omnicrawl/agent/tools.py`：profile 收窄、child-only 工具接线、动态能力提示与命令串行屏障。
- `tests/test_subagent_verify.py`、`tests/test_subagent_config.py`、`tests/test_subagent_integration.py`：安全边界与回归覆盖。
- `README.md`、`docs/README.md`、`docs/SUBAGENT_DESIGN.md`、`config.example.yaml`、`config.example.json`：使用说明、实施状态和配置示例。
- `.codex/tasks/subagent-verify-agent-controlled-commands.md`（归档后移入 `archived/`）。

## 验证

- 改动前：`python -m unittest tests.test_subagent_config tests.test_subagent_definitions tests.test_subagent_coordinator tests.test_subagent_tasks tests.test_subagent_approval tests.test_subagent_integration -q`：77 项通过。
- Red：新增 `tests.test_subagent_verify` 时因 `omnicrawl.agent.subagents.verify` 不存在而按预期失败。
- 定向：`python -m unittest tests.test_subagent_verify tests.test_subagent_config tests.test_subagent_definitions tests.test_subagent_coordinator tests.test_subagent_tasks tests.test_subagent_approval tests.test_subagent_integration tests.test_workspace_tools -q`：99 项通过。
- 全量：`python -m unittest discover -s tests -q`：540 项通过（37.847 秒）。测试日志中的损坏会话、模拟 API 异常、取消与 observer 异常均为既有负向夹具输出，最终退出码为 0。
- 编译：`python -m compileall -q omnicrawl main.py tests`：通过。
- 差异检查：`git diff --check`：通过。
- 独立安全审查：固定 argv、`shell=False` 与 Coordinator profile 交集收窄范围内未发现高/中严重度问题。

## 残余风险与后续

- `unit_tests` 会执行项目自身测试代码；这是验证目的的一部分，但不是操作系统沙箱。模型无法更换可执行文件、参数、Shell 或工作目录。
- 本阶段不实现任意命令、依赖安装、网络访问、文件写入、Fork、模型覆盖、worktree 或跨父 Run 远程审批。
