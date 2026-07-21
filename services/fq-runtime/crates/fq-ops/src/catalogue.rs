//! The resource catalogue: the heart of the domain model
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
//! Generic operations (Get, List, Create, and the Stream overlay)
//! derive from a catalogue entry — one [`ResourceType`] impl buys a
//! resource its whole read surface. Adding a resource to [`ResourceId`]
//! is the P11 curation gate.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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
pub enum ResourceId {
    Agent,
    Control,
    DeadLetter,
    Event,
    Invocation,
    Operation,
    TranscriptEntry,
    Trigger,
    Worker,
}

impl ResourceId {
    /// The rendered name segment (`transcript_entry`, `dead_letter`).
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

/// One catalogue entry: the single definition from which a resource's
/// generic read surface derives. `Key` addresses one resource (Get);
/// `Filter` is the typed, per-resource selection for List and Stream —
/// deliberately a struct, never a query language; `State` is what
/// comes back (an atom's immutable content, or a view's fold).
///
/// The nature is not declared here: it is determined by how the
/// resource is registered (`register_atom` vs `register_view`), with
/// the [`AtomResource`] bound making "only atoms stream" a
/// compile-time fact rather than a review rule.
pub trait ResourceType {
    const ID: ResourceId;
    /// Schema version for this resource's wire types (P10): additive
    /// changes keep it, observable breaks bump it.
    const VERSION: u32 = 1;
    type Key: Serialize + DeserializeOwned + JsonSchema;
    type State: Serialize + DeserializeOwned + JsonSchema;
    type Filter: Serialize + DeserializeOwned + JsonSchema;
}

/// Marker: this resource is an atom — immutable once created, and
/// therefore streamable ("send me resources of type X, at or after
/// sequence S, as soon as they exist").
pub trait AtomResource: ResourceType {}

/// Marker: operators may create this resource (rare by design —
/// Trigger today, Traversal when the graph executor lands). Create is
/// per-resource opt-in where the read surface is uniform.
pub trait CreatableResource: ResourceType {
    type CreateInput: Serialize + DeserializeOwned + JsonSchema;
}
