//! The generic envelopes the edge carries — designed against the real
//! tarpc service (deferred from Phase 1 by review): one
//! `invoke`/`next_batch` pair for every operation, so auth, audit,
//! versioning, and cost middleware have a single choke point.

use fq_ops::OpId;
use serde::{Deserialize, Serialize};

/// One `invoke` call: the operation as its native [`OpId`] (rendered
/// names are documentation, not transport), the schema version beside
/// it (P10), its input as schema'd JSON, and — for reads — the
/// optional D4 watermark. `min_seq` lives on the envelope, not per-op
/// input, so every derived surface inherits watermarking without
/// per-op plumbing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeRequest {
    pub op: OpId,
    pub version: u32,
    pub input: serde_json::Value,
    pub min_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeResponse {
    pub output: serde_json::Value,
}

/// The tarpc binding of the stream overlay (D5): long-poll
/// `next_batch(from_seq, max_wait)` — push latency, zero transport
/// work, resumable by construction because sequence is the cursor.
/// `op` must resolve to a Stream operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextBatchRequest {
    pub op: OpId,
    pub version: u32,
    pub filter: serde_json::Value,
    pub from_seq: u64,
    pub max_wait_ms: u64,
}

/// One streamed atom. Every item carries its sequence (D5) — the
/// single invariant that makes each transport binding mechanical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamItem {
    pub seq: u64,
    pub item: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamBatch {
    pub items: Vec<StreamItem>,
    /// Where the next `next_batch` resumes. Always valid to pass back,
    /// even when `items` is empty (the long poll timed out).
    pub next_from_seq: u64,
}

/// The wire-level failure vocabulary. Domain failures are op outputs;
/// these are the envelope's own: registration, schema, authorisation,
/// and the daemon-side catch-all. `op` fields carry the rendered name
/// (these errors are for humans and logs).
// Externally tagged (serde's default) and no skipped fields: the
// envelopes must survive any serde transport, self-describing or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum WireError {
    /// The identity is valid (it type-checked) but this daemon has no
    /// handler registered for it — client/daemon version skew, or a
    /// category mismatch (streaming a non-stream op).
    #[error(
        "operation `{op}` is not registered on this daemon — version skew? List(Operation) shows what it serves"
    )]
    NotRegistered { op: String },
    #[error("input rejected by `{op}`: {message}")]
    InvalidInput { op: String, message: String },
    /// The token's grants do not cover the operation's required
    /// authority — the read-only-dashboard case, as an error.
    #[error("denied `{op}`: {message}")]
    Denied { op: String, message: String },
    #[error("internal error: {message}")]
    Internal { message: String },
}
