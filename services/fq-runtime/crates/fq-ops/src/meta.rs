//! Operation metadata: the contract every derived surface inherits
//! (ADR-0006 D7, P9). Everything here is const-constructible so an
//! implementation can declare `const META: OpMeta`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The four operation kinds (ADR-0006 D2). The surface is CQRS because
/// the system is: commands end at the log, projections answer queries,
/// the log streams back out. `Probe` exists for the live-infrastructure
/// reads (`runtime.health`, `runtime.status`) that are neither
/// projection queries nor commands, and is kept deliberately small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Command,
    Query,
    Stream,
    Probe,
}

/// Permission verb vocabulary — mirrors fq-store's grant model
/// (`grants.rs`: `Verb` × scope, enforced by biscuit tokens) so both
/// registries speak one authz language (D7). A mirror rather than a
/// dependency: fq-store is a separate workspace and this crate is a
/// leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    Read,
    Write,
    Delete,
    List,
    Grant,
}

/// What an operation requires of its caller: a verb over a scope
/// (e.g. `Write` over `"invocation"`). Enforcement is registry
/// middleware on the edge, never per-surface code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OpPermission {
    pub verb: Verb,
    pub scope: &'static str,
}

/// Registry curation state (P11). Deprecation is a first-class
/// workflow, not a deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stability {
    Experimental,
    Stable,
    Deprecated,
}

/// The per-operation contract metadata (P9): promoted from help-text
/// lore to contract, so every derived surface inherits it.
///
/// Read auditing is deliberately absent: the edge middleware observes
/// every invocation (identity, op, allow/deny) and can log uniformly —
/// per-op audit metadata returns only if the audit middleware phase
/// proves a real need for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OpMeta {
    pub permission: OpPermission,
    pub stability: Stability,
    /// Contract caveats the caller must know: retention bounds,
    /// idempotency, semantics (e.g. requeue's non-idempotency, the
    /// dead-letter listing's 30-day horizon). Empty means "none".
    pub caveats: &'static str,
}
