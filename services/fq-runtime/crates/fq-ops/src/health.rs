//! Stream and consumer health, as the operator surface declares them.
//!
//! These are the shapes alone. Producing them means talking to
//! JetStream, which is the daemon's job and stays in `fq-runtime`
//! alongside the probe — a reader that only renders health links this
//! crate and none of that.

use serde::{Deserialize, Serialize};

/// Health of one JetStream stream plus its primary durable consumer.
///
/// Externally tagged (serde's default) rather than internally: internal
/// tagging is a JSON-only representation, and the edge's session is
/// bincode once its JSON preamble is done, so an internally-tagged enum
/// would fail to encode there.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamHealth {
    /// The stream (or its info) could not be fetched; `error` carries
    /// the reason verbatim.
    Unavailable { stream: String, error: String },
    Available {
        stream: String,
        messages: u64,
        bytes: u64,
        first_seq: u64,
        last_seq: u64,
        consumer: ConsumerHealth,
    },
}

impl StreamHealth {
    /// The stream name, whichever state it is in.
    pub fn stream(&self) -> &str {
        match self {
            StreamHealth::Unavailable { stream, .. } => stream,
            StreamHealth::Available { stream, .. } => stream,
        }
    }
}

/// Health of one durable consumer on a stream. Externally tagged, for
/// the same encoding reason as [`StreamHealth`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerHealth {
    /// The durable does not exist yet (no daemon has initialised it).
    Missing { name: String },
    /// The durable exists but its info could not be fetched.
    Error { name: String, error: String },
    Active {
        name: String,
        /// Stream sequence the consumer has been delivered up to.
        delivered: u64,
        /// `last_seq - delivered` — how far behind the stream head.
        lag: u64,
        ack_pending: u64,
        num_pending: u64,
        /// Outstanding redeliveries — messages delivered more than
        /// once and not yet acked. The retry-pressure signal: a
        /// non-zero value means work is being NAK'd or timing out and
        /// walking toward the consumer's delivery bound, past which a
        /// trigger is dead-lettered rather than retried again.
        num_redelivered: u64,
    },
}
