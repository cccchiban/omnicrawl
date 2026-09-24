//! 稳定 prompt 前缀的身份指纹，对齐 Python `omnicrawl/agent/context/prompt_context.py`。
//!
//! 用途：给声明了 prompt caching 的 Provider 一个稳定的 `prompt_cache_key` 派生源，
//! 让同一段稳定前缀在不同回合/会话间落到同一个缓存项上。
//!
//! 身份只由**稳定前缀**的成分构成——system prompt、项目规范、Skill 索引、活动 Skill、
//! 工具声明与工作区根；**不含**用户输入、对话历史与工具结果（它们每轮都在变，
//! 混进来会让 key 失去复用价值）。
//!
//! 哈希规则必须与 Python 逐字节一致，否则两侧对同一前缀会派生出不同的 key：
//!
//! * `_hash_text` = `sha256(文本 UTF-8)`
//! * `_hash_json` = `sha256(json.dumps(值, ensure_ascii=False, separators=(",", ":"), sort_keys=True))`
//!
//! 后者直接复用 [`python_dumps_compact_sorted`]——它的注释就声明了这条等价关系，
//! 所以这里不再自行拼 JSON。

use std::collections::BTreeMap;
use std::path::Path;

use omnicrawl_controllers::json::python_dumps_compact_sorted;
use omnicrawl_controllers::turn::prompt_context::AGENT_PROMPT_VERSION;
use omnicrawl_extensions::skill::{SkillMatchResult, SkillMeta};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 稳定前缀的身份信息；字段名与顺序对齐 Python 的 `PromptCacheIdentity`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheIdentity {
    pub agent_prompt_version: String,
    pub system_prompt_hash: String,
    pub workspace_root: String,
    pub project_instructions_hash: String,
    pub skill_index_hash: String,
    pub active_skill_context_hash: String,
    pub tool_schema_hash: String,
}

impl PromptCacheIdentity {
    /// 转成 `initialize.model.prompt_cache_identity` 的映射。
    ///
    /// 这里**不放** `model`：内核的 `build_prompt_cache_key` 会自己把模型名补进去
    /// （对应 Python `build_prompt_cache_key` 里那句 `stable_identity["model"] = ...`），
    /// 放两遍反而会掩盖「内核才是补模型名的一方」这条边界。
    pub fn to_identity_map(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        map.insert(
            "agent_prompt_version".to_string(),
            self.agent_prompt_version.clone(),
        );
        map.insert(
            "system_prompt_hash".to_string(),
            self.system_prompt_hash.clone(),
        );
        map.insert("workspace_root".to_string(), self.workspace_root.clone());
        map.insert(
            "project_instructions_hash".to_string(),
            self.project_instructions_hash.clone(),
        );
        map.insert(
            "skill_index_hash".to_string(),
            self.skill_index_hash.clone(),
        );
        map.insert(
            "active_skill_context_hash".to_string(),
            self.active_skill_context_hash.clone(),
        );
        map.insert(
            "tool_schema_hash".to_string(),
            self.tool_schema_hash.clone(),
        );
        map
    }
}

/// 组装稳定前缀的身份指纹；参数与 Python `build_prompt_cache_identity` 一一对应。
///
/// `skills` 是当前可见的全部 Skill（Python 的 `skill_manager.list_all()`），
/// `active_skills` 是本轮命中的 Skill（无命中时传空切片——此时
/// `active_skill_context_hash` 就是空数组的哈希 `sha256("[]")`，与 Python 一致）。
/// `chat_tools` 是发给模型的工具声明（Python 的 `chat_tools`）。
pub fn build_prompt_cache_identity(
    system_prompt: &str,
    workspace_root: &Path,
    project_instructions: &str,
    skills: &[SkillMeta],
    active_skills: &[SkillMatchResult],
    chat_tools: &[Value],
) -> PromptCacheIdentity {
    let skill_index: Vec<Value> = skills.iter().map(skill_meta_payload).collect();
    let active: Vec<Value> = active_skills
        .iter()
        .map(|matched| {
            json!({
                "meta": skill_meta_payload(&matched.skill.meta),
                "body_hash": hash_text(&matched.skill.body),
            })
        })
        .collect();
    PromptCacheIdentity {
        agent_prompt_version: AGENT_PROMPT_VERSION.to_string(),
        system_prompt_hash: hash_text(system_prompt),
        workspace_root: workspace_root.to_string_lossy().to_string(),
        // Python 用的是 `.strip()`：首尾空白或换行变化不应改变身份。
        project_instructions_hash: hash_text(project_instructions.trim()),
        skill_index_hash: hash_json(&Value::Array(skill_index)),
        active_skill_context_hash: hash_json(&Value::Array(active)),
        tool_schema_hash: hash_json(&Value::Array(chat_tools.to_vec())),
    }
}

/// Skill 参与身份计算的字段集。
///
/// 刻意**不**复用 [`SkillMeta::to_dict`]：那份还带了 `base_dir`，而 Python 的
/// `_skill_meta_payload` 只有这五项。字段集多了会让两侧哈希必然不同。
fn skill_meta_payload(meta: &SkillMeta) -> Value {
    json!({
        "name": meta.name,
        "description": meta.description,
        "scope": meta.scope,
        "source_path": meta.source_path.to_string_lossy(),
        "disable_model_invocation": meta.disable_model_invocation,
    })
}

/// `sha256(文本 UTF-8)` 的小写十六进制，对齐 Python `_hash_text`。
fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `sha256(Python 风格紧凑有序 JSON)`，对齐 Python `_hash_json`。
fn hash_json(value: &Value) -> String {
    hash_text(&python_dumps_compact_sorted(value))
}
