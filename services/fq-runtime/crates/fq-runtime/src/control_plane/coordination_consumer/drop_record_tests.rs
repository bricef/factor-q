//! The fold of a drop records who asked, and reads an archive that may
//! come as one still pending (issue 1070): a drop's `requested_by` is the
//! caller of the request its fold records on the ownership row, a drop
//! without it records none ("not recorded"), and a drop whose `archive`
//! is `pending`, or a value this build does not know, leaves the row as a
//! drop without the field does. A drop saying `none` is
//! `archive_none_tests.rs`'s.
//!
//! The payload is built from JSON, as the daemon writes it.

use super::*;
use crate::agent::AgentId;
use crate::control_plane::store::{OwnerRow, OwnerStatus, TerminationRequest};
use crate::events::InvocationOperatorRecoveredPayload;
use crate::test_support::nats::{NatsServer, test_nats};
use tempfile::TempDir;

/// What a drop's fold left: the ownership row and the archive record's
/// agent, phase and state.
type Left = (Option<OwnerRow>, Option<(String, String, Vec<u8>)>);

struct Fixture {
    consumer: CoordinationConsumer,
    store: Arc<ControlPlaneStore>,
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
            _dir: dir,
            _server: server,
        }
    }

    /// Fold a drop of `invocation_id` whose payload is `payload`.
    async fn drop_with(&self, invocation_id: Uuid, payload: serde_json::Value) {
        let payload: InvocationOperatorRecoveredPayload =
            serde_json::from_value(payload).expect("a drop's payload");
        let event = Event::new(
            AgentId::new("drop-record").unwrap(),
            invocation_id,
            EventPayload::InvocationOperatorRecovered(payload.clone()),
        );
        self.consumer
            .handle_invocation_operator_recovered(&event, &payload)
            .await
            .expect("the drop folds");
    }

    /// What the fold left: the ownership row and the archive record, less
    /// the id and the times, which are each drop's own.
    async fn left(&self, invocation_id: Uuid) -> Left {
        let id = invocation_id.to_string();
        let owner = self
            .store
            .get_invocation_owner(&id)
            .await
            .unwrap()
            .map(|row| OwnerRow {
                invocation_id: String::new(),
                assigned_at: 0,
                ended_at: row.ended_at.map(|_| 0),
                termination: row.termination.map(|request| TerminationRequest {
                    requested_at: 0,
                    ..request
                }),
                ..row
            });
        let archive = self
            .store
            .get_archive(&id)
            .await
            .unwrap()
            .map(|row| (row.agent_id, row.final_phase, row.final_state_blob));
        (owner, archive)
    }
}

fn plain() -> serde_json::Value {
    serde_json::json!({"action": "drop", "final_phase": "failed", "reason": "abandoned"})
}

fn recorded(archive: &str) -> serde_json::Value {
    serde_json::json!({
        "action": "drop",
        "final_phase": "failed",
        "reason": "abandoned",
        "requested_by": "on-call",
        "archive": archive,
    })
}

/// The caller a row's request names, if the row records a request.
fn caller(left: &Left) -> Option<Option<&str>> {
    left.0.as_ref().and_then(|row| {
        row.termination
            .as_ref()
            .map(|request| request.requested_by.as_deref())
    })
}

/// From each row a drop can find (none, in flight, finished), a drop that
/// records who asked names its caller in the request it records, and one
/// without records none; with an archive that may come, each leaves the
/// row and the archive record as the other does.
#[tokio::test]
async fn the_fold_of_a_drop_records_who_asked_and_reads_an_archive_that_may_come_as_pending() {
    let fixture = Fixture::new().await;
    for (start, seed) in [
        ("no row", None),
        ("in flight", Some(OwnerStatus::InFlight)),
        ("completed", Some(OwnerStatus::Completed)),
    ] {
        for archive in ["pending", "a value a later build adds"] {
            let (bare, with) = (Uuid::now_v7(), Uuid::now_v7());
            if let Some(status) = seed {
                for id in [bare, with] {
                    fixture
                        .store
                        .upsert_invocation_ownership(&id.to_string(), "worker-1", 1_000, status)
                        .await
                        .unwrap();
                }
            }
            fixture.drop_with(bare, plain()).await;
            fixture.drop_with(with, recorded(archive)).await;
            let (bare, mut with) = (fixture.left(bare).await, fixture.left(with).await);
            assert_eq!(
                (caller(&with), caller(&bare)),
                (Some(Some("on-call")), Some(None)),
                "from {start}: the request names who asked, and is not recorded without it"
            );
            if let Some(request) = with.0.as_mut().and_then(|row| row.termination.as_mut()) {
                request.requested_by = None;
            }
            assert_eq!(
                with, bare,
                "from {start}, a drop with archive `{archive}` moved the row as a drop without \
                 the field does not"
            );
        }
    }
}
