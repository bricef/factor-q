//! `invocation.drop` driven through its whole handler with a runner that
//! loses the arm (issue 1070).

use std::time::Duration;

use super::*;
use fq_runtime::control_plane::CoordinationConsumer;
use fq_runtime::control_plane::ownership::RowState;
use fq_runtime::control_plane::store::{ArchiveState, EndedBy, MoveFacts, OwnerStatus};

/// 2026-01-02T03:04:05Z.
const T: i64 = 1_767_323_045_000;

/// A runner that drives the run at the first answer, and has let it go by
/// the second: the run ended in between.
struct EndsBetweenTheAnswers(fq_runtime::AgentId);

impl DriveHalts for EndsBetweenTheAnswers {
    fn active_agent(&self, _: &uuid::Uuid) -> Option<fq_runtime::AgentId> {
        Some(self.0.clone())
    }

    fn arm_halt(&self, _: uuid::Uuid) -> Option<fq_runtime::worker::ArmedHalt> {
        None
    }
}

/// **A live drop whose arm is lost goes through the handler and records
/// the request beside the ending the run reached.** The runner names the
/// run's agent and then has no run to halt; the handler publishes the
/// drop under that agent and returns its receipt, and the fold records the
/// request on the run's row, which keeps its own `completed`.
#[tokio::test]
async fn a_live_drop_that_loses_the_arm_is_recorded_through_the_handler() {
    let nats = fq_test_support::NatsServer::start();
    let scratch = tempfile::tempdir().unwrap();
    let paths = fq_runtime::db::RuntimeDbPaths::under(scratch.path());
    let bus = fq_runtime::EventBus::connect(nats.url()).await.unwrap();
    let projection = Arc::new(
        fq_runtime::control_plane::projection::ProjectionStore::open(&paths.projection)
            .await
            .unwrap(),
    );
    let control_plane = Arc::new(
        fq_runtime::control_plane::store::ControlPlaneStore::open(&paths.control_plane)
            .await
            .unwrap(),
    );
    let worker_store = Arc::new(fq_runtime::WorkerStore::open(&paths.worker).await.unwrap());
    let runner = Arc::new(fq_runtime::ReducerRunner::new(
        Arc::new(
            fq_runtime::ReducerContext::builder()
                .tools(Arc::new(fq_runtime::ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            fq_runtime::RunnerConfig::builder()
                .bus(bus.clone())
                .pricing(Arc::new(fq_runtime::PricingTable::from_map(
                    std::collections::HashMap::new(),
                )))
                .store(worker_store.clone())
                .worker_id(fq_runtime::worker::WorkerId::new("drop-command".to_string()).unwrap())
                .build(),
        ),
        fq_runtime::Harness::new(),
    ));
    let deps = DropDeps {
        bus: bus.clone(),
        projection,
        control_plane: control_plane.clone(),
        workers: fq_runtime::control_plane::dispatcher::WorkerPermits::new(runner, 1),
        worker_store,
    };
    let agent = fq_runtime::AgentId::new(format!("drop-command-{}", uuid::Uuid::now_v7().simple()))
        .unwrap();
    let invocation_id = uuid::Uuid::now_v7().to_string();
    assert!(
        control_plane
            .compare_and_set_ownership(
                &invocation_id,
                None,
                RowState {
                    status: OwnerStatus::Completed,
                    archive: ArchiveState::Archived,
                    ended_by: Some(EndedBy::Run),
                    requested: false,
                },
                MoveFacts {
                    worker_id: Some(agent.as_str()),
                    assigned_at: Some(T),
                    ended_at: Some(T),
                    request: None,
                },
            )
            .await
            .unwrap(),
        "seed the run's own ending"
    );
    let name = format!("drop-command-{}", uuid::Uuid::now_v7().simple());
    let consumer = CoordinationConsumer::new(bus.clone(), control_plane.clone())
        .with_test_consumer_name(name.clone())
        .with_test_filter_subject(format!("fq.agent.{}.>", agent.as_str()));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    tokio::spawn(consumer.run(stopped));
    let stream = bus
        .jetstream()
        .get_stream(fq_runtime::bus::STREAM_NAME)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(&name)
        .await
        .is_err()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the consumer never attached"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let receipt = drop_command(
        &deps,
        &EndsBetweenTheAnswers(agent.clone()),
        DropCommandInput {
            invocation_id: invocation_id.clone(),
            reason: Some("lost the arm".to_string()),
            live: true,
        },
        &fq_edge::CallContext::none(),
    )
    .await
    .unwrap_or_else(|refused| {
        panic!("a live drop that lost its arm was refused, which loses the request: {refused:?}")
    });
    assert!(
        receipt
            .atoms
            .iter()
            .any(|atom| atom.domain == fq_ops::Domain::Event),
        "the receipt names the drop event: {receipt:?}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let row = loop {
        let row = control_plane
            .get_invocation_owner(&invocation_id)
            .await
            .unwrap()
            .expect("the row stays");
        if row.termination.is_some() {
            break row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the drop's request never reached the row: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let _ = stop.send(());
    assert_eq!(
        (row.status, row.archive_state, row.ended_by, row.ended_at),
        (
            OwnerStatus::Completed,
            ArchiveState::Archived,
            Some(EndedBy::Run),
            Some(T)
        ),
        "the run keeps its own ending"
    );
    assert_eq!(
        row.termination
            .and_then(|request| request.reason)
            .as_deref(),
        Some("lost the arm")
    );
}
