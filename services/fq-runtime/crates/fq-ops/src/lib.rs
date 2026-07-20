//! The operation registry contract (ADR-0006) — and the sqlx-free wire
//! crate ADR-0031 calls for.
//!
//! Every operator- or system-facing capability is defined exactly once
//! as an [`Operation`]: a name that parses under the P8 grammar, a
//! CQRS kind, typed input/output with derived JSON schemas, and
//! [`OpMeta`] carrying the contract metadata (permission, audit class,
//! stability, caveats) every derived surface inherits. Surface code
//! that hand-describes an operation is, per the ADR, a defect.
//!
//! This crate holds the *contract only*: the trait, the registry with
//! its registration-time invariants, and the generic wire envelopes
//! the tarpc edge carries. Handlers, transports, and auth middleware
//! live daemon-side (execution-plan Phases 2–3) — which is exactly why
//! this crate must stay a leaf (no sqlx, no NATS, no tokio; the thin
//! `fq` client links it alone).

pub mod meta;
pub mod name;
pub mod operation;
pub mod registry;
pub mod wire;

pub use meta::{OpKind, OpMeta, OpPermission, Stability, Verb};
pub use operation::{OpDescriptor, Operation};
pub use registry::{Registry, RegistryError};
pub use wire::{
    EventRef, InvokeRequest, InvokeResponse, NextBatchRequest, Receipt, StreamBatch, StreamItem,
    WireError,
};
