//! The shared durable-consumer loop for control-plane event
//! consumers (#192).
//!
//! Every control-plane consumer of the factor-q event stream has
//! the same lifecycle: create (or re-attach to) a durable
//! JetStream consumer, loop on `select!` with biased shutdown,
//! deserialise each message into an [`Event`], dispatch to a
//! handler, and ACK or NAK by error class. Before this module
//! that loop was copy-pasted per consumer and the copies drifted
//! in small ways; now the loop — and with it the ack policy —
//! lives in exactly one place.
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
//!   loop too. Every consumer on this loop halts, not only the
//!   projector: the version is a fact about the stream, and every
//!   reader of it is behind the same binary. [`admit`] is that
//!   decision as a value.
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

/// One delivered message: the deserialised event plus its position
/// in the event log. The position is what watermarks are made of —
/// handlers that don't track progress simply ignore it.
pub struct Delivery {
    pub event: Event,
    /// The `fq-events` stream sequence of this message. Absent only
    /// when JetStream metadata could not be read off the delivery.
    pub stream_seq: Option<u64>,
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
    run_loop(bus, config, shutdown, handler, NO_TICK).await
}

/// Like [`run_durable_consumer`], with a periodic housekeeping
/// tick multiplexed into the same task. The tick and the
/// handler are serialised — they never run concurrently — and,
/// per `tokio::time::interval` semantics, the first tick fires
/// as soon as the loop starts.
pub async fn run_durable_consumer_with_tick<H, HFut, T, TFut>(
    bus: &EventBus,
    config: DurableConsumerConfig,
    shutdown: oneshot::Receiver<()>,
    handler: H,
    tick_every: Duration,
    tick: T,
) -> Result<(), DurableConsumerError>
where
    H: Fn(Delivery) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
    T: Fn() -> TFut,
    TFut: Future<Output = ()>,
{
    run_loop(bus, config, shutdown, handler, Some((tick_every, tick))).await
}

/// The tick type instantiated when a consumer has no tick arm.
type NoTickFn = fn() -> std::future::Ready<()>;
const NO_TICK: Option<(Duration, NoTickFn)> = None;

async fn run_loop<H, HFut, T, TFut>(
    bus: &EventBus,
    config: DurableConsumerConfig,
    mut shutdown: oneshot::Receiver<()>,
    handler: H,
    tick: Option<(Duration, T)>,
) -> Result<(), DurableConsumerError>
where
    H: Fn(Delivery) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
    T: Fn() -> TFut,
    TFut: Future<Output = ()>,
{
    let name = config.durable_name.clone();
    info!(
        consumer = %name,
        filters = ?config.filter_subjects,
        deliver_from = ?config.deliver_from,
        "durable consumer starting"
    );
    let consumer = config.create(bus).await?;
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
    // The parse-boundary record this loop reports to. It starts empty,
    // so the figures describe this loop and not an earlier one on the
    // same durable.
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
                            &name, &handler, &msg, policy, &mut redelivery_log, &ledger,
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
    // Release the pull before parking or returning: a halted loop must
    // not keep fetching messages it will never resolve.
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

/// What the loop does with a delivered message before any handler sees
/// it — the parse half of the ack policy, as a value, so the event
/// corpus can be replayed through it without a broker and the loop and
/// that test cannot disagree about what a version means.
#[derive(Debug)]
pub enum Admission {
    /// An event this build reads: it goes to the handler. Boxed
    /// because an event is most of a kilobyte and the other two
    /// outcomes are not, and the value is moved once.
    Event(Box<Event>),
    /// Not an event in any version: acked, counted, skipped.
    AckMalformed(serde_json::Error),
    /// Well-formed history in a version this build does not read:
    /// left unacked, and the loop halts.
    Halt(UnsupportedEvent),
}

/// Decide a message's admission from its bytes and where it sat.
pub fn admit(payload: &[u8], subject: &str, stream_seq: Option<u64>) -> Admission {
    match Event::from_wire(payload) {
        Ok(event) => Admission::Event(Box::new(event)),
        Err(EventParseError::Malformed(err)) => Admission::AckMalformed(err),
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
async fn handle_message<H, HFut>(
    name: &str,
    handler: &H,
    msg: &async_nats::jetstream::Message,
    policy: ConsumerRedeliveryPolicy,
    redelivery_log: &mut RedeliveryLog,
    ledger: &ConsumerLedger,
) -> Next
where
    H: Fn(Delivery) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
{
    let info = msg.info().ok();
    let stream_seq = info.as_ref().map(|info| info.stream_sequence);
    let subject: &str = &msg.subject;
    let event = match admit(&msg.payload, subject, stream_seq) {
        Admission::Event(event) => *event,
        Admission::AckMalformed(err) => {
            warn!(
                consumer = name,
                error = %err,
                subject,
                stream_seq = stream_seq.unwrap_or(0),
                "message is not an event in any version; acking to avoid a redelivery loop"
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
    let event_id = event.envelope.event_id;
    match handler(Delivery { event, stream_seq }).await {
        Ok(()) => {
            if let Err(err) = msg.ack().await {
                error!(
                    consumer = name,
                    error = %err,
                    event_id = %event_id,
                    "failed to ack handled event"
                );
            }
        }
        Err(HandlerError::Permanent(err)) => {
            warn!(
                consumer = name,
                error = %err,
                event_id = %event_id,
                "handler rejected event permanently; acking (no retry)"
            );
            if let Err(ack_err) = msg.ack().await {
                error!(
                    consumer = name,
                    error = %ack_err,
                    event_id = %event_id,
                    "failed to ack permanently rejected event"
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
                    event_id = %event_id,
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
                    event_id = %event_id,
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
                move |Delivery { event, stream_seq }| {
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
