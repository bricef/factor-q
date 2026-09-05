//! Workspace roots (ADR-0018): what filesystem the host tells a server
//! it may reason about, where that set comes from, and how it is
//! updated once a server is running.
//!
//! Roots are nothing-by-default. They are derived from the
//! invocation's *materialised* tool sandbox, filtered by the agent's
//! per-server [`RootsGrant`], and run through the outbound validator
//! chain before a server ever sees them.

use std::sync::Arc;

use fq_tools::ToolSandbox;
use rmcp::model::Root;
use tokio::sync::Mutex;

use crate::agent::RootsGrant;
use crate::validation::ValidatorChain;

use super::{McpClient, McpError};

/// A host-side handle to a per-invocation server's advertised roots
/// (ADR-0018). Holds the shared roots cell the handler reads on
/// `roots/list`, plus the client to fire `roots/list_changed`. Lets
/// the host update the advertised workspace (e.g. when the agent's
/// sandbox changes) and notify the server, which re-fetches.
pub struct RootsHandle {
    pub(super) server: String,
    pub(super) roots: Arc<Mutex<Vec<Root>>>,
    pub(super) client: Arc<McpClient>,
}

impl RootsHandle {
    /// Replace the advertised roots and notify the server via
    /// `roots/list_changed` so it re-fetches. The full dynamic-workspace
    /// trigger (recomputing from a changed sandbox) is a later
    /// "Workspace state" concern; this exposes the mechanism.
    pub async fn set_roots(&self, roots: Vec<Root>) -> Result<(), McpError> {
        *self.roots.lock().await = roots;
        self.client
            .notify_roots_list_changed()
            .await
            .map_err(|err| McpError::RootsOp {
                server: self.server.clone(),
                reason: err.to_string(),
            })
    }
}

/// Derive the `file://` roots a server should be advertised from the
/// invocation's **materialised** tool sandbox (ADR-0018): the union of
/// its read and write prefixes, deduplicated, each as a `file://` root
/// named by its path. Taking the [`ToolSandbox`] — the same object the
/// tools enforce against, with `${workspace}` already bound (#179) —
/// makes *advertised roots ⊆ enforced boundary* hold by construction;
/// the declared sandbox's raw strings may still carry placeholders.
/// `file://` only for v1.
pub fn roots_from_tool_sandbox(sandbox: &ToolSandbox) -> Vec<Root> {
    let mut seen = std::collections::BTreeSet::new();
    let mut roots = Vec::new();
    for path in sandbox
        .read_prefixes()
        .iter()
        .chain(sandbox.write_prefixes())
    {
        let path = path.to_string_lossy().into_owned();
        let uri = format!("file://{path}");
        if seen.insert(uri.clone()) {
            roots.push(Root::new(uri).with_name(path));
        }
    }
    roots
}

/// Compute the roots to advertise to `server`: nothing unless the
/// agent's [`RootsGrant`] permits it, otherwise the sandbox-derived
/// roots run through the outbound validator chain (ADR-0018 §4). A
/// `Deny` from the chain advertises nothing rather than a partial set.
pub fn advertised_roots_from_tool_sandbox(
    sandbox: &ToolSandbox,
    grant: Option<&RootsGrant>,
    server: &str,
    validators: &ValidatorChain<Vec<Root>>,
) -> Vec<Root> {
    if !grant.is_some_and(|g| g.permits(server)) {
        return Vec::new();
    }
    validators
        .run(roots_from_tool_sandbox(sandbox))
        .unwrap_or_default()
}
