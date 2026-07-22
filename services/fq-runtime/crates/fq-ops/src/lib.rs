//! The operator-surface contract crate (ADR-0006, as refined by
//! `docs/design/aspirational/operator-surface-domain-model.md`) — and
//! the sqlx-free wire crate ADR-0031 calls for.
//!
//! Four categories of boundary promise, mirroring the domain model:
//!
//! - **Resources** ([`model`]): [`Atom`], [`View`], and [`Synthetic`]
//!   are explicit value types — the nature is the type, and one
//!   constructor call derives the whole generic read surface (atoms
//!   Get + List + Stream, views Get + List, synthetics Get) with
//!   derived Read authority. The generic surface is read-only: every
//!   mutation on the whole surface is a declared command.
//! - **Domain verbs** ([`model`]): the bespoke commands whose
//!   semantics are the contract; output is always a [`Receipt`] by
//!   construction (D3). A declaration is one site — the impl carries
//!   its identity, types, authority, and contract text.
//! - **Reports**: named, typed computations over resources.
//! - **Machinery reads**: `Get` on the synthetic `Control` resource —
//!   no category of their own.
//!
//! This crate holds the *type foundation only* — the model's value
//! types, the wire identity, and the self-describing [`Registry`].
//! Handlers, transports, auth middleware, and the generic
//! invoke/next_batch envelopes live with the daemon's edge
//! (execution-plan Phase 2), designed against the real tarpc service
//! rather than speculatively here — which is exactly why this crate
//! must stay a leaf (no sqlx, no NATS, no tokio; the thin `fq` client
//! links it alone — `tests/forbidden_dependency_gate.rs` enforces it).

pub mod model;
pub mod opid;
pub mod registry;

pub use model::{
    Atom, Authority, Command, Domain, EventRef, Receipt, Report, Stability, Synthetic, Verb, View,
};
pub use opid::{OpCategory, OpId};
pub use registry::{Entry, Registry, RegistryError, Resolved};
