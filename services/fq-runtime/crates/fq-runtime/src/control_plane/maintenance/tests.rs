//! End-to-end tests for the maintenance consumer, against a private
//! broker: a real JetStream stream, a real durable, real redelivery.
//!
//! Each test scopes its durable and its subject filter to its own
//! generated task subject where it can, so nothing here depends on
//! test ordering.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::{MaintenanceConsumer, MaintenanceContext, MaintenanceTask, UnknownTask};
use crate::bus::EventBus;
use crate::events::{Event, EventPayload, MaintenanceOutcome, OperatorSignalPayload, subjects};
use crate::pricing::refresh::{PricingOverlay, PricingRefresh};
use crate::pricing::served::ServedPricing;
use crate::pricing::{ModelPricing, PricingTable};
use crate::test_support::mock_litellm::MockLitellm;

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
    spawn_with(
        bus,
        filter_subject,
        ack_wait,
        task_delay,
        MaintenanceContext::new(),
    )
}

/// The same, with the dependencies a task runs against.
fn spawn_with(
    bus: &EventBus,
    filter_subject: String,
    ack_wait: Duration,
    task_delay: Duration,
    ctx: MaintenanceContext,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>, String) {
    let name = format!("fq-maintenance-test-{}", Uuid::now_v7().simple());
    let (tx, handle) = spawn_named(bus, &name, filter_subject, ack_wait, task_delay, ctx);
    (tx, handle, name)
}

/// [`spawn`] under a durable name the caller chooses — what a restart
/// looks like from the broker's side, where the durable is the identity
/// and the process is not.
fn spawn_named(
    bus: &EventBus,
    name: &str,
    filter_subject: String,
    ack_wait: Duration,
    task_delay: Duration,
    ctx: MaintenanceContext,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let consumer = MaintenanceConsumer::new(bus.clone(), Uuid::now_v7(), ack_wait)
        .with_context(ctx)
        .with_test_scope(name.to_string(), filter_subject)
        .with_task_delay(task_delay);
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        consumer.run(rx).await.expect("maintenance consumer runs");
    });
    (tx, handle)
}

/// Wait until the named durable exists on the maintenance stream, so a
/// test that depends on "the consumer was here" says so to the broker
/// rather than to a sleep.
async fn await_durable(bus: &EventBus, name: &str) {
    let stream = bus
        .jetstream()
        .get_stream(crate::bus::MAINTENANCE_STREAM_NAME)
        .await
        .expect("maintenance stream");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if stream
            .get_consumer::<async_nats::jetstream::consumer::pull::Config>(name)
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("durable {name} never appeared on the maintenance stream");
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
            known: "ping, pricing_refresh".to_string(),
        }
    );
}

/// The `DeliverPolicy::New` decision, asserted: a command published
/// while no durable exists is **never run**, not even when the daemon
/// that would have run it starts a second later.
///
/// This is the rule the operator relies on — fq-cron's schedule is the
/// retry, and a tick missed while the daemon was down stays missed —
/// and it is also the rule that would be silently inverted by someone
/// "fixing" the policy to `All` after a report that a command went
/// missing. The consequence being bought is on the other side: no
/// deployment ever executes a day of accumulated sweeps in one burst.
///
/// The guard against a vacuous pass is the second command: published
/// *after* the durable exists, it runs, so the consumer was alive, the
/// filter matched, and the first command's silence is the policy rather
/// than a broken fixture.
#[tokio::test]
async fn a_command_published_before_the_durable_exists_is_never_run() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");

    let subject = MaintenanceTask::Ping.subject();
    // Published into the stream with nothing consuming it: the daemon
    // is down, or this build's consumer has never run here.
    publish_command(
        &bus,
        &subject,
        Some("fq-cron/ping@while-the-daemon-was-down"),
    )
    .await;

    let (shutdown, handle, name) = spawn(
        &bus,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
    );
    await_durable(&bus, &name).await;
    publish_command(
        &bus,
        &subject,
        Some("fq-cron/ping@after-the-daemon-came-back"),
    )
    .await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(10)).await;
    let EventPayload::MaintenanceRun(payload) = &event.payload else {
        panic!("expected a maintenance_run event, got {:?}", event.payload);
    };
    assert_eq!(
        payload.run_id, "fq-cron/ping@after-the-daemon-came-back",
        "the first outcome must be the command published after the durable existed"
    );

    // The missed command would land here if the durable had started at
    // the beginning of the stream.
    let second = tokio::time::timeout(Duration::from_secs(3), outcomes.next()).await;
    assert!(
        second.is_err(),
        "a command published before the durable existed was run: {second:?}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The other half of the same decision: an **existing** durable keeps
/// its position, so an ordinary restart picks up what was published
/// while the process was gone.
///
/// `New` is a rule about the durable's first creation, not about every
/// start — a daemon that has run here before misses nothing by
/// restarting. Without that, a deploy would silently drop whichever
/// scheduled fire landed in the seconds a restart takes.
#[tokio::test]
async fn a_restart_picks_up_a_command_published_while_the_consumer_was_stopped() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");

    let subject = MaintenanceTask::Ping.subject();
    let name = format!("fq-maintenance-test-{}", Uuid::now_v7().simple());

    // First start: the durable is created, and then the process goes
    // away — a redeploy, a crash, a `fq down`.
    let (shutdown, handle) = spawn_named(
        &bus,
        &name,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
        MaintenanceContext::new(),
    );
    await_durable(&bus, &name).await;
    let _ = shutdown.send(());
    handle.await.expect("first consumer stops");

    publish_command(&bus, &subject, Some("fq-cron/ping@during-the-restart")).await;

    // Second start, same durable name: a new process, the broker's same
    // consumer.
    let (shutdown, handle) = spawn_named(
        &bus,
        &name,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
        MaintenanceContext::new(),
    );

    let event = next_outcome(&mut outcomes, Duration::from_secs(10)).await;
    let EventPayload::MaintenanceRun(payload) = &event.payload else {
        panic!("expected a maintenance_run event, got {:?}", event.payload);
    };
    assert_eq!(payload.run_id, "fq-cron/ping@during-the-restart");
    assert!(
        matches!(&payload.outcome, MaintenanceOutcome::Succeeded { .. }),
        "the missed command runs on the restart: {:?}",
        payload.outcome
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The one subject shape that reaches the "names no task" arm: a
/// dotted tail. `fq.maintenance.a.b` is inside the durable's filter
/// and is *not* a task called `a.b`, so it is refused and recorded
/// like any other refusal rather than dropped.
#[tokio::test]
async fn a_dotted_tail_is_refused_and_recorded() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");

    let subject = format!("{}.b", subjects::maintenance("a"));
    let (shutdown, handle, name) = spawn(
        &bus,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
    );
    await_durable(&bus, &name).await;
    publish_command(&bus, &subject, None).await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(10)).await;
    match outcome_of(&event) {
        MaintenanceOutcome::Refused { reason } => assert!(
            reason.contains(&subject),
            "the refusal names the subject it could not read: {reason}"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

// --- the pricing refresh (#344) ---------------------------------------

/// A LiteLLM-shaped document: the two price fields the table reads and
/// one it does not, so the cached splice can be checked for fidelity.
fn litellm(entries: &[(&str, f64)]) -> String {
    let map: serde_json::Map<String, Value> = entries
        .iter()
        .map(|(model, input)| {
            (
                (*model).to_string(),
                json!({
                    "input_cost_per_token": input,
                    "output_cost_per_token": input * 5.0,
                    "litellm_provider": "somebody",
                }),
            )
        })
        .collect();
    serde_json::to_string_pretty(&map).expect("a document serialises")
}

fn priced(input_per_token: f64) -> ModelPricing {
    ModelPricing {
        input_per_million: input_per_token * 1_000_000.0,
        output_per_million: input_per_token * 5_000_000.0,
        cache_read_per_million: None,
        cache_write_per_million: None,
    }
}

/// The table a daemon is serving when the schedule fires, and the
/// refresh wired to a mock upstream and a private cache file.
fn refresh_against(
    upstream: &MockLitellm,
    cache_path: PathBuf,
    current: &[(&str, f64)],
) -> (PricingRefresh, ServedPricing) {
    let mut table = PricingTable::empty();
    for (model, input) in current {
        table.insert(*model, priced(*input));
    }
    let served = ServedPricing::new(table);
    let refresh = PricingRefresh::new(
        crate::pricing::live::LoadSettings::default(),
        cache_path,
        PricingOverlay::new(),
        served.clone(),
        crate::pricing::episodes::PricingEpisodes::new(),
    )
    .with_upstream(upstream.url());
    (refresh, served)
}

/// Collect the operator signals a run raised, until the stream goes
/// quiet.
async fn drain_signals(
    sub: &mut (impl StreamExt<Item = Result<Event, crate::bus::BusError>> + Unpin),
    within: Duration,
) -> Vec<OperatorSignalPayload> {
    let mut signals = Vec::new();
    while let Ok(Some(Ok(event))) = tokio::time::timeout(within, sub.next()).await {
        match event.payload {
            EventPayload::OperatorSignal(signal) => signals.push(signal),
            other => panic!("expected an operator_signal event, got {other:?}"),
        }
    }
    signals
}

/// The whole refresh, through the scheduler's own path: fq-cron's
/// publish, the durable, the registry, the fetch, acceptance, the swap,
/// the cache, the outcome, the signals.
///
/// Four claims in one run because they are one run — the fixture is a
/// daemon serving four models and an upstream document that changes one
/// plausibly, changes one implausibly, adds one, and stops listing one.
#[tokio::test]
async fn a_scheduled_refresh_swaps_the_served_table_and_caches_what_it_accepted() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let cache = tempfile::tempdir().expect("cache dir");
    let cache_path = cache.path().join("pricing.json");

    // Upstream: a/one reprices within the bound, a/six-times moves 6x,
    // a/new appears, and a/retired is gone.
    let upstream = MockLitellm::start(litellm(&[
        ("a/one", 2e-6),
        ("a/six-times", 6e-6),
        ("a/new", 3e-6),
    ]))
    .await;
    let current = [("a/one", 1e-6), ("a/six-times", 1e-6), ("a/retired", 7e-6)];
    // The last accepted table, as the startup load left it. It is what
    // the 5x bound is measured against — the refresh judges each
    // document against the accepted document on disk, not against the
    // in-memory table, which is the same table plus the configuration
    // layered over it.
    std::fs::write(&cache_path, litellm(&current)).expect("seed the accepted table");
    let (refresh, served) = refresh_against(&upstream, cache_path.clone(), &current);

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");
    let mut signals = bus
        .subscribe(subjects::SYSTEM_OPERATOR_SIGNAL.to_string())
        .await
        .expect("subscribe to operator signals");

    let subject = MaintenanceTask::PricingRefresh.subject();
    let (shutdown, handle, _name) = spawn_with(
        &bus,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
        MaintenanceContext::new().with_pricing(refresh),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish_command(
        &bus,
        &subject,
        Some("fq-cron/pricing-refresh@2026-09-14T06:00:00Z"),
    )
    .await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(20)).await;
    let EventPayload::MaintenanceRun(payload) = &event.payload else {
        panic!("expected a maintenance_run event, got {:?}", event.payload);
    };
    assert_eq!(payload.task, "pricing_refresh");
    let MaintenanceOutcome::Succeeded { detail } = &payload.outcome else {
        panic!("expected a succeeded outcome, got {:?}", payload.outcome);
    };
    assert!(
        detail.contains("1 new") && detail.contains("1 repriced") && detail.contains("1 refused"),
        "the outcome says what the refresh did: {detail}"
    );
    assert!(
        detail.contains("1 no longer listed upstream"),
        "the outcome says what it is still pricing: {detail}"
    );

    // The served table: an accepted change landed, a new model was
    // admitted, an implausible change kept the prior price, and the
    // model upstream dropped is still priced — the widen-only rule, on
    // the table an in-flight invocation reads through.
    assert_eq!(
        served.current().lookup("a/one").unwrap().input_per_million,
        2.0
    );
    assert_eq!(
        served.current().lookup("a/new").unwrap().input_per_million,
        3.0
    );
    assert_eq!(
        served
            .current()
            .lookup("a/six-times")
            .unwrap()
            .input_per_million,
        1.0,
        "a 6x move keeps the prior price"
    );
    assert_eq!(
        served
            .current()
            .lookup("a/retired")
            .unwrap()
            .input_per_million,
        7.0,
        "a model upstream stopped listing stays priced until the daemon restarts"
    );

    // The refusal reached an operator, and only the refusal did.
    let raised = drain_signals(&mut signals, Duration::from_secs(3)).await;
    let kinds: Vec<&str> = raised.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, vec!["pricing.change_refused"], "{raised:?}");
    assert_eq!(
        raised[0].detail.get("model").and_then(Value::as_str),
        Some("a/six-times")
    );

    // The cache holds the accepted document — the refused model at its
    // prior price, and the retired model gone, which is what makes the
    // next daemon start the boundary at which the removal lands.
    let cached: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&cache_path).expect("cache written"))
            .expect("the cache is a document");
    assert_eq!(
        cached["a/six-times"]["input_cost_per_token"].as_f64(),
        Some(1e-6),
        "the cache holds what was accepted, not what was offered"
    );
    assert!(
        !cached.contains_key("a/retired"),
        "the accepted document is the boundary: a retired model is not carried into it"
    );
    assert_eq!(cached["a/new"]["input_cost_per_token"].as_f64(), Some(3e-6));

    let _ = shutdown.send(());
    let _ = handle.await;
    upstream.shutdown().await;
}

/// A redelivery answers from the ledger and never reaches the network.
///
/// The redelivery is genuine: the ack window is a second and the task is
/// held for two, so JetStream redelivers underneath the run in flight.
/// The proof is the mock's own count — "did not run twice" asserted on
/// the thing outside the process that would have noticed.
#[tokio::test]
async fn a_redelivered_refresh_does_not_fetch_again() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let cache = tempfile::tempdir().expect("cache dir");

    let upstream = MockLitellm::start(litellm(&[("a/one", 1e-6)])).await;
    let (refresh, _served) = refresh_against(
        &upstream,
        cache.path().join("pricing.json"),
        &[("a/one", 1e-6)],
    );

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");

    let subject = MaintenanceTask::PricingRefresh.subject();
    let (shutdown, handle, name) = spawn_with(
        &bus,
        subject.clone(),
        Duration::from_secs(1),
        Duration::from_secs(2),
        MaintenanceContext::new().with_pricing(refresh),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish_command(
        &bus,
        &subject,
        Some("fq-cron/pricing-refresh@2026-09-14T12:00:00Z"),
    )
    .await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(20)).await;
    assert!(
        matches!(outcome_of(&event), MaintenanceOutcome::Succeeded { .. }),
        "the first run succeeds: {:?}",
        outcome_of(&event)
    );
    let second = tokio::time::timeout(Duration::from_secs(8), outcomes.next()).await;
    assert!(
        second.is_err(),
        "the redelivery refreshed again: {second:?}"
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
    assert_eq!(
        upstream.fetches(),
        1,
        "the redelivery re-fetched the pricing document"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
    upstream.shutdown().await;
}

/// A failed run raises the notification the pane exists for — once, with
/// the task and the run id in its detail — and the outcome event records
/// the failure beside it.
#[tokio::test]
async fn a_failed_maintenance_run_notifies_an_operator() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");
    let mut signals = bus
        .subscribe(subjects::SYSTEM_OPERATOR_SIGNAL.to_string())
        .await
        .expect("subscribe to operator signals");

    // A consumer with no pricing refresh wired: the task is known, so
    // this is a *failure*, not a refusal.
    let subject = MaintenanceTask::PricingRefresh.subject();
    let (shutdown, handle, _name) = spawn(
        &bus,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish_command(
        &bus,
        &subject,
        Some("fq-cron/pricing-refresh@2026-09-14T18:00:00Z"),
    )
    .await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(10)).await;
    let MaintenanceOutcome::Failed { error } = outcome_of(&event) else {
        panic!("expected a failure, got {:?}", outcome_of(&event));
    };
    assert!(error.contains("no pricing refresh"), "{error}");

    let raised = drain_signals(&mut signals, Duration::from_secs(3)).await;
    assert_eq!(
        raised.len(),
        1,
        "one notification per failed run: {raised:?}"
    );
    assert_eq!(raised[0].kind.as_str(), "maintenance.run_failed");
    assert_eq!(raised[0].severity.as_str(), "notification");
    assert_eq!(
        raised[0].detail.get("task").and_then(Value::as_str),
        Some("pricing_refresh")
    );
    assert_eq!(
        raised[0].detail.get("run_id").and_then(Value::as_str),
        Some("fq-cron/pricing-refresh@2026-09-14T18:00:00Z")
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A refusal raises nothing: a task name this build does not know is a
/// `fq-cron.toml` error, and its owner reads the refusal on the log.
#[tokio::test]
async fn a_refused_task_raises_no_notification() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");

    let mut outcomes = bus
        .subscribe(subjects::SYSTEM_MAINTENANCE.to_string())
        .await
        .expect("subscribe to maintenance outcomes");
    let mut signals = bus
        .subscribe(subjects::SYSTEM_OPERATOR_SIGNAL.to_string())
        .await
        .expect("subscribe to operator signals");

    let subject = subjects::maintenance("pricing-refresh-typo");
    let (shutdown, handle, _name) = spawn(
        &bus,
        subject.clone(),
        Duration::from_secs(30),
        Duration::ZERO,
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    publish_command(&bus, &subject, None).await;

    let event = next_outcome(&mut outcomes, Duration::from_secs(10)).await;
    assert!(matches!(
        outcome_of(&event),
        MaintenanceOutcome::Refused { .. }
    ));
    assert!(
        drain_signals(&mut signals, Duration::from_secs(2))
            .await
            .is_empty()
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}
