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
//! A stored event that must not be republished (unreadable by this
//! build, or not this row's terminal event: see `HoldReason`) holds
//! its row. Neither it nor `archived` goes out; the hold is one `error!`
//! per daemon run, and the sweep carries on to the rows behind it. Archiving
//! without it would discard the only copy of the outcome. No later
//! build reads an older event version, so the hold lasts until an
//! operator releases the row (`docs/guide/operating-the-daemon.md`,
//! "When the archive sweeper holds a terminal row"); until then the
//! control plane sees that invocation as not archived.
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
//!   keeps republishing indefinitely after the warn point —
//!   the plan's "correctness over cleanup" rule says the row
//!   must be held, not deleted, even if the CP never acks.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::agent::AgentId;
use crate::bus::{EventBus, EventSink};
use crate::events::{Event, EventPayload, InvocationArchivedPayload};

use super::WorkerId;
use super::store::terminal_outcome::HoldReason;
use super::store::{InvocationStateRow, WorkerStore};

/// Default cadence between sweeps (10 seconds). Pair this with
/// the heartbeat producer's cadence — the two share the same
/// "I'm alive, things are flowing" rhythm.
pub const DEFAULT_RETRY_INTERVAL_MS: u64 = 10_000;

/// Default age (since terminal) at which to log a warning once
/// per pending row. The sweeper keeps republishing past this
/// point; the warn is the operator-visible signal that the
/// control-plane has not acknowledged for an unusually long
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
    /// Rows this process has already reported held (see `report_held`).
    held: Mutex<HashSet<String>>,
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
            held: Mutex::new(HashSet::new()),
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
        let now_ms = Utc::now().timestamp_millis();
        for row in rows {
            self.maybe_warn_once(&row, now_ms, warned);
            self.republish(&row).await?;
        }
        Ok(())
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
            "archive hand-off pending past warn threshold; row will be held"
        );
        warned.insert(row.invocation_id.clone());
    }

    /// Say a row is held: `error!` the first time this process holds it,
    /// with the release it needs; `debug!` on every later sweep, so a held
    /// row is one loud line per daemon run rather than one per tick.
    fn report_held(&self, invocation_id: &str, reason: &HoldReason) {
        let first = self
            .held
            .lock()
            .expect("held set lock poisoned")
            .insert(invocation_id.to_string());
        if first {
            error!(
                invocation_id,
                %reason,
                "stored terminal event cannot be republished; holding the row unarchived \
                 until an operator releases it (docs/guide/operating-the-daemon.md, \
                 \"When the archive sweeper holds a terminal row\")"
            );
        } else {
            debug!(invocation_id, %reason, "terminal row still held");
        }
    }

    async fn republish(&self, row: &InvocationStateRow) -> Result<(), ArchiveRetryError> {
        let Some(terminal_at_ms) = row.terminal_at else {
            // list_archive_pending filters to terminal rows, so
            // this branch is unreachable in practice. Treat as
            // a corrupt row and skip — the sweeper must not
            // crash on a malformed input.
            error!(
                invocation_id = %row.invocation_id,
                "list_archive_pending returned a non-terminal row; skipping"
            );
            return Ok(());
        };
        let agent_id = AgentId::new(&row.agent_id).map_err(|err| {
            ArchiveRetryError::InvalidAgentId(format!(
                "agent_id {:?} from invocation_state: {err}",
                row.agent_id
            ))
        })?;
        let invocation_id = uuid::Uuid::parse_str(&row.invocation_id).map_err(|err| {
            ArchiveRetryError::InvalidInvocationId(format!(
                "invocation_id {:?} from invocation_state: {err}",
                row.invocation_id
            ))
        })?;

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
                self.report_held(&row.invocation_id, &reason);
                return Ok(());
            }
        };
        if let Some(terminal) = owed {
            self.sink
                .publish(&terminal)
                .await
                .map_err(|err| ArchiveRetryError::Publish(err.to_string()))?;
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
        self.sink
            .publish(&event)
            .await
            .map_err(|err| ArchiveRetryError::Publish(err.to_string()))?;
        debug!(
            invocation_id = %row.invocation_id,
            "republished invocation.archived"
        );
        self.store
            .set_archive_pending(&row.invocation_id, Utc::now().timestamp_millis())
            .await
            .map_err(|err| ArchiveRetryError::Store(err.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveRetryError {
    #[error("worker store error: {0}")]
    Store(String),

    #[error("bus publish error: {0}")]
    Publish(String),

    #[error("invalid agent_id on persisted row: {0}")]
    InvalidAgentId(String),

    #[error("invalid invocation_id on persisted row: {0}")]
    InvalidInvocationId(String),
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
    use futures::StreamExt;
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
