//! The durable loop over `fq.maintenance.>`: resolve a task, run it,
//! record the outcome.
//!
//! It does not ride [`crate::control_plane::durable_consumer`], and
//! the reason is the payload. That loop's first act is
//! [`crate::control_plane::durable_consumer::admit`], which reads an
//! envelope version and deserialises an [`Event`] — the right thing
//! for a consumer of the event log, and the wrong thing here, where
//! every message is an opaque body a scheduler wrote. Running these
//! through it would ack every command as malformed and count it on the
//! parse ledger. The ack policy is the same in spirit; what differs is
//! what a message *is*.
//!
//! **So the `select!`/ack plumbing below is hand-rolled deliberately,
//! and it is still debt** — the fourth copy of a shape #192 exists to
//! keep in one place. The seam that would close it is a `run_loop`
//! generic over its admission (`Fn(&Message) -> Admission<T>`), with
//! this consumer passing an identity admission; that refactor touches
//! every control-plane consumer, so it is filed rather than done here:
//! <https://github.com/bricef/factor-q/issues/748>. Read that before
//! re-deriving why this module does not call
//! [`crate::control_plane::durable_consumer::run_durable_consumer`] —
//! and note the one policy difference it must preserve: a failed task
//! is **acked**, never NAK'd (the schedule is the retry); only a
//! failure to publish the outcome NAKs.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::bus::{BusError, EventBus};
use crate::events::operator_signal::kinds;
use crate::events::{
    Event, EventPayload, MaintenanceOutcome, MaintenanceRunPayload, OperatorSignalPayload,
    SignalKind, subjects,
};

use super::task::{MaintenanceContext, MaintenanceTask};

/// Name of the durable JetStream consumer this creates. Reported by
/// `fq doctor`, `fq status` and the dashboard beside the other
/// durables.
pub const CONSUMER_NAME: &str = "fq-maintenance";

/// How many resolved run ids the redelivery ledger remembers.
///
/// A ceiling, not a tuning knob: it bounds the memory a runaway
/// schedule can make the daemon hold, and 1024 is far more runs than
/// any redelivery window can span (fq-cron's own valve caps the whole
/// file at 120 fires an hour by default). Oldest ids are evicted
/// first, so the ids that could still be redelivered are the ones
/// kept.
const RUN_LEDGER_CAPACITY: usize = 1024;

/// Failures that stop the loop starting. Per-message failures never
/// surface here — they become outcome events.
#[derive(Debug, thiserror::Error)]
pub enum MaintenanceConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("jetstream message stream error: {0}")]
    Stream(String),
}

/// The daemon's maintenance consumer. Construct, then [`Self::run`].
pub struct MaintenanceConsumer {
    bus: EventBus,
    /// Stamped on every outcome event, so a fleet's maintenance
    /// history says which daemon ran what.
    runtime_id: Uuid,
    ctx: MaintenanceContext,
    ack_wait: Duration,
    consumer_name: String,
    filter_subject: String,
    ledger: Mutex<RunLedger>,
    /// Test-only: how long a task is held before it returns, so a test
    /// can outlast a short `ack_wait` and make JetStream redeliver a
    /// message that is genuinely still being worked on. Zero in
    /// production — no task is slowed down to make a test possible;
    /// this only makes the *redelivery* reachable, which is otherwise
    /// at the broker's discretion.
    task_delay: Duration,
}

impl MaintenanceConsumer {
    pub fn new(bus: EventBus, runtime_id: Uuid, ack_wait: Duration) -> Self {
        Self {
            bus,
            runtime_id,
            ctx: MaintenanceContext::new(),
            ack_wait,
            consumer_name: CONSUMER_NAME.to_string(),
            filter_subject: subjects::ALL_MAINTENANCE.to_string(),
            ledger: Mutex::new(RunLedger::new(RUN_LEDGER_CAPACITY)),
            task_delay: Duration::ZERO,
        }
    }

    /// Give the tasks what they run against. The daemon calls this with
    /// the pricing refresh it assembled; a consumer built without it
    /// runs the tasks that need nothing and records a typed failure for
    /// the rest.
    pub fn with_context(mut self, ctx: MaintenanceContext) -> Self {
        self.ctx = ctx;
        self
    }

    /// Test-only isolation: a private durable name and a narrowed
    /// filter, so parallel tests on one broker never consume each
    /// other's commands. The production consumer takes every task's
    /// subject under one well-known name.
    #[cfg(test)]
    pub(super) fn with_test_scope(mut self, name: String, filter_subject: String) -> Self {
        self.consumer_name = name;
        self.filter_subject = filter_subject;
        self
    }

    /// Test-only: see [`Self::task_delay`].
    #[cfg(test)]
    pub(super) fn with_task_delay(mut self, delay: Duration) -> Self {
        self.task_delay = delay;
        self
    }

    /// Consume until `shutdown` fires.
    pub async fn run(
        self,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), MaintenanceConsumerError> {
        info!(
            consumer = %self.consumer_name,
            filter = %self.filter_subject,
            ack_wait_ms = self.ack_wait.as_millis() as u64,
            "maintenance consumer starting"
        );
        let consumer = self
            .bus
            .maintenance_consumer(
                &self.consumer_name,
                &self.filter_subject,
                Some(self.ack_wait),
            )
            .await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| MaintenanceConsumerError::Stream(err.to_string()))?;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!(consumer = %self.consumer_name, "maintenance consumer received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => self.handle(&msg).await,
                        Some(Err(err)) => {
                            warn!(
                                consumer = %self.consumer_name,
                                error = %err,
                                "error reading next maintenance message"
                            );
                        }
                        None => {
                            warn!(
                                consumer = %self.consumer_name,
                                "maintenance message stream ended unexpectedly"
                            );
                            break;
                        }
                    }
                }
            }
        }
        info!(consumer = %self.consumer_name, "maintenance consumer stopped");
        Ok(())
    }

    /// Resolve one message. Never returns an error: everything a
    /// message can do wrong is an outcome, and the loop must survive
    /// all of them.
    async fn handle(&self, msg: &async_nats::jetstream::Message) {
        let info = msg.info().ok();
        let run_id = run_id(msg, info.as_ref().map(|i| i.stream_sequence));
        // JetStream counts the first delivery as 1; a message whose
        // metadata could not be read is treated as a first delivery, so
        // a healthy retry pays the shortest delay rather than the
        // longest (the shared durable loop makes the same call).
        let delivered = info
            .as_ref()
            .and_then(|info| u64::try_from(info.delivered).ok())
            .unwrap_or(1);

        // A run id already in the ledger is a redelivery, not a second
        // run. Either the outcome still needs publishing (the previous
        // attempt NAK'd on the publish) or it is already on the log and
        // there is nothing left to do but ack.
        if let Some(pending) = self.ledger_lookup(&run_id) {
            match pending {
                Some(held) => {
                    debug!(
                        consumer = %self.consumer_name,
                        run_id = %run_id,
                        "redelivered maintenance run; re-publishing the recorded outcome, not re-running"
                    );
                    self.settle(msg, &run_id, held, delivered).await;
                }
                None => {
                    debug!(
                        consumer = %self.consumer_name,
                        run_id = %run_id,
                        "maintenance run already resolved; acking the redelivery"
                    );
                    self.ack(msg, &run_id).await;
                }
            }
            return;
        }
        self.ledger_start(&run_id);

        let resolved = self.resolve(&msg.subject, &run_id).await;
        self.settle(msg, &run_id, resolved, delivered).await;
    }

    /// Run whatever the subject names, or refuse it. Produces the
    /// outcome payload and whatever it wants an operator told;
    /// publishing both is [`Self::settle`]'s job.
    async fn resolve(&self, subject: &str, run_id: &str) -> Resolved {
        let token = subjects::task_from_maintenance(subject);
        // The durable filters on `fq.maintenance.>`, so the only way
        // the parse fails here is a **dotted tail** — `fq.maintenance.
        // a.b` is not a task name with a dot in it. A subject outside
        // the prefix cannot reach this loop; the arm covers the parse
        // being fallible rather than a case a publisher can produce.
        let Some(token) = token else {
            warn!(
                consumer = %self.consumer_name,
                subject,
                run_id,
                "maintenance message on a subject that names no task; refusing"
            );
            return Resolved::just(MaintenanceRunPayload {
                task: subject.to_string(),
                run_id: run_id.to_string(),
                outcome: MaintenanceOutcome::Refused {
                    reason: format!("{subject:?} is not a fq.maintenance.<task> subject"),
                },
                duration_ms: 0,
            });
        };
        let task = match MaintenanceTask::parse(token) {
            Ok(task) => task,
            Err(unknown) => {
                warn!(
                    consumer = %self.consumer_name,
                    task = token,
                    run_id,
                    error = %unknown,
                    "refusing a maintenance task this build does not know"
                );
                return Resolved::just(MaintenanceRunPayload {
                    task: token.to_string(),
                    run_id: run_id.to_string(),
                    outcome: MaintenanceOutcome::Refused {
                        reason: unknown.to_string(),
                    },
                    duration_ms: 0,
                });
            }
        };

        info!(consumer = %self.consumer_name, task = %task, run_id, "running maintenance task");
        let started = Instant::now();
        if !self.task_delay.is_zero() {
            tokio::time::sleep(self.task_delay).await;
        }
        let result = task.run(&self.ctx).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let (outcome, signals) = match result {
            Ok(run) => {
                info!(
                    consumer = %self.consumer_name,
                    task = %task,
                    run_id,
                    duration_ms,
                    detail = %run.detail,
                    signals = run.signals.len(),
                    "maintenance task succeeded"
                );
                (
                    MaintenanceOutcome::Succeeded { detail: run.detail },
                    run.signals,
                )
            }
            Err(err) => {
                // A failed maintenance run is precisely what an operator
                // notification is for — unattended work that stopped
                // working, with nobody watching the log. Raised here and
                // only on this arm: a succeeded run is not news, and a
                // refusal is a configuration error the scheduler's own
                // owner sees.
                error!(
                    consumer = %self.consumer_name,
                    task = %task,
                    run_id,
                    duration_ms,
                    error = %err,
                    "maintenance task failed; the run is over, the next schedule is the retry"
                );
                let error = err.to_string();
                let signal = run_failed_signal(task, run_id, &error, duration_ms);
                (MaintenanceOutcome::Failed { error }, vec![signal])
            }
        };
        Resolved {
            payload: MaintenanceRunPayload {
                task: task.name().to_string(),
                run_id: run_id.to_string(),
                outcome,
                duration_ms,
            },
            signals,
        }
    }

    /// Publish the outcome and ack, or keep the outcome for the
    /// redelivery and NAK.
    ///
    /// The order matters: the event is the record, so it goes on the
    /// log before the message is consumed. A publish that fails leaves
    /// the payload in the ledger, so the redelivery this NAK earns
    /// re-publishes what already happened rather than making it happen
    /// again.
    ///
    /// **The outcome gates the signals**, which is what makes a signal
    /// exactly-once in every path a redelivery can take: a redelivery
    /// only ever happens when the outcome publish failed, and no signal
    /// was published in that attempt. A signal publish that fails *after*
    /// the outcome landed is logged and dropped rather than retried —
    /// re-running the task to recover a notification would be a worse
    /// trade than losing one, and the outcome event, which is the record,
    /// is already on the log.
    ///
    /// The NAK delay is the bus's, keyed on `delivered`, so a broker
    /// that will not take the outcome settles into one retry per
    /// `[bus] nak_max_ms` instead of a hot loop at round-trip speed —
    /// the escalation the shared durable loop applies, for the same
    /// reason.
    async fn settle(
        &self,
        msg: &async_nats::jetstream::Message,
        run_id: &str,
        resolved: Resolved,
        delivered: u64,
    ) {
        let event = Event::system(
            self.runtime_id,
            EventPayload::MaintenanceRun(resolved.payload.clone()),
        );
        match self.bus.publish(&event).await {
            Ok(_) => {
                self.publish_signals(run_id, &resolved.signals).await;
                self.ledger_resolved(run_id);
                self.ack(msg, run_id).await;
            }
            Err(err) => {
                self.ledger_hold(run_id, resolved);
                error!(
                    consumer = %self.consumer_name,
                    run_id,
                    error = %err,
                    "failed to publish the maintenance outcome; NAK to re-publish it on redelivery"
                );
                if let Err(nak_err) = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(
                        self.bus.redelivery_policy().nak_delay(delivered),
                    )))
                    .await
                {
                    error!(
                        consumer = %self.consumer_name,
                        run_id,
                        error = %nak_err,
                        "failed to NAK a maintenance message"
                    );
                }
            }
        }
    }

    /// Put what the run wants an operator told on the log, in order.
    async fn publish_signals(&self, run_id: &str, signals: &[OperatorSignalPayload]) {
        for signal in signals {
            let event = Event::system(
                self.runtime_id,
                EventPayload::OperatorSignal(signal.clone()),
            );
            if let Err(err) = self.bus.publish(&event).await {
                warn!(
                    consumer = %self.consumer_name,
                    run_id,
                    kind = %signal.kind,
                    error = %err,
                    "failed to publish a maintenance operator signal; the outcome event is on the log"
                );
            }
        }
    }

    async fn ack(&self, msg: &async_nats::jetstream::Message, run_id: &str) {
        if let Err(err) = msg.ack().await {
            error!(
                consumer = %self.consumer_name,
                run_id,
                error = %err,
                "failed to ack a maintenance message"
            );
        }
    }

    fn ledger_lookup(&self, run_id: &str) -> Option<Option<Resolved>> {
        self.ledger.lock().unwrap().lookup(run_id)
    }

    fn ledger_start(&self, run_id: &str) {
        self.ledger.lock().unwrap().start(run_id);
    }

    fn ledger_resolved(&self, run_id: &str) {
        self.ledger.lock().unwrap().resolved(run_id);
    }

    fn ledger_hold(&self, run_id: &str, resolved: Resolved) {
        self.ledger.lock().unwrap().hold(run_id, resolved);
    }
}

/// What one message resolved to: the outcome to record, and the signals
/// to raise beside it.
#[derive(Debug, Clone)]
struct Resolved {
    payload: MaintenanceRunPayload,
    signals: Vec<OperatorSignalPayload>,
}

impl Resolved {
    /// An outcome with nothing to tell an operator — every refusal, and
    /// every run that succeeded quietly.
    fn just(payload: MaintenanceRunPayload) -> Self {
        Self {
            payload,
            signals: Vec::new(),
        }
    }
}

/// The notification a failed run raises. `detail` carries what the
/// outcome event carries, so the pane needs no second lookup to say
/// which run this was.
fn run_failed_signal(
    task: MaintenanceTask,
    run_id: &str,
    error: &str,
    duration_ms: u64,
) -> OperatorSignalPayload {
    OperatorSignalPayload::notification(
        SignalKind::registered(kinds::MAINTENANCE_RUN_FAILED),
        format!("scheduled maintenance task `{task}` failed: {error}"),
    )
    .with_detail(serde_json::json!({
        "task": task.name(),
        "run_id": run_id,
        "error": error,
        "duration_ms": duration_ms,
    }))
}

/// What makes this delivery distinguishable from a redelivery of
/// itself.
///
/// fq-cron stamps `Nats-Msg-Id: fq-cron/<job>@<slot>` on every durable
/// publish, and that is the better id of the two: it identifies the
/// *logical fire*, so a scheduler that re-publishes a fire it crashed
/// before recording — outside the broker's own dedup window, where the
/// broker will not catch it — is deduped here as well, even though the
/// second copy has a new stream sequence. The sequence is the fallback
/// for a publisher that stamps nothing; it is stable across
/// redeliveries, which is all a redelivery guard needs.
///
/// A message with neither (no JetStream metadata at all) gets a fresh
/// id, so it runs. The wrong way to be wrong here is to invent a
/// collision and silently skip a scheduled sweep.
fn run_id(msg: &async_nats::jetstream::Message, stream_seq: Option<u64>) -> String {
    if let Some(id) = msg
        .headers
        .as_ref()
        .and_then(|headers| headers.get(async_nats::header::NATS_MESSAGE_ID))
    {
        return id.to_string();
    }
    match stream_seq {
        Some(seq) => format!("seq:{seq}"),
        None => format!("unidentified:{}", Uuid::now_v7()),
    }
}

/// The run ids this process has already answered for, oldest first.
///
/// A value of `None` means "resolved and published — nothing left to
/// do"; `Some(resolved)` means the outcome is known but not yet on the
/// event log, so a redelivery must publish it rather than re-run the
/// task.
struct RunLedger {
    /// Insertion order, for eviction.
    order: std::collections::VecDeque<String>,
    entries: HashMap<String, Option<Resolved>>,
    capacity: usize,
}

impl RunLedger {
    fn new(capacity: usize) -> Self {
        Self {
            order: std::collections::VecDeque::with_capacity(capacity),
            entries: HashMap::with_capacity(capacity),
            capacity,
        }
    }

    fn lookup(&self, run_id: &str) -> Option<Option<Resolved>> {
        self.entries.get(run_id).cloned()
    }

    /// Record a run as started *before* it runs, so a redelivery that
    /// arrives alongside it is answered rather than acted on.
    fn start(&mut self, run_id: &str) {
        if self.entries.contains_key(run_id) {
            return;
        }
        while self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
        self.order.push_back(run_id.to_string());
        self.entries.insert(run_id.to_string(), None);
    }

    fn resolved(&mut self, run_id: &str) {
        if let Some(slot) = self.entries.get_mut(run_id) {
            *slot = None;
        }
    }

    fn hold(&mut self, run_id: &str, resolved: Resolved) {
        if let Some(slot) = self.entries.get_mut(run_id) {
            *slot = Some(resolved);
        }
    }
}

#[cfg(test)]
mod tests;
