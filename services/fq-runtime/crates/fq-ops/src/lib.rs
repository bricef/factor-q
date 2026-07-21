//! The operator-surface contract crate (ADR-0006, as refined by
//! `docs/design/aspirational/operator-surface-domain-model.md`) — and
//! the sqlx-free wire crate ADR-0031 calls for.
//!
//! Four categories of boundary promise, mirroring the domain model:
//!
//! - **Resources** ([`catalogue`]): atoms, views, and synthetic
//!   resources. One [`Resource`] impl derives a resource's whole
//!   generic read surface — Get + List, Stream for atoms ([`Atom`]) —
//!   with derived Read authority. The generic surface is read-only:
//!   every mutation on the whole surface is a declared command.
//! - **Domain verbs** ([`declared`]): the bespoke commands whose
//!   semantics are the contract; output is always a [`Receipt`] by
//!   construction (D3). A declaration is one site — the impl carries
//!   its identity, types, authority, and contract text.
//! - **Reports**: named, typed computations over resources.
//! - **Machinery reads**: `Get` on the synthetic `Control` resource —
//!   no category of their own.
//!
//! This crate holds the *contract only* — the catalogue, the declared
//! traits, the self-describing [`Registry`], and the generic wire
//! envelopes. Handlers, transports, and auth middleware live
//! daemon-side (execution-plan Phases 2–3), which is exactly why this
//! crate must stay a leaf (no sqlx, no NATS, no tokio; the thin `fq`
//! client links it alone — `tests/leaf_gate.rs` enforces it).

pub mod catalogue;
pub mod declared;
pub mod meta;
pub mod opid;
pub mod registry;
pub mod wire;

pub use catalogue::{Atom, Domain, Nature, Resource, Synthetic, View};
pub use declared::{Command, Report};
pub use meta::{Authority, OpMeta, Stability, Verb};
pub use opid::{OpCategory, OpId};
pub use registry::{OpDescriptor, Registry, RegistryError, ResourceDocs};
pub use wire::{
    EventRef, InvokeRequest, InvokeResponse, NextBatchRequest, Receipt, StreamBatch, StreamItem,
    WireError,
};
