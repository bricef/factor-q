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
//! Generic operations (Get, List, and the Stream overlay) derive from
//! a catalogue entry — one [`Resource`] impl, nature included, buys a
//! resource its whole read surface, and the generic surface is
//! read-only: creation is not a generic verb (operators command the
//! machinery; atoms appear in the log as receipts), so every mutation
//! is a declared command. Adding a resource to [`Domain`] is the P11
//! curation gate.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::meta::OpMeta;

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
    /// Contract text for the whole derived surface — declared here,
    /// like every other definition on the surface (one site), and
    /// projected into the catalogue's descriptor at
    /// registration.
    const META: OpMeta;
}
