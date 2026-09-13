//! End-to-end tests for the maintenance consumer, against a private
//! broker: a real JetStream stream, a real durable, real redelivery.
//!
//! Each test scopes its durable and its subject filter to its own
//! generated task subject where it can, so nothing here depends on
//! test ordering.

use std::time::Duration;

use futures::StreamExt;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{MaintenanceConsumer, MaintenanceTask, UnknownTask};
use crate::bus::EventBus;
use crate::events::{Event, EventPayload, MaintenanceOutcome, subjects};

/// Publish one maintenance command, optionally stamped with the
/// dedup/run id fq-cron would set.
async fn publish_command(bus: &EventBus, subject: &str, msg_id: Option<&str>) {
    let js = bus.jetstream();
    let ack = match msg_id {
        Some(id) => {
            let mut headers = async_nats::HeaderMap::new();
            headers.insert(async_nats::header::NATS_MESSAGE_ID, id);
            js.publish_with_headers(subject.to_string(), headers, "{}".into())
                .await
        }
        None => js.publish(subject.to_string(), "{}".into()).await,
    }
    .expect("publish maintenance command");
    ack.await.expect("maintenance publish ack");
}

/// Collect maintenance outcome events until `want` of them have
/// arrived, or fail loudly.
async fn next_outcome(
    sub: &mut (impl StreamExt<Item = Result<Event, crate::bus::BusError>> + Unpin),
    within: Duration,
) -> Event {
    tokio::time::timeout(within, sub.next())
        .await
        .expect("timed out waiting for a maintenance outcome event")
        .expect("outcome subscription closed")
        .expect("outcome event deserialises")
}

fn outcome_of(event: &Event) -> &MaintenanceOutcome {
    match &event.payload {
        EventPayload::MaintenanceRun(payload) => &payload.outcome,
        other => panic!("expected a maintenance_run event, got {other:?}"),
    }
}

/// Spawn the consumer scoped to one subject, and hand back its
/// shutdown.
fn spawn(
    bus: &EventBus,
    filter_subject: String,
    ack_wait: Duration,
    task_delay: Duration,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>, String) {
    let name = format!("fq-maintenance-test-{}", Uuid::now_v7().simple());
    let consumer = MaintenanceConsumer::new(bus.clone(), Uuid::now_v7(), ack_wait)
        .with_test_scope(name.clone(), filter_subject)
        .with_task_delay(task_delay);
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        consumer.run(rx).await.expect("maintenance consumer runs");
    });
    (tx, handle, name)
}

/// The whole path, end to end: fq-cron's publish, the durable, the
/// registry, the task, the outcome on the event log.
#[tokio::test]
async fn a_published_maintenance_command_runs_the_task_and_records_the_outcome() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");

    let subject = MaintenanceTask::Ping.subject();
    let (shutdown, handle, _name) = spawn(
        &bus,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
    );
    // The durable starts at `New`, so the command has to be published
    // after the consumer exists — which is also how it runs in
    // production, where the daemon outlives every schedule.
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish_command(&bus, &subject, Some("fq-cron/ping@2026-09-13T02:00:00Z")).await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(10)).await;
    let EventPayload::MaintenanceRun(payload) = &event.payload else {
        panic!("expected a maintenance_run event, got {:?}", event.payload);
    };
    assert_eq!(payload.task, "ping");
    assert_eq!(payload.run_id, "fq-cron/ping@2026-09-13T02:00:00Z");
    match &payload.outcome {
        MaintenanceOutcome::Succeeded { detail } => assert_eq!(detail, "pong"),
        other => panic!("expected a succeeded outcome, got {other:?}"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A task name this build has no variant for is refused *and
/// recorded*: the message is answered with an event, never dropped.
#[tokio::test]
async fn an_unknown_task_is_refused_and_recorded() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");

    let subject = subjects::maintenance("no-such-task");
    let (shutdown, handle, _name) = spawn(
        &bus,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish_command(&bus, &subject, None).await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(10)).await;
    let EventPayload::MaintenanceRun(payload) = &event.payload else {
        panic!("expected a maintenance_run event, got {:?}", event.payload);
    };
    assert_eq!(payload.task, "no-such-task");
    assert_eq!(payload.duration_ms, 0, "nothing ran");
    match &payload.outcome {
        MaintenanceOutcome::Refused { reason } => assert!(
            reason.contains("no-such-task") && reason.contains("ping"),
            "the refusal names what was asked for and what is known: {reason}"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The at-least-once rule (#327 is unfixed, so this is real): a
/// message JetStream delivers twice runs its task **once**.
///
/// The redelivery is genuine, not simulated — the durable's ack window
/// is a second and the task is held for two, so the server redelivers
/// underneath the run in flight. The proof is on both sides: the
/// consumer reports more than one delivery, and the event log holds
/// exactly one outcome for the run.
///
/// The quiet window below is deliberately longer than the held run: a
/// second run would take another `task_delay` to reach its publish, so
/// a shorter window would let this test pass with the guard removed —
/// which it did, before the window was sized to outlast the task.
#[tokio::test]
async fn a_redelivered_command_does_not_run_the_task_twice() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");

    let subject = MaintenanceTask::Ping.subject();
    let (shutdown, handle, name) = spawn(
        &bus,
        subject.clone(),
        Duration::from_secs(1),
        Duration::from_secs(2),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish_command(&bus, &subject, Some("fq-cron/ping@2026-09-13T03:00:00Z")).await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(20)).await;
    assert!(
        matches!(outcome_of(&event), MaintenanceOutcome::Succeeded { .. }),
        "the first run succeeds: {:?}",
        outcome_of(&event)
    );

    // A quiet window: a second run would publish a second outcome, and
    // it would land here.
    let second = tokio::time::timeout(Duration::from_secs(8), outcomes.next()).await;
    assert!(
        second.is_err(),
        "the redelivery ran the task again: {second:?}"
    );

    let stream = bus
        .jetstream()
        .get_stream(crate::bus::MAINTENANCE_STREAM_NAME)
        .await
        .expect("maintenance stream");
    let mut durable = stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(&name)
        .await
        .expect("the test durable");
    let info = durable.info().await.expect("consumer info");
    assert!(
        info.delivered.consumer_sequence > 1,
        "no redelivery happened, so this test proved nothing: {:?}",
        info.delivered
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The registry is closed and its name table is total: every name
/// round-trips to the variant that produced it, which is what fails
/// when a new variant is added without a `MaintenanceTask::ALL` entry.
#[test]
fn every_task_round_trips_through_its_name() {
    for task in MaintenanceTask::ALL {
        assert_eq!(
            MaintenanceTask::parse(task.name()),
            Ok(*task),
            "{} does not round-trip",
            task.name()
        );
        assert_eq!(task.subject(), subjects::maintenance(task.name()));
    }
}

/// The refusal is a value that says what was asked and what exists.
#[test]
fn an_unknown_name_is_a_typed_refusal() {
    let err = MaintenanceTask::parse("compact-everything").expect_err("unknown");
    assert_eq!(
        err,
        UnknownTask {
            task: "compact-everything".to_string(),
            known: "ping".to_string(),
        }
    );
}
