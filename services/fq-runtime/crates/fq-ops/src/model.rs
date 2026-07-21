//! The domain model, declared as values
//! (`docs/design/aspirational/operator-surface-domain-model.md`).
//!
//! [`Resource`], [`Command`], and [`Report`] are **value types**: a
//! declaration is a constructor call, and the value handed to the
//! registry *is* the definition — there is no descriptor projection,
//! no trait/value duality, nothing to drift (D1 made literal). The
//! constructors are generic over the declaration's Rust types, so the
//! JSON schemas are captured at the single declaration site and the
//! same generic slot types the handler when Phase 2 binds one.
//!
//! Natures: **atoms** are immutable once created — the only
//! streamable nature (streaming is creation-notification). **Views**
//! fold atoms: stable identity, state read at a watermark, never
//! streamed directly (you stream their atoms). **Synthetic**
//! resources stand for live machinery, not recorded truth — Get alone
//! derives, and their verbs carry manual authority.
//!
//! The generic surface is read-only: creation is not a generic verb
//! (operators command the machinery; atoms appear in the log as
//! receipts), so every mutation is a declared [`Command`]. Adding a
//! resource to [`Domain`] is the P11 curation gate.

use schemars::{JsonSchema, Schema, schema_for};
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
    Trigger,
    Turn,
    Worker,
}

impl Domain {
    /// The rendered name segment (`dead_letter`, `turn`).
    pub fn segment(&self) -> &'static str {
        self.into()
    }
}

/// A resource's nature. The distinction is load-bearing: it decides
/// which generic operations derive.
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
/// belongs to a command, which declares its authority manually.
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

/// One catalogue entry, as a value. `key_schema` addresses one
/// resource (Get); `filter_schema` is the typed, per-resource
/// selection for List and Stream — deliberately a schema'd struct,
/// never a query language; `state_schema` is what comes back (an
/// atom's immutable content, or a view's fold). All three are
/// captured from the Rust types by [`Resource::new`].
#[derive(Debug, Clone, Serialize)]
pub struct Resource {
    pub domain: Domain,
    pub nature: Nature,
    pub version: u32,
    /// Contract text for callers, inherited by the whole derived
    /// surface. Convention: the first sentence is the one-line
    /// summary (listings truncate there); anything the caller must
    /// know — retention bounds, fold semantics — follows in the same
    /// text.
    pub description: &'static str,
    pub stability: Stability,
    pub key_schema: Schema,
    pub state_schema: Schema,
    pub filter_schema: Schema,
}

impl Resource {
    /// Declare a resource. The generic parameters are the declaration:
    /// `Key` (Get identity), `State` (what reads return), `Filter`
    /// (List/Stream selection); their schemas are captured here, at
    /// the one declaration site.
    pub fn new<Key, State, Filter>(
        domain: Domain,
        nature: Nature,
        description: &'static str,
        stability: Stability,
    ) -> Self
    where
        Key: Serialize + DeserializeOwned + JsonSchema,
        State: Serialize + DeserializeOwned + JsonSchema,
        Filter: Serialize + DeserializeOwned + JsonSchema,
    {
        Resource {
            domain,
            nature,
            version: 1,
            description,
            stability,
            key_schema: schema_for!(Key),
            state_schema: schema_for!(State),
            filter_schema: schema_for!(Filter),
        }
    }

    /// Schema version (P10): additive changes keep it, observable
    /// breaks bump it.
    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }
}

/// A bespoke command, as a value — attached to a resource (machinery
/// verbs attach to the synthetic `Control` resource). Its output is
/// always a [`crate::wire::Receipt`]: commands return references to
/// the atoms they appended, never state (D3) — there is no output to
/// declare, so the rule cannot be broken. Authority is declared, not
/// derived: the semantics that make a verb bespoke are exactly what
/// generic derivation would get wrong.
#[derive(Debug, Clone, Serialize)]
pub struct Command {
    pub domain: Domain,
    /// The verb word itself; renders as `{domain}.{verb}`. Opaque
    /// identity plus documentation — never parsed.
    pub verb: &'static str,
    pub version: u32,
    pub authority: Authority,
    /// Contract text for callers. Convention: first sentence is the
    /// summary; the semantics that make this verb bespoke —
    /// idempotency, kill-switch behaviour, delivery guarantees —
    /// follow in the same text.
    pub description: &'static str,
    pub stability: Stability,
    pub input_schema: Schema,
}

impl Command {
    /// Declare a command. `Input` is the declaration's typed input;
    /// its schema is captured here, and the same type parameter will
    /// type the handler when Phase 2 binds one.
    pub fn new<Input>(
        domain: Domain,
        verb: &'static str,
        authority: Authority,
        description: &'static str,
        stability: Stability,
    ) -> Self
    where
        Input: Serialize + DeserializeOwned + JsonSchema,
    {
        Command {
            domain,
            verb,
            version: 1,
            authority,
            description,
            stability,
            input_schema: schema_for!(Input),
        }
    }

    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// This command's wire identity.
    pub fn op(&self) -> OpId {
        OpId::Verb {
            domain: self.domain,
            verb: self.verb.to_string(),
        }
    }
}

/// A named, typed computation over resources, as a value — the kind
/// the original taxonomy was missing. Not a Get on a pretend-resource
/// and not a query language: few by design, watermarked like any
/// read. `reads` declares the resource scopes consumed; authority is
/// Read on each.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The report's full rendered name (`cost.summary`). Reports are
    /// not resource-attached, so the name is free-standing — declared
    /// here, never parsed.
    pub name: &'static str,
    pub version: u32,
    pub reads: &'static [Domain],
    /// Contract text for callers; first sentence is the summary.
    pub description: &'static str,
    pub stability: Stability,
    pub params_schema: Schema,
    pub output_schema: Schema,
}

impl Report {
    /// Declare a report. `Params` and `Output` are the declaration's
    /// types; their schemas are captured here.
    pub fn new<Params, Output>(
        name: &'static str,
        reads: &'static [Domain],
        description: &'static str,
        stability: Stability,
    ) -> Self
    where
        Params: Serialize + DeserializeOwned + JsonSchema,
        Output: Serialize + DeserializeOwned + JsonSchema,
    {
        Report {
            name,
            version: 1,
            reads,
            description,
            stability,
            params_schema: schema_for!(Params),
            output_schema: schema_for!(Output),
        }
    }

    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// This report's wire identity.
    pub fn op(&self) -> OpId {
        OpId::Report {
            name: self.name.to_string(),
        }
    }
}
