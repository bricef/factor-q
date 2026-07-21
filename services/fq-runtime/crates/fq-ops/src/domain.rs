//! The domain model, declared in types
//! (`docs/design/aspirational/operator-surface-domain-model.md`).
//!
//! A resource has one of three natures. **Atoms** are immutable once
//! created — the only streamable nature, because streaming is
//! creation-notification and nothing else needs modelling. **Views**
//! are folds of atoms: stable identity, state read at a watermark,
//! never streamed directly (you stream their atoms). **Synthetic**
//! resources stand for live machinery rather than recorded truth —
//! nothing derives for them; they exist to give the machinery's verbs
//! and reads a home and a permission scope.
//!
//! Generic operations (Get, List, and the Stream overlay) derive from
//! a catalogue entry — one [`Resource`] impl, nature included, buys a
//! resource its whole read surface, and the generic surface is
//! read-only: creation is not a generic verb (operators command the
//! machinery; atoms appear in the log as receipts), so every mutation
//! is a declared command. Adding a resource to [`Domain`] is the P11
//! curation gate.
//!
//! The declared surface lives here too — [`Command`] and [`Report`]
//! are domain entities exactly as resources are. A declaration is
//! **one site**: the impl carries its own identity, types, authority,
//! and contract text; adding a verb is writing the impl and
//! registering it — no enum to extend, nowhere else to touch.
//! Identity collisions are caught at registration.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::opid::OpId;

/// Every resource the surface can speak about — including synthetic
/// ones that exist only as verb carriers and permission scopes
/// (`Control`). The rendered segment derives from the variant name;
/// there is no name table.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Domain {
    Agent,
    Control,
    DeadLetter,
    Event,
    Invocation,
    Operation,
    Turn,
    Trigger,
    Worker,
}

impl Domain {
    /// The rendered name segment (`turn`, `dead_letter`).
    pub fn segment(&self) -> &'static str {
        self.into()
    }
}

/// A resource's nature, recorded in descriptors so every derived
/// surface can explain the semantics (views answer at a watermark;
/// atoms stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Nature {
    Atom,
    View,
    Synthetic,
}

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

/// One catalogue entry: the single definition from which a resource's
/// generic read surface derives. `Key` addresses one resource (Get);
/// `Filter` is the typed, per-resource selection for List and Stream —
/// deliberately a struct, never a query language; `State` is what
/// comes back (an atom's immutable content, or a view's fold).
///
/// Everything about a resource is declared here, nature included:
/// registration reads the impl and derives the surface, so there is
/// no registration-time choice to get wrong — "only atoms stream" is
/// enforced by derivation (non-atoms simply get no stream op).
pub trait Resource {
    const DOMAIN: Domain;
    const NATURE: Nature;
    /// Schema version for this resource's wire types (P10): additive
    /// changes keep it, observable breaks bump it.
    const VERSION: u32 = 1;
    type Key: Serialize + DeserializeOwned + JsonSchema;
    type State: Serialize + DeserializeOwned + JsonSchema;
    type Filter: Serialize + DeserializeOwned + JsonSchema;
    /// One-line description of the resource, inherited by its whole
    /// derived surface (List(Operation), MCP listings, docs).
    const DESCRIPTION: &'static str;
    const STABILITY: Stability;
    /// What a caller must know about this resource's surface:
    /// retention bounds, fold semantics. Defaults to "none".
    const CAVEATS: &'static str = "";
}

/// A bespoke command, attached to a resource — machinery verbs attach
/// to the synthetic `Control` resource. Its output is always a
/// [`crate::wire::Receipt`] — commands return references to the atoms
/// they appended, never state (D3); there is no `Output` to declare,
/// so the rule cannot be broken. Authority is declared, not derived:
/// the semantics that make a verb bespoke are exactly what generic
/// derivation would get wrong.
pub trait Command {
    const DOMAIN: Domain;
    /// The verb word itself; renders as `{resource}.{verb}`. Opaque
    /// identity plus documentation — never parsed.
    const VERB: &'static str;
    const VERSION: u32 = 1;
    type Input: Serialize + DeserializeOwned + JsonSchema;
    const AUTHORITY: Authority;
    /// One-line description of the command.
    const DESCRIPTION: &'static str;
    const STABILITY: Stability;
    /// The contract text that makes this verb bespoke: idempotency,
    /// kill-switch semantics, delivery guarantees. Defaults to "none".
    const CAVEATS: &'static str = "";

    /// This command's wire identity.
    fn op() -> OpId {
        OpId::Verb {
            domain: Self::DOMAIN,
            verb: Self::VERB.to_string(),
        }
    }
}

/// A named, typed computation over resources — the kind the original
/// taxonomy was missing. Not a Get on a pretend-resource and not a
/// query language: few by design, watermarked like any read. `READS`
/// declares the resource scopes the computation consumes; authority
/// is Read on each.
pub trait Report {
    /// The report's full rendered name (`cost.summary`). Reports are
    /// not resource-attached, so the name is free-standing — declared
    /// here, never parsed.
    const NAME: &'static str;
    const VERSION: u32 = 1;
    type Params: Serialize + DeserializeOwned + JsonSchema;
    type Output: Serialize + DeserializeOwned + JsonSchema;
    const READS: &'static [Domain];
    /// One-line description of the report.
    const DESCRIPTION: &'static str;
    const STABILITY: Stability;
    const CAVEATS: &'static str = "";

    /// This report's wire identity.
    fn op() -> OpId {
        OpId::Report {
            name: Self::NAME.to_string(),
        }
    }
}
