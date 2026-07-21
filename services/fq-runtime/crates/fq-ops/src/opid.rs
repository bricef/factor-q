//! Composite operation identity: the wire's native address for every
//! promise on the surface. Generic operations are (verb, resource)
//! pairs; declared operations carry the identity their definitions
//! declared. Rendered names derive from structure for
//! self-documentation (MCP tool names, docs, List(Operation)) —
//! nothing parses them, ever; equality is the only operation the
//! declared leaf strings support.
//!
//! Machinery reads have no variant here: Control is a synthetic
//! resource, so "ask the machinery about itself" is just
//! `Get(Control)`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::catalogue::Domain;

/// Every operation the surface can carry, natively serializable for
/// the tarpc edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpId {
    Get(Domain),
    List(Domain),
    Create(Domain),
    Stream(Domain),
    Verb { domain: Domain, leaf: String },
    Report { name: String },
}

/// The category an operation belongs to — what replaced the old
/// four-kind taxonomy. Recorded in descriptors; each category implies
/// its envelope shape (streams ride `next_batch`, verbs return
/// receipts, reads answer at a watermark). This match is structural:
/// it grows with categories, never with operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpCategory {
    Get,
    List,
    Create,
    Stream,
    DomainVerb,
    Report,
}

impl OpId {
    pub fn category(&self) -> OpCategory {
        match self {
            OpId::Get(_) => OpCategory::Get,
            OpId::List(_) => OpCategory::List,
            OpId::Create(_) => OpCategory::Create,
            OpId::Stream(_) => OpCategory::Stream,
            OpId::Verb { .. } => OpCategory::DomainVerb,
            OpId::Report { .. } => OpCategory::Report,
        }
    }
}

impl std::fmt::Display for OpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpId::Get(r) => write!(f, "{}.get", r.segment()),
            OpId::List(r) => write!(f, "{}.list", r.segment()),
            OpId::Create(r) => write!(f, "{}.create", r.segment()),
            OpId::Stream(r) => write!(f, "{}.stream", r.segment()),
            OpId::Verb { domain, leaf } => write!(f, "{}.{leaf}", domain.segment()),
            OpId::Report { name } => f.write_str(name),
        }
    }
}
