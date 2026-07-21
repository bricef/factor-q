//! Contract metadata every derived surface inherits (P9), and the
//! authority vocabulary (D7).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::catalogue::ResourceId;

/// Permission verb vocabulary — mirrors fq-store's grant model
/// (`grants.rs`: `Verb` × scope, enforced by biscuit tokens) so both
/// registries speak one authz language. A mirror rather than a
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

/// What an operation requires of its caller: a verb over a resource
/// scope. Derived for the generic surface (reads ⇒ `Read` on the
/// resource, Create ⇒ `Write`); declared manually by domain verbs and
/// reports — always manually on the synthetic `Control` resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Authority {
    pub verb: Verb,
    pub scope: ResourceId,
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

/// Human-facing contract text, colocated with the definition it
/// describes and inherited by every derived surface: `description` is
/// the one-liner (`registry.describe`, MCP tool listings); `caveats`
/// is what the caller must know (retention bounds, idempotency,
/// semantics — requeue's non-idempotency, drop's kill-switch). Empty
/// caveats means "none".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OpMeta {
    pub description: &'static str,
    pub stability: Stability,
    pub caveats: &'static str,
}
