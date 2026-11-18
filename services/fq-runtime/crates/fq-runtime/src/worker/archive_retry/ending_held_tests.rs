//! A finished run whose stored ending the sweeper holds, and what the
//! control plane's ownership row learns of it (issue 1071).
//!
//! The ownership row learns that a run ended from the run's `completed` or
//! `failed` on the event stream. A run whose finishing write landed and
//! whose publish of that event was lost has it republished by the archive
//! sweeper, from the event stored in its WAL row. When the sweeper holds
//! the row because that stored ending cannot go out, nothing about the
//! ending reaches the stream. These tests run the sweeper over a real
//! broker, with the coordination consumer folding the stream into a real
//! control-plane store, and read the ownership row after the sweep.

use super::*;
use crate::control_plane::coordination_consumer::CoordinationConsumer;
use crate::control_plane::ownership::{self, OwnerFacts, OwnershipEvent};
use crate::control_plane::{ControlPlaneStore, OwnerStatus};
use crate::events::{
    CompletedPayload, EventKind, FailedPayload, FailureKind, FailurePhase, HostNoticePayload,
    InvocationTotals, TaskStatus,
};
use crate::test_support::nats::{NatsServer, test_nats};
use crate::worker::store::terminal_outcome;
use fq_ops::health::ArchiveHold;
use tempfile::TempDir;
use uuid::Uuid;

const AGENT: &str = "held-ending-agent";

/// The worker's and the control plane's stores, a broker, and the
/// coordination consumer folding it.
struct World {
    _server: NatsServer,
    _dir: TempDir,
    bus: EventBus,
    worker_store: Arc<WorkerStore>,
    cp_store: Arc<ControlPlaneStore>,
    folded: crate::watermark::Watermark,
    stop: Option<oneshot::Sender<()>>,
}

impl World {
    async fn new() -> Self {
        let server = test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let dir = tempfile::tempdir().unwrap();
        let worker_store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .expect("worker store"),
        );
        let cp_store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .expect("control-plane store"),
        );
        let (mark, folded) = crate::watermark::channel();
        let consumer = CoordinationConsumer::new(bus.clone(), cp_store.clone())
            .with_worker_store(worker_store.clone())
            .with_watermark(mark);
        let (stop, stopped) = oneshot::channel();
        tokio::spawn(async move { consumer.run(stopped).await });
        Self {
            _server: server,
            _dir: dir,
            bus,
            worker_store,
            cp_store,
            folded,
            stop: Some(stop),
        }
    }

    /// A finished WAL row in `phase` that stored `stored(its id)` as its
    /// ending and never stamped it published.
    async fn finished_row(&self, phase: &str, stored: impl FnOnce(Uuid) -> String) -> Uuid {
        let invocation_id = Uuid::now_v7();
        let row = InvocationStateRow {
            invocation_id: invocation_id.to_string(),
            agent_id: AGENT.to_string(),
            schema_version: 1,
            phase: phase.to_string(),
            state_blob: vec![1, 2, 3],
            step_index: 2,
            started_at: 1_000,
            updated_at: 5_000,
            terminal_at: Some(5_000),
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
            terminal_outcome: Some(stored(invocation_id)),
            terminal_published_at: None,
        };
        self.worker_store
            .upsert_invocation_state(&row)
            .await
            .expect("WAL write");
        invocation_id
    }

    /// Put the ownership row back to `in_flight`, as startup recovery's
    /// restore does for a run it found in flight before this one ended.
    async fn restored(&self, invocation_id: Uuid) {
        ownership::apply(
            self.cp_store.as_ref(),
            &invocation_id.to_string(),
            OwnershipEvent::Restored,
            OwnerFacts {
                worker_id: "the-last-daemon",
                at_ms: 1_000,
            },
        )
        .await
        .expect("restore");
    }

    /// One sweep publishing through `sink`, then wait until the consumer
    /// has folded everything on the stream.
    async fn sweep_through(&self, sink: Arc<dyn EventSink>) -> Arc<ArchiveHolds> {
        let sweeper = ArchiveRetrySweeper::new_with_sink(
            sink,
            WorkerId::new("held-ending-worker").unwrap(),
            self.worker_store.clone(),
        );
        sweeper
            .sweep_once(&mut HashSet::new())
            .await
            .expect("the sweep");
        let barrier = self
            .bus
            .publish(&Event::new(
                AgentId::new(AGENT).unwrap(),
                Uuid::now_v7(),
                EventPayload::HostNotice(HostNoticePayload {
                    kind: "barrier".to_string(),
                    body: String::new(),
                }),
            ))
            .await
            .expect("publish the barrier");
        self.folded
            .wait_for(barrier, Duration::from_secs(10))
            .await
            .expect("the consumer folds to the barrier");
        sweeper.holds()
    }

    async fn sweep(&self) -> Arc<ArchiveHolds> {
        self.sweep_through(Arc::new(self.bus.clone())).await
    }

    async fn owner(&self, invocation_id: Uuid) -> Option<OwnerStatus> {
        self.cp_store
            .get_invocation_owner(&invocation_id.to_string())
            .await
            .expect("read the ownership row")
            .map(|owner| owner.status)
    }

    async fn wal_finished(&self, invocation_id: Uuid) -> bool {
        self.worker_store
            .get_invocation_state(&invocation_id.to_string())
            .await
            .expect("read the WAL row")
            .is_some_and(|row| row.terminal_at.is_some())
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn its_own_failed(invocation_id: Uuid) -> Event {
    Event::new(
        AgentId::new(AGENT).unwrap(),
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
            detail: None,
        }),
    )
}

fn its_own_completed(invocation_id: Uuid) -> Event {
    Event::new(
        AgentId::new(AGENT).unwrap(),
        invocation_id,
        EventPayload::Completed(CompletedPayload {
            task_status: TaskStatus::Success,
            result_summary: Some("done".to_string()),
            total_llm_calls: 3,
            total_tool_calls: 2,
            total_cost: 0.25,
            total_duration_ms: 1_234,
        }),
    )
}

/// A sink that refuses the run's stored ending for its size, as a broker
/// whose `max_payload` was lowered after the run stored it would, and
/// publishes everything else to the broker.
struct RefusesTheEnding {
    bus: EventBus,
}

#[async_trait::async_trait]
impl EventSink for RefusesTheEnding {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        let kind = EventKind::from(&event.payload);
        if matches!(kind, EventKind::Completed | EventKind::Failed) {
            return Err(BusError::PayloadTooLarge {
                event_type: kind,
                size: 2_000_000,
                limit: 1_048_576,
            });
        }
        self.bus.publish(event).await
    }
}

/// **Red before the fix.** A run that failed by itself, whose stored
/// `failed` this build refuses to republish (an envelope version it does
/// not read), and whose ownership row recovery had put back to
/// `in_flight`. The sweeper holds the row as `stored_event_refused`, the
/// worker's row says the run ended, and the ownership row must not still
/// read `in_flight`.
#[tokio::test]
async fn a_run_whose_stored_ending_is_refused_does_not_read_in_flight() {
    let world = World::new().await;
    let invocation_id = world
        .finished_row("failed", |id| {
            let mut stored = its_own_failed(id);
            stored.envelope.schema_version = crate::events::SCHEMA_VERSION - 1;
            terminal_outcome::encode(&stored).unwrap()
        })
        .await;
    world.restored(invocation_id).await;

    let holds = world.sweep().await;
    assert!(
        matches!(
            holds.snapshot()[..],
            [ArchiveHold {
                reason: ArchiveHoldReason::StoredEventRefused { .. },
                ..
            }]
        ),
        "the sweeper holds the row: {:?}",
        holds.snapshot()
    );
    assert!(world.wal_finished(invocation_id).await, "the run ended");
    assert_eq!(
        world.owner(invocation_id).await,
        Some(OwnerStatus::Failed),
        "the ownership row follows the ending the worker's row records"
    );
}

/// **Red before the fix.** A run that completed, triggered (so it has no
/// ownership row until its ending is folded), whose stored `completed` the
/// broker refuses for its size. The sweeper holds the row as
/// `payload_too_large` naming `completed`; the ownership row must say the
/// run completed.
#[tokio::test]
async fn a_run_whose_stored_ending_is_too_large_does_not_read_in_flight() {
    let world = World::new().await;
    let invocation_id = world
        .finished_row("completed", |id| {
            terminal_outcome::encode(&its_own_completed(id)).unwrap()
        })
        .await;

    let holds = world
        .sweep_through(Arc::new(RefusesTheEnding {
            bus: world.bus.clone(),
        }))
        .await;
    assert!(
        matches!(
            holds.snapshot()[..],
            [ArchiveHold {
                reason: ArchiveHoldReason::PayloadTooLarge { .. },
                ..
            }]
        ),
        "the sweeper holds the row for size: {:?}",
        holds.snapshot()
    );
    assert!(world.wal_finished(invocation_id).await, "the run ended");
    assert_eq!(
        world.owner(invocation_id).await,
        Some(OwnerStatus::Completed),
        "the ownership row follows the ending the worker's row records"
    );
}

/// **A guard: what this cannot reach.** A finished row whose agent id
/// cannot go on a subject publishes nothing at all: the sweeper holds it
/// as malformed and reports it, and the ownership row is left as it was.
#[tokio::test]
async fn a_malformed_row_publishes_nothing_and_stays_reported() {
    let world = World::new().await;
    let invocation_id = world
        .finished_row("failed", |id| {
            terminal_outcome::encode(&its_own_failed(id)).unwrap()
        })
        .await;
    world.restored(invocation_id).await;
    // The runtime never writes such an id: a hand edit, or another build.
    let operator = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(world._dir.path().join("worker.db")),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE invocation_state SET agent_id = 'not a token' WHERE invocation_id = ?")
        .bind(invocation_id.to_string())
        .execute(&operator)
        .await
        .unwrap();
    operator.close().await;

    let recording = Arc::new(crate::test_support::sim::RecordingSink::new());
    let holds = world
        .sweep_through(Arc::clone(&recording) as Arc<dyn EventSink>)
        .await;
    assert!(recording.events().is_empty(), "nothing is published");
    assert!(
        matches!(
            holds.snapshot()[..],
            [ArchiveHold {
                reason: ArchiveHoldReason::MalformedRow { .. },
                ..
            }]
        ),
        "the sweeper reports it: {:?}",
        holds.snapshot()
    );
    assert_eq!(
        world.owner(invocation_id).await,
        Some(OwnerStatus::InFlight),
        "the ownership row is left as it was"
    );
}
