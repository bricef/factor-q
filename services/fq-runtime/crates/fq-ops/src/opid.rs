//! Composite operation identity: the wire's native address for every
//! promise on the surface. Generic operations are (verb, resource)
//! pairs; declared operations carry their own identity. Rendered
//! names derive from structure — nothing is parsed, and the rendering
//! exists for self-documentation (MCP tool names, docs,
//! `registry.describe`), not transport.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::catalogue::ResourceId;
use crate::declared::{DomainVerbId, MetaReadId, ReportId};

/// Every operation the surface can carry, natively serializable for
/// the tarpc edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpId {
    Get(ResourceId),
    List(ResourceId),
    Create(ResourceId),
    Stream(ResourceId),
    Verb(DomainVerbId),
    Report(ReportId),
    MetaRead(MetaReadId),
}

/// The category an operation belongs to — what replaced the old
/// four-kind taxonomy. Recorded in descriptors; each category implies
/// its envelope shape (streams ride `next_batch`, commands return
/// receipts, reads answer at a watermark).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpCategory {
    Get,
    List,
    Create,
    Stream,
    DomainVerb,
    Report,
    MetaRead,
}

impl OpId {
    pub fn category(&self) -> OpCategory {
        match self {
            OpId::Get(_) => OpCategory::Get,
            OpId::List(_) => OpCategory::List,
            OpId::Create(_) => OpCategory::Create,
            OpId::Stream(_) => OpCategory::Stream,
            OpId::Verb(_) => OpCategory::DomainVerb,
            OpId::Report(_) => OpCategory::Report,
            OpId::MetaRead(_) => OpCategory::MetaRead,
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
            OpId::Verb(v) => write!(f, "{}.{}", v.resource().segment(), v.leaf()),
            OpId::Report(r) => f.write_str(r.render()),
            OpId::MetaRead(m) => f.write_str(m.render()),
        }
    }
}
