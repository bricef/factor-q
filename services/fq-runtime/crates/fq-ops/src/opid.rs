//! Composite operation identity: the wire's native address for every
//! promise on the surface. Generic operations are (verb, resource)
//! pairs; declared operations carry the identity their definitions
//! declared. Rendered names derive from structure for
//! self-documentation (MCP tool names, docs, List(Operation)) —
//! nothing parses them, ever; equality is the only operation the
//! declared verb strings support.
//!
//! Machinery reads have no variant here: Control is a synthetic
//! resource, so "ask the machinery about itself" is just
//! `Get(Control)`.

use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::Domain;

/// The verbs each domain declares, as types — one enum per domain
/// that has bespoke verbs, one variant per verb, each enum named
/// after its domain so a declaration reads as the surface renders
/// (`Control::Down` → `control.down`). Constructed only here and
/// named at exactly one site: the declaration that takes them
/// (`Command::new`). Nonsense pairs (`cost.drop`) are
/// unrepresentable by construction.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum Invocation {
    Drop,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum Control {
    Down,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum Trigger {
    Publish,
}

/// The reports each domain declares, same construction discipline as
/// the verb enums.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum Cost {
    Summary,
}

/// A domain verb's identity, typed: the domain and the verb arrive
/// together, so only pairs a domain actually declares can be named
/// in code. `Unknown` exists for one reason — graceful version skew:
/// a peer naming vocabulary this build doesn't know parses to
/// `Unknown` and the registry refuses it as not-registered, instead
/// of the wire failing to deserialise. Nothing constructs `Unknown`
/// on purpose.
///
/// On the wire this serialises as the flat `{"domain": …, "verb": …}`
/// pair — byte-identical to the pre-typed encoding; the typing is a
/// code-level guarantee, not a wire change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "VerbIdWire", into = "VerbIdWire")]
pub enum VerbId {
    Invocation(Invocation),
    Control(Control),
    Trigger(Trigger),
    Unknown { domain: String, verb: String },
}

impl VerbId {
    /// The typed domain, when this build knows the vocabulary.
    pub fn domain(&self) -> Option<Domain> {
        match self {
            VerbId::Invocation(_) => Some(Domain::Invocation),
            VerbId::Control(_) => Some(Domain::Control),
            VerbId::Trigger(_) => Some(Domain::Trigger),
            VerbId::Unknown { .. } => None,
        }
    }

    pub fn domain_segment(&self) -> &str {
        match self {
            VerbId::Unknown { domain, .. } => domain,
            known => known
                .domain()
                .expect("typed variants have a domain")
                .segment(),
        }
    }

    pub fn verb_segment(&self) -> &str {
        match self {
            VerbId::Invocation(v) => (*v).into(),
            VerbId::Control(v) => (*v).into(),
            VerbId::Trigger(v) => (*v).into(),
            VerbId::Unknown { verb, .. } => verb,
        }
    }
}

impl From<Invocation> for VerbId {
    fn from(verb: Invocation) -> Self {
        VerbId::Invocation(verb)
    }
}

impl From<Control> for VerbId {
    fn from(verb: Control) -> Self {
        VerbId::Control(verb)
    }
}

impl From<Trigger> for VerbId {
    fn from(verb: Trigger) -> Self {
        VerbId::Trigger(verb)
    }
}

/// The wire shape of [`VerbId`] — the same flat pair the surface has
/// always spoken.
#[derive(Serialize, Deserialize, JsonSchema)]
struct VerbIdWire {
    domain: String,
    verb: String,
}

impl From<VerbId> for VerbIdWire {
    fn from(id: VerbId) -> Self {
        VerbIdWire {
            domain: id.domain_segment().to_string(),
            verb: id.verb_segment().to_string(),
        }
    }
}

impl From<VerbIdWire> for VerbId {
    fn from(wire: VerbIdWire) -> Self {
        let typed = Domain::from_str(&wire.domain)
            .ok()
            .and_then(|domain| match domain {
                Domain::Invocation => Invocation::from_str(&wire.verb)
                    .ok()
                    .map(VerbId::Invocation),
                Domain::Control => Control::from_str(&wire.verb).ok().map(VerbId::Control),
                Domain::Trigger => Trigger::from_str(&wire.verb).ok().map(VerbId::Trigger),
                _ => None,
            });
        typed.unwrap_or(VerbId::Unknown {
            domain: wire.domain,
            verb: wire.verb,
        })
    }
}

impl JsonSchema for VerbId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "VerbId".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        VerbIdWire::json_schema(generator)
    }
}

/// A report's identity, typed; the same shape and skew story as
/// [`VerbId`], with the wire pair spelled `{"domain": …, "name": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "ReportIdWire", into = "ReportIdWire")]
pub enum ReportId {
    Cost(Cost),
    Unknown { domain: String, name: String },
}

impl ReportId {
    pub fn domain(&self) -> Option<Domain> {
        match self {
            ReportId::Cost(_) => Some(Domain::Cost),
            ReportId::Unknown { .. } => None,
        }
    }

    pub fn domain_segment(&self) -> &str {
        match self {
            ReportId::Unknown { domain, .. } => domain,
            known => known
                .domain()
                .expect("typed variants have a domain")
                .segment(),
        }
    }

    pub fn name_segment(&self) -> &str {
        match self {
            ReportId::Cost(r) => (*r).into(),
            ReportId::Unknown { name, .. } => name,
        }
    }
}

impl From<Cost> for ReportId {
    fn from(report: Cost) -> Self {
        ReportId::Cost(report)
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct ReportIdWire {
    domain: String,
    name: String,
}

impl From<ReportId> for ReportIdWire {
    fn from(id: ReportId) -> Self {
        ReportIdWire {
            domain: id.domain_segment().to_string(),
            name: id.name_segment().to_string(),
        }
    }
}

impl From<ReportIdWire> for ReportId {
    fn from(wire: ReportIdWire) -> Self {
        let typed = Domain::from_str(&wire.domain)
            .ok()
            .and_then(|domain| match domain {
                Domain::Cost => Cost::from_str(&wire.name).ok().map(ReportId::Cost),
                _ => None,
            });
        typed.unwrap_or(ReportId::Unknown {
            domain: wire.domain,
            name: wire.name,
        })
    }
}

impl JsonSchema for ReportId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ReportId".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        ReportIdWire::json_schema(generator)
    }
}

/// Every operation the surface can carry, natively serializable for
/// the tarpc edge.
///
/// This is **request vocabulary, not declaration vocabulary** — but
/// the two refusal modes now live at different layers. Nonsense
/// pairs (`cost.drop`) are unrepresentable: [`VerbId`]/[`ReportId`]
/// carry domain and word together, typed. Unknown-but-well-formed
/// vocabulary (version skew: a peer naming an op this build doesn't
/// have) parses to the ids' `Unknown` variants and resolves to
/// nothing — the edge refuses it as not-registered rather than the
/// wire failing to deserialise. Category mismatches (`Stream(d)`
/// where `d` is a view) remain constructable on purpose and refuse
/// the same way: whether an address is served is a registry
/// question, answered at resolve time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpId {
    Get(Domain),
    List(Domain),
    Stream(Domain),
    Verb(VerbId),
    Report(ReportId),
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
    Stream,
    DomainVerb,
    Report,
}

impl OpId {
    pub fn category(&self) -> OpCategory {
        match self {
            OpId::Get(_) => OpCategory::Get,
            OpId::List(_) => OpCategory::List,
            OpId::Stream(_) => OpCategory::Stream,
            OpId::Verb(_) => OpCategory::DomainVerb,
            OpId::Report(_) => OpCategory::Report,
        }
    }
}

impl std::fmt::Display for OpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpId::Get(r) => write!(f, "{}.get", r.segment()),
            OpId::List(r) => write!(f, "{}.list", r.segment()),
            OpId::Stream(r) => write!(f, "{}.stream", r.segment()),
            OpId::Verb(id) => write!(f, "{}.{}", id.domain_segment(), id.verb_segment()),
            OpId::Report(id) => write!(f, "{}.{}", id.domain_segment(), id.name_segment()),
        }
    }
}
