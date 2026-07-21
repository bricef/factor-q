//! The declared surface: what stays hand-written because it is
//! semantically bespoke — five domain verbs, three reports, three
//! machinery reads. Everything else derives from the catalogue, and
//! that division is the model's own line: what remains declared is
//! exactly what a generic verb would bury.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::catalogue::ResourceId;
use crate::meta::{Authority, OpMeta};

// ------------------------------------------------------------------
// Domain verbs
// ------------------------------------------------------------------

/// Wire identity of each bespoke command. The `resource`/`leaf` pair
/// is the declaration (compiler-exhaustive, colocated here), and the
/// rendered name derives from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DomainVerbId {
    InvocationDrop,
    DeadletterRequeue,
    WorkerPrune,
    ControlDown,
    ControlReload,
}

impl DomainVerbId {
    /// The resource this verb attaches to — verbs attach to resources
    /// everywhere in the model, machinery verbs to the synthetic
    /// `Control` resource.
    pub fn resource(&self) -> ResourceId {
        match self {
            DomainVerbId::InvocationDrop => ResourceId::Invocation,
            DomainVerbId::DeadletterRequeue => ResourceId::Trigger,
            DomainVerbId::WorkerPrune => ResourceId::Worker,
            DomainVerbId::ControlDown => ResourceId::Control,
            DomainVerbId::ControlReload => ResourceId::Control,
        }
    }

    pub fn leaf(&self) -> &'static str {
        match self {
            DomainVerbId::InvocationDrop => "drop",
            DomainVerbId::DeadletterRequeue => "requeue",
            DomainVerbId::WorkerPrune => "prune",
            DomainVerbId::ControlDown => "down",
            DomainVerbId::ControlReload => "reload",
        }
    }
}

/// A bespoke command. Its output is always a [`crate::wire::Receipt`]
/// — commands return references to the atoms they appended, never
/// state (D3); there is no `Output` to declare, so the rule cannot be
/// broken. Authority is declared, not derived: the semantics that make
/// a verb bespoke are exactly what generic derivation would get wrong.
pub trait Command {
    const ID: DomainVerbId;
    const VERSION: u32 = 1;
    type Input: Serialize + DeserializeOwned + JsonSchema;
    const AUTHORITY: Authority;
    const META: OpMeta;
}

// ------------------------------------------------------------------
// Reports
// ------------------------------------------------------------------

/// Wire identity of each report. Reports are named computations, not
/// resource reads, so their rendered names are declared here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportId {
    CostSummary,
    CostByAgent,
    Doctor,
}

impl ReportId {
    pub fn render(&self) -> &'static str {
        match self {
            ReportId::CostSummary => "cost.summary",
            ReportId::CostByAgent => "cost.by_agent",
            ReportId::Doctor => "runtime.doctor",
        }
    }
}

/// A named, typed computation over resources — the kind the original
/// taxonomy was missing. Not a Get on a pretend-resource and not a
/// query language: few by design, watermarked like any read. `READS`
/// declares the resource scopes the computation consumes; authority
/// is Read on each.
pub trait Report {
    const ID: ReportId;
    const VERSION: u32 = 1;
    type Params: Serialize + DeserializeOwned + JsonSchema;
    type Output: Serialize + DeserializeOwned + JsonSchema;
    const READS: &'static [ResourceId];
    const META: OpMeta;
}

// ------------------------------------------------------------------
// The meta surface
// ------------------------------------------------------------------

/// Wire identity of the machinery reads — a flat, closed set (bring
/// taxonomy when it stops being closed). All scope to the synthetic
/// `Control` resource for authority: Read control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetaReadId {
    Health,
    Status,
    Version,
}

impl MetaReadId {
    pub fn render(&self) -> &'static str {
        match self {
            MetaReadId::Health => "control.health",
            MetaReadId::Status => "control.status",
            MetaReadId::Version => "control.version",
        }
    }
}

/// One machinery read: questions about the daemon itself, not the
/// records — the old "Probe" misfit, now outside the resource model
/// but behind the same edge and authority semantics.
pub trait MetaRead {
    const ID: MetaReadId;
    const VERSION: u32 = 1;
    type Output: Serialize + DeserializeOwned + JsonSchema;
    const META: OpMeta;
}
