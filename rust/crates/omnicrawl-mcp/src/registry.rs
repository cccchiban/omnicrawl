//! MCP 能力注册表：本轮会话里可用的 Tool、Resource 与 Prompt（对应 `omnicrawl/mcp/registry.py`）。
//!
//! 表按登记顺序保存：工具声明下发给模型的顺序跟着注册顺序走，Python 的 dict 也是插入序。

use serde_json::{Map, Value};

/// MCP 运行期诊断，用于状态展示与降级提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpDiagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub server_name: Option<String>,
}

/// 已发现的 MCP Tool 元数据。
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolMeta {
    pub logical_name: String,
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub input_schema: Map<String, Value>,
    pub requires_confirmation: bool,
    pub risk_level: String,
}

impl McpToolMeta {
    /// 参数契约文本：空 Schema 给 `{}`，否则给 Python 紧凑风格的 JSON。
    pub fn argument_schema(&self) -> String {
        if self.input_schema.is_empty() {
            return "{}".to_string();
        }
        serde_json::to_string(&Value::Object(self.input_schema.clone()))
            .unwrap_or_else(|_| "{}".to_string())
    }
}

/// 已发现的 MCP Resource 元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceMeta {
    pub logical_uri: String,
    pub server_name: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime_type: String,
}

/// 已发现的 MCP Prompt 元数据。
#[derive(Debug, Clone, PartialEq)]
pub struct McpPromptMeta {
    pub logical_name: String,
    pub server_name: String,
    pub prompt_name: String,
    pub description: String,
    pub arguments: Vec<Value>,
}

/// 保存本轮会话中可用的 MCP Tool、Resource 和 Prompt。
#[derive(Debug, Clone, Default)]
pub struct McpCapabilityRegistry {
    tools: Vec<(String, McpToolMeta)>,
    resources: Vec<(String, McpResourceMeta)>,
    prompts: Vec<(String, McpPromptMeta)>,
    diagnostics: Vec<McpDiagnostic>,
}

impl McpCapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tools(&self) -> impl Iterator<Item = &McpToolMeta> {
        self.tools.iter().map(|(_, meta)| meta)
    }

    pub fn resources(&self) -> impl Iterator<Item = &McpResourceMeta> {
        self.resources.iter().map(|(_, meta)| meta)
    }

    pub fn prompts(&self) -> impl Iterator<Item = &McpPromptMeta> {
        self.prompts.iter().map(|(_, meta)| meta)
    }

    pub fn tool(&self, logical_name: &str) -> Option<&McpToolMeta> {
        self.tools
            .iter()
            .find(|(key, _)| key == logical_name)
            .map(|(_, meta)| meta)
    }

    pub fn resource(&self, logical_uri: &str) -> Option<&McpResourceMeta> {
        self.resources
            .iter()
            .find(|(key, _)| key == logical_uri)
            .map(|(_, meta)| meta)
    }

    pub fn prompt(&self, logical_name: &str) -> Option<&McpPromptMeta> {
        self.prompts
            .iter()
            .find(|(key, _)| key == logical_name)
            .map(|(_, meta)| meta)
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    pub fn prompt_count(&self) -> usize {
        self.prompts.len()
    }

    pub fn diagnostics(&self) -> &[McpDiagnostic] {
        &self.diagnostics
    }

    pub fn add_diagnostic(
        &mut self,
        severity: &str,
        code: &str,
        message: impl Into<String>,
        server_name: Option<&str>,
    ) {
        self.diagnostics.push(McpDiagnostic {
            severity: severity.to_string(),
            code: code.to_string(),
            message: message.into(),
            server_name: server_name.map(|name| name.to_string()),
        });
    }

    pub fn add_tool(&mut self, meta: McpToolMeta) {
        if self.tool(&meta.logical_name).is_some() {
            self.add_diagnostic(
                "warning",
                "CAPABILITY_DUPLICATED",
                format!("重复的 MCP Tool 名称已跳过：{}", meta.logical_name),
                Some(&meta.server_name),
            );
            return;
        }
        self.tools.push((meta.logical_name.clone(), meta));
    }

    pub fn add_resource(&mut self, meta: McpResourceMeta) {
        if self.resource(&meta.logical_uri).is_some() {
            self.add_diagnostic(
                "warning",
                "CAPABILITY_DUPLICATED",
                format!("重复的 MCP Resource URI 已跳过：{}", meta.logical_uri),
                Some(&meta.server_name),
            );
            return;
        }
        self.resources.push((meta.logical_uri.clone(), meta));
    }

    pub fn add_prompt(&mut self, meta: McpPromptMeta) {
        if self.prompt(&meta.logical_name).is_some() {
            self.add_diagnostic(
                "warning",
                "CAPABILITY_DUPLICATED",
                format!("重复的 MCP Prompt 名称已跳过：{}", meta.logical_name),
                Some(&meta.server_name),
            );
            return;
        }
        self.prompts.push((meta.logical_name.clone(), meta));
    }
}

/// 生成 Host 侧唯一名称，避免不同 Server 暴露同名能力。
pub fn namespace_capability_name(server_name: &str, capability_name: &str) -> String {
    format!("{}.{}", server_name, capability_name.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(logical_name: &str, server: &str) -> McpToolMeta {
        McpToolMeta {
            logical_name: logical_name.to_string(),
            server_name: server.to_string(),
            tool_name: logical_name.rsplit('.').next().unwrap_or("").to_string(),
            description: "描述".to_string(),
            input_schema: Map::new(),
            requires_confirmation: true,
            risk_level: "restricted".to_string(),
        }
    }

    #[test]
    fn duplicate_tool_keeps_first_and_reports_warning() {
        let mut registry = McpCapabilityRegistry::new();
        registry.add_tool(tool("a.echo", "a"));
        registry.add_tool(tool("a.echo", "b"));
        assert_eq!(registry.tool_count(), 1);
        assert_eq!(registry.tool("a.echo").expect("首条保留").server_name, "a");
        let diagnostic = &registry.diagnostics()[0];
        assert_eq!(diagnostic.code, "CAPABILITY_DUPLICATED");
        assert_eq!(diagnostic.message, "重复的 MCP Tool 名称已跳过：a.echo");
    }

    #[test]
    fn empty_schema_renders_as_empty_object() {
        assert_eq!(tool("a.echo", "a").argument_schema(), "{}");
    }

    #[test]
    fn namespaced_name_trims_capability() {
        assert_eq!(namespace_capability_name("files", "  read "), "files.read");
    }
}
