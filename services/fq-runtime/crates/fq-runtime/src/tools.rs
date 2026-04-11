//! Tool registry for the factor-q runtime.
//!
//! Holds the set of tool implementations available to agents, keyed
//! by tool name. The executor looks tools up here at invocation
//! time, using the agent's declared tool list to decide which ones
//! to expose in a given run.
//!
//! This crate owns the registry (rather than fq-tools) because the
//! registry is a runtime concern — it binds tool instances to their
//! names and is consulted on every tool call. The fq-tools crate
//! owns the primitives (trait, sandbox, built-ins) without knowing
//! how they get assembled.

use std::collections::HashMap;
use std::sync::Arc;

use fq_tools::builtin::{FileReadTool, FileWriteTool};
use fq_tools::Tool;

use crate::events::ToolSchema;

/// A collection of tool implementations keyed by name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry with every built-in tool pre-registered.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(FileReadTool::new()));
        registry.register(Arc::new(FileWriteTool::new()));
        registry
    }

    /// Register a tool. Replaces any existing tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Number of tools in the registry.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// True when the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Build the [`ToolSchema`] list to send to the LLM for a given
    /// subset of tool names. Names that aren't in the registry are
    /// silently dropped — the caller is responsible for warning or
    /// failing if that matters.
    pub fn build_schemas(&self, names: &[String]) -> Vec<ToolSchema> {
        names
            .iter()
            .filter_map(|name| {
                self.tools.get(name).map(|t| ToolSchema {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters_schema: t.parameters_schema(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_builtins_has_file_tools() {
        let reg = ToolRegistry::with_builtins();
        assert!(reg.get("file_read").is_some());
        assert!(reg.get("file_write").is_some());
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn unknown_tool_returns_none() {
        let reg = ToolRegistry::with_builtins();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn build_schemas_filters_to_known_tools() {
        let reg = ToolRegistry::with_builtins();
        let schemas = reg.build_schemas(&[
            "file_read".to_string(),
            "unknown".to_string(),
            "file_write".to_string(),
        ]);
        assert_eq!(schemas.len(), 2);
        assert!(schemas.iter().any(|s| s.name == "file_read"));
        assert!(schemas.iter().any(|s| s.name == "file_write"));
    }

    #[test]
    fn build_schemas_preserves_metadata() {
        let reg = ToolRegistry::with_builtins();
        let schemas = reg.build_schemas(&["file_read".to_string()]);
        let schema = &schemas[0];
        assert_eq!(schema.name, "file_read");
        assert!(!schema.description.is_empty());
        assert!(schema.parameters_schema.is_object());
    }
}
