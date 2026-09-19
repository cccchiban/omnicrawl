//! `omnicrawl/agent/subagents/execution.py` 的移植：SubAgent Phase 3 的私有执行快照类型。
//!
//! Coordinator 只保存不可变的任务请求快照；真正的模型 Runtime 与父 Agent 可变状态仍由
//! 宿主持有。任务创建时冻结的 `llm_config` / `profile` / `descriptor` 只在进程内用于构造
//! 该任务的独立 Runtime——它们绝不进入 Session、SSE、artifact、日志或公开 ToolResult，
//! 因此留在宿主侧；内核只保留判定与诊断真正读到的 `selection` 与 wire model。

use serde_json::Value;

use crate::subagents::worktrees::WorktreeSession;

/// 派生工作进程的固定约束文本。
pub const FORK_BOILERPLATE: &str = r#"<fork_boilerplate>
你是从 OmniCrawl 主 Agent 派生的工作进程，不是面向用户的主助手。
不可协商规则：
1. 不得再次创建 SubAgent。
2. 不得向用户提问；遇到需要产品或权限决策的问题应停止并报告。
3. 严格限制在分配任务范围内。
4. 只使用 Host 提供的工具，不得绕过审批、路径和安全策略。
5. 最终只返回结构化工作报告，不输出隐藏推理。
</fork_boilerplate>"#;

/// 任务创建时冻结的模型选择（只保留判定面读到的两项）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubAgentModelSnapshot {
    pub selection: String,
    /// 对应 Python `descriptor.model_id`，诊断里作为 `wire_model`。
    pub wire_model: String,
}

/// 单任务独立执行所需的不可变输入。
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentExecutionContext {
    pub context: String,
    pub model_snapshot: Option<SubAgentModelSnapshot>,
    /// 从父回合开始时构造的公开消息深拷贝，已完成脱敏。
    pub fork_messages: Vec<Value>,
    pub parent_system_prompt: String,
    pub skill_context: String,
    pub worktree_session: Option<WorktreeSession>,
    pub workspace_root: String,
    pub isolation: String,
    pub task_id: String,
    pub batch_id: String,
}

impl Default for SubAgentExecutionContext {
    fn default() -> Self {
        Self {
            context: "fresh".to_string(),
            model_snapshot: None,
            fork_messages: Vec::new(),
            parent_system_prompt: String::new(),
            skill_context: String::new(),
            worktree_session: None,
            workspace_root: String::new(),
            isolation: "shared".to_string(),
            task_id: String::new(),
            batch_id: String::new(),
        }
    }
}
