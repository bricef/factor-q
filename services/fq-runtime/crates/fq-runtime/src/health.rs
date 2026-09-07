//! JetStream health probe — the NATS-side half of the operator health
//! surface, so every consumer renders the same typed data (#105 layer
//! 2). The DB-side half lives in [`crate::views`], which deliberately
//! performs no NATS access; a health *report* composes the two at the
//! caller.
//!
//! The shapes themselves are [`fq_ops::health`] and are re-exported
//! here. Only the probing needs a JetStream connection, so a consumer
//! that renders health rather than measuring it links the leaf crate
//! and none of this.
//!
//! **Every durable, not two of them (review finding B4, #549).** The
//! probe used to name one "primary" consumer per stream, which meant
//! `control.status` reported the projector and the dispatcher while the
//! coordination, heartbeat, summary and advisory consumers could wedge
//! unseen. What each stream carries is now a list, and a consumer that
//! has stopped making progress is reported as such — by name, so an
//! operator reads which one rather than that something is wrong.

pub use fq_ops::health::{ConsumerHealth, McpServerHealth, StreamHealth, UnsupportedEvent};

use crate::bus::{
    ADVISORY_STREAM_NAME, ConsumerLedger, ConsumerRecord, ConsumerRedeliveryPolicy, STREAM_NAME,
    TRIGGER_STREAM_NAME,
};
use crate::control_plane::advisory_watch::CONSUMER_NAME as ADVISORY_CONSUMER;
use crate::control_plane::coordination_consumer::CONSUMER_NAME as COORDINATION_CONSUMER;
use crate::control_plane::dispatcher::CONSUMER_NAME as DISPATCHER_CONSUMER;
use crate::control_plane::heartbeat_consumer::CONSUMER_NAME as HEARTBEAT_CONSUMER;
use crate::control_plane::projection::consumer::CONSUMER_NAME as PROJECTOR_CONSUMER;
use crate::control_plane::summary_consumer::CONSUMER_NAME as SUMMARY_CONSUMER;

/// The durables the event stream carries in every deployment. The
/// summariser is the one that is conditional, so it is not here — see
/// [`core_streams`].
pub const EVENT_STREAM_CONSUMERS: [&str; 3] = [
    PROJECTOR_CONSUMER,
    COORDINATION_CONSUMER,
    HEARTBEAT_CONSUMER,
];

/// Which durables a daemon expects to find, per stream, in the order
/// health reports them.
///
/// `summary_enabled` is the daemon's own `[summary] model`: the
/// summariser is only a required consumer when one is configured, and a
/// daemon without one would otherwise report a permanent, unfixable
/// `Missing`.
pub fn core_streams(summary_enabled: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    let mut event_consumers = EVENT_STREAM_CONSUMERS.to_vec();
    if summary_enabled {
        event_consumers.push(SUMMARY_CONSUMER);
    }
    vec![
        (STREAM_NAME, event_consumers),
        (TRIGGER_STREAM_NAME, vec![DISPATCHER_CONSUMER]),
        (ADVISORY_STREAM_NAME, vec![ADVISORY_CONSUMER]),
    ]
}

/// Probe one stream and each durable it is expected to carry. Never
/// errors — every failure mode is a value, so a caller renders partial
/// health rather than losing the whole report.
///
/// `ledger` is the loops' own account of their parse boundary, which
/// the broker cannot give: a consumer halted on an event it cannot
/// read looks, from JetStream, like one that is merely behind.
pub async fn probe_stream(
    js: &async_nats::jetstream::Context,
    stream_name: &str,
    expected_consumers: &[&str],
    policy: ConsumerRedeliveryPolicy,
    ledger: &ConsumerLedger,
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

    let mut consumers = Vec::with_capacity(expected_consumers.len());
    for name in expected_consumers {
        consumers.push(
            probe_consumer(
                &mut stream,
                name,
                info.state.last_sequence,
                policy,
                ledger.record(name),
            )
            .await,
        );
    }

    StreamHealth::Available {
        stream: stream_name.to_string(),
        messages: info.state.messages,
        bytes: info.state.bytes,
        first_seq: info.state.first_sequence,
        last_seq: info.state.last_sequence,
        consumers,
    }
}

/// Probe one durable. `last_seq` is its stream's head, for the lag;
/// `record` is what the loop behind the durable has said about its own
/// parse boundary, and a recorded halt is reported over whatever the
/// broker's figures would have made of the consumer.
async fn probe_consumer(
    stream: &mut async_nats::jetstream::stream::Stream,
    name: &str,
    last_seq: u64,
    policy: ConsumerRedeliveryPolicy,
    record: ConsumerRecord,
) -> ConsumerHealth {
    let mut consumer = match stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(name)
        .await
    {
        Ok(consumer) => consumer,
        Err(_) => {
            return ConsumerHealth::Missing {
                name: name.to_string(),
            };
        }
    };
    let info = match consumer.info().await {
        Ok(info) => info,
        Err(err) => {
            return ConsumerHealth::Error {
                name: name.to_string(),
                error: format!("{err}"),
            };
        }
    };

    let ConsumerRecord {
        malformed_acked,
        halted_on,
    } = record;
    // The loop's own account wins over the broker's figures: a halted
    // consumer has one message delivered and unacked and a lag that
    // only grows, which the arithmetic below would call "behind" and
    // never "stuck". The loop knows it stopped, and why.
    if let Some(halted_on) = halted_on {
        return ConsumerHealth::Halted {
            name: name.to_string(),
            halted_on,
            malformed_acked,
        };
    }

    let delivered = info.delivered.stream_sequence;
    let ack_pending = info.num_ack_pending as u64;
    let num_redelivered = info.num_redelivered as u64;
    let redeliveries = redeliveries(info);
    ConsumerHealth::Active {
        name: name.to_string(),
        delivered,
        lag: last_seq.saturating_sub(delivered),
        ack_pending,
        num_pending: info.num_pending,
        num_redelivered,
        redeliveries,
        stuck: is_stuck(ack_pending, num_redelivered, redeliveries, policy),
        malformed_acked,
    }
}

/// Deliveries made past the acked floor, beyond the first delivery of
/// each still-pending message.
///
/// `delivered.consumer_sequence` counts *every* delivery, redeliveries
/// included, while `ack_floor.consumer_sequence` only moves when a
/// message is resolved. Their difference is every delivery still
/// outstanding; one per pending message is the honest first attempt, and
/// what is left over is the consumer re-reading work it cannot finish.
/// That is exactly "the delivered count climbing while the watermark is
/// frozen", readable from a single probe with no state kept between
/// calls.
///
/// **It is an upper bound, not a count, on a consumer that acks out of
/// order.** `ack_floor` is the lowest *contiguous* acked position, so
/// every ack that lands above a lagging floor stays inside the
/// difference and is counted here as though it were a redelivery. Every
/// consumer built on [`crate::control_plane::durable_consumer`] resolves
/// one message at a time, which makes the number exact for them; the
/// dispatcher acks from concurrently spawned tasks, so on it five later
/// triggers acking while an earlier one is still on its honest first
/// delivery would read as five redeliveries. That is why the verdict
/// below is gated on the server's own count as well.
fn redeliveries(info: &async_nats::jetstream::consumer::Info) -> u64 {
    info.delivered
        .consumer_sequence
        .saturating_sub(info.ack_floor.consumer_sequence)
        .saturating_sub(info.num_ack_pending as u64)
}

/// The stuck verdict: work is outstanding, the server says at least one
/// outstanding message has been delivered more than once, *and* the
/// deliveries past the acked floor exceed the daemon's
/// `[bus] stuck_after_redeliveries`.
///
/// All three matter. A consumer with nothing pending is idle, not
/// stuck, however many redeliveries it survived in the past. And
/// `num_redelivered` — the server's own count of pending messages
/// delivered more than once — is what keeps an out-of-order acker off
/// the red list: a consumer that has never redelivered anything cannot
/// be stuck retrying, whatever the arithmetic above makes of its
/// contiguous floor.
///
/// A threshold of zero would make every retry a fault, so one
/// redelivery is the floor: the first redelivery of anything is normal.
fn is_stuck(
    ack_pending: u64,
    num_redelivered: u64,
    redeliveries: u64,
    policy: ConsumerRedeliveryPolicy,
) -> bool {
    ack_pending > 0 && num_redelivered > 0 && redeliveries >= policy.stuck_after_redeliveries.max(1)
}

/// Every expected durable's health, flattened across streams — what
/// `fq doctor` reports, against the exit criterion's wording: "`fq
/// doctor` reports every consumer".
///
/// A stream that could not be read contributes its expected consumers
/// as [`ConsumerHealth::Missing`], so a broker that has lost a stream
/// reads as consumers an operator can name rather than as a silence
/// where four of them used to be. The stream-level figures behind them
/// are `control.status`'s subject and are not repeated here.
pub async fn probe_core_consumers(
    js: &async_nats::jetstream::Context,
    summary_enabled: bool,
    policy: ConsumerRedeliveryPolicy,
    ledger: &ConsumerLedger,
) -> Vec<ConsumerHealth> {
    let mut out = Vec::new();
    for (stream, expected) in core_streams(summary_enabled) {
        match probe_stream(js, stream, &expected, policy, ledger).await {
            StreamHealth::Available { consumers, .. } => out.extend(consumers),
            StreamHealth::Unavailable { .. } => {
                out.extend(expected.into_iter().map(|name| ConsumerHealth::Missing {
                    name: name.to_string(),
                }))
            }
        }
    }
    out
}

/// Probe every stream this daemon expects, in order.
pub async fn probe_core_streams(
    js: &async_nats::jetstream::Context,
    summary_enabled: bool,
    policy: ConsumerRedeliveryPolicy,
    ledger: &ConsumerLedger,
) -> Vec<StreamHealth> {
    let streams = core_streams(summary_enabled);
    let mut out = Vec::with_capacity(streams.len());
    for (stream, consumers) in streams {
        out.push(probe_stream(js, stream, &consumers, policy, ledger).await);
    }
    out
}

#[cfg(test)]
mod tests;

/// The shared MCP servers as a health surface reports them (#548).
///
/// The translation from the manager's live table to the declared wire
/// shape lives here, beside the JetStream probe, for the same reason:
/// `fq-ops` declares what health *is*, and this crate is where the
/// daemon's own state gets read into it.
pub fn mcp_server_health(states: &crate::mcp::McpServerStates) -> Vec<McpServerHealth> {
    states
        .snapshot()
        .into_iter()
        .map(|(name, state)| match state {
            crate::mcp::McpServerState::Starting => McpServerHealth::Starting { name },
            crate::mcp::McpServerState::Ready { tools } => McpServerHealth::Ready { name, tools },
            crate::mcp::McpServerState::Unavailable {
                reason,
                attempts,
                next_retry_at_ms,
            } => McpServerHealth::Unavailable {
                name,
                reason,
                attempts,
                next_retry_at_ms,
            },
        })
        .collect()
}
