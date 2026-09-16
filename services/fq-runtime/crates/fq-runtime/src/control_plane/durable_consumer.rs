//! The shared durable-consumer loop for control-plane event
//! consumers (#192).
//!
//! Every control-plane consumer has the same lifecycle: create
//! (or re-attach to) a durable JetStream consumer, loop on
//! `select!` with biased shutdown, admit each message as a value
//! the handler can take, dispatch to that handler, and ACK or NAK
//! by error class. Before this module that loop was copy-pasted
//! per consumer and the copies drifted in small ways; now the
//! loop — and with it the ack policy — lives in exactly one
//! place.
//!
//! **This is the way to add a control-plane consumer.** Give it
//! a durable name, a subject filter, and a handler; do not
//! hand-roll the `select!`/ack plumbing:
//!
//! ```ignore
//! let config = DurableConsumerConfig {
//!     durable_name: "fq-mything".to_string(),
//!     filter_subjects: vec!["fq.agent.*.mything".to_string()],
//!     deliver_from: DeliverFrom::Beginning,
//!     strict_order: false,
//!     ack_wait: None,
//! };
//! run_durable_consumer(&bus, config, shutdown, |delivery| async move {
//!     handle(&delivery.event).await.map_err(HandlerError::transient)
//! })
//! .await?;
//! ```
//!
//! Ack policy — the decisions this module centralises:
//!
//! - **Malformed message** → logged, counted on the bus's
//!   [`crate::bus::ConsumerLedger`], and ACK'd. Bytes that are not an
//!   event in any version will never decode on retry; leaving them
//!   un-acked would just create a redelivery loop.
//! - **Unsupported schema version** → the loop **halts**. The message
//!   is left unacked, the halt is recorded on the ledger with the
//!   version found, the versions this build reads, the event id and
//!   the subject, and the task parks until shutdown so the daemon
//!   stays up and `fq doctor` can say so. Acking would drop readable
//!   history from every projection built from the stream — the
//!   silent-loss path of
//!   <https://github.com/bricef/factor-q/issues/409>; NAKing would
//!   retry a message this build can never parse. The version is read
//!   before the shape ([`Event::from_wire`]), so an older envelope
//!   whose body happens to parse against the current types halts the
//!   loop too. Every *event-stream* consumer on this loop halts,
//!   not only the projector: the version is a fact about the
//!   stream, and every reader of it is behind the same binary.
//!   [`admit`] is that decision as a value.
//! - **Handler `Ok`** → ACK'd.
//! - **[`HandlerError::Transient`]** → NAK'd with an escalating
//!   delay, and logged at a bounded rate. The delay comes from the
//!   bus's [`crate::bus::ConsumerRedeliveryPolicy`] — 1s doubling to
//!   a 60s cap at the defaults — keyed on the message's delivery
//!   count. A bare `Nak(None)` redelivers immediately, which on
//!   durables with unlimited redelivery turned a persistent transient
//!   fault (a full disk under the projection) into a hot loop at
//!   broker round-trip speed with a frozen watermark behind it and
//!   nothing in `control.status` saying so (review finding B4). The
//!   delivery bound is *not* the answer to that: dropping the event
//!   would skip it from the projection for good.
//! - **[`HandlerError::Permanent`]** → logged and ACK'd. The
//!   event can never be handled (malformed for this consumer's
//!   purpose); redelivery would only repeat the failure.
//! - **Stream read error** → logged; the loop continues.
//! - **Stream end** → logged; the loop exits.
//!
//! That table is the *whole* policy, but the first two rows of it
//! are reachable only through an admission that can produce them.
//! The loop is generic over an admission —
//! `Fn(&Message) -> Admission<T>` — which turns a delivery into
//! whatever the handler takes. [`admit`] is the event-stream
//! admission: it reads the envelope version, deserialises an
//! [`Event`], and is the only admission that yields `Halt` or
//! `AckMalformed`. The maintenance consumer
//! ([`crate::control_plane::maintenance`]) rides the same loop
//! from the `fq-maintenance` stream with an identity admission
//! that yields only `Accept` — its messages are opaque scheduler
//! bodies with no envelope to read — so that consumer can never
//! halt and never counts a malformed message, by construction
//! rather than by convention
//! (<https://github.com/bricef/factor-q/issues/669> is unaffected).
//! The handler-verdict rows — `Ok`, `Transient`, `Permanent` —
//! apply to every consumer alike.
//!
//! Delivery is at-least-once, so handlers MUST be idempotent
//! under redelivery. Every current handler is: upserts by
//! primary key, `ON CONFLICT DO NOTHING` inserts, last-write-
//! wins projections.
//!
//! A consumer that also needs periodic housekeeping multiplexed
//! into the same task (the coordination consumer's stale-worker
//! sweep) uses [`run_durable_consumer_with_tick`]; the tick and
//! the handler are serialised on one task, never concurrent.

use std::future::Future;
use std::time::Duration;

use fq_ops::health::UnsupportedEvent;
use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use crate::bus::{BusError, ConsumerLedger, ConsumerRedeliveryPolicy, EventBus, RedeliveryLog};
use crate::events::{Event, EventParseError};

/// Where a durable consumer starts reading when it is *first
/// created*. `get_or_create` semantics apply: an existing
/// durable keeps its acked position and ignores this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverFrom {
    /// The start of the stream (the production default): first
    /// creation replays the stream's history, so a consumer
    /// deployed after events were published still sees them.
    Beginning,
    /// New messages only. Test-oriented: a fresh per-test
    /// durable on a shared stream skips the accumulated history
    /// instead of churning through it. Pair with a unique
    /// durable name, or an existing durable's position wins.
    New,
    /// A stream position: the durable's first delivery is the message
    /// at this sequence, or the first one after it that the stream
    /// still holds. The projection replay after a rebuild starts here
    /// — at the replay floor, the first sequence whose envelope
    /// version this build reads — rather than at the beginning, so a
    /// stream still holding older history is replayed from the point
    /// this build can read and the rows below it are kept as they
    /// were (<https://github.com/bricef/factor-q/issues/648>).
    /// Whole-stream consumers only.
    Sequence(u64),
}

impl DeliverFrom {
    /// Start at `floor`, reading sequence 0 as the beginning. JetStream
    /// numbers messages from 1; 0 is what an empty stream reports as
    /// its first sequence, and `ByStartSequence` refuses it, so a floor
    /// of 0 means "nothing to skip" and is spelled as such.
    pub fn from_floor(floor: u64) -> Self {
        if floor == 0 {
            Self::Beginning
        } else {
            Self::Sequence(floor)
        }
    }

    /// The JetStream policy this start maps to.
    fn policy(self) -> async_nats::jetstream::consumer::DeliverPolicy {
        use async_nats::jetstream::consumer::DeliverPolicy;
        match self {
            Self::Beginning => DeliverPolicy::All,
            Self::New => DeliverPolicy::New,
            Self::Sequence(start_sequence) => DeliverPolicy::ByStartSequence { start_sequence },
        }
    }
}

/// Configuration for one durable consumer on the factor-q event
/// stream: what it is called, what it sees, and where it starts.
#[derive(Debug, Clone)]
pub struct DurableConsumerConfig {
    /// Durable JetStream consumer name. Also the `consumer`
    /// field on the loop's log lines.
    pub durable_name: String,
    /// Subject filters: empty means the whole event stream; one
    /// or many narrow the durable to the subjects the handler
    /// acts on.
    pub filter_subjects: Vec<String>,
    /// Where a newly created durable starts reading.
    pub deliver_from: DeliverFrom,
    /// Resolved-contiguous delivery (`max_ack_pending = 1`): the
    /// server never delivers past an unresolved message, so after a
    /// NAK the next delivery is the retry of the same message.
    /// Required by handlers whose progress mark must never expose a
    /// sequence while an earlier one is still pending redelivery
    /// (the projection and coordination watermarks). Whole-stream,
    /// from-beginning consumers only: a filtered mark cannot vouch
    /// for the gaps between its matches. Costs throughput — one
    /// outstanding message per server round-trip.
    pub strict_order: bool,
    /// Ack window for this durable, when the bus-wide `[bus]
    /// ack_wait_ms` is too short for what this handler does.
    ///
    /// `None` — every consumer but the summariser — takes the bus
    /// default, which is sized for a handler that writes to SQLite and
    /// returns. The summariser's handler makes an LLM call inline and
    /// runs under the worker's response budget, so a summary slower
    /// than the default window would be redelivered *behind the one
    /// still generating it* and then paid for twice (#611 review).
    pub ack_wait: Option<Duration>,
}

impl DurableConsumerConfig {
    /// Create (or re-attach to) the durable via the bus factory
    /// matching this config.
    async fn create(
        &self,
        bus: &EventBus,
    ) -> Result<async_nats::jetstream::consumer::PullConsumer, BusError> {
        if self.strict_order {
            // Strict order refuses filters and from-new starts, for
            // the same reason: a progress mark must vouch for EVERY
            // sequence at or below it. A filtered consumer never sees
            // the gaps between its matches, so its mark cannot speak
            // for them (a reader gated at an unmatched sequence would
            // wait forever); a from-new start skips history outright.
            // A start at a sequence is allowed: the sequences below a
            // replay floor are not skipped from the projection, they
            // are the part of it the replay cannot re-derive and the
            // file already holds.
            if !self.filter_subjects.is_empty() || self.deliver_from == DeliverFrom::New {
                return Err(BusError::Stream(format!(
                    "strict_order requires a whole-stream consumer that starts at the \
                     beginning or at a sequence (consumer `{}` has filters or a from-new \
                     start)",
                    self.durable_name
                )));
            }
            return bus
                .durable_consumer_strict_from(
                    &self.durable_name,
                    self.deliver_from.policy(),
                    self.ack_wait,
                )
                .await;
        }
        match self.deliver_from {
            DeliverFrom::Sequence(_) => match self.filter_subjects.as_slice() {
                [] => {
                    bus.durable_consumer_from(
                        &self.durable_name,
                        self.deliver_from.policy(),
                        self.ack_wait,
                    )
                    .await
                }
                // No consumer starts filtered at a sequence, and the
                // bus has no factory for one; a config that asks is a
                // mistake to name rather than a shape to guess at.
                _ => Err(BusError::Stream(format!(
                    "a start at a sequence needs a whole-stream consumer (consumer `{}` \
                     has subject filters)",
                    self.durable_name
                ))),
            },
            DeliverFrom::Beginning => match self.filter_subjects.as_slice() {
                [] => {
                    bus.durable_consumer(&self.durable_name, self.ack_wait)
                        .await
                }
                [filter] => {
                    bus.durable_consumer_with_filter(&self.durable_name, filter, self.ack_wait)
                        .await
                }
                filters => {
                    let refs: Vec<&str> = filters.iter().map(|s| s.as_str()).collect();
                    bus.durable_consumer_with_filters(&self.durable_name, &refs, self.ack_wait)
                        .await
                }
            },
            DeliverFrom::New => match self.filter_subjects.as_slice() {
                [filter] => {
                    bus.durable_consumer_with_filter_from_new(
                        &self.durable_name,
                        filter,
                        self.ack_wait,
                    )
                    .await
                }
                filters => {
                    bus.durable_consumer_with_filters_from_new(
                        &self.durable_name,
                        filters,
                        self.ack_wait,
                    )
                    .await
                }
            },
        }
    }
}

/// How the loop treats a handler failure. The variant IS the ack
/// decision — the transient/permanent split lives here, not in
/// individual consumers.
#[derive(Debug)]
pub enum HandlerError {
    /// Retryable (a transient store or publish failure): the
    /// message is NAK'd and JetStream redelivers it after the
    /// ack deadline.
    Transient(Box<dyn std::error::Error + Send + Sync>),
    /// Not retryable: logged and ACK'd so the event is never
    /// redelivered.
    Permanent(Box<dyn std::error::Error + Send + Sync>),
}

impl HandlerError {
    /// A retryable failure — the message is NAK'd for
    /// redelivery.
    pub fn transient(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Transient(err.into())
    }

    /// A terminal failure — the message is ACK'd, never
    /// retried.
    pub fn permanent(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Permanent(err.into())
    }
}

/// Errors that prevent the loop from starting or attaching to
/// the stream. Per-message failures never surface here — they
/// are acked or NAK'd inside the loop (see the module doc).
#[derive(Debug, thiserror::Error)]
pub enum DurableConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("jetstream message stream error: {0}")]
    Stream(String),
}

/// One delivered message plus the JetStream metadata shared handlers need.
///
/// The field remains named `event` so existing event-stream handlers keep their
/// source-compatible `delivery.event` access while the loop can also carry an
/// opaque non-event value.
pub struct Delivery<T = Event> {
    pub event: T,
    /// The source stream sequence. Absent only when metadata was unavailable.
    pub stream_seq: Option<u64>,
    /// JetStream's delivery count (the first delivery is one).
    pub delivered: u64,
    /// The subject the message was delivered on.
    pub subject: String,
}

/// A stable value used to identify a generic delivery in handler log lines.
pub trait DeliveryIdent {
    /// Event id for event-stream items; non-event streams return `None` and
    /// are identified by the subject and stream sequence already on the log.
    fn delivery_ident(&self) -> Option<String>;
}

impl DeliveryIdent for Event {
    fn delivery_ident(&self) -> Option<String> {
        Some(self.envelope.event_id.to_string())
    }
}

/// Where the shared loop gets its pull consumer.
pub enum ConsumerSource {
    EventStream(DurableConsumerConfig),
    Prebuilt {
        name: String,
        consumer: Box<async_nats::jetstream::consumer::PullConsumer>,
    },
}

/// Run a durable consumer loop until `shutdown` fires.
///
/// `handler` is called once per delivered message with the
/// [`Delivery`] (the deserialised [`Event`] plus its stream
/// position); its result decides the ack (see the module doc's
/// policy table). Handlers must be idempotent — delivery is
/// at-least-once.
pub async fn run_durable_consumer<H, HFut>(
    bus: &EventBus,
    config: DurableConsumerConfig,
    shutdown: oneshot::Receiver<()>,
    handler: H,
) -> Result<(), DurableConsumerError>
where
    H: Fn(Delivery) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
{
    run_loop(
        bus,
        ConsumerSource::EventStream(config),
        shutdown,
        handler,
        NO_TICK,
        admit_event,
    )
    .await
}

/// Like [`run_durable_consumer`], with a periodic housekeeping
/// tick multiplexed into the same task. The tick and the
/// handler are serialised — they never run concurrently — and,
/// per `tokio::time::interval` semantics, the first tick fires
/// as soon as the loop starts.
pub async fn run_durable_consumer_with_tick<H, HFut, Tick, TFut>(
    bus: &EventBus,
    config: DurableConsumerConfig,
    shutdown: oneshot::Receiver<()>,
    handler: H,
    tick_every: Duration,
    tick: Tick,
) -> Result<(), DurableConsumerError>
where
    H: Fn(Delivery) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
    Tick: Fn() -> TFut,
    TFut: Future<Output = ()>,
{
    run_loop(
        bus,
        ConsumerSource::EventStream(config),
        shutdown,
        handler,
        Some((tick_every, tick)),
        admit_event,
    )
    .await
}

/// The event-stream admission as the loop takes it: [`admit`] against the
/// delivery's bytes, subject and position, unboxed for the handler. Both
/// event entry points pass this and nothing else, so "event consumer"
/// means exactly "this admission".
fn admit_event(msg: &async_nats::jetstream::Message) -> Admission<Event> {
    let stream_seq = msg.info().ok().map(|info| info.stream_sequence);
    match admit(&msg.payload, &msg.subject, stream_seq) {
        Admission::Accept(event) => Admission::Accept(*event),
        Admission::AckMalformed(err) => Admission::AckMalformed(err),
        Admission::Halt(on) => Admission::Halt(on),
    }
}

type NoTickFn = fn() -> std::future::Ready<()>;
const NO_TICK: Option<(Duration, NoTickFn)> = None;

/// The one durable-consumer loop used by event and maintenance consumers.
pub(crate) async fn run_loop<Item, H, HFut, Tick, TFut, A>(
    bus: &EventBus,
    source: ConsumerSource,
    mut shutdown: oneshot::Receiver<()>,
    handler: H,
    tick: Option<(Duration, Tick)>,
    admission: A,
) -> Result<(), DurableConsumerError>
where
    Item: DeliveryIdent,
    H: Fn(Delivery<Item>) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
    Tick: Fn() -> TFut,
    TFut: Future<Output = ()>,
    A: Fn(&async_nats::jetstream::Message) -> Admission<Item>,
{
    let (name, consumer) = match source {
        ConsumerSource::EventStream(config) => {
            let name = config.durable_name.clone();
            info!(
                consumer = %name,
                filters = ?config.filter_subjects,
                deliver_from = ?config.deliver_from,
                "durable consumer starting"
            );
            let consumer = Box::new(config.create(bus).await?);
            (name, consumer)
        }
        ConsumerSource::Prebuilt { name, consumer } => {
            info!(consumer = %name, "durable consumer starting");
            (name, consumer)
        }
    };
    let mut messages = consumer
        .messages()
        .await
        .map_err(|err| DurableConsumerError::Stream(err.to_string()))?;
    let mut tick_timer = tick
        .as_ref()
        .map(|(every, _)| tokio::time::interval(*every));
    // The redelivery policy is the bus's, so a consumer cannot retry on
    // terms its durable was not created with. The log limiter is this
    // loop's own: two consumers failing at once each still say so.
    let policy = bus.redelivery_policy();
    let mut redelivery_log = RedeliveryLog::new(policy);
    // Start a fresh parse-boundary record for this invocation. Identity
    // admissions never increment it, but still appear beside other durables.
    let ledger = bus.consumer_ledger().clone();
    ledger.start(&name);

    let halted = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!(consumer = %name, "durable consumer received shutdown signal");
                break None;
            }
            msg = messages.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        let next = handle_message(
                            &name,
                            &handler,
                            &admission,
                            &msg,
                            policy,
                            &mut redelivery_log,
                            &ledger,
                        )
                        .await;
                        if let Next::Halt(on) = next {
                            break Some(on);
                        }
                    }
                    Some(Err(err)) => {
                        warn!(consumer = %name, error = %err, "error reading next JetStream message");
                    }
                    None => {
                        warn!(consumer = %name, "JetStream message stream ended unexpectedly");
                        break None;
                    }
                }
            }
            _ = maybe_tick(tick_timer.as_mut()) => {
                if let Some((_, tick_fn)) = &tick {
                    tick_fn().await;
                }
            }
        }
    };
    // A halted event consumer must release the pull before parking so it does
    // not fetch messages it cannot resolve.
    drop(messages);
    if let Some(on) = halted {
        park_halted(&name, on, &ledger, shutdown).await;
    }
    info!(consumer = %name, "durable consumer stopped");
    Ok(())
}

/// The halt: said once, at error level, with everything an operator
/// needs to find the message; recorded where `fq doctor` reads; and
/// then the task waits for shutdown. It must not return — the daemon
/// supervises every consumer task and reads any exit, clean or not, as
/// a task failure that takes the whole daemon down, which is exactly
/// the state in which nothing could report why.
async fn park_halted(
    name: &str,
    on: UnsupportedEvent,
    ledger: &ConsumerLedger,
    shutdown: oneshot::Receiver<()>,
) {
    error!(
        consumer = name,
        schema_version = on.schema_version,
        supported = ?on.supported,
        event_id = on.event_id.as_deref().unwrap_or("-"),
        subject = %on.subject,
        stream_seq = on.stream_seq.unwrap_or(0),
        "event declares a schema version this build does not read; halting — the message \
         stays unacked and nothing after it is consumed until a build that reads it runs"
    );
    ledger.halt(name, on);
    let _ = shutdown.await;
    info!(consumer = name, "halted consumer received shutdown signal");
}

/// Whether the loop goes on after a message.
enum Next {
    Continue,
    Halt(UnsupportedEvent),
}

/// The parse/admission decision made before a generic handler runs.
#[derive(Debug)]
pub enum Admission<T> {
    Accept(T),
    AckMalformed(String),
    Halt(UnsupportedEvent),
}

/// Event-stream admission: decode current events, ack poison, halt on versions
/// this build cannot read.
pub fn admit(payload: &[u8], subject: &str, stream_seq: Option<u64>) -> Admission<Box<Event>> {
    match Event::from_wire(payload) {
        Ok(event) => Admission::Accept(Box::new(event)),
        Err(EventParseError::Malformed(err)) => Admission::AckMalformed(err.to_string()),
        Err(EventParseError::UnsupportedSchemaVersion {
            found,
            supported,
            event_id,
        }) => Admission::Halt(UnsupportedEvent {
            schema_version: found,
            supported: supported.to_vec(),
            event_id,
            subject: subject.to_string(),
            stream_seq,
        }),
    }
}

/// Await the next tick, or forever when the consumer has no
/// tick arm — keeps the `select!` uniform without an `Option`
/// precondition.
async fn maybe_tick(timer: Option<&mut tokio::time::Interval>) {
    match timer {
        Some(timer) => {
            timer.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Admit one message and apply the ack policy to the handler's
/// verdict. Never returns an error: per-message failures must not
/// kill the loop. The one thing it can say besides "go on" is that
/// the loop must halt, which is not a failure of this message but a
/// fact about the stream.
async fn handle_message<Item, H, HFut, A>(
    name: &str,
    handler: &H,
    admission: &A,
    msg: &async_nats::jetstream::Message,
    policy: ConsumerRedeliveryPolicy,
    redelivery_log: &mut RedeliveryLog,
    ledger: &ConsumerLedger,
) -> Next
where
    Item: DeliveryIdent,
    H: Fn(Delivery<Item>) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
    A: Fn(&async_nats::jetstream::Message) -> Admission<Item>,
{
    let info = msg.info().ok();
    let stream_seq = info.as_ref().map(|info| info.stream_sequence);
    let subject = msg.subject.to_string();
    let item = match admission(msg) {
        Admission::Accept(item) => item,
        Admission::AckMalformed(err) => {
            warn!(
                consumer = name,
                error = %err,
                subject,
                stream_seq = stream_seq.unwrap_or(0),
                "message was refused as malformed; acking to avoid a redelivery loop"
            );
            ledger.note_malformed(name);
            if let Err(ack_err) = msg.ack().await {
                error!(consumer = name, error = %ack_err, "failed to ack malformed message");
            }
            return Next::Continue;
        }
        Admission::Halt(on) => return Next::Halt(on),
    };
    // JetStream counts the first delivery as 1. A message whose
    // metadata could not be read is treated as a first delivery, which
    // costs the shortest delay rather than the longest — the wrong way
    // to be wrong here would be to stall a healthy retry.
    let delivered = info
        .as_ref()
        .and_then(|info| u64::try_from(info.delivered).ok())
        .unwrap_or(1);
    let event_id = item.delivery_ident();
    let delivery = Delivery {
        event: item,
        stream_seq,
        delivered,
        subject: subject.clone(),
    };
    match handler(delivery).await {
        Ok(()) => {
            if let Err(err) = msg.ack().await {
                error!(
                    consumer = name,
                    error = %err,
                    event_id = event_id.as_deref().unwrap_or("-"),
                    subject,
                    "failed to ack handled message"
                );
            }
        }
        Err(HandlerError::Permanent(err)) => {
            warn!(
                consumer = name,
                error = %err,
                event_id = event_id.as_deref().unwrap_or("-"),
                subject,
                "handler rejected message permanently; acking (no retry)"
            );
            if let Err(ack_err) = msg.ack().await {
                error!(
                    consumer = name,
                    error = %ack_err,
                    event_id = event_id.as_deref().unwrap_or("-"),
                    subject,
                    "failed to ack permanently rejected message"
                );
            }
        }
        Err(HandlerError::Transient(err)) => {
            let delay = policy.nak_delay(delivered);
            // One line per escalation step, then one per interval. A
            // handler that fails forever is worth saying so about; it
            // is not worth a line per broker round-trip, which is what
            // buried the signal when the NAK had no delay at all.
            if redelivery_log.admit(delivered, std::time::Instant::now()) {
                error!(
                    consumer = name,
                    error = %err,
                    event_id = event_id.as_deref().unwrap_or("-"),
                    subject,
                    stream_seq = stream_seq.unwrap_or(0),
                    delivered,
                    retry_in_ms = delay.as_millis() as u64,
                    "handler failed; NAK for redelivery"
                );
            }
            if let Err(nak_err) = msg
                .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
                .await
            {
                error!(
                    consumer = name,
                    error = %nak_err,
                    event_id = event_id.as_deref().unwrap_or("-"),
                    subject,
                    "failed to NAK message"
                );
            }
        }
    }
    Next::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventPayload, WorkerHeartbeatPayload};
    use crate::worker::WorkerId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// The Transient arm of the ack policy: a NAK'd message
    /// comes back, and the loop survives the failure.
    #[tokio::test]
    async fn transient_handler_error_naks_for_redelivery() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");

        let worker_id = WorkerId::new(format!("loop-nak-{}", Uuid::now_v7().simple())).unwrap();
        let event = Event::system(
            Uuid::now_v7(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: worker_id.clone(),
                last_step_at_ms: None,
            }),
        );
        bus.publish(&event).await.expect("publish");

        let attempts = Arc::new(AtomicUsize::new(0));
        let config = DurableConsumerConfig {
            durable_name: "fq-loop-nak-test".to_string(),
            filter_subjects: vec![format!("fq.worker.{}.heartbeat", worker_id.as_str())],
            deliver_from: DeliverFrom::Beginning,
            strict_order: false,
            ack_wait: None,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let bus_for_loop = bus.clone();
        let attempts_for_loop = attempts.clone();
        let handle = tokio::spawn(async move {
            run_durable_consumer(&bus_for_loop, config, shutdown_rx, |_event| {
                let attempts = attempts_for_loop.clone();
                async move {
                    // First delivery fails transiently; the NAK
                    // must bring the message back.
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(HandlerError::transient(std::io::Error::other(
                            "transient store failure",
                        )))
                    } else {
                        Ok(())
                    }
                }
            })
            .await
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::SeqCst) < 2 {
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "NAK'd message was not redelivered; attempts = {}",
                    attempts.load(Ordering::SeqCst)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _ = shutdown_tx.send(());
        let _ = handle.await;
    }

    /// The Permanent arm: the poison event is acked exactly
    /// once (never redelivered) and the loop moves on to the
    /// next message.
    #[tokio::test]
    async fn permanent_handler_error_acks_and_never_redelivers() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");

        let tag = Uuid::now_v7().simple().to_string();
        let poison_worker = WorkerId::new(format!("loop-poison-{tag}")).unwrap();
        let good_worker = WorkerId::new(format!("loop-good-{tag}")).unwrap();
        for worker in [&poison_worker, &good_worker] {
            let event = Event::system(
                Uuid::now_v7(),
                EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                    worker_id: worker.clone(),
                    last_step_at_ms: None,
                }),
            );
            bus.publish(&event).await.expect("publish");
        }

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let config = DurableConsumerConfig {
            durable_name: "fq-loop-permanent-test".to_string(),
            // The private per-test broker means the wildcard only
            // sees this test's two heartbeats.
            filter_subjects: vec!["fq.worker.*.heartbeat".to_string()],
            deliver_from: DeliverFrom::Beginning,
            strict_order: false,
            ack_wait: None,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let bus_for_loop = bus.clone();
        let seen_for_loop = seen.clone();
        let poison_id = poison_worker.as_str().to_string();
        let handle = tokio::spawn(async move {
            run_durable_consumer(
                &bus_for_loop,
                config,
                shutdown_rx,
                move |Delivery { event, .. }| {
                    let seen = seen_for_loop.clone();
                    let poison_id = poison_id.clone();
                    async move {
                        let EventPayload::WorkerHeartbeat(p) = &event.payload else {
                            return Ok(());
                        };
                        let id = p.worker_id.as_str().to_string();
                        let is_poison = id == poison_id;
                        seen.lock().unwrap().push(id);
                        if is_poison {
                            Err(HandlerError::permanent(std::io::Error::other(
                                "event this consumer can never handle",
                            )))
                        } else {
                            Ok(())
                        }
                    }
                },
            )
            .await
        });

        // Wait until the second (good) heartbeat is handled —
        // deliveries are in order, so the poison one has been
        // decided by then.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if seen
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == good_worker.as_str())
            {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "good heartbeat never handled; seen = {:?}",
                    seen.lock().unwrap()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Quiet window: a wrongly-NAK'd poison event would come
        // back almost immediately.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let poison_deliveries = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|id| *id == poison_worker.as_str())
            .count();
        assert_eq!(
            poison_deliveries, 1,
            "a Permanent error must ack: the event was redelivered"
        );

        let _ = shutdown_tx.send(());
        let _ = handle.await;
    }

    #[derive(Debug)]
    struct FakeItem(String);

    impl DeliveryIdent for FakeItem {
        fn delivery_ident(&self) -> Option<String> {
            Some(self.0.clone())
        }
    }

    /// A non-event admission rides the same loop and receives all metadata;
    /// malformed admission is counted and acked without reaching the handler.
    #[tokio::test]
    async fn generic_admission_populates_delivery_and_acks_malformed() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let tag = Uuid::now_v7().simple().to_string();
        let name = format!("fq-generic-loop-{tag}");
        let subject = format!("fq.maintenance.generic-{tag}");
        let consumer = bus
            .maintenance_consumer(&name, &subject, Some(Duration::from_millis(200)))
            .await
            .expect("create consumer");
        for payload in ["accepted", "malformed"] {
            bus.jetstream()
                .publish(subject.clone(), payload.into())
                .await
                .expect("publish")
                .await
                .expect("publish ack");
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_loop = Arc::clone(&seen);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let bus_for_loop = bus.clone();
        let name_for_loop = name.clone();
        let handle = tokio::spawn(async move {
            run_loop(
                &bus_for_loop,
                ConsumerSource::Prebuilt {
                    name: name_for_loop,
                    consumer: Box::new(consumer),
                },
                shutdown_rx,
                move |delivery: Delivery<FakeItem>| {
                    let seen = Arc::clone(&seen_for_loop);
                    async move {
                        seen.lock().unwrap().push((
                            delivery.event.0,
                            delivery.delivered,
                            delivery.subject,
                            delivery.stream_seq,
                        ));
                        Ok(())
                    }
                },
                None::<(Duration, fn() -> std::future::Ready<()>)>,
                |msg| {
                    if msg.payload.as_ref() == b"malformed" {
                        Admission::AckMalformed("fake malformed message".to_string())
                    } else {
                        Admission::Accept(FakeItem("accepted".to_string()))
                    }
                },
            )
            .await
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while (
            seen.lock().unwrap().len(),
            bus.consumer_ledger().record(&name).malformed_acked,
        ) != (1, 1)
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "generic deliveries timed out"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        {
            let got = seen.lock().unwrap();
            assert_eq!(
                got.len(),
                1,
                "malformed admission must not call the handler"
            );
            assert_eq!(got[0].0, "accepted");
            assert_eq!(got[0].1, 1);
            assert_eq!(got[0].2, subject);
            assert!(got[0].3.is_some());
            drop(got);
        }

        // Both verdicts end in an ACK, and the only way to see that is
        // to outlast the ack window: an un-acked malformed message
        // would be redelivered after `ack_wait` and admitted again.
        // Counting up to (1, 1) and stopping there would have looked
        // the same either way.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            (
                seen.lock().unwrap().len(),
                bus.consumer_ledger().record(&name).malformed_acked
            ),
            (1, 1),
            "nothing was redelivered after the ack window"
        );
        let mut durable = bus
            .jetstream()
            .get_stream(crate::bus::MAINTENANCE_STREAM_NAME)
            .await
            .expect("maintenance stream")
            .get_consumer::<async_nats::jetstream::consumer::pull::Config>(&name)
            .await
            .expect("the test durable");
        let info = durable.info().await.expect("consumer info");
        assert_eq!(
            info.num_ack_pending, 0,
            "the accepted and the malformed message were both acked"
        );
        assert_eq!(
            info.delivered.consumer_sequence, 2,
            "two messages, one delivery each: {:?}",
            info.delivered
        );

        let _ = shutdown_tx.send(());
        handle.await.expect("join loop").expect("run loop");
    }

    /// Sequence 0 names no message, so a floor of 0 is the beginning;
    /// any other floor is the position itself.
    #[test]
    fn a_floor_of_zero_is_the_beginning() {
        assert_eq!(DeliverFrom::from_floor(0), DeliverFrom::Beginning);
        assert_eq!(DeliverFrom::from_floor(7), DeliverFrom::Sequence(7));
    }

    /// The start-at-a-sequence variant (#648), on the projector's
    /// shape — strict order, whole stream: a durable created at
    /// sequence S is delivered S and what follows, and nothing below
    /// it. Proved at the server as well as at the handler: the
    /// durable's policy names S, and its acked floor after the loop is
    /// the last sequence.
    #[tokio::test]
    async fn a_sequence_start_delivers_from_that_position_and_not_below() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");

        // Three heartbeats on the private broker's own stream.
        let mut published = Vec::new();
        for i in 0..3 {
            let worker_id =
                WorkerId::new(format!("loop-seq-{i}-{}", Uuid::now_v7().simple())).unwrap();
            let event = Event::system(
                Uuid::now_v7(),
                EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                    worker_id: worker_id.clone(),
                    last_step_at_ms: None,
                }),
            );
            let seq = bus.publish(&event).await.expect("publish");
            published.push((seq, worker_id.as_str().to_string()));
        }
        let start = published[1].0;

        let seen = Arc::new(Mutex::new(Vec::<(Option<u64>, String)>::new()));
        let name = format!("fq-loop-seq-test-{}", Uuid::now_v7().simple());
        let config = DurableConsumerConfig {
            durable_name: name.clone(),
            filter_subjects: Vec::new(),
            deliver_from: DeliverFrom::Sequence(start),
            strict_order: true,
            ack_wait: None,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let bus_for_loop = bus.clone();
        let seen_for_loop = seen.clone();
        let handle = tokio::spawn(async move {
            run_durable_consumer(
                &bus_for_loop,
                config,
                shutdown_rx,
                move |Delivery {
                          event, stream_seq, ..
                      }| {
                    let seen = seen_for_loop.clone();
                    async move {
                        if let EventPayload::WorkerHeartbeat(p) = &event.payload {
                            seen.lock()
                                .unwrap()
                                .push((stream_seq, p.worker_id.as_str().to_string()));
                        }
                        Ok(())
                    }
                },
            )
            .await
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().len() < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the two heartbeats at and after the start were not delivered: {:?}",
                seen.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Quiet window: a delivery from below the start would land here.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let expected: Vec<(Option<u64>, String)> = published[1..]
            .iter()
            .map(|(seq, worker)| (Some(*seq), worker.clone()))
            .collect();
        assert_eq!(
            *seen.lock().unwrap(),
            expected,
            "exactly the messages at and after sequence {start}, in order"
        );

        let _ = shutdown_tx.send(());
        handle
            .await
            .expect("loop task")
            .expect("the loop stops clean");

        let stream = bus
            .jetstream()
            .get_stream(crate::bus::STREAM_NAME)
            .await
            .unwrap();
        let mut durable = stream
            .get_consumer::<async_nats::jetstream::consumer::pull::Config>(&name)
            .await
            .unwrap();
        let info = durable.info().await.unwrap();
        assert_eq!(
            info.config.deliver_policy,
            async_nats::jetstream::consumer::DeliverPolicy::ByStartSequence {
                start_sequence: start
            },
            "the server holds the start the config asked for"
        );
        assert_eq!(
            info.ack_floor.stream_sequence, published[2].0,
            "everything from the start on is acked"
        );
    }

    /// A start at a sequence is a whole-stream shape: a config that
    /// pairs it with subject filters is refused by name rather than
    /// created as something else.
    #[tokio::test]
    async fn a_sequence_start_with_filters_is_refused() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let config = DurableConsumerConfig {
            durable_name: "fq-loop-seq-filtered".to_string(),
            filter_subjects: vec!["fq.worker.*.heartbeat".to_string()],
            deliver_from: DeliverFrom::Sequence(1),
            strict_order: false,
            ack_wait: None,
        };
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let err = run_durable_consumer(&bus, config, shutdown_rx, |_delivery| async { Ok(()) })
            .await
            .expect_err("a filtered sequence start has no factory");
        assert!(
            matches!(&err, DurableConsumerError::Bus(BusError::Stream(msg))
                if msg.contains("whole-stream") && msg.contains("fq-loop-seq-filtered")),
            "the refusal names the shape and the consumer: {err:?}"
        );
    }
}
