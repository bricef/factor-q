//! Worker archive retry sweeper.
//!
//! A long-lived tokio task spawned alongside the worker. It
//! periodically scans `invocation_state` for terminal rows that
//! are still in archive flow (`archive_status = "pending"` or
//! NULL with `terminal_at != NULL`) and republishes
//! `invocation.archived`. The control-plane's coordination
//! consumer is idempotent on `invocation_id`, so a duplicate
//! `archived` is safe — the second insert is a no-op and a
//! fresh ack is published, which is exactly what an offline
//! worker would have missed on first transmission.
//!
//! Before `archived`, it republishes the row's stored terminal
//! event (`terminal_outcome`, v12) while that is not stamped
//! published — the healer for a `completed` (#823), or a `failed`
//! the reducer decided (#946), whose publish was lost after the
//! row went terminal. It is the stored event itself,
//! with the original's `event_id`, and delivery is at-least-once:
//! if a copy did land (a lost ack, or the run loop racing this
//! sweep), JetStream's `Nats-Msg-Id` dedup drops the republish only
//! inside the stream's 120 s `duplicate_window`. Past it the stream
//! holds a second message with the same `event_id`, which a
//! consumer idempotent on `event_id` (the projection's upsert)
//! still reads as one event.
//!
//! Some rows cannot go out, and the sweeper **holds** them: it publishes
//! nothing more for the row, leaves it in the store exactly as it was, and
//! carries on to the rows behind it. A hold is one `error!` per daemon run,
//! and [`ArchiveHolds`] lists it for `control.status` and `control.doctor`
//! with the reason and what lifts it. While a row is held, the control
//! plane sees that invocation as not archived. Three causes hold a row
//! (`docs/guide/operating-the-daemon.md`, "When the archive sweeper holds a
//! terminal row"):
//!
//! - Ids that cannot go on an event: an `agent_id` that is not a subject
//!   token, or an `invocation_id` that is not a UUID. The runtime never
//!   writes either, so the row was edited by hand or written by a build
//!   with other rules, and it waits for an operator to correct it.
//! - A stored event that must not be republished (unreadable by this
//!   build, or not this row's terminal event: see `HoldReason`). Archiving
//!   without it would discard the only copy of the outcome, and no later
//!   build reads an older event version, so the hold lasts until an
//!   operator releases the row.
//! - An event the broker refuses for its size: the stored terminal event,
//!   or `invocation.archived`, which carries the run's whole state (#981).
//!   The same event is refused on every attempt, and nothing that could
//!   let it fit changes while the daemon runs: a terminal row is final,
//!   and the bus reads the broker's `max_payload` once, when it connects.
//!   So a row held for size sits out every later pass of the run, and is
//!   offered again at the next start, when a new run's sweeper begins with
//!   no holds. Lifting such a hold without a restart needs the bus to read
//!   the limit again (#1015).
//!
//! A size refusal is the only publish error treated per row. Any other is
//! the bus's, and a broker that is down refuses every row the same way, so
//! it ends the pass, as it always has, and the next tick starts again.
//!
//! Two configurable timings:
//!
//! - `retry_interval_ms` — how often the sweeper wakes up.
//!   Default 10s, matching the heartbeat cadence. Lower
//!   intervals shorten the time-to-recovery after a CP
//!   restart at the cost of more NATS traffic during steady-
//!   state outages.
//!
//! - `warn_after_ms` — how long after `terminal_at` to log a
//!   warning once per pending row. Default 60s. The sweeper
//!   keeps republishing a row the control plane has not acked,
//!   indefinitely, after the warn point; a row it holds is the
//!   exception, and nothing more goes out for it. Either way the
//!   plan's "correctness over cleanup" rule says the row is kept,
//!   not deleted, even if the CP never acks.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use fq_ops::health::ArchiveHoldReason;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::agent::AgentId;
use crate::bus::{BusError, EventBus, EventSink};
use crate::events::{Event, EventPayload, InvocationArchivedPayload};

use super::WorkerId;
use super::store::{InvocationStateRow, WorkerStore};

mod holds;
pub use holds::ArchiveHolds;

/// Default cadence between sweeps (10 seconds). Pair this with
/// the heartbeat producer's cadence — the two share the same
/// "I'm alive, things are flowing" rhythm.
pub const DEFAULT_RETRY_INTERVAL_MS: u64 = 10_000;

/// Default age (since terminal) at which to log a warning once
/// per pending row. The sweeper keeps republishing a row the
/// control plane has not acknowledged past this point (a row it
/// holds excepted); the warn is the operator-visible signal that
/// the control-plane has not acknowledged for an unusually long
/// time.
pub const DEFAULT_WARN_AFTER_MS: i64 = 60_000;

/// Retry sweeper. Spawn its [`run`](Self::run) method as a
/// tokio task and feed it a shutdown signal.
pub struct ArchiveRetrySweeper {
    sink: Arc<dyn EventSink>,
    worker_id: WorkerId,
    store: Arc<WorkerStore>,
    retry_interval_ms: u64,
    warn_after_ms: i64,
    /// The rows this sweeper holds (see `report_held`), shared with the
    /// health reports.
    holds: Arc<ArchiveHolds>,
}

impl ArchiveRetrySweeper {
    pub fn new(bus: EventBus, worker_id: WorkerId, store: Arc<WorkerStore>) -> Self {
        Self::new_with_sink(Arc::new(bus), worker_id, store)
    }

    /// Construct over an arbitrary [`EventSink`] — the hermetic
    /// sim's entry point (reducer verification, slice 5).
    pub fn new_with_sink(
        sink: Arc<dyn EventSink>,
        worker_id: WorkerId,
        store: Arc<WorkerStore>,
    ) -> Self {
        Self {
            sink,
            worker_id,
            store,
            retry_interval_ms: DEFAULT_RETRY_INTERVAL_MS,
            warn_after_ms: DEFAULT_WARN_AFTER_MS,
            holds: Arc::new(ArchiveHolds::default()),
        }
    }

    /// Override the retry cadence. Production callers set this
    /// from the `[worker]` section of `fqd.toml`; tests call it
    /// to shorten the loop.
    pub fn with_retry_interval_ms(mut self, ms: u64) -> Self {
        self.retry_interval_ms = ms;
        self
    }

    /// Override the warn threshold.
    pub fn with_warn_after_ms(mut self, ms: i64) -> Self {
        self.warn_after_ms = ms;
        self
    }

    /// The rows this sweeper holds: what `control.status` and
    /// `control.doctor` list.
    pub fn holds(&self) -> Arc<ArchiveHolds> {
        Arc::clone(&self.holds)
    }

    /// Run the sweep loop until `shutdown` fires.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), ArchiveRetryError> {
        info!(
            worker_id = %self.worker_id,
            retry_interval_ms = self.retry_interval_ms,
            warn_after_ms = self.warn_after_ms,
            "archive retry sweeper starting"
        );
        let mut ticker = tokio::time::interval(Duration::from_millis(self.retry_interval_ms));
        // The first tick fires immediately; consume it so we
        // wait one full interval before the first sweep. This
        // gives the happy path (publish-then-ack inside
        // retry_interval_ms) a chance to finish without the
        // sweeper republishing.
        ticker.tick().await;

        // Rows we've already logged at the warn threshold —
        // prevents log spam on every sweep tick.
        let mut warned: HashSet<String> = HashSet::new();

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!(
                        worker_id = %self.worker_id,
                        "archive retry sweeper received shutdown signal"
                    );
                    break;
                }
                _ = ticker.tick() => {
                    if let Err(err) = self.sweep_once(&mut warned).await {
                        warn!(
                            worker_id = %self.worker_id,
                            error = %err,
                            "archive retry sweep failed; will retry next tick"
                        );
                    }
                }
            }
        }

        info!(worker_id = %self.worker_id, "archive retry sweeper stopped");
        Ok(())
    }

    /// One sweep pass. Public for tests.
    pub async fn sweep_once(&self, warned: &mut HashSet<String>) -> Result<(), ArchiveRetryError> {
        let rows = self
            .store
            .list_archive_pending()
            .await
            .map_err(|err| ArchiveRetryError::Store(err.to_string()))?;
        self.holds
            .retain_pending(&rows.iter().map(|r| r.invocation_id.as_str()).collect());
        let now_ms = Utc::now().timestamp_millis();
        for row in rows {
            self.maybe_warn_once(&row, now_ms, warned);
            if self.held_for_size(&row) {
                continue;
            }
            match self.republish(&row).await? {
                Some(reason) => self.report_held(&row, reason),
                None => self.holds.release(&row.invocation_id),
            }
        }
        // Only a pass that reached every row says what is held; one the
        // bus or the store ended early returned above.
        self.holds.pass_finished(Utc::now().timestamp_millis());
        Ok(())
    }

    /// Whether `row` sits this pass out: held this run for an event the
    /// broker refused for size. Nothing that could let it fit changes while
    /// the daemon runs (the row is final, and the bus keeps the limit it
    /// read at connect), so offering it again would serialise the same
    /// event to be refused the same way. The next start offers it again.
    fn held_for_size(&self, row: &InvocationStateRow) -> bool {
        let Some(held_under) = self.holds.too_large_under(&row.invocation_id) else {
            return false;
        };
        debug!(
            invocation_id = %row.invocation_id,
            limit_bytes = held_under,
            "terminal row still held for size until the next start"
        );
        true
    }

    fn maybe_warn_once(&self, row: &InvocationStateRow, now_ms: i64, warned: &mut HashSet<String>) {
        let Some(terminal_at_ms) = row.terminal_at else {
            return;
        };
        if now_ms - terminal_at_ms < self.warn_after_ms {
            return;
        }
        if warned.contains(&row.invocation_id) {
            return;
        }
        warn!(
            invocation_id = %row.invocation_id,
            agent_id = %row.agent_id,
            terminal_age_ms = now_ms - terminal_at_ms,
            "archive hand-off unacknowledged past the warn threshold; the row is kept \
             until the control plane acks it"
        );
        warned.insert(row.invocation_id.clone());
    }

    /// Hold a row and say so: `error!` when that is news (the row was not
    /// held, or was held for something else), with what lifts the hold;
    /// `debug!` on every later sweep, so a held row is one loud line per
    /// daemon run rather than one per tick.
    fn report_held(&self, row: &InvocationStateRow, reason: ArchiveHoldReason) {
        let invocation_id = row.invocation_id.as_str();
        if !self
            .holds
            .hold(invocation_id, &row.agent_id, reason.clone())
        {
            debug!(invocation_id, reason = %reason.summary(), "terminal row still held");
            return;
        }
        match &reason {
            ArchiveHoldReason::StoredEventRefused { detail } => error!(
                invocation_id,
                reason = %detail,
                "stored terminal event cannot be republished; holding the row unarchived \
                 until an operator releases it ({})",
                holds::GUIDE
            ),
            ArchiveHoldReason::PayloadTooLarge {
                event_type,
                size_bytes,
                limit_bytes,
            } => error!(
                invocation_id,
                event_type = event_type.as_str(),
                size_bytes = *size_bytes,
                limit_bytes = *limit_bytes,
                "an event this row must publish is larger than the broker's max_payload; \
                 holding the row unarchived until it fits: {}",
                holds::remedy(&reason)
            ),
            ArchiveHoldReason::MalformedRow { detail } => error!(
                invocation_id,
                reason = %detail,
                "this row's ids cannot go on an event; holding the row unarchived until an \
                 operator corrects it: {}",
                holds::remedy(&reason)
            ),
        }
    }

    /// Publish what `row` still owes the bus: its stored terminal event,
    /// while that is not stamped published, then `invocation.archived`.
    /// `Ok(Some(reason))` when the row is held instead, `Ok(None)` when
    /// nothing held it, and `Err` for a failure that ends the pass.
    async fn republish(
        &self,
        row: &InvocationStateRow,
    ) -> Result<Option<ArchiveHoldReason>, ArchiveRetryError> {
        let Some(terminal_at_ms) = row.terminal_at else {
            // list_archive_pending filters to terminal rows, so
            // this branch is unreachable in practice. Treat as
            // a corrupt row and skip — the sweeper must not
            // crash on a malformed input.
            error!(
                invocation_id = %row.invocation_id,
                "list_archive_pending returned a non-terminal row; skipping"
            );
            return Ok(None);
        };
        // A row whose ids cannot go on an event can never be published, so
        // it is held like any other row that cannot go out, rather than
        // ending the pass for every row behind it.
        let (agent_id, invocation_id) = match event_ids(row) {
            Ok(ids) => ids,
            Err(detail) => return Ok(Some(ArchiveHoldReason::MalformedRow { detail })),
        };

        // The terminal event first, if its publish was lost (#823, #946): the
        // stored event itself, envelope and `event_id` included, so a copy
        // that did land carries the same `event_id` as the original.
        let owed = match row.unpublished_terminal_event() {
            Ok(owed) => owed,
            // Held, not archived without it: archiving would discard the
            // only copy of the outcome. No later build reads an older event
            // version, so the hold lasts until an operator releases the row
            // (`docs/guide/operating-the-daemon.md`). The likely cause is an
            // event an older build stored, unpublished across an envelope
            // bump, not corruption. Like the non-terminal branch above, it
            // must not end the pass.
            Err(reason) => {
                return Ok(Some(ArchiveHoldReason::StoredEventRefused {
                    detail: reason.to_string(),
                }));
            }
        };
        if let Some(terminal) = owed {
            if let Some(held) = self.publish_for_row(&terminal).await? {
                return Ok(Some(held));
            }
            self.store
                .mark_terminal_published(&row.invocation_id, Utc::now().timestamp_millis())
                .await
                .map_err(|err| ArchiveRetryError::Store(err.to_string()))?;
            info!(
                invocation_id = %row.invocation_id,
                event_id = %terminal.envelope.event_id,
                "republished the stored terminal event"
            );
        }

        let event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: self.worker_id.clone(),
                final_phase: row.phase.clone(),
                final_state_blob: row.state_blob.clone(),
                started_at_ms: row.started_at,
                terminal_at_ms,
            }),
        );
        if let Some(held) = self.publish_for_row(&event).await? {
            return Ok(Some(held));
        }
        debug!(
            invocation_id = %row.invocation_id,
            "republished invocation.archived"
        );
        self.store
            .set_archive_pending(&row.invocation_id, Utc::now().timestamp_millis())
            .await
            .map_err(|err| ArchiveRetryError::Store(err.to_string()))?;
        Ok(None)
    }

    /// Publish one of a row's events. A refusal for size is the row's own:
    /// the event it names is refused on every attempt, so it holds the row
    /// (`Ok(Some)`) and the pass goes on. Any other failure is the bus's,
    /// the same for every row, and ends the pass.
    async fn publish_for_row(
        &self,
        event: &Event,
    ) -> Result<Option<ArchiveHoldReason>, ArchiveRetryError> {
        match self.sink.publish(event).await {
            Ok(_) => Ok(None),
            Err(BusError::PayloadTooLarge { size, limit, .. }) => {
                Ok(Some(ArchiveHoldReason::PayloadTooLarge {
                    event_type: event.payload.event_type().to_string(),
                    size_bytes: size as u64,
                    limit_bytes: limit as u64,
                }))
            }
            Err(err) => Err(ArchiveRetryError::Publish(err.to_string())),
        }
    }
}

/// The ids `row`'s events are published under, or why it has none: an
/// `agent_id` that is not a subject token, or an `invocation_id` that is
/// not a UUID. The runtime never writes either.
fn event_ids(row: &InvocationStateRow) -> Result<(AgentId, uuid::Uuid), String> {
    let agent_id = AgentId::new(&row.agent_id)
        .map_err(|err| format!("agent_id {:?} is not an agent id: {err}", row.agent_id))?;
    let invocation_id = uuid::Uuid::parse_str(&row.invocation_id)
        .map_err(|err| format!("invocation_id {:?} is not a UUID: {err}", row.invocation_id))?;
    Ok((agent_id, invocation_id))
}

/// A failure that ends a pass: the store's or the bus's, the same for
/// every row. A row that cannot go out for a reason of its own is held
/// instead (`ArchiveHoldReason`).
#[derive(Debug, thiserror::Error)]
pub enum ArchiveRetryError {
    #[error("worker store error: {0}")]
    Store(String),

    #[error("bus publish error: {0}")]
    Publish(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        CompletedPayload, EventKind, EventPayload, FailedPayload, FailureDetail, FailureKind,
        FailurePhase, InvocationTotals, TaskStatus,
    };
    use crate::test_support::events::event_kind_of;
    use crate::test_support::sim::RecordingSink;
    use crate::worker::store::{InvocationStateRow, terminal_outcome};
    use fq_ops::health::ArchiveHold;
    use futures::StreamExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn terminal_row(inv: &str, agent: &str, terminal_at_ms: i64) -> InvocationStateRow {
        InvocationStateRow {
            invocation_id: inv.to_string(),
            agent_id: agent.to_string(),
            schema_version: 1,
            phase: "completed".to_string(),
            state_blob: vec![1, 2, 3],
            step_index: 0,
            started_at: 0,
            updated_at: terminal_at_ms,
            terminal_at: Some(terminal_at_ms),
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
            terminal_outcome: None,
            terminal_published_at: None,
        }
    }

    #[tokio::test]
    async fn sweep_republishes_pending_terminal_rows() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .expect("worker store"),
        );

        let worker_id =
            WorkerId::new(format!("sweep-test-{}", Uuid::now_v7().simple())).expect("worker id");
        let invocation_id = Uuid::now_v7();
        let inv_str = invocation_id.to_string();
        let agent_id = format!("agent-{}", Uuid::now_v7().simple());

        store
            .upsert_invocation_state(&terminal_row(&inv_str, &agent_id, 1_000))
            .await
            .unwrap();
        store.set_archive_pending(&inv_str, 1_000).await.unwrap();

        let mut sub = bus
            .subscribe(format!("fq.agent.{agent_id}.invocation.archived"))
            .await
            .expect("subscribe");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Run one explicit sweep — avoids waiting for the ticker.
        let sweeper = ArchiveRetrySweeper::new(bus.clone(), worker_id.clone(), store.clone());
        let mut warned = HashSet::new();
        sweeper.sweep_once(&mut warned).await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .expect("republish timeout")
            .expect("stream closed")
            .expect("deserialise");
        assert_eq!(event.envelope.invocation_id, invocation_id);
        match &event.payload {
            EventPayload::InvocationArchived(p) => {
                assert_eq!(p.worker_id, worker_id);
                assert_eq!(p.final_phase, "completed");
                assert_eq!(p.final_state_blob, vec![1, 2, 3]);
                assert_eq!(p.terminal_at_ms, 1_000);
            }
            other => panic!("wrong payload variant: {other:?}"),
        }

        // Sweep bumped archive_published_at.
        let row = store.get_invocation_state(&inv_str).await.unwrap().unwrap();
        let published = row.archive_published_at.expect("publish time set");
        assert!(
            published > 1_000,
            "archive_published_at should advance past the seed value"
        );
    }

    /// A terminal row holding the terminal event a crash lost — the
    /// `completed` of #823, or a reducer-decided `failed` (#946) — written
    /// the way the run loop's terminal upsert writes it, and a hermetic
    /// sweeper over a recording sink.
    struct LostTerminal {
        _dir: tempfile::TempDir,
        store: Arc<WorkerStore>,
        sink: Arc<RecordingSink>,
        sweeper: ArchiveRetrySweeper,
        stored: Event,
    }

    impl LostTerminal {
        /// A lost `completed`.
        async fn new() -> Self {
            Self::storing("completed", completed_event).await
        }

        /// A row in `phase` whose stored terminal event is
        /// `event(invocation_id)`.
        async fn storing(phase: &str, event: impl FnOnce(Uuid) -> Event) -> Self {
            let dir = tempdir().unwrap();
            let store = Arc::new(
                WorkerStore::open(&dir.path().join("worker.db"))
                    .await
                    .expect("worker store"),
            );
            let invocation_id = Uuid::now_v7();
            let mut stored = event(invocation_id);
            stored.envelope.parent_event_id = Some(Uuid::now_v7());
            let mut row = terminal_row(&invocation_id.to_string(), "sweep-agent", 1_000);
            row.phase = phase.to_string();
            row.terminal_outcome = Some(terminal_outcome::encode(&stored).unwrap());
            store.upsert_invocation_state(&row).await.unwrap();
            let sink = Arc::new(RecordingSink::new());
            let sweeper = ArchiveRetrySweeper::new_with_sink(
                Arc::clone(&sink) as Arc<dyn EventSink>,
                WorkerId::new("sweep-worker").unwrap(),
                Arc::clone(&store),
            );
            Self {
                _dir: dir,
                store,
                sink,
                sweeper,
                stored,
            }
        }

        fn inv(&self) -> String {
            self.stored.envelope.invocation_id.to_string()
        }

        fn kinds(&self) -> Vec<&'static str> {
            self.sink
                .events()
                .iter()
                .map(|e| event_kind_of(&e.payload))
                .collect()
        }

        async fn published_at(&self) -> Option<i64> {
            let row = self.store.get_invocation_state(&self.inv()).await.unwrap();
            row.expect("row held until acked").terminal_published_at
        }
    }

    #[tokio::test]
    async fn sweep_republishes_a_lost_terminal_event_before_archiving() {
        let world = LostTerminal::new().await;
        let mut warned = HashSet::new();
        world.sweeper.sweep_once(&mut warned).await.unwrap();

        assert_eq!(world.kinds(), ["completed", "invocation_archived"]);
        assert_eq!(
            serde_json::to_value(&world.sink.events()[0]).unwrap(),
            serde_json::to_value(&world.stored).unwrap(),
            "the stored event itself, envelope and event_id included"
        );
        assert!(world.published_at().await.is_some(), "stamped once out");

        // The ack is still pending, so `archived` goes again; `completed`
        // does not.
        world.sweeper.sweep_once(&mut warned).await.unwrap();
        assert_eq!(
            world.kinds(),
            ["completed", "invocation_archived", "invocation_archived"]
        );
    }

    #[tokio::test]
    async fn sweep_leaves_a_published_terminal_event_alone() {
        let world = LostTerminal::new().await;
        let stamped = world
            .store
            .mark_terminal_published(&world.inv(), 1_001)
            .await
            .unwrap();
        assert_eq!(stamped, 1);
        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(world.kinds(), ["invocation_archived"]);
        assert_eq!(world.published_at().await, Some(1_001));
    }

    #[tokio::test]
    async fn a_failed_republish_leaves_the_terminal_event_owed() {
        let world = LostTerminal::new().await;
        world.sink.fail_publish_at(0);
        assert!(world.sweeper.sweep_once(&mut HashSet::new()).await.is_err());
        assert_eq!(world.published_at().await, None, "stamped only once out");
        // The fault refuses every publish, so `archived` would be refused
        // too had it been tried: what the sink was offered is the check.
        let refused: Vec<_> = world
            .sink
            .refused()
            .iter()
            .map(|e| event_kind_of(&e.payload))
            .collect();
        assert_eq!(refused, ["completed"], "nothing tried after the failure");

        world.sink.clear_fault();
        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(world.kinds(), ["completed", "invocation_archived"]);
    }

    /// `archived` after a failed `completed` is the bug #823 fixes, one
    /// step removed: the control plane acks, the row is deleted, and the
    /// outcome is gone. So a `completed` republish that fails once ends the
    /// row's turn in that pass, with nothing else published for it.
    #[tokio::test]
    async fn a_completed_republish_that_fails_once_is_not_followed_by_archived() {
        let world = LostTerminal::new().await;
        world.sink.fail_publish_at_invocation_count(0);
        assert!(world.sweeper.sweep_once(&mut HashSet::new()).await.is_err());
        let refused: Vec<_> = world
            .sink
            .refused()
            .iter()
            .map(|e| event_kind_of(&e.payload))
            .collect();
        assert_eq!(refused, ["completed"]);
        assert!(world.kinds().is_empty(), "no `invocation_archived` either");
        let row = world
            .store
            .get_invocation_state(&world.inv())
            .await
            .unwrap();
        let row = row.expect("row held until acked");
        assert_eq!(row.archive_status, None, "not archive-pending");
        assert_eq!(row.terminal_published_at, None);
        assert!(
            row.unpublished_terminal_event().unwrap().is_some(),
            "still owed"
        );

        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(world.kinds(), ["completed", "invocation_archived"]);
    }

    /// A `failed` the reducer decided (#946) is healed exactly as
    /// `completed` is: the stored event itself goes out first, its
    /// `detail` with it, then `invocation.archived` for the row's `failed`
    /// phase; once stamped it is not re-emitted.
    #[tokio::test]
    async fn sweep_republishes_a_lost_failed_event_before_archiving() {
        let world = LostTerminal::storing("failed", failed_event).await;
        let mut warned = HashSet::new();
        world.sweeper.sweep_once(&mut warned).await.unwrap();

        assert_eq!(world.kinds(), ["failed", "invocation_archived"]);
        let events = world.sink.events();
        let republished = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(
            republished,
            serde_json::to_value(&world.stored).unwrap(),
            "the stored event itself, envelope, event_id and detail included"
        );
        assert_eq!(
            republished["payload"]["payload"]["detail"]["kind"], "payload_too_large",
            "the comparison above covers a detail, not an absent one"
        );
        match &events[1].payload {
            EventPayload::InvocationArchived(p) => assert_eq!(p.final_phase, "failed"),
            other => panic!("wrong payload variant: {other:?}"),
        }
        assert!(world.published_at().await.is_some(), "stamped once out");

        world.sweeper.sweep_once(&mut warned).await.unwrap();
        assert_eq!(
            world.kinds(),
            ["failed", "invocation_archived", "invocation_archived"]
        );
    }

    /// A `failed` this invocation's reducer could have decided. Nothing
    /// emits a `detail` yet, but a stored one must survive the round trip,
    /// so this one carries the only kind of detail there is; the pairing
    /// with `MaxIterations` is arbitrary, since the two are orthogonal.
    fn failed_event(invocation_id: Uuid) -> Event {
        Event::new(
            AgentId::new("sweep-agent").unwrap(),
            invocation_id,
            EventPayload::Failed(FailedPayload {
                error_kind: FailureKind::MaxIterations,
                error_message: "exceeded max iterations (2)".to_string(),
                phase: FailurePhase::Reducer,
                partial_totals: InvocationTotals {
                    total_llm_calls: 2,
                    total_tool_calls: 1,
                    total_cost: 0.125,
                    total_duration_ms: 987,
                    ..InvocationTotals::default()
                },
                detail: Some(FailureDetail::PayloadTooLarge {
                    event_type: EventKind::LlmRequest,
                    size_bytes: 2_000_000,
                    limit_bytes: 1_048_576,
                }),
            }),
        )
    }

    /// A `completed` this invocation could have stored.
    fn completed_event(invocation_id: Uuid) -> Event {
        Event::new(
            AgentId::new("sweep-agent").unwrap(),
            invocation_id,
            EventPayload::Completed(CompletedPayload {
                task_status: TaskStatus::Partial,
                result_summary: Some("half done".to_string()),
                total_llm_calls: 3,
                total_tool_calls: 2,
                total_cost: 0.25,
                total_duration_ms: 1_234,
            }),
        )
    }

    /// One sweep over a row whose stored terminal event must not go out,
    /// sorted ahead of a healthy archive-pending row.
    struct Held {
        dir: tempfile::TempDir,
        store: Arc<WorkerStore>,
        sink: Arc<RecordingSink>,
        sweeper: ArchiveRetrySweeper,
        bad: String,
        healthy: String,
        stored: String,
    }

    impl Held {
        /// Store `stored(invocation_id)` as a terminal row's outcome, sweep
        /// once, and assert the hold (see [`Self::assert_held`]).
        async fn sweep(stored: impl FnOnce(Uuid) -> String) -> Self {
            let dir = tempdir().unwrap();
            let store = Arc::new(
                WorkerStore::open(&dir.path().join("worker.db"))
                    .await
                    .expect("worker store"),
            );
            let bad_id = Uuid::now_v7();
            let (bad, healthy) = (bad_id.to_string(), Uuid::now_v7().to_string());
            let stored = stored(bad_id);
            let mut row = terminal_row(&bad, "sweep-agent", 1_000);
            row.terminal_outcome = Some(stored.clone());
            store.upsert_invocation_state(&row).await.unwrap();
            store
                .upsert_invocation_state(&terminal_row(&healthy, "sweep-agent", 1_000))
                .await
                .unwrap();
            store.set_archive_pending(&healthy, 1_000).await.unwrap();
            let order = store.list_archive_pending().await.unwrap();
            assert_eq!(order[0].invocation_id, bad, "never archived, so first");

            let sink = Arc::new(RecordingSink::new());
            let sweeper = ArchiveRetrySweeper::new_with_sink(
                Arc::clone(&sink) as Arc<dyn EventSink>,
                WorkerId::new("sweep-worker").unwrap(),
                Arc::clone(&store),
            );
            sweeper
                .sweep_once(&mut HashSet::new())
                .await
                .expect("a held row does not end the pass");
            let world = Self {
                dir,
                store,
                sink,
                sweeper,
                bad,
                healthy,
                stored,
            };
            world.assert_held().await;
            world
        }

        /// The healthy row behind was archived; nothing went out for the
        /// held row, which is unstamped, not archive-pending, its copy kept.
        async fn assert_held(&self) {
            let events = self.sink.events();
            let kinds: Vec<_> = events.iter().map(|e| event_kind_of(&e.payload)).collect();
            assert_eq!(kinds, ["invocation_archived"], "no completed, one archived");
            assert_eq!(events[0].envelope.invocation_id.to_string(), self.healthy);
            let held = self.store.get_invocation_state(&self.bad).await.unwrap();
            let held = held.expect("held, not dropped");
            assert_eq!(held.terminal_published_at, None, "not stamped");
            assert_eq!(held.archive_status, None, "not archived without it");
            assert_eq!(held.archive_published_at, None);
            assert_eq!(held.terminal_outcome.as_deref(), Some(self.stored.as_str()));

            // Listed for the health reports, with its reason and the release
            // the guide describes.
            let holds = self.sweeper.holds().snapshot();
            assert_eq!(holds.len(), 1, "the held row alone: {holds:?}");
            assert_eq!(holds[0].invocation_id, self.bad);
            assert_eq!(holds[0].agent_id, "sweep-agent");
            assert!(
                matches!(
                    holds[0].reason,
                    ArchiveHoldReason::StoredEventRefused { .. }
                ),
                "got: {holds:?}"
            );
            assert!(
                holds[0].remedy.contains("terminal_outcome")
                    && holds[0].remedy.contains(holds::GUIDE),
                "the remedy names the release and where it is described: {holds:?}"
            );
        }
    }

    /// A stored outcome that is not an event at all holds its own row and
    /// does not end the pass: the rows behind it are still archived.
    #[tokio::test]
    async fn an_undecodable_terminal_event_holds_its_row_and_the_sweep_goes_on() {
        Held::sweep(|_| r#"{"not": "an event"}"#.to_string()).await;
    }

    /// The realistic hold: an event an older build stored, still unpublished
    /// across an envelope bump. It may well still parse, but republishing it
    /// would halt every durable consumer on the stream (`Event::from_wire`).
    #[tokio::test]
    async fn a_stored_event_of_an_older_schema_version_is_held() {
        Held::sweep(|inv| {
            let mut event = completed_event(inv);
            event.envelope.schema_version = crate::events::SCHEMA_VERSION - 1;
            terminal_outcome::encode(&event).unwrap()
        })
        .await;
    }

    #[tokio::test]
    async fn a_stored_event_for_another_invocation_is_held() {
        Held::sweep(|_| terminal_outcome::encode(&completed_event(Uuid::now_v7())).unwrap()).await;
    }

    #[tokio::test]
    async fn a_stored_event_that_is_not_terminal_is_held() {
        Held::sweep(|inv| {
            let notice = EventPayload::HostNotice(crate::events::HostNoticePayload {
                kind: "sim".to_string(),
                body: "not an outcome".to_string(),
            });
            terminal_outcome::encode(&Event::new(AgentId::new("a").unwrap(), inv, notice)).unwrap()
        })
        .await;
    }

    /// An `event_type` this build has never heard of reads as
    /// `EventPayload::Unknown` rather than failing: still not terminal.
    #[tokio::test]
    async fn a_stored_event_of_an_unknown_type_is_held() {
        Held::sweep(|inv| {
            let mut value = serde_json::to_value(completed_event(inv)).unwrap();
            value["payload"]["event_type"] = "from_the_future".into();
            value.to_string()
        })
        .await;
    }

    /// The documented release (`operating-the-daemon.md`): once an operator
    /// clears the stored outcome by hand, the next sweep archives the held
    /// row without it.
    #[tokio::test]
    async fn a_released_row_is_archived_without_its_outcome() {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

        let world = Held::sweep(|_| r#"{"not": "an event"}"#.to_string()).await;
        let operator = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(world.dir.path().join("worker.db")))
            .await
            .unwrap();
        sqlx::query("UPDATE invocation_state SET terminal_outcome = NULL WHERE invocation_id = ?")
            .bind(&world.bad)
            .execute(&operator)
            .await
            .unwrap();
        operator.close().await;

        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        let released: Vec<_> = world.sink.events()[1..]
            .iter()
            .filter(|e| e.envelope.invocation_id.to_string() == world.bad)
            .map(|e| event_kind_of(&e.payload))
            .collect();
        assert_eq!(released, ["invocation_archived"], "archived, no completed");
        let row = world.store.get_invocation_state(&world.bad).await.unwrap();
        assert_eq!(row.unwrap().archive_status.as_deref(), Some("pending"));
        let holds = world.sweeper.holds().snapshot();
        assert!(
            holds.is_empty(),
            "a released row is no longer listed: {holds:?}"
        );
    }

    /// A [`RecordingSink`] behind a broker's size guard: an event that
    /// serialises to more than the limit is refused with
    /// `BusError::PayloadTooLarge`, measured as the bus measures it (the
    /// serialised event, against a strict `>`), and kept in `too_large`.
    /// A test can raise the limit between sweepers, as an operator raises a
    /// broker's before restarting `fqd`.
    struct SizeLimitedSink {
        recorded: RecordingSink,
        limit: AtomicUsize,
        too_large: Mutex<Vec<Event>>,
    }

    impl SizeLimitedSink {
        fn new(limit: usize) -> Arc<Self> {
            Arc::new(Self {
                recorded: RecordingSink::new(),
                limit: AtomicUsize::new(limit),
                too_large: Mutex::new(Vec::new()),
            })
        }

        fn set_limit(&self, limit: usize) {
            self.limit.store(limit, Ordering::SeqCst);
        }

        /// What was published, in order.
        fn events(&self) -> Vec<Event> {
            self.recorded.events()
        }

        /// What was refused for its size, in order.
        fn too_large(&self) -> Vec<Event> {
            self.too_large.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl EventSink for SizeLimitedSink {
        async fn publish(&self, event: &Event) -> Result<u64, BusError> {
            let size = serde_json::to_vec(event)
                .expect("the event serialises")
                .len();
            let limit = self.limit.load(Ordering::SeqCst);
            if size > limit {
                self.too_large.lock().unwrap().push(event.clone());
                return Err(BusError::PayloadTooLarge { size, limit });
            }
            self.recorded.publish(event).await
        }
    }

    /// The size guard of the sinks below: a stock broker's 1 MiB, scaled
    /// down so the test rows stay small.
    const LIMIT: usize = 64 * 1024;

    /// A state blob whose `invocation.archived` is over twice [`LIMIT`]:
    /// the blob is a JSON array of integers on the wire, and `200,` is four
    /// bytes per byte of state.
    const OVERSIZED_STATE: usize = LIMIT / 2;

    /// `(invocation, event type)` for each event, in order.
    fn trail(events: &[Event]) -> Vec<(String, &'static str)> {
        events
            .iter()
            .map(|e| {
                let inv = e.envelope.invocation_id.to_string();
                (inv, event_kind_of(&e.payload))
            })
            .collect()
    }

    /// A pass over three rows, in the order the store lists them: one whose
    /// `invocation.archived` is larger than the sink accepts, one owed the
    /// `completed` a crash lost, and one awaiting its ack. The first is the
    /// row that used to end every pass.
    struct Oversized {
        _dir: tempfile::TempDir,
        store: Arc<WorkerStore>,
        sink: Arc<SizeLimitedSink>,
        sweeper: ArchiveRetrySweeper,
        big: String,
        owed: String,
        healthy: String,
    }

    impl Oversized {
        async fn new() -> Self {
            let dir = tempdir().unwrap();
            let store = Arc::new(
                WorkerStore::open(&dir.path().join("worker.db"))
                    .await
                    .expect("worker store"),
            );
            let ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
            let [big, owed, healthy] = [0, 1, 2].map(|i| ids[i].to_string());

            let mut row = terminal_row(&big, "sweep-agent", 1_000);
            row.state_blob = vec![200; OVERSIZED_STATE];
            store.upsert_invocation_state(&row).await.unwrap();

            let mut row = terminal_row(&owed, "sweep-agent", 1_000);
            row.terminal_outcome =
                Some(terminal_outcome::encode(&completed_event(ids[1])).unwrap());
            store.upsert_invocation_state(&row).await.unwrap();

            store
                .upsert_invocation_state(&terminal_row(&healthy, "sweep-agent", 1_000))
                .await
                .unwrap();
            store.set_archive_pending(&healthy, 1_000).await.unwrap();

            let order: Vec<String> = store
                .list_archive_pending()
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.invocation_id)
                .collect();
            assert_eq!(
                order,
                [big.clone(), owed.clone(), healthy.clone()],
                "the oversized row first"
            );

            let sink = SizeLimitedSink::new(LIMIT);
            let sweeper = ArchiveRetrySweeper::new_with_sink(
                Arc::clone(&sink) as Arc<dyn EventSink>,
                WorkerId::new("sweep-worker").unwrap(),
                Arc::clone(&store),
            );
            Self {
                _dir: dir,
                store,
                sink,
                sweeper,
                big,
                owed,
                healthy,
            }
        }

        async fn row(&self, inv: &str) -> InvocationStateRow {
            let row = self.store.get_invocation_state(inv).await.unwrap();
            row.expect("the row is kept")
        }

        /// The oversized row is in the store as it was written: its state
        /// whole, not stamped archive-pending.
        async fn assert_kept_whole(&self) {
            let row = self.row(&self.big).await;
            assert_eq!(row.state_blob, vec![200; OVERSIZED_STATE], "not truncated");
            assert_eq!(row.archive_status, None, "not marked archived");
            assert_eq!(row.archive_published_at, None);
        }
    }

    /// The defect this pins: the first row whose `invocation.archived` the
    /// broker refuses for its size used to end the pass, every pass, so no
    /// row behind it was archived and none was owed its terminal event.
    #[tokio::test]
    async fn an_archive_too_large_for_the_sink_does_not_end_the_pass() {
        let world = Oversized::new().await;
        world
            .sweeper
            .sweep_once(&mut HashSet::new())
            .await
            .expect("a refusal for size does not end the pass");

        assert_eq!(
            trail(&world.sink.too_large()),
            [(world.big.clone(), "invocation_archived")],
            "the oversized archive was offered and refused"
        );
        assert_eq!(
            trail(&world.sink.events()),
            [
                (world.owed.clone(), "completed"),
                (world.owed.clone(), "invocation_archived"),
                (world.healthy.clone(), "invocation_archived"),
            ],
            "every row behind it was swept"
        );
        world.assert_kept_whole().await;
        assert_eq!(
            world.row(&world.owed).await.archive_status.as_deref(),
            Some("pending")
        );
    }

    /// A stored terminal event too large for the broker is held the same
    /// way: not stamped published, `invocation.archived` not attempted
    /// without it, and the rows behind it swept.
    #[tokio::test]
    async fn a_stored_terminal_event_too_large_is_held_and_the_pass_goes_on() {
        let world = LostTerminal::storing("completed", |inv| {
            let mut event = completed_event(inv);
            if let EventPayload::Completed(p) = &mut event.payload {
                p.result_summary = Some("x".repeat(2 * LIMIT));
            }
            event
        })
        .await;
        let healthy = Uuid::now_v7().to_string();
        world
            .store
            .upsert_invocation_state(&terminal_row(&healthy, "sweep-agent", 1_000))
            .await
            .unwrap();
        world
            .store
            .set_archive_pending(&healthy, 1_000)
            .await
            .unwrap();
        let sink = SizeLimitedSink::new(LIMIT);
        let sweeper = ArchiveRetrySweeper::new_with_sink(
            Arc::clone(&sink) as Arc<dyn EventSink>,
            WorkerId::new("sweep-worker").unwrap(),
            Arc::clone(&world.store),
        );

        sweeper
            .sweep_once(&mut HashSet::new())
            .await
            .expect("a refusal for size does not end the pass");

        assert_eq!(trail(&sink.too_large()), [(world.inv(), "completed")]);
        assert_eq!(
            trail(&sink.events()),
            [(healthy, "invocation_archived")],
            "no `invocation_archived` without the outcome; the row behind is swept"
        );
        let row = world
            .store
            .get_invocation_state(&world.inv())
            .await
            .unwrap();
        let row = row.expect("held, not dropped");
        assert_eq!(row.terminal_published_at, None, "still owed");
        assert_eq!(row.archive_status, None);
        assert!(row.terminal_outcome.is_some(), "its only copy is kept");
    }

    /// A broker that is down refuses every row the same way, so any
    /// publish error but a size refusal still ends the pass: the next row
    /// is not even offered, rather than each row waiting out its own
    /// failure.
    #[tokio::test]
    async fn a_publish_error_that_is_not_a_size_refusal_still_ends_the_pass() {
        let world = Oversized::new().await;
        let sink = Arc::new(RecordingSink::new());
        sink.fail_publish_at(0);
        let sweeper = ArchiveRetrySweeper::new_with_sink(
            Arc::clone(&sink) as Arc<dyn EventSink>,
            WorkerId::new("sweep-worker").unwrap(),
            Arc::clone(&world.store),
        );

        let err = sweeper
            .sweep_once(&mut HashSet::new())
            .await
            .expect_err("the pass ends");
        assert!(
            matches!(err, ArchiveRetryError::Publish(_)),
            "a bus failure, not a hold: {err:?}"
        );
        assert_eq!(
            trail(&sink.refused()),
            [(world.big.clone(), "invocation_archived")],
            "nothing offered after the failure"
        );
        world.assert_kept_whole().await;
    }

    impl Oversized {
        /// A sweeper over the same store and sink with nothing carried over:
        /// what the next daemon start runs.
        fn fresh_sweeper(&self) -> ArchiveRetrySweeper {
            ArchiveRetrySweeper::new_with_sink(
                Arc::clone(&self.sink) as Arc<dyn EventSink>,
                WorkerId::new("sweep-worker").unwrap(),
                Arc::clone(&self.store),
            )
        }

        /// How many times the oversized row's `invocation.archived` was
        /// offered and refused.
        fn refusals(&self) -> usize {
            self.sink
                .too_large()
                .iter()
                .filter(|e| e.envelope.invocation_id.to_string() == self.big)
                .count()
        }
    }

    /// What `control.status` and `control.doctor` list for a row held for
    /// size: the invocation, the event refused with its size and the limit,
    /// and the remedy.
    #[tokio::test]
    async fn a_row_held_for_size_is_listed_with_its_reason_and_what_lifts_it() {
        let world = Oversized::new().await;
        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();

        let holds = world.sweeper.holds().snapshot();
        assert_eq!(holds.len(), 1, "the oversized row alone: {holds:?}");
        let hold = &holds[0];
        assert_eq!(hold.invocation_id, world.big);
        assert_eq!(hold.agent_id, "sweep-agent");
        let refused = &world.sink.too_large()[0];
        let size = serde_json::to_vec(refused).unwrap().len() as u64;
        assert_eq!(
            hold.reason,
            ArchiveHoldReason::PayloadTooLarge {
                event_type: "invocation_archived".to_string(),
                size_bytes: size,
                limit_bytes: LIMIT as u64,
            }
        );
        assert!(
            hold.remedy
                .contains(&format!("max_payload well above {size} bytes"))
                && hold.remedy.contains("header")
                && hold.remedy.contains("restart fqd")
                && hold.remedy.contains(holds::GUIDE),
            "the remedy says what to raise, past what with a margin for the header the size \
             leaves out, and where it is described: {hold:?}"
        );
    }

    /// The held row is not serialised again while nothing that could let
    /// it fit has changed (the daemon's bus reads its limit once, at
    /// connect), and it stays one hold, recorded once: holding it again for
    /// the same reason is not news, so it is not logged again. Whether the
    /// line is written is `ArchiveHolds::hold`'s answer (`holds/tests.rs`);
    /// the line itself is read off the real daemon's log in
    /// `fq-daemon/tests/archive_holds.rs`. A thread-local log capture here
    /// is unreliable: a callsite's interest is cached process-wide, so a
    /// non-capturing test that reaches the same `error!` first silences it.
    #[tokio::test]
    async fn a_row_held_for_size_is_not_offered_again_this_run() {
        let world = Oversized::new().await;
        let mut warned = HashSet::new();
        for _ in 0..3 {
            world.sweeper.sweep_once(&mut warned).await.unwrap();
        }

        assert_eq!(world.refusals(), 1, "offered once, then left alone");
        let holds = world.sweeper.holds().snapshot();
        assert_eq!(holds.len(), 1, "still held, once: {holds:?}");
        assert!(
            !world
                .sweeper
                .holds()
                .hold(&world.big, "sweep-agent", holds[0].reason.clone()),
            "the same hold again is not news"
        );
        world.assert_kept_whole().await;
    }

    /// The next daemon start offers a held row again: its sweeper begins
    /// with no holds. Still too large, it is held again; once the broker
    /// takes it, it is archived.
    #[tokio::test]
    async fn the_next_start_offers_a_held_row_again() {
        let world = Oversized::new().await;
        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();

        let restarted = world.fresh_sweeper();
        restarted.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(world.refusals(), 2, "offered again, refused again");
        assert_eq!(restarted.holds().snapshot().len(), 1, "and held again");

        world.sink.set_limit(4 * LIMIT);
        let restarted = world.fresh_sweeper();
        restarted.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(world.refusals(), 2);
        assert_eq!(
            world.row(&world.big).await.archive_status.as_deref(),
            Some("pending"),
            "archived once the broker takes it"
        );
        assert!(restarted.holds().snapshot().is_empty());
    }

    /// The stored terminal event refused for size is listed under its own
    /// type, which is what the operator raises the limit for.
    #[tokio::test]
    async fn a_terminal_event_held_for_size_is_listed_under_its_own_type() {
        let world = LostTerminal::storing("completed", |inv| {
            let mut event = completed_event(inv);
            if let EventPayload::Completed(p) = &mut event.payload {
                p.result_summary = Some("x".repeat(2 * LIMIT));
            }
            event
        })
        .await;
        let sink = SizeLimitedSink::new(LIMIT);
        let sweeper = ArchiveRetrySweeper::new_with_sink(
            Arc::clone(&sink) as Arc<dyn EventSink>,
            WorkerId::new("sweep-worker").unwrap(),
            Arc::clone(&world.store),
        );
        sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        sweeper.sweep_once(&mut HashSet::new()).await.unwrap();

        assert_eq!(sink.too_large().len(), 1, "not offered again");
        let holds = sweeper.holds().snapshot();
        assert!(
            matches!(
                &holds[..],
                [ArchiveHold {
                    reason: ArchiveHoldReason::PayloadTooLarge { event_type, .. },
                    ..
                }] if event_type == "completed"
            ),
            "got: {holds:?}"
        );
    }

    /// A stored event this build will not republish is judged afresh on
    /// every pass (it is small, and an operator's release must be seen on
    /// the next one), and stays one hold for one reason, so later passes
    /// hold it again without news: one error line per run.
    #[tokio::test]
    async fn a_stored_event_hold_stays_one_hold_across_passes() {
        let world = Held::sweep(|_| r#"{"not": "an event"}"#.to_string()).await;
        let first = world.sweeper.holds().snapshot();
        for _ in 0..2 {
            world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        }
        let holds = world.sweeper.holds().snapshot();
        assert_eq!(holds, first, "the same hold, for the same reason");
        assert!(
            !world
                .sweeper
                .holds()
                .hold(&world.bad, "sweep-agent", holds[0].reason.clone()),
            "the same hold again is not news"
        );
    }

    /// A row owed a lost `completed` whose archive is too large: the
    /// outcome goes out before the hold, and is stamped, so the hold is on
    /// `invocation.archived` alone. The next start offers only the archive,
    /// never the `completed` again.
    #[tokio::test]
    async fn the_outcome_goes_out_before_the_archive_is_held() {
        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .expect("worker store"),
        );
        let id = Uuid::now_v7();
        let inv = id.to_string();
        let stored = completed_event(id);
        let mut row = terminal_row(&inv, "sweep-agent", 1_000);
        row.state_blob = vec![200; OVERSIZED_STATE];
        row.terminal_outcome = Some(terminal_outcome::encode(&stored).unwrap());
        store.upsert_invocation_state(&row).await.unwrap();
        let sink = SizeLimitedSink::new(LIMIT);
        let sweeper = |store: &Arc<WorkerStore>| {
            ArchiveRetrySweeper::new_with_sink(
                Arc::clone(&sink) as Arc<dyn EventSink>,
                WorkerId::new("sweep-worker").unwrap(),
                Arc::clone(store),
            )
        };

        let first = sweeper(&store);
        first.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(
            trail(&sink.events()),
            [(inv.clone(), "completed")],
            "the outcome went out"
        );
        assert_eq!(
            serde_json::to_value(&sink.events()[0]).unwrap(),
            serde_json::to_value(&stored).unwrap(),
            "the stored event itself"
        );
        assert_eq!(
            trail(&sink.too_large()),
            [(inv.clone(), "invocation_archived")]
        );
        let kept = store.get_invocation_state(&inv).await.unwrap().unwrap();
        assert!(kept.terminal_published_at.is_some(), "stamped published");
        assert_eq!(kept.archive_status, None, "not archived");
        let holds = first.holds().snapshot();
        assert!(
            matches!(
                &holds[..],
                [ArchiveHold {
                    reason: ArchiveHoldReason::PayloadTooLarge { event_type, .. },
                    ..
                }] if event_type == "invocation_archived"
            ),
            "held on the archive: {holds:?}"
        );

        let restarted = sweeper(&store);
        restarted.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(
            trail(&sink.events()),
            [(inv.clone(), "completed")],
            "the completed is not republished"
        );
        assert_eq!(
            trail(&sink.too_large()),
            [
                (inv.clone(), "invocation_archived"),
                (inv.clone(), "invocation_archived")
            ],
            "only the archive is offered again"
        );
        assert_eq!(restarted.holds().snapshot().len(), 1, "and held again");
    }

    /// A row whose ids cannot go on an event can never be published. It
    /// used to end the pass for every row behind it; it is held with its
    /// own reason, kept as it is, and the rows behind it are swept. Once an
    /// operator corrects the field, the next pass archives it.
    #[tokio::test]
    async fn a_row_whose_ids_cannot_go_on_an_event_is_held_and_the_pass_goes_on() {
        let ok_id = Uuid::now_v7().to_string();
        for (agent, inv, field) in [
            ("not a token", ok_id.as_str(), "agent_id"),
            ("sweep-agent", "not-a-uuid", "invocation_id"),
        ] {
            let dir = tempdir().unwrap();
            let store = Arc::new(
                WorkerStore::open(&dir.path().join("worker.db"))
                    .await
                    .expect("worker store"),
            );
            store
                .upsert_invocation_state(&terminal_row(inv, agent, 1_000))
                .await
                .unwrap();
            let healthy = Uuid::now_v7().to_string();
            store
                .upsert_invocation_state(&terminal_row(&healthy, "sweep-agent", 1_000))
                .await
                .unwrap();
            store.set_archive_pending(&healthy, 1_000).await.unwrap();
            let sink = Arc::new(RecordingSink::new());
            let sweeper = ArchiveRetrySweeper::new_with_sink(
                Arc::clone(&sink) as Arc<dyn EventSink>,
                WorkerId::new("sweep-worker").unwrap(),
                Arc::clone(&store),
            );

            sweeper
                .sweep_once(&mut HashSet::new())
                .await
                .expect("a malformed row does not end the pass");
            assert_eq!(
                trail(&sink.events()),
                [(healthy.clone(), "invocation_archived")],
                "{field}: the row behind it is swept"
            );
            let holds = sweeper.holds().snapshot();
            assert!(
                matches!(
                    &holds[..],
                    [ArchiveHold {
                        reason: ArchiveHoldReason::MalformedRow { detail },
                        ..
                    }] if detail.starts_with(field)
                ),
                "{field}: held with its own reason: {holds:?}"
            );
            assert_eq!(holds[0].invocation_id, inv);
            assert!(holds[0].remedy.contains("correct the field"), "{holds:?}");
            let kept = store.get_invocation_state(inv).await.unwrap();
            assert_eq!(kept.expect("kept").archive_status, None, "{field}");
        }

        // The documented remedy: correct the field, and the next pass
        // archives the row.
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .expect("worker store"),
        );
        store
            .upsert_invocation_state(&terminal_row(&ok_id, "not a token", 1_000))
            .await
            .unwrap();
        let sink = Arc::new(RecordingSink::new());
        let sweeper = ArchiveRetrySweeper::new_with_sink(
            Arc::clone(&sink) as Arc<dyn EventSink>,
            WorkerId::new("sweep-worker").unwrap(),
            Arc::clone(&store),
        );
        sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(sweeper.holds().snapshot().len(), 1);
        let operator = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(dir.path().join("worker.db")))
            .await
            .unwrap();
        sqlx::query("UPDATE invocation_state SET agent_id = 'sweep-agent' WHERE invocation_id = ?")
            .bind(&ok_id)
            .execute(&operator)
            .await
            .unwrap();
        operator.close().await;
        sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(
            trail(&sink.events()),
            [(ok_id.clone(), "invocation_archived")]
        );
        assert!(sweeper.holds().snapshot().is_empty(), "no longer held");
    }

    /// The holds are known only once a pass has reached every row: not
    /// before the first pass, and not after a pass the bus ended early,
    /// which may have stopped short of a row it would have held.
    #[tokio::test]
    async fn the_holds_are_known_only_after_a_pass_reaches_every_row() {
        let world = Oversized::new().await;
        let before = world.sweeper.holds().report();
        assert_eq!(before.last_pass_at_ms, None, "not looked yet");
        assert!(before.holds.is_empty());

        let sink = Arc::new(RecordingSink::new());
        sink.fail_publish_at(0);
        let ended_early = ArchiveRetrySweeper::new_with_sink(
            Arc::clone(&sink) as Arc<dyn EventSink>,
            WorkerId::new("sweep-worker").unwrap(),
            Arc::clone(&world.store),
        );
        assert!(ended_early.sweep_once(&mut HashSet::new()).await.is_err());
        assert_eq!(
            ended_early.holds().report().last_pass_at_ms,
            None,
            "a pass the bus ended does not count"
        );

        let at = Utc::now().timestamp_millis();
        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        let after = world.sweeper.holds().report();
        assert!(
            after.last_pass_at_ms.is_some_and(|t| t >= at),
            "got: {after:?}"
        );
        assert_eq!(after.holds, world.sweeper.holds().snapshot());
        assert_eq!(after.holds.len(), 1, "the oversized row");
    }

    /// A held row that leaves the store (an operator deleted it, or an ack
    /// for an earlier archive arrived) is no longer listed.
    #[tokio::test]
    async fn a_held_row_gone_from_the_store_is_no_longer_listed() {
        let world = Oversized::new().await;
        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        assert_eq!(world.sweeper.holds().snapshot().len(), 1);

        world
            .store
            .delete_invocation_state(&world.big)
            .await
            .unwrap();
        world.sweeper.sweep_once(&mut HashSet::new()).await.unwrap();
        let holds = world.sweeper.holds().snapshot();
        assert!(holds.is_empty(), "got: {holds:?}");
    }

    #[tokio::test]
    async fn sweep_warns_once_after_threshold() {
        // Pure unit test against the warn-once gate; no bus,
        // no real time. The sweep_once method needs the bus to
        // republish, so we exercise the warn logic directly via
        // maybe_warn_once.
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let dir = tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .expect("worker store"),
        );
        let worker_id = WorkerId::new("warn-test").expect("worker id");
        let sweeper = ArchiveRetrySweeper::new(bus, worker_id, store).with_warn_after_ms(1_000);

        let row = terminal_row("inv-1", "agent-1", 100);
        let mut warned = HashSet::new();

        // now_ms - terminal_at_ms < warn_after_ms → no warn.
        sweeper.maybe_warn_once(&row, 500, &mut warned);
        assert!(warned.is_empty());

        // now_ms - terminal_at_ms >= warn_after_ms → warn once.
        sweeper.maybe_warn_once(&row, 2_000, &mut warned);
        assert_eq!(warned.len(), 1);
        assert!(warned.contains("inv-1"));

        // Calling again at a later timestamp does NOT add a
        // duplicate entry — warn-once is idempotent per row.
        sweeper.maybe_warn_once(&row, 5_000, &mut warned);
        assert_eq!(warned.len(), 1);
    }
}
