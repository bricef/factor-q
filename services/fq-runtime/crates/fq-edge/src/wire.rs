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
/// these nine are the envelope's own: registration
/// ([`NotRegistered`](WireError::NotRegistered)), input schema
/// ([`InvalidInput`](WireError::InvalidInput)), the three ways a
/// well-formed request finds nothing whole
/// ([`NotFound`](WireError::NotFound),
/// [`Unlocatable`](WireError::Unlocatable), [`Gone`](WireError::Gone) —
/// each variant's doc says why they are not one error), idempotence
/// ([`Conflict`](WireError::Conflict)), authorisation
/// ([`Denied`](WireError::Denied)), read freshness
/// ([`Lagging`](WireError::Lagging)), and the daemon-side catch-all
/// ([`Internal`](WireError::Internal)). `op` fields carry the rendered
/// name (these errors are for humans and logs).
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
    /// A well-formed Get whose key names nothing — a normal outcome,
    /// distinct from invalid input: the request was fine, the entity
    /// isn't there.
    #[error("`{op}`: {message}")]
    NotFound { op: String, message: String },
    /// The entity is indexed, but where its content lives was never
    /// recorded — the row is here, the locator that reads the whole
    /// fact back is not.
    ///
    /// Deliberately not a `NotFound`: the entity exists, and
    /// answering "no such thing" would be a lie about the world. A
    /// caller that cannot tell these apart cannot tell "I asked
    /// wrongly" from "the fact is no longer whole".
    #[error("`{op}`: {message}")]
    Unlocatable { op: String, message: String },
    /// The content's location is known and the store no longer holds
    /// it — retention has passed the payload, or the log that held it
    /// was replaced under an index that outlived it.
    ///
    /// Also not a `NotFound`, and for a sharper reason: retention
    /// policies differ per store, so a row kept indefinitely
    /// (factor-q keeps cost-bearing rows forever) routinely outlives
    /// the log its position pointed into. `Gone` is then a normal
    /// answer about an old fact, not a fault.
    #[error("`{op}`: {message}")]
    Gone { op: String, message: String },
    /// The request asked for something that has already happened. The
    /// operation is idempotent on a key, that key has been used, and
    /// this is the refusal that keeps the guarantee.
    ///
    /// Not `InvalidInput`, which says "you asked wrongly" and invites
    /// an edit — there is no edit here, because the input was right
    /// and the work is done. Not a silent success either: an operator
    /// who asks twice is usually unsure whether the first attempt
    /// landed, and the useful answer names what it produced. **The
    /// message must carry that name**, so the second call is not a
    /// dead end but a redirection to the atom the first one made.
    #[error("`{op}`: {message}")]
    Conflict { op: String, message: String },
    /// The token's grants do not cover the operation's required
    /// authority — the read-only-dashboard case, as an error.
    #[error("denied `{op}`: {message}")]
    Denied { op: String, message: String },
    /// A read gated at `min_seq` timed out before the projection's
    /// fold reached it — retryable: the daemon is alive but behind.
    /// Carries both positions so the caller can decide (retry, widen
    /// the bound, or read ungated and accept staleness).
    #[error("`{op}` lagging: wanted at least sequence {wanted}, applied {applied}")]
    Lagging {
        op: String,
        wanted: u64,
        applied: u64,
    },
    #[error("internal error: {message}")]
    Internal { message: String },
}
