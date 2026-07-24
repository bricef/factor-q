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
//! };
//! run_durable_consumer(&bus, config, shutdown, |delivery| async move {
//!     handle(&delivery.event).await.map_err(HandlerError::transient)
//! })
//! .await?;
//! ```
//!
//! Ack policy — the decisions this module centralises:
//!
//! - **Deserialise failure** → logged and ACK'd. A payload we
//!   cannot decode will never decode on retry; leaving it
//!   un-acked would just create a redelivery loop.
//! - **Handler `Ok`** → ACK'd.
//! - **[`HandlerError::Transient`]** → logged and NAK'd.
//!   JetStream redelivers after the ack deadline; transient
//!   store/publish failures recover, persistent ones stay
//!   visible in logs.
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

use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use crate::bus::{BusError, EventBus};
use crate::events::Event;

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
}

impl DurableConsumerConfig {
    /// Create (or re-attach to) the durable via the bus factory
    /// matching this config.
    async fn create(
        &self,
        bus: &EventBus,
    ) -> Result<async_nats::jetstream::consumer::PullConsumer, BusError> {
        match self.deliver_from {
            DeliverFrom::Beginning => match self.filter_subjects.as_slice() {
                [] => bus.durable_consumer(&self.durable_name).await,
                [filter] => {
                    bus.durable_consumer_with_filter(&self.durable_name, filter)
                        .await
                }
                filters => {
                    let refs: Vec<&str> = filters.iter().map(|s| s.as_str()).collect();
                    bus.durable_consumer_with_filters(&self.durable_name, &refs)
                        .await
                }
            },
            DeliverFrom::New => match self.filter_subjects.as_slice() {
                [filter] => {
                    bus.durable_consumer_with_filter_from_new(&self.durable_name, filter)
                        .await
                }
                filters => {
                    bus.durable_consumer_with_filters_from_new(&self.durable_name, filters)
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

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!(consumer = %name, "durable consumer received shutdown signal");
                break;
            }
            msg = messages.next() => {
                match msg {
                    Some(Ok(msg)) => handle_message(&name, &handler, &msg).await,
                    Some(Err(err)) => {
                        warn!(consumer = %name, error = %err, "error reading next JetStream message");
                    }
                    None => {
                        warn!(consumer = %name, "JetStream message stream ended unexpectedly");
                        break;
                    }
                }
            }
            _ = maybe_tick(tick_timer.as_mut()) => {
                if let Some((_, tick_fn)) = &tick {
                    tick_fn().await;
                }
            }
        }
    }

    info!(consumer = %name, "durable consumer stopped");
    Ok(())
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

/// Deserialise one message and apply the ack policy to the
/// handler's verdict. Never returns an error: per-message
/// failures must not kill the loop.
async fn handle_message<H, HFut>(name: &str, handler: &H, msg: &async_nats::jetstream::Message)
where
    H: Fn(Delivery) -> HFut,
    HFut: Future<Output = Result<(), HandlerError>>,
{
    let event = match serde_json::from_slice::<Event>(&msg.payload) {
        Ok(event) => event,
        Err(err) => {
            warn!(
                consumer = name,
                error = %err,
                "failed to deserialise event; acking to avoid a redelivery loop"
            );
            if let Err(ack_err) = msg.ack().await {
                error!(consumer = name, error = %ack_err, "failed to ack malformed message");
            }
            return;
        }
    };

    let stream_seq = msg.info().ok().map(|info| info.stream_sequence);
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
            error!(
                consumer = name,
                error = %err,
                event_id = %event_id,
                "handler failed; NAK for redelivery"
            );
            if let Err(nak_err) = msg
                .ack_with(async_nats::jetstream::AckKind::Nak(None))
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
            }),
        );
        bus.publish(&event).await.expect("publish");

        let attempts = Arc::new(AtomicUsize::new(0));
        let config = DurableConsumerConfig {
            durable_name: "fq-loop-nak-test".to_string(),
            filter_subjects: vec![format!("fq.worker.{}.heartbeat", worker_id.as_str())],
            deliver_from: DeliverFrom::Beginning,
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
}
