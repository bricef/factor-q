//! JetStream health probe — the NATS-side half of the operator health
//! surface, lifted out of the CLI's `fq status` so the CLI, the read
//! service, and the dashboard all render the same typed data (#105
//! layer 2). The DB-side half lives in [`crate::views`], which
//! deliberately performs no NATS access; a health *report* composes the
//! two at the caller.

use serde::{Deserialize, Serialize};

use crate::bus::{STREAM_NAME, TRIGGER_STREAM_NAME};
use crate::control_plane::dispatcher::CONSUMER_NAME as DISPATCHER_CONSUMER;
use crate::control_plane::projection::consumer::CONSUMER_NAME as PROJECTOR_CONSUMER;

/// The runtime's core streams and their primary durable consumers — the
/// pairs `fq status` reports and the read service probes.
pub const CORE_STREAMS: [(&str, &str); 2] = [
    (STREAM_NAME, PROJECTOR_CONSUMER),
    (TRIGGER_STREAM_NAME, DISPATCHER_CONSUMER),
];

/// Health of one JetStream stream plus its primary durable consumer.
/// Externally tagged (serde's default) — internal tagging is JSON-only
/// and breaks bincode, the read service's wire format.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
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

/// Health of one durable consumer on a stream. Externally tagged, same
/// bincode rationale as [`StreamHealth`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerHealth {
    /// The durable does not exist yet (no `fq run` has initialised it).
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
    },
}

/// Probe one stream and its primary durable consumer. Never errors —
/// every failure mode is a value, so callers (the CLI, the read
/// service) render partial health rather than losing the whole report.
pub async fn probe_stream(
    js: &async_nats::jetstream::Context,
    stream_name: &str,
    primary_consumer: &str,
) -> StreamHealth {
    let mut stream = match js.get_stream(stream_name).await {
        Ok(s) => s,
        Err(err) => {
            return StreamHealth::Unavailable {
                stream: stream_name.to_string(),
                error: format!("stream not found: {err}"),
            };
        }
    };
    let info = match stream.info().await {
        Ok(i) => i.clone(),
        Err(err) => {
            return StreamHealth::Unavailable {
                stream: stream_name.to_string(),
                error: format!("failed to fetch info: {err}"),
            };
        }
    };

    let consumer = match stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(primary_consumer)
        .await
    {
        Ok(mut consumer) => match consumer.info().await {
            Ok(cinfo) => {
                let delivered = cinfo.delivered.stream_sequence;
                ConsumerHealth::Active {
                    name: primary_consumer.to_string(),
                    delivered,
                    lag: info.state.last_sequence.saturating_sub(delivered),
                    ack_pending: cinfo.num_ack_pending as u64,
                    num_pending: cinfo.num_pending,
                }
            }
            Err(err) => ConsumerHealth::Error {
                name: primary_consumer.to_string(),
                error: format!("{err}"),
            },
        },
        Err(_) => ConsumerHealth::Missing {
            name: primary_consumer.to_string(),
        },
    };

    StreamHealth::Available {
        stream: stream_name.to_string(),
        messages: info.state.messages,
        bytes: info.state.bytes,
        first_seq: info.state.first_sequence,
        last_seq: info.state.last_sequence,
        consumer,
    }
}

/// Probe every core stream ([`CORE_STREAMS`]), in order.
pub async fn probe_core_streams(js: &async_nats::jetstream::Context) -> Vec<StreamHealth> {
    let mut out = Vec::with_capacity(CORE_STREAMS.len());
    for (stream, consumer) in CORE_STREAMS {
        out.push(probe_stream(js, stream, consumer).await);
    }
    out
}
