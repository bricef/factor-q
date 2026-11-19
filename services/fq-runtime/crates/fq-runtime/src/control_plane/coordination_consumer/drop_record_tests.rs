//! The fold of a drop ignores what its event records of who asked and of
//! the run's archive (issue 1070): `requested_by` and `archive` are
//! carried for the fold that will record a termination request and an
//! archive that never comes, and until then the drop moves a row exactly as
//! a drop without them does.
//!
//! The payload is built from JSON, so this reads the same before the fields
//! exist (an unknown key is ignored) as after (the fields are read and
//! ignored by the fold).

use super::*;
use crate::agent::AgentId;
use crate::control_plane::store::{OwnerRow, OwnerStatus};
use crate::events::InvocationOperatorRecoveredPayload;
use crate::test_support::nats::{NatsServer, test_nats};
use tempfile::TempDir;

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
    /// the id and the times the fold takes from its own clock.
    async fn left(
        &self,
        invocation_id: Uuid,
    ) -> (Option<OwnerRow>, Option<(String, String, Vec<u8>)>) {
        let id = invocation_id.to_string();
        let owner = self
            .store
            .get_invocation_owner(&id)
            .await
            .unwrap()
            .map(|row| OwnerRow {
                invocation_id: String::new(),
                assigned_at: 0,
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

/// From each row a drop can find (none, in flight, finished), a drop that
/// records who asked and either archive leaves the row and the archive
/// record a drop without them leaves.
#[tokio::test]
async fn the_fold_of_a_drop_ignores_who_asked_and_the_archive() {
    let fixture = Fixture::new().await;
    for (start, seed) in [
        ("no row", None),
        ("in flight", Some(OwnerStatus::InFlight)),
        ("completed", Some(OwnerStatus::Completed)),
    ] {
        for archive in ["pending", "none", "a value a later build adds"] {
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
            assert_eq!(
                fixture.left(with).await,
                fixture.left(bare).await,
                "from {start}, a drop recording who asked and archive `{archive}` moved the row \
                 as a drop without them does not"
            );
        }
    }
}
