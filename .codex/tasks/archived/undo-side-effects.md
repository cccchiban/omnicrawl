# 任务：/undo 事务式副作用回退

状态：已完成
创建：2026-07-27
更新：2026-07-27

## 需求摘要

- 将 `/undo` 从仅回退会话转录升级为原子回退最近一轮会话及其受控副作用。
- 使用独立 Git 影子对象库快照工作区文件、Prompt History、当前 Session 工具产物及 project/session/user 三类记忆目录，不修改用户仓库 HEAD、index 或提交历史。
- 回退前校验当前状态必须等于该轮结束快照；任一冲突则文件、记忆和对话均不回退。
- 旧轮次若存在成功副作用但没有快照，拒绝回退；纯对话旧轮次保留兼容软回退。

## 关键决策

- 快照存放在当前 Session artifact 目录并从工作区快照中排除 `.git`、当前 Session 存储和另行快照的记忆根，避免修改用户 Git 或产生自引用；`.agent_tmp` 中的本轮文件变化也纳入回退。
- 工作区、Session 运行产物、项目记忆、会话记忆、用户记忆分别形成 Git tree，并共享 Session 级影子 object database。
- `/undo` 顺序为：定位最后轮次 -> 校验快照/不可逆操作/冲突 -> 预检恢复 -> 恢复全部根目录 -> 追加 `turn_undone` -> 重建运行时上下文。
- Session 事件提交失败时，将全部受控根从轮次起点反向恢复到轮次终点，保持会话与文件状态一致。
- Shell、MCP、桌面、SubAgent、外部服务等无法证明仅修改受控根目录的成功操作标记为不可逆；存在此标记时按原子策略拒绝回退。
- Git 只作为内部内容寻址与树快照机制，不执行用户仓库的 reset、checkout、stash、commit 或 index 写入。

## 实现结果

- [x] 1. 确认需求和现有 undo / Session / 工具 / 记忆架构
- [x] 2. 添加 Git 影子快照模型、事件协议和失败测试
- [x] 3. 接入单轮 begin/end/取消快照与副作用分类
- [x] 4. 实现原子校验、恢复和 `/undo` 编排
- [x] 5. 更新命令文案、README、Session 设计文档和兼容测试
- [x] 6. 执行定向回归、全量回归审计并归档任务

## 关键修改文件

- `omnicrawl/state/turn_snapshot.py`
- `omnicrawl/state/session_models.py`
- `omnicrawl/state/session.py`
- `omnicrawl/agent/core.py`
- `omnicrawl/agent/session_facade.py`
- `omnicrawl/commands/slash.py`
- `tests/test_turn_snapshot.py`
- `tests/test_agent_context.py`
- `tests/test_context_overflow_recovery.py`
- `README.md`
- `omnicrawl/docs/session_design.md`

## 验证

- `python -m unittest tests.test_turn_snapshot ... tests.test_vision_tool_images`：126 项通过（其中事务快照专项 8 项）。
- `python -m unittest tests.test_context_overflow_recovery -v`：4 项通过。
- `python -m compileall -q omnicrawl main.py`：通过。
- `git diff --check`：通过。
- 全量 `python -m unittest`：运行 851 项；发现 6 项失败，其中本功能引入的 `turn_snapshot` 事件顺序断言已修复并单模块验证通过；其余 5 项为既有 Fullscreen TUI（3 项）和搜索索引配置期望（2 项）失败，与本功能无直接关联。
- Ruff：当前环境未安装，未执行；已由编译、专项测试和差异检查降级覆盖。
- Reviewer 子代理：两次只读审查均超时，无有效结论；主代理补充了冲突零修改、CRLF、Session commit 失败反向恢复、Prompt History、Session artifact 和用户 Git HEAD/index 不变专项测试。
