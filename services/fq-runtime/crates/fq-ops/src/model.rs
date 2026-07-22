//! The domain model, declared as values
//! (`docs/design/aspirational/operator-surface-domain-model.md`).
//!
//! [`Atom`], [`View`], [`Synthetic`], [`Command`], and [`Report`]
//! are **value types**: a declaration is a constructor call, and the
//! value handed to the registry *is* the definition — there is no descriptor projection,
//! no trait/value duality, nothing to drift (D1 made literal). The
//! constructors are generic over the declaration's Rust types, so the
//! JSON schemas are captured at the single declaration site and the
//! same generic slot types the handler when Phase 2 binds one.
//!
//! The three resource types carry their nature structurally, because
//! the natures differ in exactly what they declare and derive:
//! [`Atom`]s are immutable once created — the only streamable kind
//! (streaming is creation-notification) — and derive Get+List+Stream.
//! [`View`]s fold atoms (stable identity, state read at a watermark,
//! never streamed directly — you stream their atoms) and derive
//! Get+List. [`Synthetic`]s stand for live machinery, not recorded
//! truth: a machinery singleton has no key and no filter, so the type
//! has neither — Get alone derives, and its verbs carry manual
//! authority.
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
    Cost,
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

/// An atom: a resource immutable once created — the only streamable
/// kind. Derives Get + List + Stream. `key_schema` addresses one atom
/// (Get); `filter_schema` is the typed, per-resource selection for
/// List and Stream — deliberately a schema'd struct, never a query
/// language; `state_schema` is the atom's immutable content.
#[derive(Debug, Clone, Serialize)]
pub struct Atom {
    pub domain: Domain,
    pub version: u32,
    /// The one-line summary, inherited by the whole derived surface
    /// (listings, MCP tool lists).
    pub summary: &'static str,
    /// The fuller contract text — anything the caller must know:
    /// retention bounds, delivery semantics. Empty when the summary
    /// says it all.
    pub description: &'static str,
    pub stability: Stability,
    pub key_schema: Schema,
    pub state_schema: Schema,
    pub filter_schema: Schema,
}

impl Atom {
    /// Declare an atom. The generic parameters are the declaration:
    /// `Key` (Get identity), `State` (the immutable content), `Filter`
    /// (List/Stream selection); their schemas are captured here, at
    /// the one declaration site.
    pub fn new<Key, State, Filter>(
        domain: Domain,
        summary: &'static str,
        stability: Stability,
    ) -> Self
    where
        Key: Serialize + DeserializeOwned + JsonSchema,
        State: Serialize + DeserializeOwned + JsonSchema,
        Filter: Serialize + DeserializeOwned + JsonSchema,
    {
        Atom {
            domain,
            version: 1,
            summary,
            description: "",
            stability,
            key_schema: schema_for!(Key),
            state_schema: schema_for!(State),
            filter_schema: schema_for!(Filter),
        }
    }

    /// The fuller contract text, when the summary doesn't say it all.
    pub fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// Schema version (P10): additive changes keep it, observable
    /// breaks bump it.
    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }
}

/// A view: a fold of atoms — stable identity, state read as of a
/// watermark, never streamed directly (you stream its atoms). Derives
/// Get + List.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub domain: Domain,
    pub version: u32,
    /// The one-line summary (listings, MCP tool lists).
    pub summary: &'static str,
    /// The fuller contract text — fold semantics, watermark caveats.
    /// Empty when the summary says it all.
    pub description: &'static str,
    pub stability: Stability,
    pub key_schema: Schema,
    pub state_schema: Schema,
    pub filter_schema: Schema,
}

impl View {
    /// Declare a view. `Key` (Get identity), `State` (the fold),
    /// `Filter` (List selection).
    pub fn new<Key, State, Filter>(
        domain: Domain,
        summary: &'static str,
        stability: Stability,
    ) -> Self
    where
        Key: Serialize + DeserializeOwned + JsonSchema,
        State: Serialize + DeserializeOwned + JsonSchema,
        Filter: Serialize + DeserializeOwned + JsonSchema,
    {
        View {
            domain,
            version: 1,
            summary,
            description: "",
            stability,
            key_schema: schema_for!(Key),
            state_schema: schema_for!(State),
            filter_schema: schema_for!(Filter),
        }
    }

    /// The fuller contract text, when the summary doesn't say it all.
    pub fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// Schema version (P10): additive changes keep it, observable
    /// breaks bump it.
    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }
}

/// A synthetic resource: stands for live machinery, not recorded
/// truth. A machinery singleton has no key and no filter, so this
/// type declares neither — Get alone derives (the machinery
/// describing itself); its verbs register as [`Command`]s with manual
/// authority.
#[derive(Debug, Clone, Serialize)]
pub struct Synthetic {
    pub domain: Domain,
    pub version: u32,
    /// The one-line summary (listings, MCP tool lists).
    pub summary: &'static str,
    /// The fuller contract text. Empty when the summary says it all.
    pub description: &'static str,
    pub stability: Stability,
    pub state_schema: Schema,
}

impl Synthetic {
    /// Declare a synthetic resource. `State` is what Get returns.
    pub fn new<State>(domain: Domain, summary: &'static str, stability: Stability) -> Self
    where
        State: Serialize + DeserializeOwned + JsonSchema,
    {
        Synthetic {
            domain,
            version: 1,
            summary,
            description: "",
            stability,
            state_schema: schema_for!(State),
        }
    }

    /// The fuller contract text, when the summary doesn't say it all.
    pub fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
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
    /// The one-line summary (listings, MCP tool lists).
    pub summary: &'static str,
    /// The fuller contract text — the semantics that make this verb
    /// bespoke: idempotency, kill-switch behaviour, delivery
    /// guarantees. Empty when the summary says it all.
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
        summary: &'static str,
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
            summary,
            description: "",
            stability,
            input_schema: schema_for!(Input),
        }
    }

    /// The fuller contract text, when the summary doesn't say it all.
    pub fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
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
/// read.
///
/// A report attaches to a [`Domain`] as its **permission scope** —
/// authority is Read on that scope, which is what makes aggregates a
/// privilege boundary: `cost.summary` is grantable without granting
/// the raw event log it computes from. The domain needn't carry a
/// catalogue resource (`Cost` carries only reports, as `Control`
/// carries the machinery); handlers read their inputs with system
/// authority regardless.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub domain: Domain,
    /// The report's name word; renders as `{domain}.{name}`. Opaque
    /// identity plus documentation — never parsed.
    pub name: &'static str,
    pub version: u32,
    /// The one-line summary (listings, MCP tool lists).
    pub summary: &'static str,
    /// The fuller contract text. Empty when the summary says it all.
    pub description: &'static str,
    pub stability: Stability,
    pub params_schema: Schema,
    pub output_schema: Schema,
}

impl Report {
    /// Declare a report. `Params` and `Output` are the declaration's
    /// types; their schemas are captured here.
    pub fn new<Params, Output>(
        domain: Domain,
        name: &'static str,
        summary: &'static str,
        stability: Stability,
    ) -> Self
    where
        Params: Serialize + DeserializeOwned + JsonSchema,
        Output: Serialize + DeserializeOwned + JsonSchema,
    {
        Report {
            domain,
            name,
            version: 1,
            summary,
            description: "",
            stability,
            params_schema: schema_for!(Params),
            output_schema: schema_for!(Output),
        }
    }

    /// The fuller contract text, when the summary doesn't say it all.
    pub fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// This report's wire identity.
    pub fn op(&self) -> OpId {
        OpId::Report {
            domain: self.domain,
            name: self.name.to_string(),
        }
    }
}
