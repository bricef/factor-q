//! The domain model, declared as values
//! (`docs/design/committed/operator-surface-domain-model.md`).
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

use std::collections::BTreeMap;

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
    // Ord so a receipt can key its watermarks by domain in a map whose
    // serialised form is stable — the ordering carries no meaning
    // beyond that.
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
    strum::IntoStaticStr,
    strum::EnumString,
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    JsonSchema,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Verb {
    Read,
    Write,
    Delete,
    List,
    Grant,
}

impl Verb {
    /// The rendered verb segment — one source of truth for the authz
    /// wire word, kept equal to the serde encoding by
    /// `tests/verb_encoding.rs`.
    pub fn segment(&self) -> &'static str {
        self.into()
    }
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

/// Reference to one atom a command appended (D3): the domain it
/// belongs to, and **its identity** — exactly the key that domain's
/// Get takes, so a receipt reads back as "here is what I appended, and
/// here is how to fetch it".
///
/// It used to be the atom's sequence, and the doc that accompanied it
/// claimed bus coordinates were "never exposed in a receipt" while
/// exposing one: a JetStream sequence is a position in a log, not a
/// name for a thing. A caller who stored it and re-presented it later
/// could be answered with a different atom — the log is recreatable
/// and sequences restart at 1, while the index that outlives it does
/// not.
///
/// The rule this settles, and which the rest of the surface follows:
/// **cursors may be transport coordinates; identities may not.** The
/// cursor did not disappear — it moved to [`Receipt::watermarks`],
/// where being a log position is the point rather than an accident.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AtomRef {
    pub domain: Domain,
    /// The atom's key, shaped by its domain's declared `key_schema` —
    /// hand it to `<domain>.get` unchanged.
    pub key: serde_json::Value,
}

/// A command's output: references to the atoms it appended, never
/// state (D3, P4). Freshness is the caller's to compose — a receipt's
/// watermark feeds the next read's `min_seq` for read-your-writes.
///
/// The two fields answer two different questions, which is why they
/// are no longer the same field: `atoms` says *what was written* and
/// is addressed by identity; `watermarks` says *how far the log got*
/// and is addressed by position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Receipt {
    pub atoms: Vec<AtomRef>,
    /// Per-domain high-water marks: the highest sequence this command
    /// appended in each domain it touched. Per-domain because
    /// sequences from different domains are not comparable, and a
    /// `BTreeMap` rather than a list because a caller looks one up by
    /// domain and the ordering keeps the serialised form stable.
    #[serde(default)]
    pub watermarks: BTreeMap<Domain, u64>,
}

impl Receipt {
    /// The highest appended sequence in one domain — what a caller
    /// passes as `min_seq` to watermark a read of that domain (D4).
    ///
    /// This was derived from the atoms by taking a maximum, which
    /// only worked while every atom carried a position. It is now
    /// recorded directly, so a command can report a watermark for a
    /// domain whose atoms it names by identity — or, for that matter,
    /// for a domain whose atoms it does not enumerate at all.
    pub fn watermark(&self, domain: Domain) -> Option<u64> {
        self.watermarks.get(&domain).copied()
    }

    /// A receipt for a command that appended exactly one atom — the
    /// common case, and the one every command in the migration's
    /// remaining cohorts wants. Keeping it a constructor means the
    /// identity and the watermark cannot be filled in inconsistently
    /// at each call site.
    pub fn one(domain: Domain, key: serde_json::Value, seq: u64) -> Self {
        Receipt {
            atoms: vec![AtomRef { domain, key }],
            watermarks: [(domain, seq)].into_iter().collect(),
        }
    }

    /// A receipt for a command that appended nothing. It did
    /// something — a command always does — but there is no atom to
    /// point a caller at and no position to wait for.
    pub fn empty() -> Self {
        Receipt {
            atoms: Vec::new(),
            watermarks: BTreeMap::new(),
        }
    }
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
    /// The shape of one List row. **Defaults to the state** — an atom
    /// is a fact, and listing facts hands back facts, which is what
    /// `turn.list` does and must keep doing.
    ///
    /// An atom whose List answers from a cheaper store than its Get
    /// declares this separately ([`Atom::with_index`]): the index row
    /// is then a different shape from the state, and the declaration
    /// says so rather than leaving a caller to discover it from the
    /// payload. Stream always answers with the state — a stream is
    /// creation-notification, and half a fact is not a notification of
    /// it.
    pub index_schema: Schema,
    pub filter_schema: Schema,
}

impl Atom {
    /// Declare an atom whose List answers with the atoms themselves.
    /// The generic parameters are the declaration: `Key` (Get
    /// identity), `State` (the immutable content), `Filter`
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
        Self::with_index::<Key, State, State, Filter>(domain, summary, stability)
    }

    /// Declare an atom whose List answers with an **index** row rather
    /// than the atom — `Index` is that row's shape.
    ///
    /// The opt-in exists because an atom's List may legitimately be
    /// served from a different store than its Get: an index that
    /// carries extracted fields answers "what happened recently"
    /// cheaply, where a scan of the substrate would not. The rule that
    /// makes it safe is a contract on `Index`, not on this
    /// constructor — **an index row must carry the identity `Get`
    /// takes**, so a caller can walk from any row to the whole atom —
    /// and the declaration's `description` must say so, because that
    /// text is what the surface publishes about itself.
    ///
    /// A separate constructor rather than a fourth type parameter on
    /// [`Atom::new`]: the default is not "some index" but *the state
    /// itself*, and spelling that as `Atom::new::<K, S, S, F>` at every
    /// existing site would state the default by repetition — noise that
    /// a transposition could silently get wrong.
    pub fn with_index<Key, State, Index, Filter>(
        domain: Domain,
        summary: &'static str,
        stability: Stability,
    ) -> Self
    where
        Key: Serialize + DeserializeOwned + JsonSchema,
        State: Serialize + DeserializeOwned + JsonSchema,
        Index: Serialize + DeserializeOwned + JsonSchema,
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
            index_schema: schema_for!(Index),
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
    /// The shape of one List item. A view's List returns its
    /// **index** — one row per fold, cheap to enumerate — not N full
    /// folds: Get answers with the state, List answers with index
    /// rows, and the two shapes are declared separately because they
    /// genuinely differ (the dashboard's list pages are exactly the
    /// index).
    pub index_schema: Schema,
    pub filter_schema: Schema,
}

impl View {
    /// Declare a view. `Key` (Get identity), `State` (the fold, what
    /// Get returns), `Index` (one List row — the view's index),
    /// `Filter` (List selection).
    pub fn new<Key, State, Index, Filter>(
        domain: Domain,
        summary: &'static str,
        stability: Stability,
    ) -> Self
    where
        Key: Serialize + DeserializeOwned + JsonSchema,
        State: Serialize + DeserializeOwned + JsonSchema,
        Index: Serialize + DeserializeOwned + JsonSchema,
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
            index_schema: schema_for!(Index),
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
/// always a [`Receipt`]: commands return references to
/// the atoms they appended, never state (D3) — there is no output to
/// declare, so the rule cannot be broken. Authority is declared, not
/// derived: the semantics that make a verb bespoke are exactly what
/// generic derivation would get wrong.
#[derive(Debug, Clone, Serialize)]
pub struct Command {
    pub domain: Domain,
    /// The verb's typed identity; renders as `{domain}.{verb}`. The
    /// domain field above is derived from it at construction — one
    /// site, no drift. Serialises as the bare verb word so describe
    /// output stays flat.
    #[serde(serialize_with = "serialize_verb_word")]
    pub verb: crate::opid::VerbId,
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
    /// type the handler when Phase 2 binds one. The verb arrives as
    /// its typed identity (`Invocation::Drop`, …) — the domain is
    /// derived from it, so a command cannot be declared under the
    /// wrong domain.
    pub fn new<Input>(
        verb: impl Into<crate::opid::VerbId>,
        authority: Authority,
        summary: &'static str,
        stability: Stability,
    ) -> Self
    where
        Input: Serialize + DeserializeOwned + JsonSchema,
    {
        let verb = verb.into();
        let domain = verb
            .domain()
            .expect("declarations use typed verb identities, never Unknown");
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
        OpId::Verb(self.verb.clone())
    }
}

/// Serialise a typed verb id as its bare word (`"drop"`), keeping the
/// declaration's describe output flat alongside its `domain` field.
fn serialize_verb_word<S: serde::Serializer>(
    verb: &crate::opid::VerbId,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(verb.verb_segment())
}

/// Serialise a typed report id as its bare name word, same rationale.
fn serialize_report_word<S: serde::Serializer>(
    name: &crate::opid::ReportId,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(name.name_segment())
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
    /// The report's typed identity; renders as `{domain}.{name}`. The
    /// domain field above is derived from it at construction.
    /// Serialises as the bare name word so describe output stays flat.
    #[serde(serialize_with = "serialize_report_word")]
    pub name: crate::opid::ReportId,
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
        name: impl Into<crate::opid::ReportId>,
        summary: &'static str,
        stability: Stability,
    ) -> Self
    where
        Params: Serialize + DeserializeOwned + JsonSchema,
        Output: Serialize + DeserializeOwned + JsonSchema,
    {
        let name = name.into();
        let domain = name
            .domain()
            .expect("declarations use typed report identities, never Unknown");
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
        OpId::Report(self.name.clone())
    }
}
