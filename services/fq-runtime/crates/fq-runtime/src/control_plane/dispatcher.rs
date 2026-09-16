//! NATS-triggered agent dispatcher.
//!
//! Sits on the `fq.trigger.>` JetStream stream and dispatches each
//! incoming message to the appropriate agent via the executor.
//! Runs as a long-lived tokio task inside `fqd`, alongside the
//! projection consumer.
//!
//! # Delivery semantics
//!
//! - Work-queue: each trigger is delivered to exactly one consumer
//!   and deleted after ack. There is no replay of already-processed
//!   triggers on restart.
//! - Durable consumer: if the dispatcher crashes or is restarted,
//!   JetStream remembers its position and redelivers any unacked
//!   triggers after the ack deadline.
//! - Ack-after-durable-start: the trigger is acked once the
//!   invocation has *durably started* — signalled through the
//!   [`Worker`] seam after its first WAL write — not at dispatch and
//!   not at completion (issue #41). From the first WAL write on,
//!   in-flight durability is the reducer WAL's job
//!   (`recovery::scan_in_flight`), so a crash resumes the invocation
//!   exactly once from the WAL with no redelivered duplicate. This
//!   closes both failure windows: a crash *before* the first WAL write
//!   leaves the trigger unacked, so JetStream redelivers it and the
//!   otherwise-missed run recovers; and the ack still fires at the
//!   first WAL write (seconds in), *well before* completion, so a
//!   long-running invocation is not redelivered and re-run past the
//!   30s ack-wait — the redelivery storm the M0 dogfood loop found
//!   (2026-07-06), where an invocation longer than the ack-wait was
//!   re-run, stays fixed.
//!
//! # Error handling
//!
//! Most errors are **acked, not NAK'd**: unknown agent ids, invalid
//! JSON payloads, and executor errors are all permanent problems
//! that retrying would not fix. We log and move on. The only
//! situations that intentionally propagate are the bus/consumer
//! itself failing (a bigger problem) and receive-side protocol
//! errors.
//!
//! An executor error already produces a `Failed` event on the
//! event stream, so downstream consumers (the projection, tailers)
//! see the failure even though the trigger is acked.

mod admission;
mod agent_cap;
mod claim;
mod dead_letter;
mod deferral;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::agent::{AgentId, AgentRegistry};
use crate::bus::{BusError, EventBus, TRIGGER_MAX_DELIVER};
use crate::control_plane::agent_cap::AgentConcurrency;
use crate::hot_swap::HotSwap;
use crate::llm::{LlmClient, ModelThrottle};
use crate::trigger::agent_id_from_subject;
use crate::worker::{DeferralQueue, DrainState, DueResume, DurableStart, ExecutorError, Worker};

/// Name of the durable JetStream consumer the dispatcher creates.
pub const CONSUMER_NAME: &str = "fq-dispatcher";

/// The name a trigger's ack or NAK line reports (review finding F).
///
/// `Some` once the dispatcher has taken responsibility for the message
/// and named it. The reject-before-naming paths — a bad subject, an
/// unknown agent, a payload that is not JSON — pass what the publisher
/// stamped, if anything: on those the honest answer may be that the
/// message never had a name. Rendered as `-` then, so the field is on
/// every line and a log query for it can rely on being able to find it.
fn trigger_name(id: Option<uuid::Uuid>) -> String {
    id.map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// Whether the current JetStream delivery is the terminal retry. Kept pure so
/// the delivery-limit boundary is testable without a live broker.
fn trigger_retry_exhausted(delivery_attempt: u32) -> bool {
    delivery_attempt >= TRIGGER_MAX_DELIVER as u32
}

/// Escalating redelivery delay for a transient pre-WAL failure. A
/// `Nak(None)` redelivers immediately — five retries in milliseconds
/// give a transient condition (broker blip, store contention) no time
/// to clear, defeating the retry's purpose. Indexed by the 1-based
/// delivery attempt; saturates at the last entry. Kept pure so the
/// schedule is testable without a broker. Mirrors
/// [`crate::bus::TRIGGER_RETRY_BACKOFF`], which paces the ack-wait
/// redelivery path (a crashed dispatcher never reaches this NAK).
fn trigger_retry_backoff(delivery_attempt: u32) -> std::time::Duration {
    let schedule = crate::bus::TRIGGER_RETRY_BACKOFF;
    let index = (delivery_attempt.saturating_sub(1) as usize).min(schedule.len() - 1);
    schedule[index]
}

/// A trigger's fate once its invocation returned without durably
/// starting — the whole pre-WAL retry policy as one pure decision, so
/// the exhaustion/retry/consume split is unit-testable without a
/// broker (the escalating backoff makes a live full-loop test take
/// minutes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerFate {
    /// Transient failure with retries remaining: NAK with this delay.
    Retry(std::time::Duration),
    /// Transient failure on the final delivery: dead-letter, then ack.
    DeadLetter,
    /// Permanent failure or a clean return: ack, nothing to retry.
    Consume,
}

fn trigger_fate(
    result: &Result<crate::worker::InvocationOutcome, ExecutorError>,
    delivery_attempt: u32,
) -> TriggerFate {
    match result {
        Err(err) if err.is_transient() && trigger_retry_exhausted(delivery_attempt) => {
            TriggerFate::DeadLetter
        }
        Err(err) if err.is_transient() => {
            TriggerFate::Retry(trigger_retry_backoff(delivery_attempt))
        }
        _ => TriggerFate::Consume,
    }
}

/// Shared, hot-swappable agent registry.
///
/// The dispatcher reads through this handle on every trigger, so a
/// hot-reload ([`fq reload`](crate::control_plane)) swaps in a
/// freshly-loaded registry and the *next* trigger picks it up. In-flight
/// invocations already hold their own `Agent` clone (snapshotted at
/// trigger time), so a swap never disturbs them — matching the ADR-0020
/// refresh-between-invocations precedent, which is the property
/// [`HotSwap`] exists to carry.
pub type SharedRegistry = HotSwap<AgentRegistry>;

/// Wrap an owned registry in a fresh [`SharedRegistry`] handle.
pub fn shared_registry(registry: AgentRegistry) -> SharedRegistry {
    HotSwap::new(registry)
}

/// NATS-triggered dispatcher. Owns references to the pieces of the
/// runtime it needs — call [`TriggerDispatcher::run`] to drive it.
///
/// The dispatcher lives on the control-plane side of the role
/// boundary; it talks to workers exclusively through the
/// [`Worker`] trait. v1 hands it an in-process worker
/// (`Arc::new(ReducerRunner::new(...))`); v2 will hand it a
/// remote-worker adapter that proxies over NATS.
pub struct TriggerDispatcher {
    bus: EventBus,
    registry: SharedRegistry,
    worker: Arc<dyn Worker>,
    llm: Arc<dyn LlmClient>,
    /// In-executor fan-out bound (#70): how many invocations this
    /// dispatcher runs concurrently. `1` is the serial behavior.
    max_concurrent: usize,
    /// The worker's provider throttle (#278): asked before every
    /// invocation starts, so a paused model's trigger is held rather
    /// than run. Inert unless [`Self::with_throttle`] hands in the
    /// daemon's.
    throttle: Arc<ModelThrottle>,
    /// Where a deferred invocation is put down and picked up again
    /// (#278): `handle` defers into it, and the consume loop drains it
    /// into a task that takes a worker permit of its own to resume.
    deferrals: DeferralQueue,
    /// The queue's drain end, taken by the loop when it starts.
    due: std::sync::Mutex<Option<mpsc::Receiver<DueResume>>>,
    /// Per-agent in-flight counts (#718): consulted before a trigger
    /// starts, so an agent at its definition's `max_concurrent` has its
    /// next trigger held rather than run. Shared with the daemon's
    /// resume paths, which count without being gated.
    agent_caps: Arc<AgentConcurrency>,
    /// The worker-cap permits (#70). Owned here rather than created by
    /// the consume loop because the loop is not the one that takes them
    /// (#733): a permit is taken in `handle`, after both admission
    /// rules, by a trigger that is ready to run, and by a due resume.
    /// The worker cap bounds *running* invocations; waiting — on a
    /// pause, on an agent's cap, on a permit itself — is free.
    permits: Arc<Semaphore>,
    /// Set once the loop has seen its shutdown signal, so a trigger held
    /// for a paused model lets go rather than blocking the stop.
    stopping: AtomicBool,
    /// Durable stream-sequence arbiter, installed by the daemon.
    claim_store: Option<claim::ClaimStore>,
}

impl TriggerDispatcher {
    pub fn new(
        bus: EventBus,
        registry: SharedRegistry,
        worker: Arc<dyn Worker>,
        llm: Arc<dyn LlmClient>,
        max_concurrent: usize,
    ) -> Self {
        let (deferrals, due) = DeferralQueue::new();
        let max_concurrent = max_concurrent.max(1);
        Self {
            bus,
            registry,
            worker,
            llm,
            permits: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            throttle: Arc::new(ModelThrottle::inert()),
            deferrals,
            due: std::sync::Mutex::new(Some(due)),
            agent_caps: AgentConcurrency::new(),
            stopping: AtomicBool::new(false),
            claim_store: None,
        }
    }

    /// Share the daemon's throttle, so the hold on a paused model's
    /// triggers and the permits its calls take agree on one state.
    pub fn with_throttle(mut self, throttle: Arc<ModelThrottle>) -> Self {
        self.throttle = throttle;
        self
    }

    /// Share the daemon's per-agent counts (#718), so the cap sees the
    /// invocations recovery and `fq invocation resume` re-drive too, and
    /// `fq doctor` reads the same numbers this dispatcher admits on.
    pub fn with_agent_caps(mut self, agent_caps: Arc<AgentConcurrency>) -> Self {
        self.agent_caps = agent_caps;
        self
    }

    /// Share the daemon's deferral queue, so a resume deferred by
    /// startup recovery or `fq invocation resume` is drained here too.
    pub fn with_deferrals(
        mut self,
        deferrals: DeferralQueue,
        due: mpsc::Receiver<DueResume>,
    ) -> Self {
        self.deferrals = deferrals;
        self.due = std::sync::Mutex::new(Some(due));
        self
    }

    /// Run the dispatcher loop until `shutdown` fires.
    pub async fn run(self, shutdown: oneshot::Receiver<()>) -> Result<(), DispatcherError> {
        self.run_on_consumer(CONSUMER_NAME, None, shutdown).await
    }

    /// The dispatch loop on an explicit consumer identity. Production
    /// enters through [`Self::run`]; tests pass a unique durable name
    /// and a narrow filter so parallel runs don't compete for each
    /// other's messages on the work-queue stream.
    pub(crate) async fn run_on_consumer(
        self,
        consumer_name: &str,
        filter_subject: Option<&str>,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), DispatcherError> {
        info!(
            max_concurrent = self.max_concurrent,
            "trigger dispatcher starting"
        );
        self.bus.consumer_ledger().start(consumer_name);
        // Ack-window sizing: with the one-message pull batch below,
        // unacked genuinely stays around the in-dispatch window
        // (ack-on-durable-start fires seconds into a run). The window is
        // never set *below* the server default: running deployments'
        // durable consumers already carry that effective value, and
        // `get_or_create` won't rewrite an existing consumer's config.
        let max_ack_pending =
            (self.max_concurrent as i64 * 2).max(crate::bus::NATS_DEFAULT_MAX_ACK_PENDING);
        let consumer = match filter_subject {
            Some(filter) => {
                self.bus
                    .trigger_consumer_with_filter(consumer_name, filter, max_ack_pending)
                    .await?
            }
            None => {
                self.bus
                    .trigger_consumer(consumer_name, max_ack_pending)
                    .await?
            }
        };
        // One message per pull: without this, async-nats prefetches up
        // to a 200-message batch into the client buffer, where triggers
        // sit delivered-and-unacked with the ack_wait ticking behind
        // whatever the loop is doing — a saturated dispatcher would then
        // see every buffered trigger redelivered and run twice (the
        // one-trigger-to-N-invocations storm the ack-on-durable-start
        // fix exists to prevent). With batch = 1 nothing is prefetched:
        // excess triggers stay *queued on the server* and reach the next
        // binary on drain rather than after ack_wait expiry. The extra
        // round-trip per trigger is noise against minutes-long
        // invocations.
        let mut messages = consumer
            .stream()
            .max_messages_per_batch(1)
            .messages()
            .await
            .map_err(|err| DispatcherError::Stream(err.to_string()))?;

        // In-executor fan-out (#70): up to `max_concurrent` invocations
        // run at once, each on its own spawned task. The JoinSet makes
        // in-flight invocations explicit so drain and shutdown wait for
        // them — before fan-out the single inline `handle().await` was
        // covered implicitly by awaiting `run` itself, and spawning
        // without tracking would silently regress that drain coverage.
        let this = Arc::new(self);
        let mut in_flight: JoinSet<()> = JoinSet::new();
        // Deferred invocations come back through here (#278), and take a
        // worker permit of their own before they resume.
        let mut due = this
            .due
            .lock()
            .expect("dispatcher deferral lock poisoned")
            .take()
            .expect("a dispatcher runs its consume loop once");

        'consume: loop {
            // Reap finished invocations so the set doesn't accumulate
            // join results over the daemon's lifetime.
            while let Some(joined) = in_flight.try_join_next() {
                log_invocation_task(joined);
            }

            // ADR-0027 graceful drain: once the worker is draining, stop
            // pulling new triggers. In-flight invocations suspend at their
            // next step boundary via the shared drain signal (the reducer
            // polls it); un-pulled triggers stay queued on the durable
            // work-queue consumer for the next binary. Final teardown
            // still arrives through `shutdown`.
            if this.draining("at loop top") {
                break 'consume;
            }

            // **The loop holds no worker permit** (#733). A permit is
            // taken in `handle`, after both admission rules, by a
            // trigger that is ready to run — never here, and never by
            // anything that is only waiting. What bounds how much is
            // pulled is the consumer's `max_ack_pending`, not the worker
            // cap. Why taking one before the pull ("capacity before
            // consumption") is no longer needed, and what it cost, is in
            // `admission`'s module doc.
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("trigger dispatcher received shutdown signal");
                    break 'consume;
                }
                Some(resume) = due.recv() => {
                    let dispatcher = Arc::clone(&this);
                    in_flight.spawn(async move {
                        // A resume queues for its permit like everything
                        // else, with no delivery to keep alive: its
                        // trigger was acked at the first WAL write.
                        let Some(_permit) = dispatcher.acquire_run_permit(None, None).await else {
                            return;
                        };
                        dispatcher.resume_deferred(resume).await;
                    });
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            let dispatcher = Arc::clone(&this);
                            in_flight.spawn(async move {
                                dispatcher.handle(&msg).await;
                            });
                        }
                        Some(Err(err)) => {
                            // Warn-and-continue (pre-fan-out behavior),
                            // but with a pause: nothing else in this
                            // iteration blocks, so a persistently
                            // erroring consumer would hot-spin the loop.
                            warn!(error = %err, "error reading next JetStream trigger");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        None => {
                            warn!("trigger stream ended unexpectedly");
                            break 'consume;
                        }
                    }
                }
                // Nothing arrived: go round, so the drain check at the
                // top runs. A drain is a flag the worker sets, not a
                // signal this can await (`Worker::drain_status`), and
                // every other wait in the dispatcher polls it on this
                // cadence. The loop used to notice one only when an
                // interrupted hold released the permit it was waiting
                // for, which left an idle dispatcher parked on
                // `messages.next()` until shutdown; this is the
                // ADR-0027 exit for that case.
                () = tokio::time::sleep(admission::HOLD_KEEPALIVE) => {}
            }
        }

        // Wait for every spawned invocation before returning. On drain
        // they suspend at their next step boundary (the reducer polls
        // the shared signal), so this completes promptly; on shutdown it
        // mirrors the pre-fan-out behavior, where `run` never returned
        // mid-invocation. The daemon awaits `run` through its
        // dispatcher handle, so teardown ordering is unchanged. A
        // trigger held for a paused model lets go on this flag.
        // Stop pulling first: an interrupted hold requeues its trigger
        // (#718) and an open pull request would swallow the fresh copy.
        drop(messages);
        this.stopping.store(true, Ordering::SeqCst);
        while let Some(joined) = in_flight.join_next().await {
            log_invocation_task(joined);
        }

        info!("trigger dispatcher stopped");
        Ok(())
    }

    /// Drain check with a call-site label, so the log line says *where*
    /// the drain was observed. The loop asks once, at its top; `handle`
    /// asks again for the trigger it has just been handed, which is the
    /// drain that landed between the two.
    fn draining(&self, at: &str) -> bool {
        if self.worker.drain_status() == DrainState::Draining {
            info!(
                at,
                "trigger dispatcher draining — no longer consuming new triggers"
            );
            true
        } else {
            false
        }
    }

    /// Who the trigger is for, or `None` once it has been acked and
    /// dropped: a subject that is not `fq.trigger.<agent>`, or an agent
    /// id that is not a legal subject token. Both are permanent — a
    /// redelivery would fail identically — so both consume the message
    /// rather than NAK it, and neither has a name to report yet beyond
    /// whatever the publisher stamped.
    ///
    /// Its own function because `handle` is at the 250-line cap and this
    /// is the part of it with no bearing on anything after: two
    /// validations that either yield an `AgentId` or end the delivery.
    async fn addressee(&self, msg: &async_nats::jetstream::Message) -> Option<AgentId> {
        let Some(agent_id_str) = agent_id_from_subject(&msg.subject) else {
            warn!(
                subject = %msg.subject,
                "trigger with unexpected subject format, dropping"
            );
            self.ack(
                msg,
                crate::trigger::trigger_id_in(msg.headers.as_ref()),
                "bad subject",
            )
            .await;
            return None;
        };
        match AgentId::new(agent_id_str) {
            Ok(id) => Some(id),
            Err(err) => {
                warn!(
                    agent_id = %agent_id_str,
                    error = %err,
                    "trigger for invalid agent id, dropping"
                );
                self.ack(
                    msg,
                    crate::trigger::trigger_id_in(msg.headers.as_ref()),
                    "invalid agent id",
                )
                .await;
                None
            }
        }
    }

    /// Dispatch one trigger. It arrives owning nothing: the worker-cap
    /// permit is taken at the *end* of the admission sequence, once the
    /// trigger is ready to run and the only thing it can still be
    /// waiting for is a free worker, and dropped when this returns.
    async fn handle(&self, msg: &async_nats::jetstream::Message) {
        // First operation by design: no drain, routing, parsing, or trigger-id
        // minting happens until this broker identity has been arbitrated.
        let claim::ClaimVerdict::Proceed {
            stream_seq,
            delivered: delivery_attempt,
        } = self.claim_delivery(msg).await
        else {
            return;
        };

        // A drain requested after this trigger was pulled but before it
        // was dispatched: leave it un-acked so it redelivers to the next
        // binary rather than starting an invocation that would only
        // suspend at step 0. The common case — a drain between
        // invocations — is caught by the consume loop's top-of-loop
        // check; this closes the pulled-just-as-drain-lands race.
        if self.worker.drain_status() == DrainState::Draining {
            debug!(
                subject = %msg.subject,
                "draining — leaving trigger queued for the next binary"
            );
            return;
        }

        let Some(agent_id) = self.addressee(msg).await else {
            return;
        };
        // Read the registry through the swappable handle. Cloning the
        // inner Arc under a short read lock gives this invocation a
        // stable snapshot for its whole lifetime: a concurrent reload
        // that swaps in a new Arc does not disturb an in-flight run
        // (ADR-0020 refresh-between-invocations).
        let registry = self.registry.current();
        let loaded = match registry.get_loaded(&agent_id) {
            Some(loaded) => loaded,
            None => {
                warn!(
                    agent_id = %agent_id,
                    "trigger for unknown agent, dropping"
                );
                self.ack(
                    msg,
                    crate::trigger::trigger_id_in(msg.headers.as_ref()),
                    "unknown agent",
                )
                .await;
                return;
            }
        };

        // Admission (#278): a paused model starts no invocation. The
        // trigger is held here, un-acked, un-started and owning no
        // worker permit, until the pause ends; a drain or shutdown
        // meanwhile leaves it for the next binary.
        let header_id = crate::trigger::trigger_id_in(msg.headers.as_ref());
        if self.admit(msg, loaded.agent.model(), header_id).await
            == admission::Admission::Interrupted
        {
            return;
        }

        // Parse the payload as JSON. Empty body becomes null. Before the
        // per-agent cap below, so a poison payload is refused now rather
        // than after occupying a hold for however long the agent stays
        // full.
        let payload: serde_json::Value = if msg.payload.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_slice(&msg.payload) {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        agent_id = %agent_id,
                        error = %err,
                        "trigger payload is not valid JSON, dropping"
                    );
                    self.ack(
                        msg,
                        crate::trigger::trigger_id_in(msg.headers.as_ref()),
                        "invalid payload",
                    )
                    .await;
                    return;
                }
            }
        };

        // Admission, second rule (#718): an agent already running
        // `max_concurrent` invocations starts no more. The slot rides
        // the rest of `handle` and comes back through `Drop` — except on
        // a deferral, where `conclude` hands it on.
        let Some(agent_slot) = self
            .admit_agent_slot(msg, &agent_id, header_id, &payload)
            .await
        else {
            return;
        };

        // Last (#733): the worker permit. Both admission rules have
        // passed and the agent's slot is taken, so the only thing left
        // to wait for is a free worker. The wait keeps the delivery
        // alive on the same cadence a hold does, and an interrupted one
        // leaves it un-acked for the next binary, exactly as a pause
        // hold does. The permit is released when this returns.
        let Some(_permit) = self.acquire_run_permit(Some(msg), header_id).await else {
            return;
        };

        // The trigger's *first handling*: the message becomes a named
        // Trigger here and nowhere else on this path. A publisher's own
        // `Fq-Trigger-Id` header is honoured; a header-less external
        // trigger is assigned one now, because this is the moment the
        // system takes responsibility for it. Everything downstream —
        // the `triggered` event, the dead letter — reads the name off
        // this value rather than deciding again.
        let trigger = crate::trigger::delivered(msg, payload);
        let trigger_id = trigger.id;

        // Kept for the dead-letter path: an exhausted trigger's event
        // must carry what an operator needs to requeue or diagnose it.
        let trigger_payload = trigger.payload.clone();

        debug!(
            agent_id = %agent_id,
            subject = %msg.subject,
            trigger_id = %trigger_id,
            "dispatching trigger"
        );

        // Ack the trigger once the invocation has *durably started* —
        // signalled through the Worker seam after its first WAL write —
        // rather than at dispatch (issue #41). The trigger's job ends
        // once the run is recoverable: from the first WAL write on, the
        // reducer's three-state WAL owns in-flight durability and crash
        // recovery (`recovery::scan_in_flight` → `categorise` → resume),
        // so a crash resumes the invocation exactly once from the WAL
        // with no redelivered duplicate.
        //
        // Two failure windows this closes / preserves:
        //
        // - **Before durable start** — a crash here leaves the trigger
        //   unacked, so JetStream redelivers it after the ack-wait,
        //   recovering the otherwise-missed run. (This is the gap the
        //   old ack-on-dispatch left: a crash between dispatch and the
        //   first WAL write was a missed, re-triggerable run.)
        //
        // - **After durable start, before completion** — the ack fires
        //   at the first WAL write (seconds in), *well before* the
        //   invocation completes, so a long-running invocation is not
        //   redelivered and re-run past the 30s ack-wait. This preserves
        //   the redelivery-storm fix (M0 dogfood loop, 2026-07-06):
        //   holding the ack until completion re-ran invocations longer
        //   than the ack-wait — one trigger produced N invocations.
        //
        // The signal is fired at most once; if the invocation returns
        // before firing it (a permanent error before any WAL write, or a
        // worker that never signals), we ack on return — retrying a
        // permanent error would not help, and the run already happened.
        let delivery_attempt = u32::try_from(delivery_attempt).unwrap_or(1);
        let (durable_start, mut durably_started) = DurableStart::channel();
        let mut invocation = std::pin::pin!(self.worker.run_invocation(
            &loaded.agent,
            self.llm.as_ref(),
            trigger,
            Some(delivery_attempt),
            durable_start,
        ));

        let mut acked = false;
        let mut durable_start_observed = false;
        let result = loop {
            tokio::select! {
                biased;
                signal = &mut durably_started, if !acked => {
                    // `Ok` = fired (first WAL write landed); `Err` = the
                    // sender was dropped without firing (the invocation
                    // returned before its first WAL write). Either way,
                    // stop waiting on this branch; the ack below (on
                    // return) covers the drop case.
                    if let Ok(invocation_id) = signal {
                        durable_start_observed = true;
                        acked = self
                            .mark_durable_started(msg, stream_seq, trigger_id, invocation_id)
                            .await;
                    }
                }
                outcome = &mut invocation => break outcome,
            }
        };

        // The invocation returned before the durable-start signal fired,
        // so the run left nothing durable behind. Decide the trigger's
        // fate from the result:
        //
        // - **Transient** error (bus/store unavailable, a transient LLM
        //   error) — the run could succeed on a retry, so NAK: JetStream
        //   redelivers the trigger, recovering the otherwise-lost run.
        // - **Permanent** error (a terminal `failed` outcome / poison
        //   payload) or a clean `Ok` return — nothing to retry, so ACK.
        //   The `Failed` event already recorded why, and redelivering a
        //   poison trigger would loop under the consumer's unbounded
        //   redelivery.
        //
        if !acked && !durable_start_observed {
            match (trigger_fate(&result, delivery_attempt), &result) {
                (TriggerFate::DeadLetter, Err(err)) => {
                    self.dead_letter_exhausted(
                        &agent_id,
                        msg.subject.as_str(),
                        trigger_id,
                        &trigger_payload,
                        msg.info().map(|info| info.stream_sequence).unwrap_or(0),
                        delivery_attempt,
                        err,
                    )
                    .await;
                    self.ack(msg, Some(trigger_id), "transient retry limit exhausted")
                        .await;
                }
                (TriggerFate::Retry(delay), _) => {
                    self.nak(
                        msg,
                        trigger_id,
                        delay,
                        "transient failure before first WAL write",
                    )
                    .await;
                }
                _ => {
                    self.ack(
                        msg,
                        Some(trigger_id),
                        "invocation returned before durable start (permanent)",
                    )
                    .await;
                }
            }
        }

        self.conclude(agent_id, result, agent_slot);
    }

    fn log_executor_error(&self, err: &ExecutorError) {
        match err {
            ExecutorError::Llm(e) => error!(error = %e, "llm error during dispatch"),
            ExecutorError::Bus(e) => error!(error = %e, "bus error during dispatch"),
            ExecutorError::WorkerStore(msg) => {
                error!(error = %msg, "worker store error during dispatch")
            }
            ExecutorError::Resume(e) => error!(error = %e, "resume refused during dispatch"),
            ExecutorError::Workspace(e) => {
                error!(error = %e, "workspace error during dispatch")
            }
            ExecutorError::InvocationFailed { kind, message } => {
                error!(kind = ?kind, error = %message, "invocation failed during dispatch")
            }
        }
    }
}

/// Surface a spawned invocation task that failed to join. `handle`
/// never returns an error (failures are logged and acked/NAK'd
/// inside), so a join error means the task panicked or was cancelled —
/// loud, because a silently-vanished invocation is exactly the failure
/// mode #64 exists for.
fn log_invocation_task(joined: Result<(), tokio::task::JoinError>) {
    if let Err(err) = joined {
        error!(error = %err, "spawned invocation task did not complete cleanly");
    }
}

/// Errors that prevent the dispatcher from starting or progressing.
#[derive(Debug, thiserror::Error)]
pub enum DispatcherError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("trigger stream error: {0}")]
    Stream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, Sandbox};
    use crate::bus::TRIGGER_STREAM_NAME;
    use crate::events::{EventPayload, FailureKind, StopReason, TokenUsage};
    use crate::llm::ChatResponse;
    use crate::llm::fixture::FixtureClient;
    use crate::pricing::{ModelPricing, PricingTable};
    use crate::tools::ToolRegistry;
    use crate::trigger::Trigger;
    use crate::worker::{
        Harness, ReducerContext, ReducerRunner, RunnerConfig, WorkerId, WorkerStore,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::sync::OwnedSemaphorePermit;
    use uuid::Uuid;

    fn test_pricing() -> Arc<PricingTable> {
        let mut entries = HashMap::new();
        entries.insert(
            "claude-haiku".to_string(),
            ModelPricing {
                input_per_million: 1.0,
                output_per_million: 5.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
                cache_write_1h_per_million: None,
            },
        );
        Arc::new(PricingTable::from_map(entries))
    }

    fn test_tools() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::with_builtins())
    }

    fn sample_agent(name: &str) -> Agent {
        Agent::builder()
            .id(name)
            .model("claude-haiku")
            .system_prompt("You are a test agent.")
            .sandbox(Sandbox::new())
            .budget(1.0)
            .build()
            .unwrap()
    }

    fn canned_response() -> ChatResponse {
        ChatResponse {
            parts: crate::events::assistant_parts(
                None,
                vec![crate::events::MessageToolCall {
                    tool_call_id: crate::events::ToolCallId::new("report-outcome").unwrap(),
                    tool_name: crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
                    parameters: serde_json::json!({"status": "success", "summary": "Hello from the test agent."}),
                }],
            ),
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_5m_tokens: None,
                cache_write_1h_tokens: None,
                reasoning_tokens: None,
            },
            reported_cost_usd: None,
        }
    }

    fn unique_agent_id(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::now_v7().simple())
    }

    /// A worker-cap permit off the dispatcher's own semaphore, for the
    /// tests that need the worker cap to be *occupied* — nothing else
    /// takes one until a trigger is ready to run (#733), so this is how
    /// a test puts a dispatcher at its cap without running anything.
    async fn a_permit(d: &TriggerDispatcher) -> OwnedSemaphorePermit {
        Arc::clone(&d.permits)
            .acquire_owned()
            .await
            .expect("dispatcher semaphore is never closed")
    }

    /// The claim key a dispatcher on `bus` would use for `stream_seq`:
    /// the trigger stream, the incarnation this broker created, and the
    /// sequence. A test that seeds a claim by hand has to agree with the
    /// dispatcher about the epoch or it would be seeding a row for a
    /// different stream.
    fn test_key(bus: &EventBus, stream_seq: u64) -> crate::control_plane::TriggerKey<'static> {
        crate::control_plane::TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: bus.trigger_stream_epoch(),
            stream_seq,
        }
    }

    fn unique_consumer_name() -> String {
        format!("fq-dispatcher-test-{}", Uuid::now_v7().simple())
    }

    /// A variant of TriggerDispatcher that uses a custom consumer
    /// name AND a narrow filter subject so parallel test runs do
    /// not compete for each other's messages on the work-queue
    /// trigger stream. Drives the *production* loop via
    /// `run_on_consumer`, so every run-based test exercises the
    /// fan-out path (#70).
    struct TestDispatcher {
        bus: EventBus,
        registry: SharedRegistry,
        worker: Arc<dyn Worker>,
        llm: Arc<dyn LlmClient>,
        consumer_name: String,
        filter_subject: String,
        max_concurrent: usize,
    }

    impl TestDispatcher {
        async fn run(self, shutdown: oneshot::Receiver<()>) -> Result<(), DispatcherError> {
            let TestDispatcher {
                bus,
                registry,
                worker,
                llm,
                consumer_name,
                filter_subject,
                max_concurrent,
            } = self;
            TriggerDispatcher::new(bus, registry, worker, llm, max_concurrent)
                .run_on_consumer(&consumer_name, Some(&filter_subject), shutdown)
                .await
        }
    }

    /// The whole pre-WAL retry policy at every boundary: transient
    /// errors retry with the escalating schedule, the final delivery
    /// dead-letters, permanent errors and clean returns consume.
    #[test]
    fn trigger_fate_covers_retry_deadletter_and_consume() {
        use crate::bus::TRIGGER_RETRY_BACKOFF;
        let transient = || {
            Err(ExecutorError::Bus(crate::bus::BusError::Publish(
                "broker away".to_string(),
            )))
        };
        let permanent = || {
            Err(ExecutorError::InvocationFailed {
                kind: crate::events::FailureKind::RuntimeError,
                message: "poison".to_string(),
            })
        };

        // Deliveries 1..limit retry, each with its scheduled delay.
        for attempt in 1..TRIGGER_MAX_DELIVER as u32 {
            let idx = (attempt as usize - 1).min(TRIGGER_RETRY_BACKOFF.len() - 1);
            assert_eq!(
                trigger_fate(&transient(), attempt),
                TriggerFate::Retry(TRIGGER_RETRY_BACKOFF[idx]),
                "attempt {attempt}"
            );
        }
        // The final delivery dead-letters instead of NAKing (a NAK
        // there would end redelivery with no event).
        assert_eq!(
            trigger_fate(&transient(), TRIGGER_MAX_DELIVER as u32),
            TriggerFate::DeadLetter
        );
        // Past the bound (defensive: the server should not deliver
        // again) still dead-letters rather than retrying.
        assert_eq!(
            trigger_fate(&transient(), TRIGGER_MAX_DELIVER as u32 + 1),
            TriggerFate::DeadLetter
        );
        // Permanent errors never retry, at any attempt.
        assert_eq!(trigger_fate(&permanent(), 1), TriggerFate::Consume);
        assert_eq!(
            trigger_fate(&permanent(), TRIGGER_MAX_DELIVER as u32),
            TriggerFate::Consume
        );
        // A clean return consumes.
        assert_eq!(
            trigger_fate(
                &Ok(crate::worker::InvocationOutcome::Suspended {
                    invocation_id: Uuid::now_v7(),
                }),
                1
            ),
            TriggerFate::Consume
        );
    }

    /// The NAK schedule escalates and saturates at the last entry.
    #[test]
    fn trigger_retry_backoff_escalates_and_saturates() {
        use crate::bus::TRIGGER_RETRY_BACKOFF;
        assert_eq!(trigger_retry_backoff(1), TRIGGER_RETRY_BACKOFF[0]);
        assert_eq!(trigger_retry_backoff(2), TRIGGER_RETRY_BACKOFF[1]);
        assert_eq!(trigger_retry_backoff(4), TRIGGER_RETRY_BACKOFF[3]);
        // Saturates rather than panicking past the schedule.
        assert_eq!(trigger_retry_backoff(40), TRIGGER_RETRY_BACKOFF[3]);
        // Attempt 0 (info unavailable defaults to 1 upstream) is safe.
        assert_eq!(trigger_retry_backoff(0), TRIGGER_RETRY_BACKOFF[0]);
    }

    #[test]
    fn agent_id_from_subject_happy_path() {
        assert_eq!(
            agent_id_from_subject("fq.trigger.researcher"),
            Some("researcher")
        );
        assert_eq!(
            agent_id_from_subject("fq.trigger.some-agent-id-123"),
            Some("some-agent-id-123")
        );
    }

    #[test]
    fn agent_id_from_subject_rejects_unexpected_prefix() {
        assert!(agent_id_from_subject("fq.agent.researcher.triggered").is_none());
        assert!(agent_id_from_subject("bad.prefix.name").is_none());
        assert!(agent_id_from_subject("fq.trigger.").is_none());
    }

    /// End-to-end: publish a trigger to NATS, run the dispatcher,
    /// verify that the agent's events appear in the event stream.
    #[tokio::test]
    async fn dispatcher_executes_published_trigger() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        use crate::bus::EventBus;
        use crate::control_plane::projection::store::{EventFilter, ProjectionStore};

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("dispatch-test");

        // Build an in-memory registry with a single agent.
        let mut registry = AgentRegistry::new();
        // We need the registry's load_file API, but there's no
        // direct insert; write a tempfile and load it.
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                r#"---
name: {agent_id_str}
model: claude-haiku
budget: 1.0
---

You are a test agent."#
            ),
        )
        .unwrap();
        registry.load_file(&agent_path);
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );
        let registry = shared_registry(registry);

        // Fake LLM: always returns the canned response.
        let llm = Arc::new({
            let c = FixtureClient::new();
            // Push enough responses for a few possible retries.
            for _ in 0..5 {
                c.push_response(canned_response());
            }
            c
        });

        let worker_store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .unwrap(),
        );
        let worker_id = WorkerId::new(format!("dispatcher-test-{}", Uuid::now_v7().simple()))
            .expect("worker id");
        let worker: Arc<dyn Worker> = Arc::new(ReducerRunner::new(
            Arc::new(ReducerContext::builder().tools(test_tools()).build()),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(worker_store)
                    .worker_id(worker_id)
                    .build(),
            ),
            Harness::new(),
        ));

        // Projection store, so we can verify events landed.
        let store = Arc::new(
            ProjectionStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );

        // Spawn a projection consumer so events are materialised.
        let proj_consumer =
            crate::control_plane::projection::ProjectionConsumer::new(bus.clone(), store.clone());
        let (proj_tx, proj_rx) = oneshot::channel();
        let proj_handle = tokio::spawn(async move { proj_consumer.run(proj_rx).await });

        // Spawn the dispatcher with a filter scoped to just this
        // test's agent id, so parallel tests do not compete for
        // each other's messages on the work-queue stream.
        let dispatcher = TestDispatcher {
            bus: bus.clone(),
            registry: registry.clone(),
            worker: worker.clone(),
            llm: llm.clone(),
            consumer_name: unique_consumer_name(),
            filter_subject: crate::events::subjects::trigger(&agent_id_str),
            max_concurrent: 1,
        };
        let (disp_tx, disp_rx) = oneshot::channel();
        let disp_handle = tokio::spawn(async move { dispatcher.run(disp_rx).await });

        // Give consumers a moment to register before publishing.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Publish a trigger.
        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({"input": "hi"}),
        )
        .await
        .expect("publish trigger");

        // Wait for events to land in the projection.
        let agent_id = AgentId::new(&agent_id_str).unwrap();
        let _ = agent_id; // (used by filter below via &agent_id_str)
        let filter = EventFilter {
            agent: Some(&agent_id_str),
            ..Default::default()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let rows = store.query_events(&filter, 100).await.unwrap();
            let has_triggered = rows.iter().any(|r| r.event_type == "triggered");
            let has_completed = rows.iter().any(|r| r.event_type == "completed");
            if has_triggered && has_completed {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!(
                    "timed out waiting for dispatched events; got {} rows",
                    rows.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Shut down.
        let _ = disp_tx.send(());
        let _ = proj_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), disp_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), proj_handle).await;

        // The llm should have seen at least one call.
        assert!(
            !llm.requests().is_empty(),
            "fixture client received no requests"
        );
    }

    // Silence unused warnings for the helper fn when no NATS.
    #[allow(dead_code)]
    fn _suppress_unused() {
        let _ = sample_agent("x");
    }

    /// Dispatch one trigger end to end and hand back the `triggered`
    /// payload the invocation recorded.
    ///
    /// `stamp` decides how the trigger reaches the stream: `Some(id)`
    /// publishes it named, the way the daemon's `trigger.publish` does;
    /// `None` publishes the wire contract's bare minimum — a subject and
    /// a JSON body and nothing else — which is what a header-less
    /// external publisher sends.
    async fn triggered_payload_for(stamp: Option<Uuid>) -> crate::events::TriggeredPayload {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("trigger-identity");
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nYou are a test agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let llm = Arc::new({
            let c = FixtureClient::new();
            for _ in 0..5 {
                c.push_response(canned_response());
            }
            c
        });
        let worker_store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .unwrap(),
        );
        let worker: Arc<dyn Worker> = Arc::new(ReducerRunner::new(
            Arc::new(ReducerContext::builder().tools(test_tools()).build()),
            Arc::new(
                RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(test_pricing())
                    .store(worker_store)
                    .worker_id(
                        WorkerId::new(format!("trigger-identity-{}", Uuid::now_v7().simple()))
                            .expect("worker id"),
                    )
                    .build(),
            ),
            Harness::new(),
        ));
        let dispatcher = TestDispatcher {
            bus: bus.clone(),
            registry: shared_registry(registry),
            worker,
            llm,
            consumer_name: unique_consumer_name(),
            filter_subject: crate::events::subjects::trigger(&agent_id_str),
            max_concurrent: 1,
        };
        let (disp_tx, disp_rx) = oneshot::channel();
        let disp_handle = tokio::spawn(async move { dispatcher.run(disp_rx).await });
        // Let the durable consumer register before publishing.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let body = json!({"input": "hi"});
        match stamp {
            Some(id) => {
                bus.publish_trigger_named(&AgentId::new(&agent_id_str).unwrap(), id, &body)
                    .await
                    .expect("publish named trigger");
            }
            None => {
                bus.jetstream()
                    .publish(
                        crate::events::subjects::trigger(&agent_id_str),
                        bytes::Bytes::from(serde_json::to_vec(&body).unwrap()),
                    )
                    .await
                    .expect("publish")
                    .await
                    .expect("ack");
            }
        }

        let stream = bus
            .jetstream()
            .get_stream(crate::bus::STREAM_NAME)
            .await
            .expect("event stream");
        let subject = crate::events::subjects::agent_triggered(&agent_id_str);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let triggered = loop {
            if let Ok(raw) = stream.get_last_raw_message_by_subject(&subject).await {
                let event: crate::events::Event =
                    serde_json::from_slice(&raw.payload).expect("event parses");
                match event.payload {
                    EventPayload::Triggered(p) => break p,
                    other => panic!("expected Triggered, got {other:?}"),
                }
            }
            assert!(
                tokio::time::Instant::now() <= deadline,
                "timed out waiting for the triggered event"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let _ = disp_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), disp_handle).await;
        triggered
    }

    /// An inbound trigger that already carries an identity keeps it end
    /// to end. Re-minting here would fork the name of a trigger its
    /// publisher can already refer to.
    #[tokio::test]
    async fn a_named_trigger_keeps_its_publisher_s_identity() {
        let stamped = Uuid::now_v7();
        let triggered = triggered_payload_for(Some(stamped)).await;
        assert_eq!(
            triggered.trigger_id,
            Some(stamped),
            "the invocation must name the trigger it came from, not a fresh id"
        );
    }

    /// A header-less external trigger — everything the wire contract
    /// requires and nothing more — is assigned an identity at the
    /// dispatcher's first handling, and the invocation records *that*
    /// id.
    #[tokio::test]
    async fn a_header_less_trigger_is_assigned_an_identity() {
        let triggered = triggered_payload_for(None).await;
        let id = triggered
            .trigger_id
            .expect("an unnamed trigger is named on first handling");
        assert_eq!(id.get_version_num(), 7, "assigned ids are UUIDv7");
    }

    /// Regression (M0 dogfood loop, 2026-07-06): the trigger is acked as
    /// soon as the invocation is *dispatched*, not when it *completes* —
    /// so an invocation longer than the consumer's 30s ack-wait is not
    /// redelivered and re-run. A worker that blocks the invocation
    /// in-flight lets us assert the trigger has already been acked
    /// (`num_ack_pending` → 0) without waiting real seconds. With the old
    /// ack-on-completion behaviour this times out (the message stays
    /// unacked while the invocation runs).
    #[tokio::test]
    async fn started_claim_acks_duplicate_without_second_invocation() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        struct BlockingWorker {
            started: Arc<AtomicUsize>,
            release: Arc<Notify>,
            /// The invocation id this worker's first WAL write carries,
            /// fixed so the test can read it back off the claim row.
            wal_invocation: Uuid,
        }
        #[async_trait::async_trait]
        impl Worker for BlockingWorker {
            async fn run_invocation(
                &self,
                _agent: &Agent,
                _llm: &dyn crate::llm::LlmClient,
                _trigger: Trigger,
                _delivery_attempt: Option<u32>,
                mut durable_start: crate::worker::DurableStart,
            ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
                self.started.fetch_add(1, Ordering::SeqCst);
                // Simulate the first WAL write landing: this is what lets
                // the dispatcher ack while the invocation is still
                // in-flight (issue #41).
                durable_start.fire(self.wal_invocation);
                self.release.notified().await;
                Ok(crate::worker::InvocationOutcome::Completed {
                    invocation_id: Uuid::now_v7(),
                    response: canned_response(),
                    cost: 0.0,
                    duration_ms: 0,
                })
            }

            async fn request_drain(&self, _req: crate::worker::DrainRequest) {}

            fn drain_status(&self) -> crate::worker::DrainState {
                crate::worker::DrainState::Running
            }
        }

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("ack-before-run");

        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let durable_invocation_id = Uuid::now_v7();
        let worker: Arc<dyn Worker> = Arc::new(BlockingWorker {
            started: started.clone(),
            release: release.clone(),
            wal_invocation: durable_invocation_id,
        });
        let claim_dir = tempfile::tempdir().unwrap();
        let claim_store = Arc::new(
            crate::control_plane::ControlPlaneStore::open(
                &claim_dir.path().join("control-plane.db"),
            )
            .await
            .unwrap(),
        );
        let dispatcher = Arc::new(
            TriggerDispatcher::new(
                bus.clone(),
                shared_registry(registry),
                worker,
                Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
                2,
            )
            .with_trigger_claims(claim_store.clone(), "worker-a"),
        );

        let mut consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::events::subjects::trigger(&agent_id_str),
                crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .expect("consumer");

        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({"input": "hi"}),
        )
        .await
        .expect("publish");
        let msg = {
            let mut stream = consumer.messages().await.expect("messages");
            tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("a message within 5s")
                .expect("stream open")
                .expect("message ok")
        };

        let duplicate = msg.clone();
        let duplicate_seq = duplicate.info().unwrap().stream_sequence;
        let d = dispatcher.clone();
        let handle = tokio::spawn(async move { d.handle(&msg).await });

        // Wait until the invocation has actually entered (and blocked).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while started.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "invocation never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "invocation ran more than once"
        );

        // The invocation is blocked in-flight; the trigger must already
        // be acked (num_ack_pending drops to 0). Poll to absorb ack
        // propagation; a stuck 1 is the redelivery-storm regression.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pending = consumer.info().await.expect("info").num_ack_pending;
            if pending == 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "trigger not acked while the invocation is still in-flight \
                 (num_ack_pending={pending}) — the redelivery-storm regression"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Feed the same stream message through `handle` again, modelling
        // the incident's attempt-2 copy after the WAL write. The durable
        // claim must ack it without starting a second invocation.
        //
        // Under a timeout because the failure this pins has two faces: a
        // duplicate that runs, and a duplicate that *waits* — skip
        // `mark_trigger_started` and the row still reads `claimed`, so
        // this copy takes the same-worker `Held` arm and, driven from the
        // run loop, parks. Timed out here, that reads as a failure of
        // this test rather than as a suite that hangs for a minute.
        tokio::time::timeout(Duration::from_secs(5), dispatcher.handle(&duplicate))
            .await
            .expect("the duplicate is answered rather than held");
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "duplicate was refused before invocation"
        );
        // The row is what made that answer: `durably_started` is written
        // before the first copy's ack, and reading it back is how a
        // missing `mark_trigger_started` fails an assertion here instead
        // of turning into a hang somewhere else.
        assert_eq!(
            claim_store
                .claim_trigger(test_key(&bus, duplicate_seq), "worker-a", 0)
                .await
                .unwrap(),
            crate::control_plane::TriggerClaim::Started {
                invocation_id: durable_invocation_id.to_string()
            },
            "the claim must record the durable start, with the WAL's invocation id"
        );
        assert_eq!(
            bus.consumer_ledger()
                .record(CONSUMER_NAME)
                .duplicate_dropped,
            1,
            "duplicate drop is exposed in the consumer ledger"
        );

        release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// Issue #41: the trigger is acked only *after* the invocation's
    /// first durable (WAL) write, signalled through the `Worker` seam.
    /// A worker that blocks in-flight *without* firing the durable-start
    /// signal models a crash in the ack->first-WAL-write window: the
    /// trigger must stay unacked (`num_ack_pending` stuck at 1) so
    /// JetStream can redeliver it. With the old ack-on-dispatch
    /// behaviour this would drop to 0 immediately — the missed-run gap.
    #[tokio::test]
    async fn trigger_is_not_acked_before_the_first_wal_write() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        // Enters the invocation and blocks, but never fires
        // durable_start — i.e. it dies before its first WAL write.
        struct NeverSignalsWorker {
            started: Arc<AtomicUsize>,
            release: Arc<Notify>,
        }
        #[async_trait::async_trait]
        impl Worker for NeverSignalsWorker {
            async fn run_invocation(
                &self,
                _agent: &Agent,
                _llm: &dyn crate::llm::LlmClient,
                _trigger: Trigger,
                _delivery_attempt: Option<u32>,
                _durable_start: crate::worker::DurableStart,
            ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
                self.started.fetch_add(1, Ordering::SeqCst);
                // Never fire the signal; just block. Dropping
                // `_durable_start` here (on return) would signal nothing.
                self.release.notified().await;
                Ok(crate::worker::InvocationOutcome::Completed {
                    invocation_id: Uuid::now_v7(),
                    response: canned_response(),
                    cost: 0.0,
                    duration_ms: 0,
                })
            }

            async fn request_drain(&self, _req: crate::worker::DrainRequest) {}

            fn drain_status(&self) -> crate::worker::DrainState {
                crate::worker::DrainState::Running
            }
        }

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("no-ack-before-wal");

        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let worker: Arc<dyn Worker> = Arc::new(NeverSignalsWorker {
            started: started.clone(),
            release: release.clone(),
        });
        let dispatcher = Arc::new(TriggerDispatcher::new(
            bus.clone(),
            shared_registry(registry),
            worker,
            Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
            1,
        ));

        let mut consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::events::subjects::trigger(&agent_id_str),
                crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .expect("consumer");

        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({"input": "hi"}),
        )
        .await
        .expect("publish");
        let msg = {
            let mut stream = consumer.messages().await.expect("messages");
            tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("a message within 5s")
                .expect("stream open")
                .expect("message ok")
        };

        let d = dispatcher.clone();
        let handle = tokio::spawn(async move { d.handle(&msg).await });

        // Wait until the invocation is in-flight (and blocked).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while started.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "invocation never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The invocation is blocked before any WAL write and never fired
        // the durable-start signal: the trigger must remain unacked. Poll
        // for a stretch to absorb ack propagation; it must stay at 1.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
        while tokio::time::Instant::now() < deadline {
            let pending = consumer.info().await.expect("info").num_ack_pending;
            assert_eq!(
                pending, 1,
                "trigger must stay unacked before the first WAL write \
                 (redeliverable) — got num_ack_pending={pending}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Let it finish so the spawned task and consumer tear down.
        release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// Which error `FailsBeforeWalWorker` returns before the first WAL
    /// write, exercising the dispatcher's ACK (permanent) / NAK
    /// (transient) split (#41 / #46).
    #[derive(Clone, Copy)]
    enum PreWalFailure {
        /// A retryable store outage — must NAK for redelivery.
        TransientStore,
        /// A permanent poison payload — must ACK.
        PermanentInvocation,
        /// A resume protocol verdict redelivery cannot heal — must ACK.
        AmbiguousWalResume,
    }

    impl PreWalFailure {
        fn label(self) -> &'static str {
            match self {
                Self::TransientStore => "pre-wal-transient",
                Self::PermanentInvocation => "pre-wal-permanent",
                Self::AmbiguousWalResume => "pre-wal-ambiguous-wal",
            }
        }

        fn error(self) -> ExecutorError {
            match self {
                Self::TransientStore => {
                    ExecutorError::WorkerStore("simulated transient store outage".to_string())
                }
                Self::PermanentInvocation => ExecutorError::InvocationFailed {
                    kind: crate::events::FailureKind::RuntimeError,
                    message: "permanent poison payload".to_string(),
                },
                Self::AmbiguousWalResume => {
                    ExecutorError::Resume(crate::worker::ResumeError::AmbiguousWal {
                        invocation_id: Uuid::nil(),
                    })
                }
            }
        }
    }

    /// A worker that returns an error *before* firing `durable_start` —
    /// models a failure before the first WAL write. `failure` picks which
    /// error that is, exercising the dispatcher's ACK (permanent) / NAK
    /// (transient) split (#41 / #46).
    struct FailsBeforeWalWorker {
        failure: PreWalFailure,
    }

    #[async_trait::async_trait]
    impl Worker for FailsBeforeWalWorker {
        async fn run_invocation(
            &self,
            _agent: &Agent,
            _llm: &dyn crate::llm::LlmClient,
            _trigger: Trigger,
            _delivery_attempt: Option<u32>,
            _durable_start: crate::worker::DurableStart,
        ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
            // Return without firing durable_start (i.e. before any WAL
            // write). A transient error should be NAK'd for redelivery, a
            // permanent one ACK'd.
            Err(self.failure.error())
        }

        async fn request_drain(&self, _req: crate::worker::DrainRequest) {}

        fn drain_status(&self) -> crate::worker::DrainState {
            crate::worker::DrainState::Running
        }
    }

    /// Dispatch a trigger whose invocation fails before the first WAL
    /// write, and report whether the trigger was **redelivered** (NAK)
    /// rather than consumed (ACK). Requires a live broker.
    async fn dispatched_pre_wal_failure_is_redelivered(failure: PreWalFailure) -> bool {
        use std::sync::Arc;
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id(failure.label());

        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let worker: Arc<dyn Worker> = Arc::new(FailsBeforeWalWorker { failure });
        let dispatcher = Arc::new(TriggerDispatcher::new(
            bus.clone(),
            shared_registry(registry),
            worker,
            Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
            1,
        ));

        let consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::events::subjects::trigger(&agent_id_str),
                crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .expect("consumer");

        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({"input": "hi"}),
        )
        .await
        .expect("publish");
        let mut stream = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a message within 5s")
            .expect("stream open")
            .expect("message ok");

        // The worker returns its error before any WAL write; `handle`
        // classifies it and ACKs (permanent) or NAKs (transient).
        dispatcher.handle(&msg).await;

        // A NAK redelivers the trigger; an ACK consumes it. Re-poll the
        // same stream: a redelivered message means it was NAK'd. The
        // window covers the configured first-retry backoff with margin.
        let redelivery_timeout = crate::bus::TRIGGER_RETRY_BACKOFF[0] + Duration::from_secs(3);
        match tokio::time::timeout(redelivery_timeout, stream.next()).await {
            Ok(Some(Ok(redelivered))) => {
                // Ack the redelivery so it doesn't churn after the test.
                let _ = redelivered.ack().await;
                true
            }
            _ => false,
        }
    }

    /// #41 / #46: a *transient* failure before the first WAL write NAKs
    /// the trigger so JetStream redelivers it — the otherwise-lost run is
    /// retried, not dropped.
    #[tokio::test]
    async fn transient_failure_before_first_wal_write_naks_for_redelivery() {
        assert!(
            dispatched_pre_wal_failure_is_redelivered(PreWalFailure::TransientStore).await,
            "a transient pre-WAL failure must NAK (redeliver) the trigger"
        );
    }

    /// #41 / #46: a *permanent* failure before the first WAL write ACKs
    /// the trigger — retrying a poison run would loop under the unbounded
    /// consumer, and the Failed event already recorded why.
    #[tokio::test]
    async fn permanent_failure_before_first_wal_write_acks() {
        assert!(
            !dispatched_pre_wal_failure_is_redelivered(PreWalFailure::PermanentInvocation).await,
            "a permanent pre-WAL failure must ACK (consume) the trigger"
        );
    }

    /// An ambiguous-WAL resume verdict ACKs the trigger: redelivery cannot
    /// heal the protocol state and would loop under an unbounded consumer.
    #[tokio::test]
    async fn ambiguous_wal_resume_verdict_acks_without_redelivery() {
        assert!(
            !dispatched_pre_wal_failure_is_redelivered(PreWalFailure::AmbiguousWalResume).await,
            "an ambiguous-WAL resume verdict must ACK (consume) the trigger"
        );
    }

    /// #49: the dead-letter surface end-to-end against a live broker —
    /// the event lands on the bus with the distinguishable kind and the
    /// annotations an operator needs to requeue or diagnose the trigger.
    /// (The full five-delivery exhaustion loop is not driven live: the
    /// escalating backoff makes it minutes long; the decision that
    /// routes into this path is the pure `trigger_fate`, tested above.)
    #[tokio::test]
    async fn dead_letter_event_carries_kind_and_trigger_annotations() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        use std::sync::Arc;
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("dead-letter-shape");
        let dispatcher = TriggerDispatcher::new(
            bus.clone(),
            shared_registry(AgentRegistry::new()),
            Arc::new(FailsBeforeWalWorker {
                failure: PreWalFailure::TransientStore,
            }) as Arc<dyn Worker>,
            Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
            1,
        );

        let agent_id = crate::agent::AgentId::new(&agent_id_str).expect("agent id");
        let trigger_subject = crate::events::subjects::trigger(&agent_id_str);
        let trigger_payload = json!({"input": "hi"});
        let trigger_id = Uuid::now_v7();
        dispatcher
            .dead_letter_exhausted(
                &agent_id,
                &trigger_subject,
                trigger_id,
                &trigger_payload,
                4242,
                TRIGGER_MAX_DELIVER as u32,
                &ExecutorError::WorkerStore("simulated persistent outage".to_string()),
            )
            .await;

        let stream = bus
            .jetstream()
            .get_stream(crate::bus::STREAM_NAME)
            .await
            .expect("event stream");
        let raw = stream
            .get_last_raw_message_by_subject(&format!("fq.agent.{agent_id_str}.failed"))
            .await
            .expect("dead-letter event on the bus");
        let event: crate::events::Event =
            serde_json::from_slice(&raw.payload).expect("event parses");

        match &event.payload {
            EventPayload::Failed(p) => {
                assert!(
                    matches!(p.error_kind, FailureKind::TriggerExhausted),
                    "kind must be distinguishable, got {:?}",
                    p.error_kind
                );
                assert!(
                    p.error_message.contains("after 5 deliveries (limit 5)"),
                    "got: {}",
                    p.error_message
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            event.annotations.0.get("trigger_subject"),
            Some(&serde_json::Value::String(trigger_subject)),
            "annotations must carry the subject a requeue needs"
        );
        assert_eq!(
            event.annotations.0.get("trigger_payload"),
            Some(&trigger_payload),
            "annotations must carry the payload a requeue needs"
        );
        assert_eq!(
            event.annotations.0.get("trigger_stream_seq"),
            Some(&json!(4242)),
            "the stream sequence is the inline/advisory reconciliation key"
        );
        assert_eq!(
            event.annotations.0.get("trigger_id"),
            Some(&json!(trigger_id.to_string())),
            "the dead letter names the trigger that died — the key an \
             idempotent requeue tests"
        );
        assert_eq!(
            event.annotations.0.get("dead_letter_source"),
            Some(&json!("inline"))
        );
    }

    /// #49: a durable created by a pre-bound binary gets its retry
    /// policy upgraded on open — the dogfood deployment's existing
    /// consumer must not keep unbounded redelivery.
    #[tokio::test]
    async fn preexisting_consumer_is_upgraded_to_the_delivery_bound() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let name = unique_consumer_name();
        let filter = crate::events::subjects::trigger(&unique_agent_id("upgrade-path"));

        // Create the durable the way pre-#49 binaries did: no
        // max_deliver, no backoff.
        let stream = bus
            .jetstream()
            .get_stream(crate::bus::TRIGGER_STREAM_NAME)
            .await
            .expect("trigger stream");
        stream
            .get_or_create_consumer(
                &name,
                async_nats::jetstream::consumer::pull::Config {
                    durable_name: Some(name.clone()),
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    filter_subject: filter.clone(),
                    max_ack_pending: crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
                    ..Default::default()
                },
            )
            .await
            .expect("legacy consumer");

        let mut upgraded = bus
            .trigger_consumer_with_filter(&name, &filter, crate::bus::NATS_DEFAULT_MAX_ACK_PENDING)
            .await
            .expect("open upgrades the durable");
        let info = upgraded.info().await.expect("info");
        assert_eq!(info.config.max_deliver, TRIGGER_MAX_DELIVER);
        assert_eq!(
            info.config.backoff,
            crate::bus::TRIGGER_RETRY_BACKOFF.to_vec()
        );
    }

    /// ADR-0027: once the worker is draining, the dispatcher stops
    /// consuming — it exits its consume loop on its own (no shutdown
    /// signal needed) and never dispatches an already-available trigger.
    /// The trigger is left un-acked on the durable work-queue consumer,
    /// so JetStream retains it for the next binary.
    #[tokio::test]
    async fn a_draining_dispatcher_stops_consuming_triggers() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        use crate::bus::EventBus;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // Counts dispatches; reports drain state from a flag we control.
        struct CountingWorker {
            calls: Arc<AtomicUsize>,
            draining: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl Worker for CountingWorker {
            async fn run_invocation(
                &self,
                _agent: &Agent,
                _llm: &dyn crate::llm::LlmClient,
                _trigger: Trigger,
                _delivery_attempt: Option<u32>,
                _durable_start: crate::worker::DurableStart,
            ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::worker::InvocationOutcome::Completed {
                    invocation_id: Uuid::now_v7(),
                    response: canned_response(),
                    cost: 0.0,
                    duration_ms: 0,
                })
            }
            async fn request_drain(&self, _req: crate::worker::DrainRequest) {
                self.draining.store(true, Ordering::SeqCst);
            }
            fn drain_status(&self) -> crate::worker::DrainState {
                if self.draining.load(Ordering::SeqCst) {
                    crate::worker::DrainState::Draining
                } else {
                    crate::worker::DrainState::Running
                }
            }
        }

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("drain-stop");

        let mut registry = AgentRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        registry.load_file(&agent_path);
        let registry = shared_registry(registry);

        let llm: Arc<dyn LlmClient> = Arc::new({
            let c = FixtureClient::new();
            for _ in 0..5 {
                c.push_response(canned_response());
            }
            c
        });

        // A trigger is waiting on the stream before the dispatcher runs.
        let calls = Arc::new(AtomicUsize::new(0));
        let worker: Arc<dyn Worker> = Arc::new(CountingWorker {
            calls: calls.clone(),
            draining: Arc::new(AtomicBool::new(true)), // draining from the start
        });
        let dispatcher = TestDispatcher {
            bus: bus.clone(),
            registry,
            worker,
            llm,
            consumer_name: unique_consumer_name(),
            filter_subject: crate::events::subjects::trigger(&agent_id_str),
            max_concurrent: 1,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move { dispatcher.run(shutdown_rx).await });

        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({ "input": "hi" }),
        )
        .await
        .expect("publish trigger");

        // The draining dispatcher must exit on its own — without us ever
        // sending `shutdown` — and without dispatching the waiting trigger.
        let joined = tokio::time::timeout(Duration::from_secs(3), handle).await;
        assert!(
            joined.is_ok(),
            "a draining dispatcher must exit on its own (top-of-loop drain break), \
             not block waiting for triggers"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a draining dispatcher must not dispatch an available trigger"
        );
        drop(shutdown_tx);
    }

    /// A worker double whose invocations block on a semaphore gate, so
    /// the test controls exactly when each in-flight invocation ends.
    struct GatedWorker {
        started: Arc<std::sync::atomic::AtomicUsize>,
        gate: Arc<tokio::sync::Semaphore>,
    }
    #[async_trait::async_trait]
    impl Worker for GatedWorker {
        async fn run_invocation(
            &self,
            _agent: &Agent,
            _llm: &dyn crate::llm::LlmClient,
            _trigger: Trigger,
            _delivery_attempt: Option<u32>,
            mut durable_start: crate::worker::DurableStart,
        ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            durable_start.fire(uuid::Uuid::now_v7());
            self.gate.acquire().await.expect("gate open").forget();
            Ok(crate::worker::InvocationOutcome::Completed {
                invocation_id: Uuid::now_v7(),
                response: canned_response(),
                cost: 0.0,
                duration_ms: 0,
            })
        }
        async fn request_drain(&self, _req: crate::worker::DrainRequest) {}
        fn drain_status(&self) -> crate::worker::DrainState {
            crate::worker::DrainState::Running
        }
    }

    /// Shared scaffolding for the fan-out tests: a registry with one
    /// unique agent, a gated worker, and a TestDispatcher running the
    /// production loop at the given concurrency bound. Returns the
    /// pieces the test drives.
    async fn gated_fanout_world(
        url: &str,
        prefix: &str,
        max_concurrent: usize,
    ) -> (
        EventBus,
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<tokio::sync::Semaphore>,
        tokio::task::JoinHandle<Result<(), DispatcherError>>,
        oneshot::Sender<()>,
    ) {
        let bus = EventBus::connect(url).await.expect("connect NATS");
        let agent_id_str = unique_agent_id(prefix);

        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());

        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let worker: Arc<dyn Worker> = Arc::new(GatedWorker {
            started: started.clone(),
            gate: gate.clone(),
        });

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let dispatcher = TestDispatcher {
            bus: bus.clone(),
            registry: shared_registry(registry),
            worker,
            llm: Arc::new(FixtureClient::new()) as Arc<dyn crate::llm::LlmClient>,
            consumer_name: unique_consumer_name(),
            filter_subject: crate::events::subjects::trigger(&agent_id_str),
            max_concurrent,
        };
        let run = tokio::spawn(dispatcher.run(shutdown_rx));
        (bus, agent_id_str, started, gate, run, shutdown_tx)
    }

    async fn wait_for_started(started: &std::sync::atomic::AtomicUsize, want: usize, note: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while started.load(std::sync::atomic::Ordering::SeqCst) < want {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {want} started invocations ({note})"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// #70: with `max_concurrent = 2`, two triggers overlap — the
    /// second invocation *starts* while the first is still blocked.
    /// Releasing both and shutting down exercises the join phase.
    #[tokio::test]
    async fn fan_out_runs_invocations_concurrently() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let (bus, agent_id, started, gate, run, shutdown_tx) =
            gated_fanout_world(&url, "fanout-two", 2).await;

        bus.publish_trigger(&AgentId::new(&agent_id).unwrap(), &json!({"input": "a"}))
            .await
            .expect("publish 1");
        bus.publish_trigger(&AgentId::new(&agent_id).unwrap(), &json!({"input": "b"}))
            .await
            .expect("publish 2");

        // Both invocations enter while neither has finished (the gate
        // holds zero permits) — genuine overlap inside one dispatcher.
        wait_for_started(&started, 2, "concurrent fan-out").await;

        gate.add_permits(2);
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("dispatcher joins in-flight work and exits")
            .expect("task joins");
        assert!(result.is_ok(), "{result:?}");
    }

    /// #70: the default bound of 1 reproduces the serial behavior —
    /// the second trigger is not pulled until the first invocation
    /// finished.
    #[tokio::test]
    async fn serial_bound_runs_one_invocation_at_a_time() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let (bus, agent_id, started, gate, run, shutdown_tx) =
            gated_fanout_world(&url, "fanout-serial", 1).await;

        bus.publish_trigger(&AgentId::new(&agent_id).unwrap(), &json!({"input": "a"}))
            .await
            .expect("publish 1");
        bus.publish_trigger(&AgentId::new(&agent_id).unwrap(), &json!({"input": "b"}))
            .await
            .expect("publish 2");

        wait_for_started(&started, 1, "first invocation").await;
        // The second must NOT start while the first is blocked.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "bound of 1 must not overlap invocations"
        );

        gate.add_permits(1);
        wait_for_started(&started, 2, "second invocation after release").await;
        gate.add_permits(1);
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("dispatcher joins in-flight work and exits")
            .expect("task joins");
        assert!(result.is_ok(), "{result:?}");
    }

    // ---- #278: admission for a paused model, and the deferral queue ----

    /// A registry holding one agent on `claude-haiku`, and the tempdir
    /// its definition was loaded from.
    fn registry_with(agent_id_str: &str) -> (tempfile::TempDir, SharedRegistry) {
        let mut registry = AgentRegistry::new();
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join(format!("{agent_id_str}.md"));
        std::fs::write(
            &agent_path,
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."
            ),
        )
        .unwrap();
        registry.load_file(&agent_path);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        (dir, shared_registry(registry))
    }

    /// A registry holding one agent per `(name, model)` pair, so a test
    /// can pause one model and leave another alone, or tell several
    /// otherwise identical triggers apart by the agent they start.
    fn registry_with_models(agents: &[(&str, &str)]) -> (tempfile::TempDir, SharedRegistry) {
        let dir = tempfile::tempdir().unwrap();
        for (name, model) in agents {
            std::fs::write(
                dir.path().join(format!("{name}.md")),
                format!("---\nname: {name}\nmodel: {model}\nbudget: 1.0\n---\n\nTest agent."),
            )
            .unwrap();
        }
        let registry = AgentRegistry::load_from_directory(dir.path(), None).expect("load");
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        (dir, shared_registry(registry))
    }

    /// A throttle whose `claude-haiku` is paused for `pause` from now.
    async fn throttle_paused_for(pause: Duration) -> Arc<crate::llm::ModelThrottle> {
        use crate::llm::{CallVerdict, ModelThrottle, ThrottleBounds, ThrottleConfig};
        let throttle = Arc::new(ModelThrottle::new(
            ThrottleConfig::default(),
            ThrottleBounds {
                ceiling: 1,
                max_pause: Duration::from_secs(120),
            },
        ));
        throttle
            .acquire("claude-haiku")
            .await
            .settle(CallVerdict::RateLimited {
                retry_after: Some(pause),
            });
        throttle
    }

    /// Records every start with its delivery attempt; drains on a flag.
    #[derive(Default)]
    struct RecordingWorker {
        starts: std::sync::Mutex<Vec<(std::time::Instant, Option<u32>)>>,
        draining: std::sync::atomic::AtomicBool,
    }
    #[async_trait::async_trait]
    impl Worker for RecordingWorker {
        async fn run_invocation(
            &self,
            _agent: &Agent,
            _llm: &dyn crate::llm::LlmClient,
            _trigger: Trigger,
            delivery_attempt: Option<u32>,
            mut durable_start: crate::worker::DurableStart,
        ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
            self.starts
                .lock()
                .unwrap()
                .push((std::time::Instant::now(), delivery_attempt));
            durable_start.fire(uuid::Uuid::now_v7());
            Ok(crate::worker::InvocationOutcome::Completed {
                invocation_id: Uuid::now_v7(),
                response: canned_response(),
                cost: 0.0,
                duration_ms: 0,
            })
        }
        async fn request_drain(&self, _req: crate::worker::DrainRequest) {
            self.draining
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn drain_status(&self) -> crate::worker::DrainState {
            if self.draining.load(std::sync::atomic::Ordering::SeqCst) {
                crate::worker::DrainState::Draining
            } else {
                crate::worker::DrainState::Running
            }
        }
    }

    async fn wait_for_starts(worker: &RecordingWorker, n: usize, within: Duration) {
        let deadline = std::time::Instant::now() + within;
        while worker.starts.lock().unwrap().len() < n {
            assert!(
                std::time::Instant::now() < deadline,
                "expected {n} start(s) within {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn held_claims_stop_same_worker_and_are_adopted_from_another_worker() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("claim-arbitration");
        let (_agents, registry) = registry_with(&agent_id_str);
        let worker = Arc::new(RecordingWorker::default());
        let store_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::control_plane::ControlPlaneStore::open(&store_dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let dispatcher = Arc::new(
            TriggerDispatcher::new(
                bus.clone(),
                registry,
                worker.clone(),
                Arc::new(FixtureClient::new()),
                1,
            )
            .with_trigger_claims(store.clone(), "worker-a"),
        );
        let mut consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::events::subjects::trigger(&agent_id_str),
                crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .unwrap();

        let agent = AgentId::new(&agent_id_str).unwrap();
        bus.publish_trigger(&agent, &json!({"input": "same worker"}))
            .await
            .unwrap();
        let msg = {
            let mut messages = consumer.messages().await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        };
        let seq = msg.info().unwrap().stream_sequence;
        assert_eq!(
            store
                .claim_trigger(test_key(&bus, seq), "worker-a", 0)
                .await
                .unwrap(),
            crate::control_plane::TriggerClaim::Won
        );
        dispatcher.handle(&msg).await;
        assert!(worker.starts.lock().unwrap().is_empty());
        // Held across the ack round trip, not merely at the instant
        // `handle` returned: an ack sent from the duplicate is in flight
        // then, and a single read would pass with it on the wire.
        ack_pending_stays(&mut consumer, 1, Duration::from_millis(300)).await;

        // Once the original owner gives the unfinished claim back, that
        // same delivery remains runnable and starts exactly once.
        store
            .release_trigger_claim(test_key(&bus, seq))
            .await
            .unwrap();
        dispatcher.handle(&msg).await;
        assert_eq!(worker.starts.lock().unwrap().len(), 1);

        bus.publish_trigger(&agent, &json!({"input": "restart"}))
            .await
            .unwrap();
        let restart_msg = {
            let mut messages = consumer.messages().await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        };
        let restart_seq = restart_msg.info().unwrap().stream_sequence;
        store
            .claim_trigger(test_key(&bus, restart_seq), "dead-worker", 0)
            .await
            .unwrap();
        dispatcher.handle(&restart_msg).await;
        assert_eq!(
            worker.starts.lock().unwrap().len(),
            2,
            "dead worker's claim is adopted"
        );
    }

    /// `num_ack_pending` must *stay* at `expected` for `window`.
    ///
    /// A single read taken the instant `handle` returns proves nothing
    /// about an ack it should not have sent: `msg.ack()` is
    /// fire-and-forget, so the count it would move is still 1 when the
    /// read leaves. Holding the assertion open across the round trip is
    /// what makes "the duplicate does not ack" bite (#817, review 1
    /// finding 2).
    async fn ack_pending_stays(
        consumer: &mut async_nats::jetstream::consumer::PullConsumer,
        expected: usize,
        window: Duration,
    ) {
        let deadline = std::time::Instant::now() + window;
        while std::time::Instant::now() < deadline {
            let pending = consumer
                .info()
                .await
                .expect("consumer info")
                .num_ack_pending;
            assert_eq!(
                pending, expected,
                "num_ack_pending moved to {pending}; the delivery was resolved \
                 by something that must not resolve it"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait until `agent` has a trigger parked at its concurrency cap.
    ///
    /// The hold is entered *after* the claim, so this is also the
    /// evidence that the first copy owns the stream sequence — which is
    /// what makes the redelivery below deterministic rather than a race
    /// between the two copies for the claim.
    async fn wait_for_cap_hold(
        caps: &crate::control_plane::agent_cap::AgentConcurrency,
        agent: &str,
        within: Duration,
    ) {
        let deadline = std::time::Instant::now() + within;
        loop {
            if caps
                .snapshot()
                .iter()
                .any(|a| a.agent == agent && a.held > 0)
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no trigger was held at {agent}'s cap within {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// **The incident of 2026-09-16 (#327), end to end on the broker.**
    ///
    /// Eight triggers produced thirteen invocations: every trigger that
    /// was *held* behind the per-agent cap across a stalled keepalive
    /// tick was redelivered, and the second copy walked into `handle`
    /// with no idempotency check and started a second invocation. The
    /// other claim tests seed the `trigger_claim` row by hand or re-enter
    /// `handle` after a durable start; neither reproduces this, because
    /// neither has JetStream redeliver a message whose first copy is
    /// still parked at the cap.
    ///
    /// So this one does exactly that: T1 takes the agent's only slot, T2
    /// is held behind it, a NAK on a clone of T2's message forces the
    /// redelivery the slipped keepalive caused, and the second copy is
    /// handled while the first is still holding. The second copy must
    /// return without starting anything and *without resolving the
    /// delivery its own first copy is still holding* — and when the slot
    /// frees, that first copy must start T2 once, as its own first
    /// delivery (`delivery_attempt == Some(1)`: the `attempt: 2`
    /// preamble is the incident's fingerprint), and settle the message.
    #[tokio::test]
    async fn redelivered_held_trigger_is_dropped_and_its_original_copy_starts_once() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("redelivered-hold");
        let (_agents, registry) = registry_with_cap(&agent_id_str, 1);
        let worker = CappedWorker::new();
        let caps = crate::control_plane::agent_cap::AgentConcurrency::new();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::control_plane::ControlPlaneStore::open(&store_dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let dispatcher = Arc::new(
            TriggerDispatcher::new(
                bus.clone(),
                registry,
                worker.clone(),
                Arc::new(FixtureClient::new()),
                4,
            )
            .with_caps_and_claims(caps.clone(), store, "worker-a"),
        );

        let mut consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::events::subjects::trigger(&agent_id_str),
                crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .unwrap();
        let agent = AgentId::new(&agent_id_str).unwrap();
        bus.publish_trigger(&agent, &json!({"input": "t1"}))
            .await
            .unwrap();
        bus.publish_trigger(&agent, &json!({"input": "t2"}))
            .await
            .unwrap();
        let (first, held) = {
            let mut messages = consumer.messages().await.unwrap();
            let a = tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .expect("the first trigger within 5s")
                .expect("stream open")
                .expect("message ok");
            let b = tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .expect("the second trigger within 5s")
                .expect("stream open")
                .expect("message ok");
            (a, b)
        };
        let held_seq = held.info().unwrap().stream_sequence;

        // T1 takes the agent's only slot and blocks there.
        let running = tokio::spawn({
            let d = dispatcher.clone();
            async move { d.handle(&first).await }
        });
        worker.wait_for_starts(1, Duration::from_secs(10)).await;

        // T2 is pulled under the cap: claimed, then parked un-started.
        let holding = tokio::spawn({
            let d = dispatcher.clone();
            let held = held.clone();
            async move { d.handle(&held).await }
        });
        wait_for_cap_hold(&caps, &agent_id_str, Duration::from_secs(10)).await;

        // The slipped keepalive: JetStream redelivers the held message.
        held.clone()
            .ack_with(async_nats::jetstream::AckKind::Nak(None))
            .await
            .expect("nak the held delivery");
        let duplicate = {
            let mut messages = consumer.messages().await.unwrap();
            tokio::time::timeout(Duration::from_secs(20), messages.next())
                .await
                .expect("the redelivery within 20s")
                .expect("stream open")
                .expect("message ok")
        };
        let info = duplicate.info().unwrap();
        assert_eq!(
            info.stream_sequence, held_seq,
            "the redelivery must be the held trigger, not a third one"
        );
        assert_eq!(
            info.delivered, 2,
            "the incident's second copy is delivery 2 of the same stream message"
        );

        // Handling it must return promptly — the duplicate is refused
        // before the drain check, the registry, or the cap, so it never
        // parks — and must start nothing.
        tokio::time::timeout(Duration::from_secs(5), dispatcher.handle(&duplicate))
            .await
            .expect("the duplicate is refused rather than held");
        assert_eq!(
            worker.started(),
            1,
            "the redelivered copy started a second invocation — the #327 incident"
        );
        // ...and must leave the delivery its first copy is still holding
        // exactly as it found it: neither acked nor NAK'd.
        ack_pending_stays(&mut consumer, 1, Duration::from_millis(300)).await;

        // Free T1's slot: the *original* copy starts T2, once.
        worker.let_finish(1);
        worker.wait_for_starts(2, Duration::from_secs(20)).await;
        assert_eq!(
            worker.attempts(),
            vec![Some(1), Some(1)],
            "the held trigger runs as its own first delivery; `attempt: 2` in a \
             preamble is the incident's fingerprint"
        );

        // The durable start of that one invocation settles the message.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let pending = consumer
                .info()
                .await
                .expect("consumer info")
                .num_ack_pending;
            if pending == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the held trigger's durable start never acked it (num_ack_pending={pending})"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(worker.started(), 2, "exactly one invocation per trigger");

        worker.let_finish(1);
        for task in [running, holding] {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("handle returns")
                .expect("task joins");
        }
    }

    #[tokio::test]
    async fn requeue_held_releases_old_stream_sequence_claim() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.unwrap();
        let agent_id_str = unique_agent_id("claim-requeue");
        let (_agents, registry) = registry_with(&agent_id_str);
        let store_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::control_plane::ControlPlaneStore::open(&store_dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let dispatcher = TriggerDispatcher::new(
            bus.clone(),
            registry,
            Arc::new(RecordingWorker::default()),
            Arc::new(FixtureClient::new()),
            1,
        )
        .with_trigger_claims(store.clone(), "worker-a");
        let agent = AgentId::new(&agent_id_str).unwrap();
        let consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &crate::events::subjects::trigger(&agent_id_str),
                1,
            )
            .await
            .unwrap();
        let payload = json!({"input": "requeue"});
        bus.publish_trigger(&agent, &payload).await.unwrap();
        let msg = {
            let mut messages = consumer.messages().await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        };
        let seq = msg.info().unwrap().stream_sequence;
        store
            .claim_trigger(test_key(&bus, seq), "worker-a", 0)
            .await
            .unwrap();
        dispatcher.requeue_held(&msg, &agent, None, &payload).await;
        assert_eq!(
            store
                .claim_trigger(test_key(&bus, seq), "worker-a", 1)
                .await
                .unwrap(),
            crate::control_plane::TriggerClaim::Won,
            "requeue ACK releases the old sequence claim"
        );
    }

    /// #278 admission: a trigger for a paused model is not started while
    /// the pause holds, is started once it lifts, and is still the first
    /// delivery when it does. The pause is longer than the trigger
    /// durable's ack window and the dispatcher has a second permit open,
    /// so this is also the proof that the hold keeps the delivery alive:
    /// without the in-progress acks JetStream redelivers when that window
    /// expires during the open pull and the worker sees a second
    /// start (`a held trigger is started exactly once: left: 2`).
    #[tokio::test]
    async fn a_paused_models_trigger_is_held_and_started_once_after_the_pause() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("held-trigger");
        let (_dir, registry) = registry_with(&agent_id_str);
        let pause = crate::bus::TRIGGER_RETRY_BACKOFF[0] + Duration::from_millis(500);
        assert!(
            pause > crate::bus::TRIGGER_RETRY_BACKOFF[0],
            "the hold must outlast the ack window for this test to prove anything"
        );
        let throttle = throttle_paused_for(pause).await;
        let paused_at = std::time::Instant::now();

        let worker = Arc::new(RecordingWorker::default());
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = unique_consumer_name();
        let filter = crate::events::subjects::trigger(&agent_id_str);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Two permits, so a second pull is open while the first delivery
        // is held: that is where a redelivered copy would land, which is
        // the saturated-fleet shape of the redelivery storm.
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker.clone(), llm, 2)
            .with_throttle(throttle);
        let run = tokio::spawn(async move {
            dispatcher
                .run_on_consumer(&consumer_name, Some(&filter), shutdown_rx)
                .await
        });

        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({"input": "hi"}),
        )
        .await
        .expect("publish trigger");

        wait_for_starts(&worker, 1, pause + Duration::from_secs(8)).await;
        let (started_at, attempt) = worker.starts.lock().unwrap()[0];
        assert!(
            started_at >= paused_at + pause - Duration::from_millis(50),
            "started {:?} after the pause was set; the pause was {pause:?}",
            started_at - paused_at
        );
        assert_eq!(
            attempt,
            Some(1),
            "held, not redelivered: still the first delivery"
        );

        // Past another ack window: a redelivered copy would start now.
        tokio::time::sleep(crate::bus::TRIGGER_RETRY_BACKOFF[0] + Duration::from_millis(500)).await;
        assert_eq!(
            worker.starts.lock().unwrap().len(),
            1,
            "a held trigger is started exactly once"
        );

        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("dispatcher exits")
            .expect("task joins")
            .expect("clean exit");
    }

    /// A drain that lands during a hold releases the trigger un-acked
    /// and un-started: the dispatcher exits on its own, the worker never
    /// sees the invocation, and the broker still owns the delivery for
    /// the next binary.
    #[tokio::test]
    async fn a_drain_during_a_hold_leaves_the_trigger_for_the_next_binary() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("held-then-drained");
        let (_dir, registry) = registry_with(&agent_id_str);
        let throttle = throttle_paused_for(Duration::from_secs(10)).await;

        let worker = Arc::new(RecordingWorker::default());
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = unique_consumer_name();
        let filter = crate::events::subjects::trigger(&agent_id_str);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker.clone(), llm, 1)
            .with_throttle(throttle);
        let run = {
            let consumer_name = consumer_name.clone();
            let filter = filter.clone();
            tokio::spawn(async move {
                dispatcher
                    .run_on_consumer(&consumer_name, Some(&filter), shutdown_rx)
                    .await
            })
        };

        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({"input": "hi"}),
        )
        .await
        .expect("publish trigger");
        tokio::time::sleep(Duration::from_millis(600)).await;
        worker
            .request_drain(crate::worker::DrainRequest::new(
                crate::worker::DrainReason::Deploy,
            ))
            .await;

        // The hold notices the drain within a keepalive tick and the
        // loop exits at its top, with no shutdown signal sent.
        tokio::time::timeout(Duration::from_secs(3), run)
            .await
            .expect("a draining dispatcher lets a held trigger go and exits")
            .expect("task joins")
            .expect("clean exit");
        assert!(
            worker.starts.lock().unwrap().is_empty(),
            "the held trigger must not start under a drain"
        );

        let mut consumer = bus
            .trigger_consumer_with_filter(&consumer_name, &filter, 1000)
            .await
            .expect("the durable still exists");
        let info = consumer.info().await.expect("consumer info");
        assert_eq!(
            info.num_ack_pending as u64 + info.num_pending,
            1,
            "the trigger is still the broker's to deliver: {info:?}"
        );
        drop(shutdown_tx);
    }

    /// The wedge PR #807's review found, as a test: at the **default**
    /// `max_concurrent_invocations = 1`, a trigger held for a paused
    /// model must start when the pause ends, and must not stop an
    /// unrelated agent from running while it waits.
    ///
    /// Both halves fail the moment the consume loop owns a permit while
    /// it is idle. It takes the only permit to pull the paused trigger,
    /// then blocks on `acquire_owned` before the next pull, so the
    /// unpaused agent's trigger is never even pulled (`expected 1
    /// start(s) of …` is what that reads as) — and if the hold gives its
    /// permit back, the loop, already queued on the fair semaphore,
    /// takes it and the held trigger waits forever: no start, no
    /// redelivery, just `ack pending 1` until a drain.
    ///
    /// Nothing here is special to a pause: it is the smallest shape that
    /// shows a permit held by something that is not running.
    #[tokio::test]
    async fn a_pause_held_trigger_starts_after_the_pause_with_one_worker_permit() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let paused = unique_agent_id("pause-wedge-held");
        let other = unique_agent_id("pause-wedge-runs");
        let (_dir, registry) =
            registry_with_models(&[(&paused, "claude-haiku"), (&other, "unpaused-model")]);
        let pause = Duration::from_secs(3);
        let throttle = throttle_paused_for(pause).await;
        let paused_at = std::time::Instant::now();

        let worker = CappedWorker::new();
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = unique_consumer_name();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Worker cap 1 — the default (`config.rs`, `[worker]
        // max_concurrent_invocations`). One permit for the whole daemon.
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker.clone(), llm, 1)
            .with_throttle(throttle);
        let run = tokio::spawn(async move {
            dispatcher
                .run_on_consumer(&consumer_name, None, shutdown_rx)
                .await
        });

        // Pulled and parked on the pause, owning nothing.
        publish_triggers(&bus, &paused, 1).await;
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Head-of-line: an unrelated agent's trigger is pulled and run
        // while the paused one waits.
        publish_triggers(&bus, &other, 1).await;
        worker
            .wait_for_agent_starts(&other, 1, Duration::from_secs(3))
            .await;
        assert_eq!(
            worker.started_for(&paused),
            0,
            "the paused model's trigger is still held while the unpaused agent runs"
        );

        // That invocation ends, so the one permit is free again by the
        // time the pause lifts — and the held trigger must take it.
        worker.let_finish(10);
        worker
            .wait_for_agent_starts(&paused, 1, pause + Duration::from_secs(8))
            .await;
        let started_at = worker
            .starts
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.2 == paused)
            .expect("the held trigger started")
            .0;
        assert!(
            started_at >= paused_at + pause - Duration::from_millis(100),
            "the held trigger started {:?} after the pause was set; the pause was {pause:?}",
            started_at - paused_at
        );
        assert!(
            worker.attempts().iter().all(|a| a == &Some(1)),
            "held, not redelivered: still the first delivery, got {:?}",
            worker.attempts()
        );

        stop(shutdown_tx, run).await;
    }

    /// The permit wait is a hold like the other two: it keeps the
    /// delivery alive for as long as it lasts, and lets go of it
    /// un-acked on a drain.
    ///
    /// The one permit is taken by the test, so nothing is running and
    /// nothing will free it — the trigger is admitted by both rules and
    /// then waits. Past the durable's whole 30-second first-delivery
    /// window, a missing keepalive shows as the trigger arriving a
    /// second time on this very stream, which is one trigger becoming
    /// two invocations.
    #[tokio::test]
    async fn a_permit_wait_keeps_the_trigger_alive_and_is_interrupted_by_drain() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("permit-wait");
        let (_dir, registry) = registry_with(&agent_id_str);
        let worker = CappedWorker::new();
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let dispatcher = Arc::new(TriggerDispatcher::new(
            bus.clone(),
            registry,
            worker.clone(),
            llm,
            1,
        ));
        // The worker cap, occupied with nothing running.
        let occupied = a_permit(&dispatcher).await;

        let consumer_name = unique_consumer_name();
        let filter = crate::events::subjects::trigger(&agent_id_str);
        let consumer = bus
            .trigger_consumer_with_filter(
                &consumer_name,
                &filter,
                crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .expect("consumer");
        publish_triggers(&bus, &agent_id_str, 1).await;
        let mut stream = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("a message within 5s")
            .expect("stream open")
            .expect("message ok");

        let d = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move { d.handle(&msg).await });

        let window = crate::bus::TRIGGER_RETRY_BACKOFF[0] + Duration::from_secs(3);
        assert!(
            tokio::time::timeout(window, stream.next()).await.is_err(),
            "a trigger waiting for a worker permit must be kept alive, not redelivered \
             — a copy here is one trigger turning into two invocations"
        );
        assert_eq!(
            worker.started(),
            0,
            "and it has not started: the only permit is still taken"
        );

        // A drain during the wait ends it, and the delivery is left
        // where it is for the next binary.
        worker
            .request_drain(crate::worker::DrainRequest::new(
                crate::worker::DrainReason::Deploy,
            ))
            .await;
        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("a permit wait lets go within a keepalive tick of a drain")
            .expect("task joins");
        assert_eq!(worker.started(), 0, "a drained trigger does not start");
        drop(occupied);

        let mut consumer = bus
            .trigger_consumer_with_filter(&consumer_name, &filter, 1000)
            .await
            .expect("the durable still exists");
        let info = consumer.info().await.expect("consumer info");
        assert_eq!(
            info.num_ack_pending as u64 + info.num_pending,
            1,
            "the trigger is still the broker's to deliver: {info:?}"
        );
    }

    /// Waiting for a worker permit is a **queue**, not a race: the
    /// longest-parked trigger runs first.
    ///
    /// Three agents so the starts can be told apart, one permit, and the
    /// second and third triggers published in order while the first
    /// invocation holds it. Each waiter registers once and keeps its
    /// place; a wait that re-registered every keepalive tick would let
    /// whichever polled first at the moment of release overtake.
    #[tokio::test]
    async fn triggers_waiting_for_a_worker_permit_start_in_arrival_order() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let first = unique_agent_id("permit-queue-first");
        let second = unique_agent_id("permit-queue-second");
        let third = unique_agent_id("permit-queue-third");
        let (_dir, registry) = registry_with_models(&[
            (&first, "claude-haiku"),
            (&second, "claude-haiku"),
            (&third, "claude-haiku"),
        ]);
        let worker = CappedWorker::new();
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = unique_consumer_name();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker.clone(), llm, 1);
        let run = tokio::spawn(async move {
            dispatcher
                .run_on_consumer(&consumer_name, None, shutdown_rx)
                .await
        });

        publish_triggers(&bus, &first, 1).await;
        worker
            .wait_for_agent_starts(&first, 1, Duration::from_secs(10))
            .await;
        publish_triggers(&bus, &second, 1).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        publish_triggers(&bus, &third, 1).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            worker.started(),
            1,
            "both later triggers are pulled and waiting on the one permit"
        );

        worker.let_finish(1);
        worker
            .wait_for_agent_starts(&second, 1, Duration::from_secs(5))
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            worker.started_for(&third),
            0,
            "the trigger that arrived later waits its turn"
        );

        worker.let_finish(1);
        worker
            .wait_for_agent_starts(&third, 1, Duration::from_secs(5))
            .await;
        let order: Vec<String> = worker
            .starts
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.2.clone())
            .collect();
        assert_eq!(order, vec![first, second, third], "arrival order");

        worker.let_finish(10);
        stop(shutdown_tx, run).await;
    }

    /// A pause that ends while the worker cap is full: the trigger goes
    /// straight from one wait into the other, starts exactly once when a
    /// permit frees, and leaks nothing.
    ///
    /// Driven through `handle` directly, so the permit the invocation
    /// runs under can only be the one this test releases.
    #[tokio::test]
    async fn a_trigger_whose_pause_ended_waits_for_a_permit_and_starts_once() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let paused = unique_agent_id("paused-then-waiting");
        let (_dir, registry) = registry_with(&paused);
        let throttle = throttle_paused_for(Duration::from_millis(800)).await;
        let worker = CappedWorker::new();
        let dispatcher = Arc::new(
            TriggerDispatcher::new(
                bus.clone(),
                registry,
                worker.clone(),
                Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
                1,
            )
            .with_throttle(throttle),
        );
        let permits = Arc::clone(&dispatcher.permits);
        let filter = crate::events::subjects::trigger(&paused);
        let consumer = bus
            .trigger_consumer_with_filter(
                &unique_consumer_name(),
                &filter,
                crate::bus::NATS_DEFAULT_MAX_ACK_PENDING,
            )
            .await
            .expect("consumer");
        publish_triggers(&bus, &paused, 1).await;
        let msg = {
            let mut stream = consumer.messages().await.expect("messages");
            tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("a message within 5s")
                .expect("stream open")
                .expect("message ok")
        };

        // The only permit, taken before the pause ends.
        let occupied = tokio::time::timeout(Duration::from_secs(5), a_permit(&dispatcher))
            .await
            .expect("the loop holds no permit, so this one is free");
        let d = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move { d.handle(&msg).await });

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            worker.started(),
            0,
            "the pause is over, but there is no worker to run it on yet"
        );

        drop(occupied);
        worker
            .wait_for_agent_starts(&paused, 1, Duration::from_secs(3))
            .await;
        worker.let_finish(1);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("handle finishes")
            .expect("task joins");
        assert_eq!(worker.started(), 1, "started exactly once");
        assert_eq!(permits.available_permits(), 1, "no worker permit leaked");
    }

    /// Defers on its first start; records every resume; defers once
    /// more on the first resume, then completes.
    struct DeferringWorker {
        invocation_id: Uuid,
        deferred_at: std::sync::Mutex<Option<std::time::Instant>>,
        resumes: std::sync::Mutex<Vec<(std::time::Instant, Uuid, AgentId)>>,
    }
    #[async_trait::async_trait]
    impl Worker for DeferringWorker {
        async fn run_invocation(
            &self,
            _agent: &Agent,
            _llm: &dyn crate::llm::LlmClient,
            _trigger: Trigger,
            _delivery_attempt: Option<u32>,
            mut durable_start: crate::worker::DurableStart,
        ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
            durable_start.fire(uuid::Uuid::now_v7());
            *self.deferred_at.lock().unwrap() = Some(std::time::Instant::now());
            Ok(crate::worker::InvocationOutcome::Deferred {
                invocation_id: self.invocation_id,
                resume_after: Duration::from_millis(400),
            })
        }
        async fn resume_invocation(
            &self,
            agent: &Agent,
            _llm: &dyn crate::llm::LlmClient,
            invocation_id: Uuid,
        ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
            let mut resumes = self.resumes.lock().unwrap();
            resumes.push((std::time::Instant::now(), invocation_id, agent.id().clone()));
            if resumes.len() == 1 {
                return Ok(crate::worker::InvocationOutcome::Deferred {
                    invocation_id,
                    resume_after: Duration::from_millis(300),
                });
            }
            Ok(crate::worker::InvocationOutcome::Completed {
                invocation_id,
                response: canned_response(),
                cost: 0.0,
                duration_ms: 0,
            })
        }
        async fn request_drain(&self, _req: crate::worker::DrainRequest) {}
        fn drain_status(&self) -> crate::worker::DrainState {
            crate::worker::DrainState::Running
        }
    }

    /// #278 deferral, the dispatcher's half: an invocation the worker
    /// puts down is resumed by this dispatcher after its delay, with the
    /// agent it belongs to; a resume that is deferred again is resumed
    /// again. The trigger itself is acked at the first WAL write and
    /// never redelivered.
    #[tokio::test]
    async fn a_deferred_invocation_is_resumed_by_the_dispatcher_after_its_delay() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("deferred");
        let (_dir, registry) = registry_with(&agent_id_str);
        let invocation_id = Uuid::now_v7();
        let worker = Arc::new(DeferringWorker {
            invocation_id,
            deferred_at: std::sync::Mutex::new(None),
            resumes: std::sync::Mutex::new(Vec::new()),
        });
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = unique_consumer_name();
        let filter = crate::events::subjects::trigger(&agent_id_str);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker.clone(), llm, 1);
        let run = tokio::spawn(async move {
            dispatcher
                .run_on_consumer(&consumer_name, Some(&filter), shutdown_rx)
                .await
        });

        bus.publish_trigger(
            &AgentId::new(&agent_id_str).unwrap(),
            &json!({"input": "hi"}),
        )
        .await
        .expect("publish trigger");

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while worker.resumes.lock().unwrap().len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "two resumes within 8s"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let deferred_at = worker.deferred_at.lock().unwrap().expect("deferred");
        let resumes = worker.resumes.lock().unwrap().clone();
        assert!(
            resumes[0].0 >= deferred_at + Duration::from_millis(400) - Duration::from_millis(20),
            "the first resume came {:?} after the deferral, before its 400ms delay",
            resumes[0].0 - deferred_at
        );
        assert!(
            resumes[1].0 >= resumes[0].0 + Duration::from_millis(300) - Duration::from_millis(20),
            "the second resume came {:?} after the first, before its 300ms delay",
            resumes[1].0 - resumes[0].0
        );
        for (_, id, agent) in &resumes {
            assert_eq!(
                *id, invocation_id,
                "the resume names the deferred invocation"
            );
            assert_eq!(agent.as_str(), agent_id_str, "with the agent it belongs to");
        }

        // No redelivery: the trigger was acked at the durable start.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(worker.resumes.lock().unwrap().len(), 2);

        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("dispatcher exits")
            .expect("task joins")
            .expect("clean exit");
    }

    // --- #718: the per-agent concurrency cap -------------------------

    /// A registry holding one agent whose definition declares
    /// `max_concurrent: cap`, plus the directory it was written into so
    /// a test can rewrite the definition and reload it.
    fn registry_with_cap(agent_id_str: &str, cap: u32) -> (tempfile::TempDir, SharedRegistry) {
        let dir = tempfile::tempdir().unwrap();
        write_capped_definition(dir.path(), agent_id_str, cap);
        let mut registry = AgentRegistry::new();
        registry.load_file(&dir.path().join(format!("{agent_id_str}.md")));
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        (dir, shared_registry(registry))
    }

    fn write_capped_definition(dir: &std::path::Path, agent_id_str: &str, cap: u32) {
        std::fs::write(
            dir.join(format!("{agent_id_str}.md")),
            format!(
                "---\nname: {agent_id_str}\nmodel: claude-haiku\nbudget: 1.0\n\
                 max_concurrent: {cap}\n---\n\nTest agent."
            ),
        )
        .unwrap();
    }

    /// Re-read the definitions from `dir` and swap them in, exactly as
    /// `fq reload` does (`control_commands::reload_agents`): a new
    /// registry behind the same handle, affecting the next admission.
    async fn reload_from(dir: &std::path::Path, shared: &SharedRegistry) {
        let registry = AgentRegistry::load_from_directory(dir, None).expect("reload");
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        shared.swap(registry);
    }

    /// Holds every invocation open until the test lets it finish, and
    /// records the highest number that were open at once — which is the
    /// only direct evidence a cap was honoured. `RecordingWorker`
    /// returns instantly, so under it two invocations never overlap
    /// whether the cap works or not.
    struct CappedWorker {
        starts: std::sync::Mutex<Vec<(std::time::Instant, Option<u32>, String)>>,
        open: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        /// One permit per invocation the test allows to finish.
        finish: tokio::sync::Semaphore,
        /// When set, an invocation ends in a terminal executor error
        /// instead of completing — the other exit path the slot must be
        /// released on.
        fail: std::sync::atomic::AtomicBool,
        draining: std::sync::atomic::AtomicBool,
    }

    impl CappedWorker {
        fn new() -> Arc<Self> {
            Arc::new(CappedWorker {
                starts: std::sync::Mutex::new(Vec::new()),
                open: std::sync::atomic::AtomicUsize::new(0),
                peak: std::sync::atomic::AtomicUsize::new(0),
                finish: tokio::sync::Semaphore::new(0),
                fail: std::sync::atomic::AtomicBool::new(false),
                draining: std::sync::atomic::AtomicBool::new(false),
            })
        }
        /// Let `n` more invocations finish.
        fn let_finish(&self, n: usize) {
            self.finish.add_permits(n);
        }
        fn started(&self) -> usize {
            self.starts.lock().unwrap().len()
        }
        fn peak(&self) -> usize {
            self.peak.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn attempts(&self) -> Vec<Option<u32>> {
            self.starts.lock().unwrap().iter().map(|s| s.1).collect()
        }
        /// How many invocations of one agent have started.
        fn started_for(&self, agent: &str) -> usize {
            self.starts
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.2 == agent)
                .count()
        }
        async fn wait_for_agent_starts(&self, agent: &str, n: usize, within: Duration) {
            let deadline = std::time::Instant::now() + within;
            while self.started_for(agent) < n {
                assert!(
                    std::time::Instant::now() < deadline,
                    "expected {n} start(s) of {agent} within {within:?}, saw {}",
                    self.started_for(agent)
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        async fn wait_for_starts(&self, n: usize, within: Duration) {
            let deadline = std::time::Instant::now() + within;
            while self.started() < n {
                assert!(
                    std::time::Instant::now() < deadline,
                    "expected {n} start(s) within {within:?}, saw {}",
                    self.started()
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    #[async_trait::async_trait]
    impl Worker for CappedWorker {
        async fn run_invocation(
            &self,
            agent: &Agent,
            _llm: &dyn crate::llm::LlmClient,
            _trigger: Trigger,
            delivery_attempt: Option<u32>,
            mut durable_start: crate::worker::DurableStart,
        ) -> Result<crate::worker::InvocationOutcome, ExecutorError> {
            use std::sync::atomic::Ordering::SeqCst;
            self.starts.lock().unwrap().push((
                std::time::Instant::now(),
                delivery_attempt,
                agent.id().as_str().to_string(),
            ));
            let open = self.open.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(open, SeqCst);
            durable_start.fire(uuid::Uuid::now_v7());
            self.finish.acquire().await.expect("gate open").forget();
            self.open.fetch_sub(1, SeqCst);
            if self.fail.load(SeqCst) {
                return Err(ExecutorError::InvocationFailed {
                    kind: FailureKind::RuntimeError,
                    message: "the invocation failed".to_string(),
                });
            }
            Ok(crate::worker::InvocationOutcome::Completed {
                invocation_id: Uuid::now_v7(),
                response: canned_response(),
                cost: 0.0,
                duration_ms: 0,
            })
        }
        async fn request_drain(&self, _req: crate::worker::DrainRequest) {
            self.draining
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn drain_status(&self) -> crate::worker::DrainState {
            if self.draining.load(std::sync::atomic::Ordering::SeqCst) {
                crate::worker::DrainState::Draining
            } else {
                crate::worker::DrainState::Running
            }
        }
    }

    /// Spawn a dispatcher on its own consumer and filter for `agent`.
    fn spawn_dispatcher(
        bus: &EventBus,
        agent_id_str: &str,
        registry: SharedRegistry,
        worker: Arc<dyn Worker>,
        agent_caps: Arc<crate::control_plane::agent_cap::AgentConcurrency>,
        max_concurrent: usize,
    ) -> (
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), DispatcherError>>,
    ) {
        spawn_dispatcher_on(
            bus,
            &unique_consumer_name(),
            agent_id_str,
            registry,
            worker,
            agent_caps,
            max_concurrent,
        )
    }

    /// The same, on a durable the caller names — so a test can stop one
    /// dispatcher and start the next on the same consumer, which is what
    /// a restart looks like to JetStream: the delivery counts carry over.
    fn spawn_dispatcher_on(
        bus: &EventBus,
        consumer_name: &str,
        agent_id_str: &str,
        registry: SharedRegistry,
        worker: Arc<dyn Worker>,
        agent_caps: Arc<crate::control_plane::agent_cap::AgentConcurrency>,
        max_concurrent: usize,
    ) -> (
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), DispatcherError>>,
    ) {
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = consumer_name.to_string();
        let filter = crate::events::subjects::trigger(agent_id_str);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker, llm, max_concurrent)
            .with_agent_caps(agent_caps);
        let run = tokio::spawn(async move {
            dispatcher
                .run_on_consumer(&consumer_name, Some(&filter), shutdown_rx)
                .await
        });
        (shutdown_tx, run)
    }

    async fn publish_triggers(bus: &EventBus, agent_id_str: &str, n: usize) {
        let agent = AgentId::new(agent_id_str).unwrap();
        for i in 0..n {
            bus.publish_trigger(&agent, &json!({"input": i}))
                .await
                .expect("publish trigger");
        }
    }

    async fn stop(
        shutdown_tx: oneshot::Sender<()>,
        run: tokio::task::JoinHandle<Result<(), DispatcherError>>,
    ) {
        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("dispatcher exits")
            .expect("task joins")
            .expect("clean exit");
    }

    /// The issue's first acceptance criterion, whole: an agent with
    /// `max_concurrent: 1` never has two invocations in flight while the
    /// worker cap is 8, and the third trigger waits and then starts *as
    /// its first delivery*.
    ///
    /// The wait is longer than the trigger durable's 30-second ack
    /// window with seven further permits open, so this is also the proof
    /// that the hold keeps the delivery alive and consumes no
    /// redelivery: without the in-progress acks JetStream would redeliver
    /// into one of those open pulls and the worker would see an extra
    /// start stamped `attempt: 2`.
    #[tokio::test]
    async fn an_agent_at_max_concurrent_one_runs_one_at_a_time_under_a_worker_cap_of_eight() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("capped-one");
        let (_dir, registry) = registry_with_cap(&agent_id_str, 1);
        let counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let worker = CappedWorker::new();
        let (shutdown_tx, run) = spawn_dispatcher(
            &bus,
            &agent_id_str,
            registry,
            worker.clone(),
            Arc::clone(&counts),
            8,
        );

        publish_triggers(&bus, &agent_id_str, 3).await;
        worker.wait_for_starts(1, Duration::from_secs(10)).await;

        // Past the ack window, with the other two triggers held.
        let hold = crate::bus::TRIGGER_RETRY_BACKOFF[0] + Duration::from_millis(500);
        tokio::time::sleep(hold).await;
        assert!(
            crate::bus::TRIGGER_RETRY_BACKOFF[0] < hold,
            "the hold must outlast the ack window for this test to prove anything"
        );
        assert_eq!(
            worker.peak(),
            1,
            "a capped agent starts one at a time even with seven permits free"
        );
        assert!(
            worker.attempts().iter().all(|a| a == &Some(1)),
            "a held trigger must not be redelivered, got {:?}. A `Some(2)` here is a \
             keepalive tick that slipped past the durable's 30-second first-delivery \
             window — the duplicate-invocation class #327 owns — and not the cap failing",
            worker.attempts()
        );
        assert_eq!(worker.started(), 1, "and so exactly one has started");
        let listed = counts.snapshot();
        assert_eq!(listed.len(), 1, "`fq doctor` names the agent: {listed:?}");
        assert_eq!(listed[0].in_flight, 1);
        assert_eq!(listed[0].cap, 1);
        assert_eq!(listed[0].held, 2, "both waiting triggers are counted");

        // One slot frees; exactly one held trigger takes it.
        worker.let_finish(1);
        worker.wait_for_starts(2, Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(worker.started(), 2, "the freed slot admits one, not both");

        worker.let_finish(2);
        worker.wait_for_starts(3, Duration::from_secs(10)).await;
        assert_eq!(
            worker.peak(),
            1,
            "never two invocations of a `max_concurrent: 1` agent at once"
        );
        assert_eq!(
            worker.attempts(),
            vec![Some(1), Some(1), Some(1)],
            "held, not redelivered: every start is still a first delivery"
        );

        stop(shutdown_tx, run).await;
        assert!(
            counts.snapshot().is_empty(),
            "every slot is given back once the runs end"
        );
    }

    /// The blocker the review found, as a test: **a held trigger must
    /// not occupy a worker permit.**
    ///
    /// If a cap hold owned a permit, a capped agent's backlog would eat
    /// the worker cap and the *rest of the fleet would stop* — the
    /// inverse of what #718 is for, and silent, because every held
    /// trigger would look healthy.
    ///
    /// Worker cap 2, agent A at `max_concurrent: 1`, three A triggers
    /// ahead of one B trigger on the queue. A runs one; A's other two
    /// park; B must still start. With the permits held by the waiting
    /// work, both are taken and B never runs — `expected 1 start(s) of
    /// B` is what that failure reads as.
    #[tokio::test]
    async fn a_held_trigger_occupies_no_worker_permit_so_other_agents_run() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let capped = unique_agent_id("capped-blocks");
        let other = unique_agent_id("uncapped-runs");

        // Both agents in one registry, and no subject filter: the broker
        // is private to this test, so the dispatcher can consume the
        // whole trigger stream without competing with anything.
        let dir = tempfile::tempdir().unwrap();
        write_capped_definition(dir.path(), &capped, 1);
        std::fs::write(
            dir.path().join(format!("{other}.md")),
            format!("---\nname: {other}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."),
        )
        .unwrap();
        let registry = AgentRegistry::load_from_directory(dir.path(), None).expect("load");
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let registry = shared_registry(registry);

        let counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let worker = CappedWorker::new();
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = unique_consumer_name();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Worker cap 2: one for the running invocation, one for the rest
        // of the fleet. Under the bug the second is swallowed by a hold.
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker.clone(), llm, 2)
            .with_agent_caps(Arc::clone(&counts));
        let run = tokio::spawn(async move {
            dispatcher
                .run_on_consumer(&consumer_name, None, shutdown_rx)
                .await
        });

        publish_triggers(&bus, &capped, 3).await;
        publish_triggers(&bus, &other, 1).await;

        worker
            .wait_for_agent_starts(&other, 1, Duration::from_secs(10))
            .await;
        assert_eq!(
            worker.started_for(&capped),
            1,
            "the capped agent still runs exactly one"
        );
        assert_eq!(
            worker.peak(),
            2,
            "the capped agent's run and the other agent's run are concurrent, \
             so both worker permits are doing work rather than waiting"
        );
        let listed = counts.snapshot();
        assert_eq!(
            listed.len(),
            1,
            "only the capped agent is a line: {listed:?}"
        );
        assert_eq!(listed[0].held, 2, "its other two triggers are parked");

        // Enough gate permits for anything that starts on the way out.
        worker.let_finish(10);
        stop(shutdown_tx, run).await;
    }

    /// The other terminal exit path: an invocation that *fails* must
    /// give its slot back, or the agent wedges at its cap forever with
    /// nothing running. Same shape as the completion case, with the
    /// worker returning a terminal executor error instead.
    #[tokio::test]
    async fn a_failed_invocation_gives_its_agents_slot_back() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("capped-fail");
        let (_dir, registry) = registry_with_cap(&agent_id_str, 1);
        let counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let worker = CappedWorker::new();
        worker.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let (shutdown_tx, run) = spawn_dispatcher(
            &bus,
            &agent_id_str,
            registry,
            worker.clone(),
            Arc::clone(&counts),
            8,
        );

        publish_triggers(&bus, &agent_id_str, 2).await;
        worker.wait_for_starts(1, Duration::from_secs(10)).await;
        worker.let_finish(1);
        worker.wait_for_starts(2, Duration::from_secs(10)).await;
        assert_eq!(
            worker.attempts()[1],
            Some(1),
            "the trigger behind a failure is still its first delivery"
        );
        let capped = AgentId::new(&agent_id_str).unwrap();
        assert_eq!(counts.in_flight(&capped), 1, "only the second is running");

        worker.let_finish(1);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while counts.in_flight(&capped) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "a failed invocation must release its slot"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        stop(shutdown_tx, run).await;
    }

    /// Review finding C4: **a restart mid-hold must not charge the held
    /// trigger a delivery.**
    ///
    /// A cap hold is bounded by the invocation ahead of it, not by a
    /// pause, so it routinely outlasts a deploy — and an un-acked
    /// delivery comes back as `attempt: 2`, five of which dead-letter a
    /// trigger nobody ever refused on its merits. The hold requeues the
    /// trigger under its own id instead, so the next binary sees a first
    /// delivery.
    ///
    /// Both dispatchers run on the *same durable consumer*, which is
    /// what makes the assertion mean anything: JetStream's delivery
    /// count is per consumer, so a fresh one would report `attempt: 1`
    /// whatever the shutdown did with the message.
    #[tokio::test]
    async fn a_restart_during_a_cap_hold_does_not_charge_the_trigger_a_delivery() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("capped-restart");
        let (_dir, registry) = registry_with_cap(&agent_id_str, 1);
        let consumer_name = unique_consumer_name();

        let counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let worker = CappedWorker::new();
        let (shutdown_tx, run) = spawn_dispatcher_on(
            &bus,
            &consumer_name,
            &agent_id_str,
            registry.clone(),
            worker.clone(),
            counts,
            8,
        );

        publish_triggers(&bus, &agent_id_str, 2).await;
        worker.wait_for_starts(1, Duration::from_secs(10)).await;
        // Past the durable's first-delivery window, so the second
        // trigger is genuinely being held alive rather than merely
        // in-flight.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(worker.started(), 1, "the second trigger is held at cap 1");

        // The deploy lands mid-hold.
        let _ = shutdown_tx.send(());
        // Long enough for the hold to see `stopping` on its next tick
        // (CAP_POLL) and requeue.
        tokio::time::sleep(Duration::from_millis(600)).await;
        worker.let_finish(10);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("dispatcher exits")
            .expect("task joins")
            .expect("clean exit");
        assert_eq!(
            worker.started(),
            1,
            "the held trigger did not start on the way out"
        );

        // The next binary, on the same durable.
        let next_counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let next = CappedWorker::new();
        let (shutdown_tx, run) = spawn_dispatcher_on(
            &bus,
            &consumer_name,
            &agent_id_str,
            registry,
            next.clone(),
            next_counts,
            8,
        );
        next.wait_for_starts(1, Duration::from_secs(10)).await;
        assert_eq!(
            next.attempts(),
            vec![Some(1)],
            "a restart during a hold must cost the trigger nothing: an `attempt: 2` here \
             is the held delivery being charged, which after {} of them dead-letters a \
             trigger that was never refused on its merits",
            TRIGGER_MAX_DELIVER
        );

        next.let_finish(10);
        stop(shutdown_tx, run).await;
    }

    /// Review finding P2: the path where a held trigger has to **wait
    /// for a permit** on its way out, which no other test reaches.
    ///
    /// `a_held_trigger_occupies_no_worker_permit_so_other_agents_run`
    /// runs at worker cap 2, where the capped agent's own cap already
    /// limits it to one — so its `peak() == 2` holds whether or not a
    /// held trigger can get a permit at all. At worker cap **1** the
    /// only permit is the one the running invocation holds, so every
    /// released trigger has to queue for it and be handed it as the
    /// invocation ahead ends, or nothing after the first ever runs: four
    /// starts through one permit, with three parked holds and the
    /// consume loop all in play.
    #[tokio::test]
    async fn a_held_trigger_waits_for_a_worker_permit_to_run() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let capped = unique_agent_id("capped-repermit");
        let other = unique_agent_id("uncapped-repermit");

        let dir = tempfile::tempdir().unwrap();
        write_capped_definition(dir.path(), &capped, 1);
        std::fs::write(
            dir.path().join(format!("{other}.md")),
            format!("---\nname: {other}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."),
        )
        .unwrap();
        let registry = AgentRegistry::load_from_directory(dir.path(), None).expect("load");
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let registry = shared_registry(registry);

        let counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let worker = CappedWorker::new();
        let llm: Arc<dyn LlmClient> = Arc::new(FixtureClient::new());
        let consumer_name = unique_consumer_name();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        // Worker cap 1: one permit for the whole fleet.
        let dispatcher = TriggerDispatcher::new(bus.clone(), registry, worker.clone(), llm, 1)
            .with_agent_caps(Arc::clone(&counts));
        let run = tokio::spawn(async move {
            dispatcher
                .run_on_consumer(&consumer_name, None, shutdown_rx)
                .await
        });

        publish_triggers(&bus, &capped, 3).await;
        publish_triggers(&bus, &other, 1).await;
        // Nothing is gated on the test's timing: each invocation returns
        // as soon as it starts, and the assertion is that all four get
        // there.
        worker.let_finish(10);

        worker.wait_for_starts(4, Duration::from_secs(20)).await;
        assert_eq!(
            worker.peak(),
            1,
            "worker cap 1 means one at a time, held triggers included"
        );
        assert_eq!(
            worker.started_for(&capped),
            3,
            "every held trigger of the capped agent took a worker permit and ran"
        );
        assert_eq!(worker.started_for(&other), 1);

        stop(shutdown_tx, run).await;
    }

    /// Review finding C7: a trigger held past its agent's **removal**
    /// must not start.
    ///
    /// The hold re-reads the registry every tick, and a removed agent
    /// used to read as "no cap" — which admitted the trigger and ran the
    /// definition clone taken before the hold. So an operator who
    /// deleted a definition and reloaded still got invocations of it,
    /// while the same trigger arriving one second later was acked and
    /// dropped as unknown. Now both are dropped.
    #[tokio::test]
    async fn a_trigger_held_past_its_agents_removal_is_dropped_not_run() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("capped-removed");
        let (dir, registry) = registry_with_cap(&agent_id_str, 1);
        let counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let worker = CappedWorker::new();
        let (shutdown_tx, run) = spawn_dispatcher(
            &bus,
            &agent_id_str,
            registry.clone(),
            worker.clone(),
            Arc::clone(&counts),
            8,
        );

        publish_triggers(&bus, &agent_id_str, 2).await;
        worker.wait_for_starts(1, Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(worker.started(), 1, "the second trigger is held at cap 1");

        // The operator deletes the definition and reloads.
        std::fs::remove_file(dir.path().join(format!("{agent_id_str}.md"))).unwrap();
        reload_from(dir.path(), &registry).await;

        // The running invocation finishes, so a slot is free — the only
        // thing that can keep the held trigger from starting now is the
        // removal itself.
        worker.let_finish(10);
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            worker.started(),
            1,
            "a deleted agent must not run: the held trigger is dropped, not admitted"
        );
        assert!(
            counts.snapshot().is_empty(),
            "and the hold is over rather than parked forever: {:?}",
            counts.snapshot()
        );

        stop(shutdown_tx, run).await;
    }

    /// The issue's third acceptance criterion: `fq reload` picks up a
    /// changed cap. Stronger than "for the next trigger" — the cap is
    /// re-read off the current registry on every pass of the hold, so
    /// the trigger *already waiting* starts, which is what an operator
    /// raising a cap to unstick a queue is asking for.
    ///
    /// Nothing is released here: the first invocation is still running
    /// when the second starts, so only the raised cap can explain it.
    #[tokio::test]
    async fn fq_reload_raises_a_cap_for_a_trigger_that_is_already_held() {
        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let agent_id_str = unique_agent_id("capped-reload");
        let (dir, registry) = registry_with_cap(&agent_id_str, 1);
        let counts = crate::control_plane::agent_cap::AgentConcurrency::new();
        let worker = CappedWorker::new();
        let (shutdown_tx, run) = spawn_dispatcher(
            &bus,
            &agent_id_str,
            registry.clone(),
            worker.clone(),
            Arc::clone(&counts),
            8,
        );

        publish_triggers(&bus, &agent_id_str, 2).await;
        worker.wait_for_starts(1, Duration::from_secs(10)).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(worker.started(), 1, "the second trigger is held at cap 1");

        write_capped_definition(dir.path(), &agent_id_str, 2);
        reload_from(dir.path(), &registry).await;

        worker.wait_for_starts(2, Duration::from_secs(10)).await;
        assert_eq!(
            worker.peak(),
            2,
            "the held trigger started beside the running one, on the reloaded cap"
        );
        assert_eq!(
            worker.attempts(),
            vec![Some(1), Some(1)],
            "and it was still its first delivery when it did"
        );

        worker.let_finish(2);
        stop(shutdown_tx, run).await;
    }
}
