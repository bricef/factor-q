//! What an `invocation.ambiguous` does to an ownership row, from each
//! status the row can hold when the event arrives (issue 1024).
//!
//! The event asks for a run to be triaged. It can reach the control plane
//! after the run is recorded finished: the next start's recovery reports a
//! run killed mid tool call whose drop was published but not yet folded
//! when the daemon stopped (`fq-daemon`'s recovery tests drive that one
//! end to end), and a resume whose worker row cannot be read is reported
//! though the run may have ended and been archived. A finished status,
//! `completed` or `failed` and an operator's drop among them, is never
//! left for `ambiguous`.
//!
//! Each test drives the handlers the consumer dispatches to, in the order
//! the stream delivers the events, over a real store.

use super::*;
use crate::agent::AgentId;
use crate::events::{
    InvocationAmbiguousPayload, InvocationArchivedPayload, InvocationOperatorRecoveredPayload,
};
use crate::test_support::nats::{NatsServer, test_nats};
use crate::worker::WorkerId;
use tempfile::TempDir;

/// A consumer over a fresh store, and the one invocation every test moves.
struct Fixture {
    consumer: CoordinationConsumer,
    store: Arc<ControlPlaneStore>,
    agent_id: AgentId,
    worker_id: WorkerId,
    invocation_id: Uuid,
    _dir: TempDir,
    _server: NatsServer,
}

impl Fixture {
    async fn new() -> Self {
        let server = test_nats();
        let bus = EventBus::connect(server.url()).await.expect("connect NATS");
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            ControlPlaneStore::open(&dir.path().join("control-plane.db"))
                .await
                .unwrap(),
        );
        Self {
            consumer: CoordinationConsumer::new(bus, store.clone()),
            store,
            agent_id: AgentId::new(format!("late-ambiguous-{}", Uuid::now_v7().simple())).unwrap(),
            worker_id: WorkerId::new(Uuid::now_v7().to_string()).unwrap(),
            invocation_id: Uuid::now_v7(),
            _dir: dir,
            _server: server,
        }
    }

    fn event(&self, payload: EventPayload) -> Event {
        Event::new(self.agent_id.clone(), self.invocation_id, payload)
    }

    /// Deliver `invocation.ambiguous`, as startup recovery publishes it.
    async fn ambiguous(&self) {
        let payload = InvocationAmbiguousPayload {
            stuck_entity: "tool_dispatch".to_string(),
            stuck_call_id: "tc-0".to_string(),
            note: "worker startup categorisation found a `dispatched` row without `completed`"
                .to_string(),
        };
        let event = self.event(EventPayload::InvocationAmbiguous(payload.clone()));
        self.consumer
            .handle_invocation_ambiguous(&event, &payload)
            .await
            .expect("the ambiguous handler succeeds");
    }

    /// Deliver the worker's `invocation.archived`, ending the run in
    /// `final_phase`.
    async fn archived(&self, final_phase: &str) {
        let payload = InvocationArchivedPayload {
            worker_id: self.worker_id.clone(),
            final_phase: final_phase.to_string(),
            final_state_blob: b"{}".to_vec(),
            started_at_ms: 1_000,
            terminal_at_ms: 2_000,
        };
        let event = self.event(EventPayload::InvocationArchived(payload.clone()));
        self.consumer
            .handle_invocation_archived(&event, &payload)
            .await
            .expect("the archived handler succeeds");
    }

    /// Deliver `invocation.operator_recovered`, as `fq invocation drop`
    /// publishes it.
    async fn dropped(&self) {
        let payload = InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: Some("abandoned by the operator".to_string()),
        };
        let event = self.event(EventPayload::InvocationOperatorRecovered(payload.clone()));
        self.consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("the operator-recovered handler succeeds");
    }

    async fn row(&self) -> Option<crate::control_plane::store::OwnerRow> {
        self.store
            .get_invocation_owner(&self.invocation_id.to_string())
            .await
            .unwrap()
    }
}

/// The whole row a finished run's late `ambiguous` must leave alone: its
/// status, and the worker and time it was recorded under.
async fn ambiguous_leaves_the_row_as_it_was(fixture: &Fixture, finished: OwnerStatus) {
    let before = fixture.row().await.expect("the run is on record");
    assert_eq!(before.status, finished);
    fixture.ambiguous().await;
    assert_eq!(
        fixture.row().await,
        Some(before),
        "a late invocation.ambiguous must not move a run recorded {} back to triage",
        finished.as_str()
    );
}

#[tokio::test]
async fn ambiguous_after_failed_leaves_the_run_failed() {
    let fixture = Fixture::new().await;
    fixture.archived("failed").await;
    ambiguous_leaves_the_row_as_it_was(&fixture, OwnerStatus::Failed).await;
}

#[tokio::test]
async fn ambiguous_after_completed_leaves_the_run_completed() {
    let fixture = Fixture::new().await;
    fixture.archived("completed").await;
    ambiguous_leaves_the_row_as_it_was(&fixture, OwnerStatus::Completed).await;
}

#[tokio::test]
async fn ambiguous_after_an_operator_drop_leaves_the_run_dropped() {
    let fixture = Fixture::new().await;
    fixture.dropped().await;
    ambiguous_leaves_the_row_as_it_was(&fixture, OwnerStatus::Failed).await;
}

/// A triggered run in flight has no row yet: the event writes one.
#[tokio::test]
async fn ambiguous_for_a_run_in_flight_with_no_row_marks_it() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.row().await, None);
    fixture.ambiguous().await;
    assert_eq!(
        fixture.row().await.map(|row| row.status),
        Some(OwnerStatus::Ambiguous)
    );
}

/// A run startup recovery restored to `in_flight`, whose resume could not
/// be carried out, is marked for triage.
#[tokio::test]
async fn ambiguous_for_a_run_restored_in_flight_marks_it() {
    let fixture = Fixture::new().await;
    fixture
        .store
        .upsert_invocation_ownership(
            &fixture.invocation_id.to_string(),
            fixture.worker_id.as_str(),
            1_000,
            OwnerStatus::InFlight,
        )
        .await
        .unwrap();
    fixture.ambiguous().await;
    assert_eq!(
        fixture.row().await.map(|row| row.status),
        Some(OwnerStatus::Ambiguous)
    );
}

/// JetStream delivers at least once: the same `ambiguous` handled twice
/// leaves the row exactly as the first left it.
#[tokio::test]
async fn a_redelivered_ambiguous_is_idempotent() {
    let fixture = Fixture::new().await;
    fixture.ambiguous().await;
    let first = fixture
        .row()
        .await
        .expect("the first delivery writes the row");
    assert_eq!(first.status, OwnerStatus::Ambiguous);
    // Past the millisecond the first was stamped in, so a second stamp
    // would show.
    tokio::time::sleep(Duration::from_millis(5)).await;
    fixture.ambiguous().await;
    assert_eq!(fixture.row().await, Some(first));
}
