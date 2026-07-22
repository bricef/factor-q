//! The authenticated generic edge (ADR-0031, as amended by its
//! Appendix A): one tarpc service — `invoke`/`next_batch` — carrying
//! every registry operation, born authenticated.
//!
//! Transport: TLS terminated by the daemon with a self-signed
//! certificate the client **pins** by fingerprint, then a bearer
//! **biscuit capability token** presented at connection establishment
//! (beneath the RPC contract). The token carries `(verb, domain)`
//! grants and a principal; per request, the edge resolves the
//! operation against the registry and subset-checks its required
//! authority against the token's grants — the declared authority
//! model made enforceable.
//!
//! This crate is the transport half of the wire crate (`fq-ops` is
//! the transport-free contract half): envelopes, the service trait,
//! identity provisioning, the server, and the pinned client.

pub mod auth;
pub mod client;
pub mod registry;
pub mod server;
pub mod service;
pub mod testing;
pub mod wire;

pub use auth::EdgeIdentity;
pub use client::EdgeClient;
pub use registry::EdgeRegistry;
pub use server::bind;
pub use wire::{
    InvokeRequest, InvokeResponse, NextBatchRequest, StreamBatch, StreamItem, WireError,
};
