//! Tool registry for the factor-q runtime.

use std::collections::HashMap;
use std::sync::Arc;

use fq_tools::Tool;
use fq_tools::builtin::{
    ExecConfig, ExecTool, FileListTool, FileReadTool, FileSearchTool, FileWriteTool,
    SelfInspectTool,
};

use crate::events::ToolSchema;

/// Number of tools registered by [`ToolRegistry::with_builtins`].
pub const BUILTIN_TOOL_COUNT: usize = 6;
/// Prefix reserved for runtime-owned tools.
pub const BUILTIN_PREFIX: &str = "builtin__";

/// A collection of tool implementations keyed by their canonical name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtins() -> Self {
        Self::with_builtins_exec(ExecConfig::default())
    }

    pub fn with_builtins_exec(exec: ExecConfig) -> Self {
        let mut registry = Self::new();
        registry
            .register_builtin("file_read", Arc::new(FileReadTool::new()))
            .expect("unique builtin");
        registry
            .register_builtin("file_list", Arc::new(FileListTool::new()))
            .expect("unique builtin");
        registry
            .register_builtin("file_search", Arc::new(FileSearchTool::new()))
            .expect("unique builtin");
        registry
            .register_builtin("file_write", Arc::new(FileWriteTool::new()))
            .expect("unique builtin");
        registry
            .register_builtin("exec", Arc::new(ExecTool::with_config(exec)))
            .expect("unique builtin");
        registry
            .register_builtin("self_inspect", Arc::new(SelfInspectTool::new()))
            .expect("unique builtin");
        registry
    }

    /// Register a runtime built-in under the reserved canonical prefix.
    pub fn register_builtin(&mut self, name: &str, tool: Arc<dyn Tool>) -> Result<(), String> {
        self.insert(format!("{BUILTIN_PREFIX}{name}"), tool)
    }

    /// Register a namespaced external tool. Bare and `builtin__` names are
    /// rejected so an MCP server cannot shadow a sandboxed capability.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), String> {
        let name = tool.name();
        if name.starts_with(BUILTIN_PREFIX) {
            return Err(format!(
                "MCP tool name '{name}' uses reserved {BUILTIN_PREFIX} prefix"
            ));
        }
        if !name.contains("__") {
            return Err(format!("tool name '{name}' is not namespaced"));
        }
        self.insert(name.to_string(), tool)
    }

    /// Test-only escape hatch for fixture tools whose deliberately simple
    /// names predate canonical namespace enforcement.
    #[cfg(test)]
    pub(crate) fn register_fixture(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    fn insert(&mut self, name: String, tool: Arc<dyn Tool>) -> Result<(), String> {
        if self.tools.contains_key(&name) {
            return Err(format!("duplicate tool registration: '{name}'"));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
    pub fn len(&self) -> usize {
        self.tools.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn build_schemas(&self, names: &[String]) -> Vec<ToolSchema> {
        names
            .iter()
            .filter_map(|name| {
                self.tools.get(name).map(|t| ToolSchema {
                    name: name.clone(),
                    description: t.description().to_string(),
                    parameters_schema: t.parameters_schema(),
                })
            })
            .collect()
    }
    pub fn missing_tools(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .filter(|name| !self.tools.contains_key(name.as_str()))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtins_are_canonically_namespaced() {
        let reg = ToolRegistry::with_builtins();
        assert!(reg.get("builtin__file_read").is_some());
        assert!(reg.get("file_read").is_none());
        assert_eq!(reg.len(), BUILTIN_TOOL_COUNT);
    }
    #[test]
    fn bare_and_reserved_external_names_are_rejected() {
        let mut reg = ToolRegistry::new();
        let bare = Arc::new(FileReadTool::new());
        assert!(reg.register(bare).unwrap_err().contains("not namespaced"));
        // Built-ins cannot be inserted twice either.
        reg.register_builtin("file_read", Arc::new(FileReadTool::new()))
            .unwrap();
        assert!(
            reg.register_builtin("file_read", Arc::new(FileReadTool::new()))
                .unwrap_err()
                .contains("duplicate")
        );
    }
}
