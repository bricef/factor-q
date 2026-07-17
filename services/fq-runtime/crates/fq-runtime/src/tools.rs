//! Tool registry for the factor-q runtime.
//!
//! Holds the set of tool implementations available to agents, keyed by
//! **canonical name**: built-ins live under the reserved `builtin__`
//! prefix (`builtin__exec`), MCP tools under their server's namespace
//! (`<server>__<tool>`, composed at discovery in `mcp.rs`). The executor
//! looks tools up here at invocation time, using the agent's declared
//! tool list to decide which ones to expose in a given run.
//!
//! Naming is enforced at registration (#177): external registrations
//! must be namespaced and can never use the reserved prefix, and a
//! duplicate name is rejected rather than replaced — so an MCP server
//! cannot shadow a sandboxed built-in by construction, not by
//! convention.
//!
//! The registry key is the single source of truth for a tool's
//! canonical name. For built-ins it deliberately differs from
//! [`Tool::name`], which keeps returning the bare name (`exec`):
//! fq-tools stays namespace-agnostic and the runtime owns naming. That
//! is also why this crate owns the registry rather than fq-tools — the
//! registry is a runtime concern that binds tool instances to canonical
//! names and is consulted on every tool call, while fq-tools owns the
//! primitives (trait, sandbox, built-ins) without knowing how they get
//! assembled.

use std::collections::HashMap;
use std::sync::Arc;

use fq_tools::Tool;
use fq_tools::builtin::{
    ExecConfig, ExecTool, FileListTool, FileReadTool, FileSearchTool, FileWriteTool,
    ReportOutcomeTool, SelfInspectTool,
};

use crate::events::ToolSchema;

/// Prefix reserved for runtime-owned tools.
pub const BUILTIN_PREFIX: &str = "builtin__";

/// Bare (pre-namespace) names of every built-in, in registration order —
/// one entry per `register_builtin` call in
/// [`ToolRegistry::with_builtins_exec`]; `builtins_cover_the_basename_list`
/// pins the correspondence. The runner uses this list to map legacy bare
/// grants and calls to canonical names for one release (#177).
pub const BUILTIN_TOOL_BASENAMES: [&str; 7] = [
    "file_read",
    "file_list",
    "file_search",
    "file_write",
    "exec",
    "self_inspect",
    "report_outcome",
];

/// Number of tools registered by [`ToolRegistry::with_builtins`].
/// Useful for callers that report "MCP tools = total - builtins".
pub const BUILTIN_TOOL_COUNT: usize = BUILTIN_TOOL_BASENAMES.len();

/// Canonical name of the host-fulfilled introspection tool. The runner
/// intercepts calls to this name instead of dispatching through
/// [`Tool::execute`] (see `worker/reducer/runner.rs`); a unit test pins
/// it to `BUILTIN_PREFIX` + fq-tools' `SELF_INSPECT_TOOL_NAME`.
pub const SELF_INSPECT_CANONICAL_NAME: &str = "builtin__self_inspect";

/// Canonical name of the terminal outcome-declaration tool (#125). The
/// reducer harness intercepts calls to this name (or its bare form) as
/// the terminal transition instead of dispatching them — see
/// `worker/reducer/harness.rs`.
pub const REPORT_OUTCOME_CANONICAL_NAME: &str = "builtin__report_outcome";

/// A collection of tool implementations keyed by their canonical name.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry with every built-in tool pre-registered, using
    /// the default `exec` configuration ([`ExecConfig::default`]). For a
    /// daemon that must honour `[tools.exec]` timeouts, use
    /// [`with_builtins_exec`](Self::with_builtins_exec) instead.
    pub fn with_builtins() -> Self {
        Self::with_builtins_exec(ExecConfig::default())
    }

    /// Like [`with_builtins`](Self::with_builtins), but registers the
    /// exec tool with an explicit [`ExecConfig`] — the seam that lets
    /// the runtime thread `[tools.exec]` timeouts from `fq.toml` into the
    /// registered tool. Every other built-in is identical.
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
            .register_builtin("report_outcome", Arc::new(ReportOutcomeTool::new()))
            .expect("unique builtin");
        debug_assert_eq!(registry.len(), BUILTIN_TOOL_COUNT);
        registry
    }

    /// Register a runtime built-in under the reserved canonical prefix.
    /// `pub(crate)` on purpose: only the runtime may mint `builtin__`
    /// names — external crates go through [`register`](Self::register),
    /// which rejects the prefix.
    pub(crate) fn register_builtin(
        &mut self,
        name: &str,
        tool: Arc<dyn Tool>,
    ) -> Result<(), String> {
        self.insert(format!("{BUILTIN_PREFIX}{name}"), tool)
    }

    /// Register a namespaced external tool under its own
    /// [`Tool::name`]. Bare (un-namespaced) and `builtin__` names are
    /// rejected so an MCP server cannot shadow a sandboxed capability,
    /// and duplicates are rejected rather than replaced.
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

    /// Look up a tool by canonical name.
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
    /// subset of canonical tool names. The schema's `name` is the
    /// registry key (canonical), not [`Tool::name`] — for built-ins the
    /// two differ, and the model must see the name the executor will
    /// dispatch on. Names that aren't in the registry are silently
    /// dropped; pair this with [`missing_tools`](Self::missing_tools)
    /// to surface the dropped names instead of losing them silently.
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

    /// The declared tool names this registry has no implementation for.
    /// [`build_schemas`](Self::build_schemas) drops these silently — the
    /// model is never offered them — so callers warn to make a typo'd or
    /// renamed tool in an agent definition visible rather than a silent
    /// capability loss. The registry itself does **not** alias legacy
    /// bare built-in names; the runner canonicalises grants before
    /// consulting it (see `canonical_tool_names` in the reducer runner).
    /// Names are returned in declared order; an empty vec is the healthy
    /// case.
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
    use fq_tools::{ToolContext, ToolError, ToolResult};
    use serde_json::Value;

    /// Minimal tool with an arbitrary name, for exercising registration
    /// policy on names the real built-ins can't produce.
    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _ctx: &ToolContext<'_>,
            _params: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok("noop"))
        }
    }

    #[test]
    fn builtins_cover_the_basename_list() {
        let reg = ToolRegistry::with_builtins();
        for base in BUILTIN_TOOL_BASENAMES {
            let canonical = format!("{BUILTIN_PREFIX}{base}");
            assert!(reg.get(&canonical).is_some(), "{canonical} registered");
            assert!(reg.get(base).is_none(), "bare '{base}' must not resolve");
        }
        assert_eq!(reg.len(), BUILTIN_TOOL_COUNT);
    }

    #[test]
    fn self_inspect_canonical_name_matches_fq_tools() {
        // The runner intercepts on this constant; it must track fq-tools'
        // own name for the tool.
        assert_eq!(
            SELF_INSPECT_CANONICAL_NAME,
            format!(
                "{BUILTIN_PREFIX}{}",
                fq_tools::builtin::SELF_INSPECT_TOOL_NAME
            )
        );
    }

    #[test]
    fn with_builtins_exec_registers_same_tool_set() {
        // An explicit exec config registers the identical built-in set —
        // only the exec tool's timeouts differ (not observable through
        // the Tool trait, so covered by exec.rs / config.rs tests).
        use std::time::Duration;
        let reg = ToolRegistry::with_builtins_exec(ExecConfig {
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
            ..ExecConfig::default()
        });
        assert!(reg.get("builtin__exec").is_some());
        assert_eq!(reg.len(), BUILTIN_TOOL_COUNT);
    }

    #[test]
    fn unknown_tool_returns_none() {
        let reg = ToolRegistry::with_builtins();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn bare_external_names_are_rejected() {
        let mut reg = ToolRegistry::new();
        let err = reg.register(Arc::new(NamedTool("echo"))).unwrap_err();
        assert!(err.contains("not namespaced"), "{err}");
        assert!(reg.is_empty());
    }

    #[test]
    fn reserved_prefix_external_names_are_rejected() {
        // The shadowing vector from #177: a server advertising a tool
        // named into the reserved namespace must be refused even though
        // the name is well-formed.
        let mut reg = ToolRegistry::with_builtins();
        let err = reg
            .register(Arc::new(NamedTool("builtin__evil")))
            .unwrap_err();
        assert!(err.contains("reserved"), "{err}");
        let err = reg
            .register(Arc::new(NamedTool("builtin__exec")))
            .unwrap_err();
        assert!(err.contains("reserved"), "{err}");
        // The genuine built-in is untouched.
        assert!(reg.get("builtin__exec").is_some());
        assert_eq!(reg.len(), BUILTIN_TOOL_COUNT);
    }

    #[test]
    fn duplicate_registrations_are_rejected_not_replaced() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(NamedTool("srv__echo"))).unwrap();
        let err = reg.register(Arc::new(NamedTool("srv__echo"))).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
        // Built-ins cannot be inserted twice either.
        reg.register_builtin("file_read", Arc::new(FileReadTool::new()))
            .unwrap();
        assert!(
            reg.register_builtin("file_read", Arc::new(FileReadTool::new()))
                .unwrap_err()
                .contains("duplicate")
        );
    }

    #[test]
    fn build_schemas_filters_to_known_tools() {
        let reg = ToolRegistry::with_builtins();
        let schemas = reg.build_schemas(&[
            "builtin__file_read".to_string(),
            "unknown".to_string(),
            "builtin__file_write".to_string(),
        ]);
        assert_eq!(schemas.len(), 2);
        assert!(schemas.iter().any(|s| s.name == "builtin__file_read"));
        assert!(schemas.iter().any(|s| s.name == "builtin__file_write"));
    }

    #[test]
    fn build_schemas_uses_the_canonical_registry_key() {
        // For built-ins the registry key ('builtin__file_read') differs
        // from Tool::name() ('file_read'); the LLM must be offered the
        // key, because that is what the executor dispatches on.
        let reg = ToolRegistry::with_builtins();
        let schemas = reg.build_schemas(&["builtin__file_read".to_string()]);
        let schema = &schemas[0];
        assert_eq!(schema.name, "builtin__file_read");
        assert!(!schema.description.is_empty());
        assert!(schema.parameters_schema.is_object());
    }

    #[test]
    fn missing_tools_reports_unregistered_declarations() {
        // A renamed or typo'd tool in an agent definition must be
        // reported, not silently dropped — the #177 namespace migration
        // is exactly the rename this guards. Bare built-in names are
        // unknown to the registry by design; the runner canonicalises
        // grants before consulting it.
        let reg = ToolRegistry::with_builtins();
        let missing = reg.missing_tools(&[
            "builtin__file_read".to_string(),
            "shell".to_string(),
            "exec".to_string(),
        ]);
        assert_eq!(missing, vec!["shell".to_string(), "exec".to_string()]);
    }

    #[test]
    fn missing_tools_empty_when_all_declared_tools_exist() {
        let reg = ToolRegistry::with_builtins();
        let missing = reg.missing_tools(&[
            "builtin__file_read".to_string(),
            "builtin__exec".to_string(),
        ]);
        assert!(missing.is_empty());
    }
}
