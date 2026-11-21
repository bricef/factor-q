//! Control-plane consumer that maintains coordination state
//! by subscribing to invocation lifecycle events from workers.
//!
//! Per data-architecture.md §7.2, the control-plane on
//! restart subscribes to the events that move a run's
//! `coordination_invocation_owner` row. What each one does to
//! the row is [`super::ownership`]'s transition table, applied
//! as a compare-and-set; here is what else each handler does:
//!
//! - `fq.agent.*.completed` and `fq.agent.*.failed` — a run's
//!   ending. The row follows it when it is folded, with its
//!   archive pending, so a run whose archive is late, refused or
//!   held reads as finished (issue 1024). A `failed` in `setup`
//!   belongs to a run that never started and writes nothing.
//! - `fq.agent.*.invocation.ambiguous` — workers publish on
//!   recovery when their WAL has a `dispatched`-without-
//!   `completed` row. A late report never puts a finished run
//!   back to triage.
//! - `fq.agent.*.invocation.archived` — workers publish on
//!   terminal handoff (step 8). The control-plane writes the
//!   `invocation_archive` row, records the archive on the
//!   ownership row, and publishes `invocation.archive_acked`
//!   back on the worker-scoped
//!   `fq.worker.{worker_id}.invocation.archive_acked` subject.
//! - `fq.agent.*.invocation.operator_recovered` — an operator's
//!   drop: the archive row, the ownership row, and the worker's
//!   WAL row when this process hosts it.
//!
//! The consumer also runs a periodic stale-worker sweep:
//! workers whose `last_heartbeat` is older than the
//! configured threshold get marked `stale` in
//! `coordination_worker`. This makes `fq workers stale`  allow-dead-command: describes the capability, served today by `fq workers list --stale-only`
//! meaningful even if a worker process disappears without
//! emitting a shutdown event.
//!
//! The same tick runs the stuck-invocation sweep (#37, see
//! [`super::liveness`]), which answers the other half of the
//! question: a healthy worker can host a wedged invocation, and
//! the roster cannot see that. One timer for both, because they
//! are one finding — nothing has moved for too long — read from
//! two different stores. Both wait while startup recovery decides
//! ([`hold_sweeps`]).
//!
//! Delivery semantics (the loop and ack policy live in
//! [`super::durable_consumer`]):
//! - **At-least-once** from JetStream. Every event applied twice
//!   leaves the row as once (the transition table's duplicates),
//!   and the archive insert is `ON CONFLICT DO NOTHING`, so
//!   re-delivery is safe.
//! - **Parse errors** are logged and acked.
//! - **Store errors** are transient handler errors: NAK'd to
//!   trigger redelivery. Two are the exception, and the event is
//!   logged and acked, because retrying would hold every later
//!   event behind it: an ownership row this build cannot read,
//!   which cannot be moved, and a write the store refuses by its
//!   own rules, which no retry changes. Where the ownership row is
//!   the one left as it is, the rest of the event still takes
//!   effect: an archive is recorded and acknowledged to the
//!   worker, and a drop still stops the run.
//!
//! The consumer is single-process in v1; v2 splits it onto
//! the dedicated control-plane node.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::oneshot;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::bus::{BusError, EventBus};
use crate::events::{
    Event, EventPayload, InvocationArchiveAckedPayload, InvocationArchivedPayload,
};

use super::durable_consumer::{
    DeliverFrom, DurableConsumerConfig, DurableConsumerError, HandlerError,
    run_durable_consumer_with_tick,
};
use super::ownership::{
    self, Applied, Ending, OwnerFacts, OwnershipError, OwnershipEvent, RowState,
};
use super::store::{ArchiveState, ControlPlaneStore, ControlPlaneStoreError, InvocationArchiveRow};

mod ack_wait;
mod handler_error;
mod operator_recovered;
mod stale_sweep;
mod sweep_hold;
pub use ack_wait::ACK_WAIT;
pub use sweep_hold::{SweepHold, SweepRelease, hold_sweeps};

use handler_error::{as_handler_error, is_unreadable, leaves_the_row_as_it_is};

/// Name of the durable JetStream consumer the coordination
/// consumer creates. Distinct from the projection consumer's
/// durable name so they advance independently.
pub const CONSUMER_NAME: &str = "fq-coordination";

/// Subject filters for the coordination consumer's durable
/// subscription when it carries no watermark (a consumer with one
/// reads the whole stream). They match every invocation lifecycle
/// event across all agents, and every run's ending. Note: NATS
/// subject wildcards use `*` for one token and `>` for one-or-more,
/// so the first matches `fq.agent.<id>.invocation.<kind>` for any
/// single id and any single kind.
pub const FILTER_SUBJECTS: [&str; 3] = [
    "fq.agent.*.invocation.*",
    "fq.agent.*.completed",
    "fq.agent.*.failed",
];

/// Default time after which a worker without a fresh
/// heartbeat is considered stale (30 seconds).
pub use fq_ops::surface::DEFAULT_STALE_THRESHOLD_MS;

/// Default cadence for the stale-worker sweep (10 seconds).
pub const DEFAULT_SWEEP_INTERVAL_MS: u64 = 10_000;

/// Coordination consumer. Owns the bus and the
/// control-plane store; spawn it as a tokio task via
/// [`Self::run`].
pub struct CoordinationConsumer {
    bus: EventBus,
    store: Arc<ControlPlaneStore>,
    worker_store: Option<Arc<crate::worker::store::WorkerStore>>,
    stale_threshold_ms: i64,
    sweep_interval_ms: u64,
    /// Worker id of the local worker, so we don't mark
    /// ourselves stale during sweeps. Optional because v2
    /// control-plane processes won't have a co-located worker.
    self_worker_id: Option<String>,
    /// Test-only override for the durable consumer name. When
    /// `Some`, `run` also uses the deliver-from-new variant of
    /// the bus's consumer factory so the test doesn't replay
    /// the stream's accumulated history. See
    /// [`Self::with_test_consumer_name`].
    test_consumer_name: Option<String>,
    /// Test-only override for the JetStream filter subject.
    /// When `Some`, used in place of [`FILTER_SUBJECTS`] so the
    /// test consumer only receives events for one agent's
    /// invocations (`fq.agent.<id>.>`, which takes in its
    /// endings). Without this, parallel acceptance tests
    /// cross-contaminate via the ack subject (one test's CP
    /// ack-then-delete races another test's sweeper).
    test_filter_subject: Option<String>,
    /// Runtime id stamped (as trace/invocation id) on the system
    /// events this consumer emits, so they correlate with the
    /// daemon's other `Event::system` publishes. The daemon sets
    /// it via [`Self::with_runtime_id`]; the constructor default
    /// (a fresh uuid) only applies in tests.
    runtime_id: Uuid,
    /// Advanced as lifecycle events apply — the coordination half of
    /// the read horizon: the Invocation view's fold spans stores this
    /// consumer writes, so gated reads wait on this mark too.
    watermark: Option<crate::watermark::WatermarkSender>,
    /// The stuck-invocation sweep (#37), when this process hosts one.
    /// Rides the same tick as the stale-worker sweep: the two are the
    /// same class of finding — nothing has moved for too long — and
    /// running them together means one timer, one serialisation point,
    /// and no possibility of the roster and the work disagreeing about
    /// what "now" was. `None` on a control plane with no co-located
    /// worker WAL to read, and in the unit tests that only exercise the
    /// handler.
    stuck_sweep: Option<Arc<super::liveness::StuckSweep>>,
    /// Holds both sweeps until startup recovery has decided, when the
    /// daemon starts this consumer before it ([`hold_sweeps`]).
    sweeps_held: Option<SweepHold>,
}

impl CoordinationConsumer {
    pub fn new(bus: EventBus, store: Arc<ControlPlaneStore>) -> Self {
        Self {
            bus,
            store,
            worker_store: None,
            stale_threshold_ms: DEFAULT_STALE_THRESHOLD_MS,
            sweep_interval_ms: DEFAULT_SWEEP_INTERVAL_MS,
            self_worker_id: None,
            test_consumer_name: None,
            test_filter_subject: None,
            runtime_id: Uuid::now_v7(),
            watermark: None,
            stuck_sweep: None,
            sweeps_held: None,
        }
    }

    /// Sweep nothing on a tick until `hold` is released (issue 1070).
    pub fn with_sweeps_held(mut self, hold: SweepHold) -> Self {
        self.sweeps_held = Some(hold);
        self
    }

    /// Run the stuck-invocation sweep on this consumer's tick (#37).
    ///
    /// Given as a built sweep rather than a threshold because the
    /// crossing memory is the sweep's, and it has to survive across
    /// ticks: a sweep rebuilt each tick would re-emit for every
    /// offender, every tick, which is the unbounded-noise mode this
    /// design exists to avoid.
    pub fn with_stuck_sweep(mut self, sweep: Arc<super::liveness::StuckSweep>) -> Self {
        self.stuck_sweep = Some(sweep);
        self
    }

    /// Attach the co-located worker store so authoritative operator
    /// terminal transitions also close the worker execution row.
    pub fn with_worker_store(
        mut self,
        worker_store: Arc<crate::worker::store::WorkerStore>,
    ) -> Self {
        self.worker_store = Some(worker_store);
        self
    }

    /// Set the daemon's runtime id, stamped on emitted system
    /// events (see the field doc).
    pub fn with_runtime_id(mut self, runtime_id: Uuid) -> Self {
        self.runtime_id = runtime_id;
        self
    }

    /// Override the stale-worker threshold. Test-only.
    pub fn with_stale_threshold_ms(mut self, ms: i64) -> Self {
        self.stale_threshold_ms = ms;
        self
    }

    /// Override the sweep cadence. Test-only.
    pub fn with_sweep_interval_ms(mut self, ms: u64) -> Self {
        self.sweep_interval_ms = ms;
        self
    }

    /// Tell the consumer which worker_id is the local worker
    /// so the sweep can skip it. In v1 the daemon is both
    /// roles; we don't want to mark our own worker stale.
    pub fn with_self_worker_id(mut self, worker_id: String) -> Self {
        self.self_worker_id = Some(worker_id);
        self
    }

    /// Override the JetStream durable consumer name and start
    /// from new messages only (skip the stream's history).
    /// Test-only — the acceptance harness uses this so each
    /// test gets an isolated consumer without replaying the
    /// shared stream's accumulated events.
    pub fn with_test_consumer_name(mut self, name: String) -> Self {
        self.test_consumer_name = Some(name);
        self
    }

    /// Narrow the JetStream filter subject for this consumer.
    /// Test-only — the acceptance harness sets this to
    /// `fq.agent.<test_agent_id>.>` so parallel tests' CP
    /// consumers don't pick up each other's events (and don't
    /// race on the worker-scoped ack subject), while still seeing
    /// the agent's `completed` and `failed`.
    pub fn with_test_filter_subject(mut self, filter: String) -> Self {
        self.test_filter_subject = Some(filter);
        self
    }

    /// Run the consumer loop until `shutdown` fires. The
    /// Advance `watermark` as lifecycle events apply (the read
    /// horizon's coordination half). Also flips the durable to
    /// strict-order delivery so the mark can never expose a gap.
    pub fn with_watermark(mut self, watermark: crate::watermark::WatermarkSender) -> Self {
        self.watermark = Some(watermark);
        self
    }

    /// stale-worker sweep rides the loop's tick arm, serialised
    /// with message handling on the same task.
    pub async fn run(
        self,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), CoordinationConsumerError> {
        // A mark-bearing consumer must see the WHOLE stream: a
        // filtered mark cannot vouch for the gaps between its matches
        // (a reader gated at a non-lifecycle sequence would wait
        // forever). Non-matching events resolve as no-ops and advance
        // the mark. Without a mark, the #118 filter optimisation
        // stands.
        let filter_subjects: Vec<String> = match (&self.watermark, &self.test_filter_subject) {
            (Some(_), _) => Vec::new(),
            (None, Some(test)) => vec![test.clone()],
            (None, None) => FILTER_SUBJECTS.iter().map(|s| s.to_string()).collect(),
        };
        // A test-name override pairs with deliver-from-new so
        // each test gets a fresh cursor on the shared stream
        // (see [`Self::with_test_consumer_name`]).
        let (durable_name, deliver_from) = match &self.test_consumer_name {
            Some(name) => (name.clone(), DeliverFrom::New),
            None => (CONSUMER_NAME.to_string(), DeliverFrom::Beginning),
        };
        let config = DurableConsumerConfig {
            durable_name,
            filter_subjects,
            deliver_from,
            // The mark's contract needs resolved-contiguous delivery;
            // without a mark the extra strictness buys nothing.
            // (Strict refuses deliver-from-new, so test consumers
            // never combine the two.)
            strict_order: self.watermark.is_some(),
            ack_wait: Some(ACK_WAIT),
        };
        let this = &self;
        run_durable_consumer_with_tick(
            &self.bus,
            config,
            shutdown,
            |delivery| async move {
                let seq = delivery.stream_seq;
                let handled = this.handle_event(delivery.event).await;
                // After the handler resolves: mirror the projection
                // consumer's advance-after-commit discipline. A permanent
                // refusal is resolved too: the loop acks it and moves on,
                // and what the handler could do it has done (an
                // unreadable row's drop or archive), so a read gated at
                // its sequence must not wait for the next event. Only a
                // transient error, redelivered, holds the mark.
                if !matches!(handled, Err(HandlerError::Transient(_)))
                    && let (Some(watermark), Some(seq)) = (&this.watermark, seq)
                {
                    watermark.advance(seq);
                }
                handled
            },
            Duration::from_millis(self.sweep_interval_ms),
            || async move {
                if this.sweeps_held.as_ref().is_some_and(SweepHold::is_held) {
                    return;
                }
                if let Err(err) = this.sweep_stale_workers().await {
                    warn!(error = %err, "stale-worker sweep failed");
                }
                // Second, and never instead: a failed roster sweep must
                // not take the work sweep with it. They read different
                // stores and fail for different reasons.
                if let Some(sweep) = &this.stuck_sweep
                    && let Err(err) = sweep.tick(Utc::now().timestamp_millis()).await
                {
                    warn!(error = %err, "stuck-invocation sweep failed");
                }
            },
        )
        .await
        .map_err(CoordinationConsumerError::from)
    }

    /// Dispatch one invocation lifecycle event to its handler.
    /// The shared loop owns the ack: `Ok` acks, a transient error
    /// NAKs for redelivery, and a permanent one is logged and acked.
    async fn handle_event(&self, event: Event) -> Result<(), HandlerError> {
        let result = match &event.payload {
            EventPayload::InvocationAmbiguous(payload) => {
                self.handle_invocation_ambiguous(&event, payload).await
            }
            EventPayload::InvocationArchived(payload) => {
                self.handle_invocation_archived(&event, payload).await
            }
            EventPayload::InvocationOperatorRecovered(payload) => {
                self.handle_invocation_operator_recovered(&event, payload)
                    .await
            }
            // A run's ending moves its row when it is folded, not when its
            // archive arrives (issue 1024).
            EventPayload::Completed(_) => {
                self.handle_ending(&event, OwnershipEvent::Ended(Ending::Completed))
                    .await
            }
            // Which `failed` is an ending is `OwnershipEvent::of_failed`'s,
            // one arm per phase.
            EventPayload::Failed(payload) => {
                self.handle_ending(&event, OwnershipEvent::of_failed(payload.phase))
                    .await
            }
            // The ending of a run whose own the worker holds (issue 1071),
            // folded as that ending; a row it writes names that worker and
            // is dated at the run's start, not at this fold.
            EventPayload::InvocationEndingHeld(held) => self
                .move_row(
                    &event,
                    OwnershipEvent::Ended(held.outcome.into()),
                    OwnerFacts {
                        worker_id: held.worker_id.as_str(),
                        at_ms: held.started_at_ms,
                    },
                    None,
                )
                .await
                .map(|_| ()),
            EventPayload::InvocationOperatorResumed(_) => Ok(()),
            _ => {
                // Unknown invocation event variant — ack and
                // move on. We only filter to invocation.*
                // subjects but new variants may surface
                // before the consumer learns about them.
                Ok(())
            }
        };
        result.map_err(|err| as_handler_error(&event, err))
    }

    /// Apply `what` to `event`'s run's ownership row, `facts` standing for
    /// the worker and the time on a row the move creates or one that takes
    /// them, and say what happened: debug for a move or a duplicate, warn for a
    /// refusal (the row records the run finished, and the event would put
    /// it back in progress) and for an ending that reaches a row recorded
    /// `failed` with its archive written or none to come, the states an
    /// operator's drop leaves (most likely the drop came first, which is
    /// no fault), error
    /// for any other conflicting ending (the run published two). `reported` on a conflict is the ending the event
    /// names, `invocation.archived`'s `final_phase` included. `report` is
    /// a late `invocation.ambiguous`'s own fields, which tell startup
    /// categorisation (`stuck_entity` a dispatch kind) from a resume that
    /// could not be carried out (`recovery`, with the error in `note`).
    async fn move_row(
        &self,
        event: &Event,
        what: OwnershipEvent,
        facts: OwnerFacts<'_>,
        report: Option<&crate::events::InvocationAmbiguousPayload>,
    ) -> Result<Applied, CoordinationConsumerError> {
        let invocation_id = event.envelope.invocation_id.to_string();
        let applied = ownership::apply(self.store.as_ref(), &invocation_id, what, facts).await?;
        let agent_id = event.envelope.agent_id.as_str();
        let event_type = event.payload.event_type();
        let stuck_entity = report.map(|report| report.stuck_entity.as_str());
        let note = report.map(|report| report.note.as_str());
        match applied {
            Applied::Ignored => {}
            Applied::Moved { from, to } => debug!(
                invocation_id,
                agent_id,
                event_type,
                from = from.map(RowState::name),
                to = to.name(),
                "ownership row moved"
            ),
            Applied::Duplicate { at } => debug!(
                invocation_id,
                agent_id,
                event_type,
                at = at.name(),
                "ownership row already says what the event says; left as it was"
            ),
            Applied::Refused { recorded } => warn!(
                invocation_id,
                agent_id,
                event_type,
                recorded = recorded.name(),
                stuck_entity,
                note,
                "event for a run already recorded finished; the ownership row keeps its status"
            ),
            Applied::Conflict { recorded, moved_to } => {
                let reported = what.ending().map(|ending| ending.status().as_str());
                if recorded == RowState::ended(Ending::Failed, ArchiveState::Archived)
                    || recorded == RowState::ended(Ending::Failed, ArchiveState::WillNotCome)
                {
                    warn!(
                        invocation_id,
                        agent_id,
                        event_type,
                        recorded = recorded.name(),
                        reported,
                        "an ending reached a run recorded failed with its archive written or none \
                         to come, as an operator's drop leaves it; the ownership row keeps the \
                         ending it recorded"
                    );
                } else {
                    error!(
                        invocation_id,
                        agent_id,
                        event_type,
                        recorded = recorded.name(),
                        reported,
                        moved_to = moved_to.map(RowState::name),
                        "two endings published for one run; the ownership row keeps the ending \
                         it recorded"
                    );
                }
            }
        }
        Ok(applied)
    }

    /// A run's ending, `completed` or `failed`: the row follows it, with
    /// its archive pending until `invocation.archived` arrives. A triggered
    /// run has no row yet, and the event's agent id stands in for the
    /// worker's until the archive names it.
    async fn handle_ending(
        &self,
        event: &Event,
        what: OwnershipEvent,
    ) -> Result<(), CoordinationConsumerError> {
        self.move_row(
            event,
            what,
            folded_now(event.envelope.agent_id.as_str()),
            None,
        )
        .await?;
        Ok(())
    }

    /// Mark the run awaiting triage, unless its row records it finished.
    ///
    /// The event can reach this consumer after the run's ending: a start
    /// whose recovery stopped waiting before this consumer folded the last
    /// process's drop of a run killed mid tool call reports that run
    /// ambiguous after the drop on the stream (issue 1070); and a
    /// resume whose worker row cannot be read is reported though the run
    /// may have ended. The transition refuses a finished row, and the
    /// refusal is logged with the report's own fields, once per delivery.
    /// A redelivery of the same report finds the row `ambiguous` and
    /// leaves it as the first delivery wrote it, and a row recovery
    /// restored keeps the worker id it names.
    pub(crate) async fn handle_invocation_ambiguous(
        &self,
        event: &Event,
        payload: &crate::events::InvocationAmbiguousPayload,
    ) -> Result<(), CoordinationConsumerError> {
        // The trigger-dispatch path writes no ownership row, so a run in
        // flight usually has none: the event's `agent_id` stands in for
        // the worker id of the row this writes.
        self.move_row(
            event,
            OwnershipEvent::Ambiguous,
            folded_now(event.envelope.agent_id.as_str()),
            Some(payload),
        )
        .await?;
        Ok(())
    }

    /// Step 8: worker → control-plane archive hand-off.
    ///
    /// 1. Write the archive row (idempotent on `invocation_id`
    ///    via `ON CONFLICT DO NOTHING` — a redelivery is a
    ///    no-op).
    /// 2. Record the archive on the ownership row, which also takes
    ///    the worker's id. A row that has not ended takes the ending
    ///    `final_phase` names; a row that has keeps its ending, and an
    ///    archive naming another is a second ending, logged at error.
    ///    An operator's drop before the archive arrived therefore
    ///    stands (what that leaves in the archive record is issue
    ///    1070's).
    /// 3. Publish `invocation.archive_acked` on
    ///    `fq.worker.{worker_id}.invocation.archive_acked` so
    ///    the originating worker can delete its local row.
    ///
    /// If step 3 fails the message is NAK'd: insert is
    /// idempotent and the worker's retry sweeper will republish
    /// `invocation.archived` anyway, so duplicates are safe. An ownership
    /// row this build cannot read, or one whose move the store refuses, is
    /// not moved, but the archive record is written by then, so the ack
    /// still goes out before the event is refused as permanent: without it
    /// the worker would keep its terminal row and republish the whole
    /// archive at every sweep, for good. Not when the store refused the
    /// row's insert, though, and no row is there: acknowledged, the worker
    /// would delete its terminal row, and the run would be on no record a
    /// trigger's claim is settled from, so its trigger could run again. The
    /// worker is not told, keeps its row and republishes, which the
    /// record's insert takes as a duplicate.
    pub(crate) async fn handle_invocation_archived(
        &self,
        event: &Event,
        payload: &InvocationArchivedPayload,
    ) -> Result<(), CoordinationConsumerError> {
        let invocation_id = event.envelope.invocation_id.to_string();
        let now_ms = Utc::now().timestamp_millis();
        debug!(
            invocation_id = %invocation_id,
            worker_id = %payload.worker_id,
            "writing invocation archive row"
        );

        self.store
            .insert_archive(&InvocationArchiveRow {
                invocation_id: invocation_id.clone(),
                agent_id: event.envelope.agent_id.as_str().to_string(),
                final_phase: payload.final_phase.clone(),
                final_state_blob: payload.final_state_blob.clone(),
                started_at: payload.started_at_ms,
                terminal_at: payload.terminal_at_ms,
                archived_at: now_ms,
            })
            .await?;

        let moved = self
            .move_row(
                event,
                OwnershipEvent::Archived(Ending::of_archived_phase(&payload.final_phase)),
                folded_now(payload.worker_id.as_str()),
                None,
            )
            .await;
        if let Err(err) = &moved {
            if !leaves_the_row_as_it_is(err) {
                return moved.map(|_| ());
            }
            if !is_unreadable(err) && !self.an_ownership_row_is_there(&invocation_id).await? {
                warn!(
                    invocation_id = %invocation_id,
                    error = %err,
                    "the archive is recorded and not acknowledged to the worker: the store refused \
                     the run's ownership row, and with no row there the worker keeps its own"
                );
                return moved.map(|_| ());
            }
        }

        let ack = Event::new(
            event.envelope.agent_id.clone(),
            event.envelope.invocation_id,
            EventPayload::InvocationArchiveAcked(InvocationArchiveAckedPayload {
                worker_id: payload.worker_id.clone(),
            }),
        );
        self.bus.publish(&ack).await?;
        if let Err(err) = &moved {
            warn!(
                invocation_id = %invocation_id,
                error = %err,
                "the archive is recorded and acknowledged to the worker; the run's ownership row \
                 is left as it is, since it cannot be read or the store refused its move"
            );
        }
        moved.map(|_| ())
    }

    /// Whether `invocation_id` has an ownership row, readable or not.
    async fn an_ownership_row_is_there(
        &self,
        invocation_id: &str,
    ) -> Result<bool, CoordinationConsumerError> {
        match self.store.get_invocation_owner(invocation_id).await {
            Ok(row) => Ok(row.is_some()),
            Err(ControlPlaneStoreError::Malformed(_)) => Ok(true),
            Err(err) => Err(err.into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinationConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("control-plane store error: {0}")]
    Store(#[from] ControlPlaneStoreError),

    #[error("worker store error: {0}")]
    WorkerStore(#[from] crate::worker::store::WorkerStoreError),

    #[error("ownership row: {0}")]
    Ownership(#[from] OwnershipError),

    #[error("jetstream message stream error: {0}")]
    Stream(String),
}

impl From<DurableConsumerError> for CoordinationConsumerError {
    fn from(err: DurableConsumerError) -> Self {
        match err {
            DurableConsumerError::Bus(err) => Self::Bus(err),
            DurableConsumerError::Stream(err) => Self::Stream(err),
        }
    }
}

/// The owner facts of an event folded now: `worker_id`, and the time of
/// the fold.
fn folded_now(worker_id: &str) -> OwnerFacts<'_> {
    OwnerFacts {
        worker_id,
        at_ms: Utc::now().timestamp_millis(),
    }
}

#[cfg(test)]
mod ambiguous_tests;
#[cfg(test)]
mod archive_none_tests;
#[cfg(test)]
mod drop_record_tests;

#[cfg(test)]
mod outcome_tests;

#[cfg(test)]
mod ack_wait_tests;

#[cfg(test)]
mod sweep_hold_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::control_plane::store::OwnerStatus;
    use futures::StreamExt;

    #[tokio::test]
    async fn sweep_emits_worker_orphaned_once_per_transition() {
        use tempfile::tempdir;
        use uuid::Uuid;

        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );

        // A worker whose last heartbeat (epoch 1s) is ancient
        // against any threshold.
        let worker_name = format!("orphan-test-{}", Uuid::now_v7().simple());
        store
            .register_worker(&worker_name, "host", 1_000)
            .await
            .unwrap();

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone())
            .with_runtime_id(Uuid::now_v7())
            .with_stale_threshold_ms(1);

        let mut sub = bus
            .subscribe(format!("fq.worker.{worker_name}.orphaned"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        consumer.sweep_stale_workers().await.expect("first sweep");
        let event = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("worker.orphaned within 5s")
            .expect("stream open")
            .expect("event deserialises");
        match &event.payload {
            EventPayload::WorkerOrphaned(p) => {
                assert_eq!(p.worker_id.as_str(), worker_name);
                assert_eq!(p.last_heartbeat_ms, 1_000);
            }
            other => panic!("expected worker.orphaned, got {other:?}"),
        }

        // Second sweep: the alive→stale transition is already
        // consumed — the event must not re-fire.
        consumer.sweep_stale_workers().await.expect("second sweep");
        let quiet = tokio::time::timeout(Duration::from_millis(500), sub.next()).await;
        assert!(
            quiet.is_err(),
            "second sweep must not re-emit worker.orphaned"
        );
    }

    #[tokio::test]
    async fn coordination_consumer_handles_invocation_ambiguous_end_to_end() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        use crate::events::{Event, EventPayload, InvocationAmbiguousPayload};
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );

        // Use a unique invocation id and a unique consumer
        // name so this test can run alongside others.
        let invocation_id = Uuid::now_v7();
        let agent_id = AgentId::new(format!("coord-test-{}", Uuid::now_v7().simple())).unwrap();
        let consumer_name = format!("fq-coordination-test-{}", Uuid::now_v7().simple());

        // Spawn the production consumer with its test overrides:
        // a unique durable name (deliver-from-new) so we don't
        // compete with the real fq-coordination consumer, and an
        // agent-scoped filter so parallel tests' events don't
        // cross-contaminate.
        let consumer = CoordinationConsumer::new(bus.clone(), store.clone())
            .with_test_consumer_name(consumer_name)
            .with_test_filter_subject(format!("fq.agent.{}.invocation.*", agent_id.as_str()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(consumer.run(shutdown_rx));

        // Give the consumer a moment to register.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Publish an invocation.ambiguous event.
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".to_string(),
                stuck_call_id: "tc-test".to_string(),
                note: "test".to_string(),
            }),
        );
        bus.publish(&event).await.expect("publish");

        // Wait for the coordination row to appear.
        let inv_str = invocation_id.to_string();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(row) = store.get_invocation_owner(&inv_str).await.unwrap()
                && row.status == OwnerStatus::Ambiguous
            {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("coordination row did not appear in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        // Re-publishing the same event is idempotent.
        bus.publish(&event).await.expect("publish 2");
        let row = store.get_invocation_owner(&inv_str).await.unwrap().unwrap();
        assert_eq!(row.status, OwnerStatus::Ambiguous);
    }

    #[tokio::test]
    async fn handler_archives_invocation_and_publishes_ack() {
        // Direct-handler test: drive `handle_invocation_archived`
        // with a real bus (so the ack publish can be observed)
        // and assert the archive row, ownership status flip, and
        // ack subject all land. Skips the JetStream consume side
        // since that's exercised by the ambiguous end-to-end test
        // above — the dispatch loop is the same code path.
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        use crate::events::EventPayload;
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );

        let agent_id = AgentId::new(format!("arch-test-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();

        // Subscribe before publishing so the ack isn't missed.
        let mut ack_sub = bus
            .subscribe(format!(
                "fq.worker.{}.invocation.archive_acked",
                worker_id.as_str()
            ))
            .await
            .expect("subscribe to ack subject");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone());
        let archived_event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: worker_id.clone(),
                final_phase: "completed".to_string(),
                final_state_blob: b"final-state".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        );
        let archived_payload = match &archived_event.payload {
            EventPayload::InvocationArchived(p) => p.clone(),
            _ => unreachable!(),
        };
        consumer
            .handle_invocation_archived(&archived_event, &archived_payload)
            .await
            .expect("archived handler succeeds");

        // Archive row written.
        let inv_str = invocation_id.to_string();
        let archive = store
            .get_archive(&inv_str)
            .await
            .unwrap()
            .expect("archive row should exist after handler");
        assert_eq!(archive.agent_id, agent_id.as_str());
        assert_eq!(archive.final_phase, "completed");
        assert_eq!(archive.final_state_blob, b"final-state");
        assert_eq!(archive.started_at, 1_000);
        assert_eq!(archive.terminal_at, 2_000);

        // Ownership flipped to Completed.
        let owner = store
            .get_invocation_owner(&inv_str)
            .await
            .unwrap()
            .expect("ownership row");
        assert_eq!(owner.status, OwnerStatus::Completed);
        assert_eq!(owner.worker_id, worker_id.as_str());

        // Ack arrived on the worker-scoped subject.
        let ack_event = tokio::time::timeout(Duration::from_secs(2), ack_sub.next())
            .await
            .expect("ack timeout")
            .expect("ack stream closed")
            .expect("ack deserialise");
        assert_eq!(ack_event.envelope.invocation_id, invocation_id);
        match &ack_event.payload {
            EventPayload::InvocationArchiveAcked(p) => {
                assert_eq!(p.worker_id, worker_id);
            }
            other => panic!("expected InvocationArchiveAcked, got {other:?}"),
        }

        // Idempotency: handling the same event twice is safe;
        // the archive row is unchanged and a second ack is
        // published (the worker's local dedupe on
        // invocation_id handles that).
        consumer
            .handle_invocation_archived(&archived_event, &archived_payload)
            .await
            .expect("redelivery is a no-op for the store");
        let archive_again = store.get_archive(&inv_str).await.unwrap().unwrap();
        assert_eq!(
            archive_again.archived_at, archive.archived_at,
            "ON CONFLICT DO NOTHING preserves the first archived_at"
        );
    }

    #[tokio::test]
    async fn handler_operator_recovered_writes_archive_and_updates_owner() {
        // Operator-issued drop: no live worker involvement.
        // Handler writes the archive row, flips the owner row
        // to Failed, and does not publish an ack.
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        use crate::events::{Event, EventPayload, InvocationOperatorRecoveredPayload};
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );
        let worker_store = Arc::new(
            crate::worker::store::WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .unwrap(),
        );

        let agent_id =
            AgentId::new(format!("op-recover-test-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // Seed an Ambiguous owner row as if the worker reported
        // it on restart.
        store
            .upsert_invocation_ownership(
                &inv_str,
                worker_id.as_str(),
                1_000,
                OwnerStatus::Ambiguous,
            )
            .await
            .unwrap();

        worker_store
            .upsert_invocation_state(&crate::worker::store::InvocationStateRow {
                invocation_id: inv_str.clone(),
                agent_id: agent_id.as_str().to_string(),
                schema_version: 1,
                phase: "dispatching_tools".into(),
                state_blob: vec![],
                step_index: 25,
                started_at: 1_000,
                updated_at: 1_500,
                terminal_at: None,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
                terminal_outcome: None,
                terminal_published_at: None,
            })
            .await
            .unwrap();

        // Subscribe to the worker's ack subject so we can
        // assert nothing is published there.
        let mut ack_sub = bus
            .subscribe(format!(
                "fq.worker.{}.invocation.archive_acked",
                worker_id.as_str()
            ))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone())
            .with_worker_store(worker_store.clone());
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: Some("flaky network".to_string()),
                requested_by: None,
                archive: None,
            }),
        );
        let payload = match &event.payload {
            EventPayload::InvocationOperatorRecovered(p) => p.clone(),
            _ => unreachable!(),
        };
        consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("handler succeeds");

        // Archive row exists.
        let archive = store
            .get_archive(&inv_str)
            .await
            .unwrap()
            .expect("archive row");
        assert_eq!(archive.agent_id, agent_id.as_str());
        assert_eq!(archive.final_phase, "failed");
        assert!(archive.final_state_blob.is_empty());

        // Owner row flipped to Failed, worker_id preserved.
        let owner = store
            .get_invocation_owner(&inv_str)
            .await
            .unwrap()
            .expect("owner row");
        assert_eq!(owner.status, OwnerStatus::Failed);
        assert_eq!(owner.worker_id, worker_id.as_str());

        let worker_state = worker_store
            .get_invocation_state(&inv_str)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(worker_state.phase, "failed");
        assert!(worker_state.terminal_at.is_some());
        assert!(
            worker_store
                .find_in_flight_invocations()
                .await
                .unwrap()
                .is_empty()
        );

        // No ack published.
        let ack = tokio::time::timeout(Duration::from_millis(200), ack_sub.next()).await;
        assert!(
            ack.is_err(),
            "operator-recovered must not publish an ack; got {ack:?}"
        );
    }

    #[tokio::test]
    async fn handler_operator_recovered_idempotent_on_redelivery() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        use crate::events::{Event, EventPayload, InvocationOperatorRecoveredPayload};
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );
        let agent_id =
            AgentId::new(format!("op-recover-idem-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        store
            .upsert_invocation_ownership(
                &inv_str,
                worker_id.as_str(),
                1_000,
                OwnerStatus::Ambiguous,
            )
            .await
            .unwrap();

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone());
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
                requested_by: None,
                archive: None,
            }),
        );
        let payload = match &event.payload {
            EventPayload::InvocationOperatorRecovered(p) => p.clone(),
            _ => unreachable!(),
        };

        consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("first apply");
        let first = store.get_archive(&inv_str).await.unwrap().unwrap();

        consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("redelivery is a no-op for the archive insert");
        let second = store.get_archive(&inv_str).await.unwrap().unwrap();
        assert_eq!(
            first.archived_at, second.archived_at,
            "redelivery preserves the first archived_at"
        );

        let owner = store.get_invocation_owner(&inv_str).await.unwrap().unwrap();
        assert_eq!(owner.status, OwnerStatus::Failed);
    }

    #[tokio::test]
    async fn handler_archived_does_not_downgrade_failed_owner() {
        // Race scenario: operator drops first, sets owner =
        // Failed; then the worker emits invocation.archived
        // with final_phase = "completed". The owner row must
        // remain Failed.
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        use crate::events::{Event, EventPayload, InvocationArchivedPayload};
        use crate::worker::WorkerId;
        use tempfile::tempdir;
        use uuid::Uuid;

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );
        let agent_id = AgentId::new(format!("race-test-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();

        // Operator already set this to Failed.
        store
            .upsert_invocation_ownership(&inv_str, worker_id.as_str(), 1_000, OwnerStatus::Failed)
            .await
            .unwrap();

        // Worker now reports archived with completed (a stale
        // success that the operator decided to ignore).
        let consumer = CoordinationConsumer::new(bus.clone(), store.clone());
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: worker_id.clone(),
                final_phase: "completed".to_string(),
                final_state_blob: b"would-have-been-result".to_vec(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        );
        let payload = match &event.payload {
            EventPayload::InvocationArchived(p) => p.clone(),
            _ => unreachable!(),
        };
        consumer
            .handle_invocation_archived(&event, &payload)
            .await
            .expect("handler succeeds");

        let owner = store.get_invocation_owner(&inv_str).await.unwrap().unwrap();
        assert_eq!(
            owner.status,
            OwnerStatus::Failed,
            "operator's Failed must stick; worker's later completed must not downgrade it"
        );
    }

    #[tokio::test]
    async fn completed_invocation_archives_and_worker_cleans_up_against_mock() {
        // The plan's deferred acceptance test, realised against
        // MockAnthropicServer instead of the live Anthropic API.
        // Full pipeline: ReducerRunner → bus → CoordinationConsumer
        // → archive row + ack → ArchiveAckConsumer → invocation_state
        // row deleted. Uses the TestRuntime harness so the setup
        // boilerplate isn't repeated across scenarios.
        use crate::Agent;
        use crate::events::TriggerSource;
        use crate::test_support::mock_anthropic::MockResponse;
        use crate::test_support::runtime::TestRuntime;
        use crate::worker::InvocationOutcome;
        use crate::worker::reducer::Harness;
        use crate::{PricingTable, ReducerContext, ReducerRunner, RunnerConfig, ToolRegistry};

        let rt = TestRuntime::start().await.expect("harness");
        rt.push_llm_response(MockResponse::report_success("done.", 12, 4));

        // Capture the agent's invocation chain on NATS for the
        // event-order assertion.
        let mut chain_sub = rt
            .bus()
            .subscribe(format!("fq.agent.{}.>", rt.agent_id().as_str()))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Drive a single invocation through a fresh runner.
        let agent = Agent::builder()
            .id(rt.agent_id().as_str())
            .model("claude-haiku-4-5")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();
        let llm = rt.llm_client();
        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(rt.bus().clone())
                    .pricing(Arc::new(PricingTable::empty()))
                    .store(rt.worker_store().clone())
                    .worker_id(rt.worker_id().clone())
                    .build(),
            ),
            Harness::new(),
        );

        let outcome = runner
            .run(
                &agent,
                &llm,
                TriggerSource::Manual,
                None,
                serde_json::json!({"input": "go"}),
            )
            .await
            .expect("run completes");
        let invocation_id = match outcome {
            InvocationOutcome::Completed { invocation_id, .. } => invocation_id,
            other => panic!("expected Completed outcome, got {other:?}"),
        };

        // Full hand-off: archive row on CP, invocation_state
        // deleted on worker.
        rt.wait_for_archive(invocation_id, Duration::from_secs(10))
            .await
            .expect("archive row");
        rt.wait_for_local_cleanup(invocation_id, Duration::from_secs(10))
            .await
            .expect("invocation_state cleanup");

        // Archive row contents.
        let archive = rt
            .cp_store()
            .get_archive(&invocation_id.to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archive.agent_id, rt.agent_id().as_str());
        assert_eq!(archive.final_phase, "completed");

        // The ownership row followed the `completed`, then recorded the
        // archive, under the worker's own id (issue 1024).
        let owner = rt
            .cp_store()
            .get_invocation_owner(&invocation_id.to_string())
            .await
            .unwrap()
            .expect("the run's ending wrote its ownership row");
        assert_eq!(
            (owner.status, owner.archive_state),
            (
                OwnerStatus::Completed,
                crate::control_plane::store::ArchiveState::Archived
            )
        );
        assert_eq!(owner.worker_id, rt.worker_id().as_str());

        // Drain captured events; verify completed precedes
        // invocation_archived.
        let mut chain_kinds = Vec::new();
        let collect_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < collect_deadline {
            match tokio::time::timeout(Duration::from_millis(200), chain_sub.next()).await {
                Ok(Some(Ok(ev))) => {
                    chain_kinds.push(crate::test_support::events::event_kind(&ev));
                }
                _ => break,
            }
        }
        assert!(
            chain_kinds
                .windows(2)
                .any(|w| w == ["completed", "invocation_archived"]),
            "expected completed followed by invocation_archived in {chain_kinds:?}",
        );

        // Mock saw exactly one request with the right model.
        let received = rt.mock().received_requests();
        assert_eq!(
            received.len(),
            1,
            "expected one chat call, got {received:?}"
        );
        assert_eq!(received[0]["model"], "claude-haiku-4-5");

        rt.shutdown().await;
    }

    /// The archive hand-off end to end with the state as base64 text
    /// (#981). The worker runs an invocation to terminal while the
    /// control plane is down, so its terminal row and the state in it can
    /// be read before the ack deletes them; the control plane then comes
    /// up, takes the sweeper's republish off the stream, and stores the
    /// state. What it stores is exactly what the worker held, and the
    /// event that carried it wrote the state as text, not as an array.
    #[tokio::test]
    async fn an_archived_run_s_stored_state_is_the_worker_s() {
        use crate::Agent;
        use crate::bus::STREAM_NAME;
        use crate::events::{TriggerSource, subjects};
        use crate::test_support::mock_anthropic::MockResponse;
        use crate::test_support::runtime::TestRuntime;
        use crate::worker::InvocationOutcome;
        use crate::worker::reducer::Harness;
        use crate::{PricingTable, ReducerContext, ReducerRunner, RunnerConfig, ToolRegistry};

        let mut rt = TestRuntime::builder()
            .with_coordination(false)
            .archive_retry_interval_ms(300)
            .start()
            .await
            .expect("harness");
        rt.push_llm_response(MockResponse::report_success("done.", 12, 4));
        let agent = Agent::builder()
            .id(rt.agent_id().as_str())
            .model("claude-haiku-4-5")
            .system_prompt("be brief")
            .budget(1.0)
            .build()
            .unwrap();
        let runner = ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(ToolRegistry::with_builtins()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .bus(rt.bus().clone())
                    .pricing(Arc::new(PricingTable::empty()))
                    .store(rt.worker_store().clone())
                    .worker_id(rt.worker_id().clone())
                    .build(),
            ),
            Harness::new(),
        );
        let outcome = runner
            .run(
                &agent,
                &rt.llm_client(),
                TriggerSource::Manual,
                None,
                serde_json::json!({"input": "go"}),
            )
            .await
            .expect("run completes");
        let InvocationOutcome::Completed { invocation_id, .. } = outcome else {
            panic!("expected Completed, got {outcome:?}");
        };
        let inv_str = invocation_id.to_string();

        let worker_row = rt
            .worker_store()
            .get_invocation_state(&inv_str)
            .await
            .unwrap()
            .expect("the terminal row waits for the control plane");
        assert!(
            !worker_row.state_blob.is_empty(),
            "the worker holds the run's final state"
        );

        let raw = rt
            .bus()
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .expect("the event stream")
            .get_last_raw_message_by_subject(&subjects::agent_invocation_archived(
                rt.agent_id().as_str(),
            ))
            .await
            .expect("the archive event is on the stream");
        let wire: serde_json::Value =
            serde_json::from_slice(&raw.payload).expect("the archive event is JSON");
        assert!(
            wire["payload"]["payload"]["final_state_blob"].is_string(),
            "the state goes on the wire as text: {}",
            wire["payload"]["payload"]["final_state_blob"]
        );

        rt.start_coordination().await.expect("start coordination");
        rt.wait_for_archive(invocation_id, Duration::from_secs(15))
            .await
            .expect("the control plane archives the sweeper's republish");
        let archive = rt.cp_store().get_archive(&inv_str).await.unwrap().unwrap();
        assert_eq!(
            archive.final_state_blob, worker_row.state_blob,
            "the archived state is the worker's, byte for byte"
        );
        rt.wait_for_local_cleanup(invocation_id, Duration::from_secs(15))
            .await
            .expect("the ack closes the hand-off");

        rt.shutdown().await;
    }

    /// An `invocation.archived` an earlier build wrote, with the state as
    /// a JSON array of integers, is still taken off the stream and
    /// archived with those bytes (#981): the stream keeps such events for
    /// its retention window after an upgrade, and the control plane must
    /// not lose a run's state, or the ack, because of its encoding.
    #[tokio::test]
    async fn an_archive_in_the_integer_array_form_is_still_archived() {
        use crate::events::subjects;
        use crate::worker::WorkerId;
        use tempfile::tempdir;

        let server = crate::test_support::nats::test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );
        let agent_id = AgentId::new(format!("old-form-{}", Uuid::now_v7().simple())).unwrap();
        let worker_id = WorkerId::new(Uuid::now_v7().to_string()).expect("worker id");
        let invocation_id = Uuid::now_v7();

        let consumer = CoordinationConsumer::new(bus.clone(), store.clone())
            .with_test_consumer_name(format!("fq-coordination-test-{}", Uuid::now_v7().simple()))
            .with_test_filter_subject(format!("fq.agent.{}.invocation.*", agent_id.as_str()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(consumer.run(shutdown_rx));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut ack_sub = bus
            .subscribe(format!(
                "fq.worker.{}.invocation.archive_acked",
                worker_id.as_str()
            ))
            .await
            .expect("subscribe to the ack");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The event as a build before #981 published it.
        let state = br#"{"phase":"completed","messages":[]}"#.to_vec();
        let event = Event::new(
            agent_id.clone(),
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: worker_id.clone(),
                final_phase: "completed".to_string(),
                final_state_blob: state.clone(),
                started_at_ms: 1_000,
                terminal_at_ms: 2_000,
            }),
        );
        let mut wire = serde_json::to_value(&event).unwrap();
        wire["payload"]["payload"]["final_state_blob"] = serde_json::json!(state);
        bus.jetstream()
            .publish(
                subjects::agent_invocation_archived(agent_id.as_str()),
                serde_json::to_vec(&wire).unwrap().into(),
            )
            .await
            .expect("publish the old form")
            .await
            .expect("the old form is stored");

        let inv_str = invocation_id.to_string();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let archive = loop {
            if let Some(archive) = store.get_archive(&inv_str).await.unwrap() {
                break archive;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the old form was never archived"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(archive.final_state_blob, state);
        let ack = tokio::time::timeout(Duration::from_secs(2), ack_sub.next())
            .await
            .expect("an ack for the old form")
            .expect("ack stream open")
            .expect("the ack reads");
        assert_eq!(ack.envelope.invocation_id, invocation_id);

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
