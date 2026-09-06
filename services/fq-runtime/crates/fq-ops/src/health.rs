//! Stream and consumer health, as the operator surface declares them.
//!
//! These are the shapes alone. Producing them means talking to
//! JetStream, which is the daemon's job and stays in `fq-runtime`
//! alongside the probe — a reader that only renders health links this
//! crate and none of that.

use serde::{Deserialize, Serialize};

/// Health of one JetStream stream plus its primary durable consumer.
///
/// Externally tagged (serde's default). The edge does not force the
/// choice: its token preamble is raw length-prefixed bytes and the
/// tarpc session that follows is length-delimited JSON end to end
/// (`fq_edge::server`, `fq_edge::client`), so an internally-tagged
/// enum would encode there too. What pins the representation is that
/// it is *declared* — the schema `describe` publishes for this type
/// spells the tagging out, so changing it is a wire break, not a
/// stylistic edit.
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
        /// Every durable this daemon expects on the stream, in a fixed
        /// order. It was one — the stream's "primary" consumer — which
        /// meant `control.status` reported two of the runtime's six
        /// durables and a wedged coordination or summary consumer was
        /// invisible to every health surface (review finding B4).
        /// A consumer the daemon does not expect is not listed: the
        /// summariser only exists when `[summary]` names a model, and
        /// reporting it missing otherwise would be a permanent false
        /// red.
        consumers: Vec<ConsumerHealth>,
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

    /// Every consumer on this stream that an operator should act on.
    /// An unavailable stream reports none of its own — the stream line
    /// is the finding there.
    pub fn faulty_consumers(&self) -> impl Iterator<Item = &ConsumerHealth> {
        match self {
            StreamHealth::Unavailable { .. } => [].iter(),
            StreamHealth::Available { consumers, .. } => consumers.as_slice().iter(),
        }
        .filter(|c| c.is_fault())
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
        /// Deliveries this consumer has made past its acked floor,
        /// beyond the first delivery of each still-pending message —
        /// the "delivered count climbing while the watermark is frozen"
        /// that a NAK loop looks like from outside. Zero on a healthy
        /// consumer, however far behind it is.
        redeliveries: u64,
        /// True once `redeliveries` has passed the daemon's
        /// `[bus] stuck_after_redeliveries`: the consumer is retrying
        /// one message rather than making progress, and the fault
        /// behind it has outlasted the whole escalation. The verdict
        /// is computed daemon-side because the threshold is the
        /// daemon's configuration — a reader that judged for itself
        /// would be quoting a number it does not have.
        stuck: bool,
    },
}

impl ConsumerHealth {
    /// The durable's name, whichever state it is in. Every health
    /// surface names the consumer it is talking about — a red line that
    /// does not say which consumer is a red line an operator cannot act
    /// on.
    pub fn name(&self) -> &str {
        match self {
            ConsumerHealth::Missing { name }
            | ConsumerHealth::Error { name, .. }
            | ConsumerHealth::Active { name, .. } => name,
        }
    }

    /// True when this consumer is something to act on: absent from a
    /// daemon that expects it, unreadable, or stuck redelivering.
    /// Lag alone is not a fault — a consumer catching up is working.
    pub fn is_fault(&self) -> bool {
        match self {
            ConsumerHealth::Missing { .. } | ConsumerHealth::Error { .. } => true,
            ConsumerHealth::Active { stuck, .. } => *stuck,
        }
    }
}

/// Health of one shared MCP server, as every operator surface reads
/// it. Externally tagged, for the same encoding reason as
/// [`StreamHealth`].
///
/// Only *shared* servers appear. A grant-bearing server runs
/// per-invocation (ADR-0018), so "unavailable" about one would be a
/// verdict on a single run reported as a standing fact about the
/// daemon.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpServerHealth {
    /// The handshake is in flight. Transient, and visible only if a
    /// report lands during boot or during a retry attempt.
    Starting { name: String },
    /// Connected, with its tools registered.
    Ready {
        name: String,
        /// How many tools it advertised, the host-synthesized resource
        /// tools included.
        tools: u32,
    },
    /// The server did not start, or its discovery was refused. Its
    /// tools are absent and any agent declaring it is refused at
    /// dispatch with this same reason.
    Unavailable {
        name: String,
        /// Why, verbatim — a deadline, a cap, a spawn failure. The
        /// message names the `[mcp]` key behind any bound it hit, so
        /// the line says what to change.
        reason: String,
        /// How many times it has been dialled, the boot attempt
        /// included.
        attempts: u32,
        /// When the next retry is due, epoch milliseconds. `None` when
        /// `[mcp] retry_initial_secs` is 0, which is the one
        /// configuration where unavailable means until restart.
        next_retry_at_ms: Option<i64>,
    },
}

impl McpServerHealth {
    /// The server's declared name, whichever state it is in.
    pub fn name(&self) -> &str {
        match self {
            McpServerHealth::Starting { name }
            | McpServerHealth::Ready { name, .. }
            | McpServerHealth::Unavailable { name, .. } => name,
        }
    }

    /// True when this server is something to act on. `Starting` is not:
    /// it is bounded by the start-up deadline and resolves either way
    /// within it, so reporting it as a fault would make every boot look
    /// unhealthy for a moment.
    pub fn is_fault(&self) -> bool {
        matches!(self, McpServerHealth::Unavailable { .. })
    }
}
