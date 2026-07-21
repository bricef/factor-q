//! Contract metadata every derived surface inherits (P9), and the
//! authority vocabulary (D7).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::catalogue::Domain;

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
/// scope. The generic surface derives `Read` on the resource — and
/// nothing else, because it is read-only; every write on the surface
/// belongs to a domain verb, which declares its authority manually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Authority {
    pub verb: Verb,
    pub scope: Domain,
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
