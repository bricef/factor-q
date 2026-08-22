//! What an invocation may call, by name.
//!
//! Two things live here: the migration mapping from bare built-in
//! grants to their `builtin__` form, and the rule that every
//! invocation may call `report_outcome` whether or not its agent asked
//! for it.
//!
//! Split from `runner.rs` rather than added to it — the file is over
//! budget and being dismantled under #78, and this is a coherent piece
//! of it: the vocabulary of tool names, with no reducer state.

use tracing::warn;

use crate::agent::AgentId;

/// Canonical form of a legacy bare built-in name (`exec` →
/// `builtin__exec`), or `None` when the name is not a bare built-in.
/// The basename list lives beside the registry so a new built-in cannot
/// miss this mapping. MCP tools are always explicitly namespaced and
/// never map.
pub(crate) fn canonicalize_bare_builtin(name: &str) -> Option<String> {
    crate::tools::BUILTIN_TOOL_BASENAMES
        .contains(&name)
        .then(|| format!("{}{name}", crate::tools::BUILTIN_PREFIX))
}

/// Map legacy bare built-in grants to their canonical names. Pure and
/// quiet — it runs on every tool call's allowed-check, so deprecation
/// warnings are emitted once per invocation setup by
/// [`warn_on_deprecated_bare_grants`], not here. Accepted for one
/// release while agent definitions migrate (#177).
pub(crate) fn canonical_tool_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|name| canonicalize_bare_builtin(name).unwrap_or_else(|| name.clone()))
        .collect()
}

/// The tools an invocation may actually use: everything the agent
/// declares, plus `report_outcome`.
///
/// `report_outcome` is not a capability an agent opts into. It is the
/// only clean end to a run — the sole path to `NextAction::Complete` —
/// so an agent that cannot call it cannot finish, and an agent
/// declaring no tools at all could never call anything. Appending it
/// here rather than asking every definition to declare it means no
/// agent can be written that is unable to terminate.
///
/// It matters for the *permission* check as well as the offered
/// schemas: a malformed declaration is deliberately not terminal and
/// falls through to normal dispatch, so that the schema-only tool's
/// own error teaches the model to correct the call. Without the name
/// here that dispatch is refused as a permission error instead, and
/// the correction never reaches the model.
pub(crate) fn effective_tool_names(declared: &[String]) -> Vec<String> {
    let mut names = canonical_tool_names(declared);
    if !names
        .iter()
        .any(|name| name == crate::tools::REPORT_OUTCOME_CANONICAL_NAME)
    {
        names.push(crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string());
    }
    names
}

/// Emit the one-per-invocation deprecation warning for legacy bare
/// built-in grants in an agent definition (#177 migration window).
pub(crate) fn warn_on_deprecated_bare_grants(agent_id: &AgentId, names: &[String]) {
    let deprecated: Vec<&str> = names
        .iter()
        .filter(|name| canonicalize_bare_builtin(name).is_some())
        .map(|name| name.as_str())
        .collect();
    if !deprecated.is_empty() {
        warn!(
            agent_id = %agent_id,
            tools = ?deprecated,
            "bare built-in tool grants are deprecated; use the builtin__ prefix"
        );
    }
}
