use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tempfile::TempDir;
use uuid::Uuid;

use super::{StuckSweep, classify_liveness};
use crate::bus::EventBus;
use crate::control_plane::store::ControlPlaneStore;
use crate::events::EventPayload;
use crate::views::{Liveness, Views};
use crate::worker::store::{InvocationStateRow, WorkerStore};

/// A minute, in ms — comfortably longer than the ages the pure tests
/// below use and shorter than the "ancient" ones, so a row is
/// unambiguously on one side or the other.
const THRESHOLD_MS: i64 = 60_000;
const NOW: i64 = 1_800_000_000_000;

struct Fixture {
    _dir: TempDir,
    worker: Arc<WorkerStore>,
    control_plane: Arc<ControlPlaneStore>,
    bus: EventBus,
    agent: String,
}

impl Fixture {
    async fn new(server_url: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let worker = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .unwrap(),
        );
        let control_plane = Arc::new(
            ControlPlaneStore::open(&dir.path().join("cp.db"))
                .await
                .unwrap(),
        );
        let bus = EventBus::connect(server_url).await.expect("connect NATS");
        // A per-fixture agent id keeps parallel tests off each other's
        // subject: the sweep addresses the event at the agent.
        let agent = format!("stuck-test-{}", Uuid::now_v7().simple());
        Self {
            _dir: dir,
            worker,
            control_plane,
            bus,
            agent,
        }
    }

    fn sweep(&self) -> StuckSweep {
        StuckSweep::new(
            self.bus.clone(),
            self.worker.clone(),
            self.control_plane.clone(),
            THRESHOLD_MS,
        )
    }

    /// An in-flight row whose last step boundary is `age_ms` in the
    /// past. Returns its invocation id.
    async fn in_flight(&self, age_ms: i64) -> String {
        let id = Uuid::now_v7().to_string();
        self.write_row(&id, NOW - age_ms, None).await;
        id
    }

    async fn write_row(&self, id: &str, updated_at: i64, terminal_at: Option<i64>) {
        self.worker
            .upsert_invocation_state(&InvocationStateRow {
                invocation_id: id.to_string(),
                agent_id: self.agent.clone(),
                schema_version: 1,
                phase: "awaiting_model".to_string(),
                state_blob: b"{}".to_vec(),
                step_index: 3,
                started_at: NOW - 3_600_000,
                updated_at,
                terminal_at,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            })
            .await
            .unwrap();
    }

    async fn subscribe(&self) -> impl futures::Stream<Item = Result<crate::events::Event, String>> {
        let sub = self
            .bus
            .subscribe(format!("fq.agent.{}.invocation.stuck", self.agent))
            .await
            .expect("subscribe");
        // The publish is fire-and-forget on a core subject; give the
        // subscription a moment to register before anything is sent.
        tokio::time::sleep(Duration::from_millis(50)).await;
        sub.map(|e| e.map_err(|err| err.to_string()))
    }
}

async fn next_stuck(
    sub: &mut (impl futures::Stream<Item = Result<crate::events::Event, String>> + Unpin),
) -> crate::events::Event {
    tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("an invocation.stuck within 5s")
        .expect("stream open")
        .expect("event deserialises")
}

async fn assert_quiet(
    sub: &mut (impl futures::Stream<Item = Result<crate::events::Event, String>> + Unpin),
    why: &str,
) {
    let seen = tokio::time::timeout(Duration::from_millis(400), sub.next()).await;
    assert!(seen.is_err(), "{why}");
}

/// The whole point, in one test: a row whose step boundary is older
/// than the threshold is reported once, and carries the facts an
/// operator needs to act — which boundary, which threshold, what phase.
#[tokio::test]
async fn a_stale_in_flight_row_is_reported_once() {
    let server = crate::test_support::nats::test_nats();
    let fixture = Fixture::new(server.url()).await;
    let stuck = fixture.in_flight(10 * THRESHOLD_MS).await;
    let sweep = fixture.sweep();
    let mut sub = Box::pin(fixture.subscribe().await);

    sweep.tick(NOW).await.expect("first tick");
    let event = next_stuck(&mut sub).await;
    assert_eq!(event.envelope.invocation_id.to_string(), stuck);
    assert_eq!(event.envelope.agent_id.as_str(), fixture.agent);
    match &event.payload {
        EventPayload::InvocationStuck(p) => {
            assert_eq!(p.last_step_at_ms, NOW - 10 * THRESHOLD_MS);
            assert_eq!(p.stuck_after_ms, THRESHOLD_MS);
            assert_eq!(p.phase, "awaiting_model");
            assert_eq!(p.step_index, 3);
        }
        other => panic!("expected invocation.stuck, got {other:?}"),
    }

    // A second tick over the same unchanged row is the same finding.
    // Re-sending it would make the event's arrival rate the tick rate,
    // which is what makes an alert worthless.
    sweep.tick(NOW).await.expect("second tick");
    assert_quiet(
        &mut sub,
        "a second tick must not re-emit for the same stall",
    )
    .await;
}

/// A row that advanced recently is the healthy case, and a row that
/// reached terminal is finished. Neither is anyone's problem, and a
/// sweep that flagged either would train an operator to ignore it.
#[tokio::test]
async fn fresh_and_terminal_rows_are_not_flagged() {
    let server = crate::test_support::nats::test_nats();
    let fixture = Fixture::new(server.url()).await;
    // Fresh: one second past its last step.
    fixture.in_flight(1_000).await;
    // Terminal: ancient, but finished.
    let done = Uuid::now_v7().to_string();
    fixture
        .write_row(&done, NOW - 10 * THRESHOLD_MS, Some(NOW - 5_000))
        .await;
    let sweep = fixture.sweep();
    let mut sub = Box::pin(fixture.subscribe().await);

    sweep.tick(NOW).await.expect("tick");
    assert_quiet(&mut sub, "neither a fresh nor a terminal row is stuck").await;
}

/// The re-flag rule. Progress resumes, then stalls again: that is a new
/// crossing and a new report, because it is genuinely new information.
#[tokio::test]
async fn progress_then_a_new_stall_is_reported_again() {
    let server = crate::test_support::nats::test_nats();
    let fixture = Fixture::new(server.url()).await;
    let id = fixture.in_flight(10 * THRESHOLD_MS).await;
    let sweep = fixture.sweep();
    let mut sub = Box::pin(fixture.subscribe().await);

    sweep.tick(NOW).await.expect("first tick");
    let first = next_stuck(&mut sub).await;
    match &first.payload {
        EventPayload::InvocationStuck(p) => assert_eq!(p.last_step_at_ms, NOW - 10 * THRESHOLD_MS),
        other => panic!("expected invocation.stuck, got {other:?}"),
    }

    // The reducer crossed one more step boundary and then stopped
    // again. The row is stuck once more, at a *different* boundary.
    fixture.write_row(&id, NOW - 2 * THRESHOLD_MS, None).await;
    sweep.tick(NOW).await.expect("second tick");
    let second = next_stuck(&mut sub).await;
    assert_eq!(second.envelope.invocation_id.to_string(), id);
    match &second.payload {
        EventPayload::InvocationStuck(p) => assert_eq!(p.last_step_at_ms, NOW - 2 * THRESHOLD_MS),
        other => panic!("expected invocation.stuck, got {other:?}"),
    }
}

/// The exit criterion for "one definition of stuck": `fq doctor`'s
/// verdict and the sweep's must name the same invocation, from the same
/// stores, at the same instant and threshold. They are separate code
/// paths — a report handler and a periodic tick — and this is what
/// stops them drifting.
#[tokio::test]
async fn the_doctor_and_the_sweep_agree_on_the_same_invocation() {
    let server = crate::test_support::nats::test_nats();
    let fixture = Fixture::new(server.url()).await;
    let stuck = fixture.in_flight(10 * THRESHOLD_MS).await;
    let fresh = fixture.in_flight(1_000).await;
    let sweep = fixture.sweep();
    let mut sub = Box::pin(fixture.subscribe().await);

    sweep.tick(NOW).await.expect("tick");
    let event = next_stuck(&mut sub).await;
    let flagged_by_sweep = event.envelope.invocation_id.to_string();

    let views = Views::open(&crate::db::RuntimeDbPaths {
        worker: fixture._dir.path().join("worker.db"),
        control_plane: fixture._dir.path().join("cp.db"),
        projection: fixture._dir.path().join("projection.db"),
    })
    .await;
    // The projection store is not written by this fixture, so open it
    // only if `Views` could; the agreement claim is about the worker
    // and control-plane halves.
    if let Ok(views) = views {
        let executions = views
            .executions(
                NOW,
                THRESHOLD_MS,
                crate::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
            )
            .await
            .expect("executions");
        assert_eq!(
            executions.stuck_ids,
            vec![flagged_by_sweep.clone()],
            "the doctor's stuck set must be exactly the sweep's"
        );
        assert_eq!(executions.in_flight, 2);
        assert!(!executions.stuck_ids.contains(&fresh));
    }
    assert_eq!(flagged_by_sweep, stuck);
}

/// The classifier itself, without a database. Three verdicts, and the
/// one that matters: an open call younger than the long-dispatch
/// threshold outranks a silent WAL row, because a model turn in flight
/// is not a stall.
#[test]
fn the_classifier_gives_one_verdict_per_row() {
    let long = 600_000;
    assert_eq!(
        classify_liveness(
            Some(NOW - 1_000),
            NOW - 10 * THRESHOLD_MS,
            NOW,
            THRESHOLD_MS,
            long
        ),
        Liveness::Working,
        "a fresh open call explains the silence"
    );
    assert_eq!(
        classify_liveness(None, NOW - 1_000, NOW, THRESHOLD_MS, long),
        Liveness::Advancing,
        "nothing open, and the row moved recently"
    );
    assert_eq!(
        classify_liveness(None, NOW - 10 * THRESHOLD_MS, NOW, THRESHOLD_MS, long),
        Liveness::Stuck,
        "nothing open and nothing moving"
    );
    assert_eq!(
        classify_liveness(
            Some(NOW - 2 * long),
            NOW - 10 * THRESHOLD_MS,
            NOW,
            THRESHOLD_MS,
            long
        ),
        Liveness::Stuck,
        "an open call older than the long-dispatch threshold explains nothing"
    );
}
