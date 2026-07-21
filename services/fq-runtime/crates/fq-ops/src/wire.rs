//! The generic wire envelopes the tarpc edge carries (ADR-0006 D6.1,
//! amended into ADR-0031): one `invoke`/`next_batch` pair for every
//! operation, so auth, audit, versioning, and cost middleware have a
//! single choke point. Generated typed client wrappers keep end-to-end
//! static typing on top of these envelopes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::opid::OpId;

/// Reference to one atom a command appended (D3): subject, stream,
/// and the event-log sequence — which is also the universal cursor
/// (P5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventRef {
    pub subject: String,
    pub stream: String,
    pub seq: u64,
}

/// A command's output: references to the atoms it appended, never
/// state (D3, P4). Freshness is the caller's to compose — a receipt's
/// watermark feeds the next read's `min_seq` for read-your-writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Receipt {
    pub events: Vec<EventRef>,
}

impl Receipt {
    /// The highest appended sequence — what a caller passes as
    /// `min_seq` to compose read-your-writes (D4).
    pub fn watermark(&self) -> Option<u64> {
        self.events.iter().map(|e| e.seq).max()
    }
}

/// One `invoke` call: the operation as its native [`OpId`] (tarpc
/// carries enums; rendered names are documentation, not transport),
/// the schema version beside it (P10), its input as schema'd JSON,
/// and — for reads — the optional D4 watermark. `min_seq` lives on
/// the envelope, not per-op input, so every derived surface inherits
/// watermarking without per-op plumbing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InvokeRequest {
    pub op: OpId,
    pub version: u32,
    pub input: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InvokeResponse {
    pub output: serde_json::Value,
}

/// The tarpc binding of the stream overlay (D5): long-poll
/// `next_batch(from_seq, max_wait)` — push latency, zero transport
/// work, resumable by construction because sequence is the cursor.
/// Only atoms stream, and `op` must be a `Stream(_)` identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NextBatchRequest {
    pub op: OpId,
    pub version: u32,
    pub filter: serde_json::Value,
    pub from_seq: u64,
    pub max_wait_ms: u64,
}

/// One streamed atom. Every item carries its event-log sequence (D5)
/// — the single invariant that makes each transport binding
/// mechanical (SSE `id:`, tarpc long-poll, MCP notifications).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StreamItem {
    pub seq: u64,
    pub item: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireError {
    /// The identity is valid (it type-checked) but this daemon has no
    /// handler registered for it — client/daemon version skew.
    #[error(
        "operation `{op}` is not registered on this daemon — version skew? \
         List(Operation) shows what it serves"
    )]
    NotRegistered { op: String },
    #[error("input rejected by `{op}`'s schema: {message}")]
    InvalidInput { op: String, message: String },
    #[error("denied: {message}")]
    Denied { message: String },
    #[error("internal error: {message}")]
    Internal { message: String },
}
