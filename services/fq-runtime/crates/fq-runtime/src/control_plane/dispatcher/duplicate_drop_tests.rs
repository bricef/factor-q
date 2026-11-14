//! The two duplicates the claim refuses, and how each one is recorded
//! (#968).
//!
//! `claim_delivery` drops a redelivered trigger in two arms. A copy whose
//! claim records a durable start is a copy of a run that has started: it
//! is acked, logged and counted as `duplicate_dropped`. A copy whose claim
//! this daemon already holds with no start recorded is a copy of a trigger
//! still in hand: it is left unacked, logged and counted as
//! `held_duplicate_dropped`. A refused copy writes nothing anywhere else,
//! so the log line and the count are the only record of it, and each arm
//! must move its own count and not the other's.
//!
//! The held copy is made the way a slow start makes one. Nothing keeps a
//! delivery alive between its worker permit and its first WAL write, so a
//! start slower than the ack window is redelivered while its first copy is
//! still setting up. The first copy is parked in workspace provisioning by
//! the real `ReducerRunner` (the harness is `boundary_stop_tests`'s), and
//! the broker's redelivery is brought forward with a NAK on a clone of the
//! delivery.
//!
//! A third refusal is logged and counted in neither: a delivery whose
//! adoption of a dead process's claim finds the row changed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::jetstream::AckKind;
use futures::StreamExt;

use super::CONSUMER_NAME;
use super::boundary_stop_tests::{
    HeldSetup, StateDir, answers_to, next_delivery, one_answer, one_delivery, settled,
};
use super::tests::{ack_pending_stays, registry_with, test_key, unique_agent_id};
use crate::bus::EventBus;
use crate::control_plane::TriggerClaim;
use crate::test_support::nats::test_nats;

/// The start of the line the held arm logs.
const HELD_DROP: &str = "dropping a redelivered trigger whose claim this daemon already holds";
/// The start of the line the started arm logs.
const STARTED_DROP: &str = "dropping duplicate trigger delivery after durable start";
/// The clause the held arm adds on a trigger's last delivery.
const LAST_DELIVERY: &str = "this was its last delivery, so no copy follows";
/// The start of the line for a delivery whose adoption found the row
/// changed.
const ADOPTION_LOST: &str = "the claim it met changed before this daemon could adopt it";

/// The warn lines emitted on this thread while a capture is installed.
/// `durable_start_signal_tests` reads its error lines through it too.
#[derive(Clone, Default)]
pub(super) struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// Install the capture on this thread until the guard drops. A
    /// `#[tokio::test]` runtime is single-threaded, so `handle` and every
    /// task it awaits log here.
    pub(super) fn install(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    /// The captured lines that contain `needle`.
    pub(super) fn lines_with(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|line| line.contains(needle))
            .map(str::to_string)
            .collect()
    }
}

/// The two counts the consumer ledger keeps for the dispatcher, as
/// `(duplicate_dropped, held_duplicate_dropped)`.
fn drop_counts(bus: &EventBus) -> (u64, u64) {
    let record = bus.consumer_ledger().record(CONSUMER_NAME);
    (record.duplicate_dropped, record.held_duplicate_dropped)
}

/// **A redelivery that arrives while the first copy is still setting up
/// is dropped, logged once and counted as a held duplicate; the started
/// count does not move.**
///
/// The first copy claims the trigger and parks in its setup, before its
/// first WAL write. Its redelivery meets a claim this daemon holds with no
/// start on it, and must return without starting anything or answering the
/// delivery, with one warn line that says which delivery, which attempt,
/// whose claim, how old it is and which invocation it names. The first
/// copy then starts the trigger once, and its durable start acks it, which
/// is not a drop and moves neither count.
#[tokio::test]
async fn a_redelivery_during_the_first_copys_setup_is_logged_and_counted_as_held() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("redelivered-in-setup");
    let (_agents, registry) = registry_with(&agent);
    let state = StateDir::new().await;
    let setup = HeldSetup::new(0);
    let model = one_answer();
    let dispatcher = state
        .dispatcher(
            &bus,
            &registry,
            state.runner(&bus, "binary-1", Some(setup.clone())),
            model.clone(),
            "binary-1",
        )
        .await;
    let (mut consumer, msg) = one_delivery(&bus, &agent).await;
    let seq = msg.info().unwrap().stream_sequence;
    let copy = msg.clone();
    let first = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move { dispatcher.handle(&msg).await }
    });
    let named = setup.parked().await;

    // The broker's redelivery, brought forward from the end of the ack
    // window that nothing renews while the first copy sets up.
    copy.ack_with(AckKind::Nak(None))
        .await
        .expect("redeliver the trigger");
    let redelivered = next_delivery(&mut consumer).await;
    let info = redelivered.info().unwrap();
    assert_eq!(
        (info.stream_sequence, info.delivered),
        (seq, 2),
        "the same trigger, delivered a second time"
    );
    let (_listener, mut answers) = answers_to(server.url(), &redelivered).await;

    let log = Captured::default();
    {
        let _capture = log.install();
        tokio::time::timeout(Duration::from_secs(5), dispatcher.handle(&redelivered))
            .await
            .expect("the copy is refused, not parked behind the first");
    }

    assert_eq!(
        drop_counts(&bus),
        (0, 1),
        "the copy is counted as held, and not as a copy of a started run"
    );
    let held = log.lines_with(HELD_DROP);
    assert_eq!(held.len(), 1, "one line per refused copy: {held:#?}");
    for field in [
        format!("stream_seq={seq}"),
        "delivered=2".to_string(),
        format!("subject={}", crate::events::subjects::trigger(&agent)),
        "claimant=binary-1".to_string(),
        "claim_age_ms=".to_string(),
        format!("claim_invocation_id={named}"),
    ] {
        assert!(held[0].contains(&field), "no `{field}` in {}", held[0]);
    }
    assert!(
        !held[0].contains(LAST_DELIVERY),
        "a second delivery is not the last: {}",
        held[0]
    );
    assert!(
        log.lines_with(STARTED_DROP).is_empty(),
        "the held copy is not reported as a copy of a started run"
    );
    // The copy answers nothing: the delivery is the first copy's to
    // answer. A NAK with a delay would leave the delivery pending too, so
    // the pending count alone cannot show that; the copy's own ack
    // subject can, since every answer for it (`+ACK`, `-NAK`, `+WPI`,
    // `+TERM`) is published there.
    ack_pending_stays(&mut consumer, 1, Duration::from_millis(300)).await;
    if let Ok(Some(answer)) = tokio::time::timeout(Duration::from_millis(300), answers.next()).await
    {
        panic!(
            "the refused copy was answered with {:?}; it must leave the delivery to its first copy",
            String::from_utf8_lossy(&answer.payload)
        );
    }

    // The first copy starts the trigger once, and its durable start acks
    // it.
    setup.let_through();
    tokio::time::timeout(Duration::from_secs(20), first)
        .await
        .expect("the first copy runs to an end")
        .expect("task joins");
    settled(
        &mut consumer,
        "the first copy's durable start acks the trigger",
    )
    .await;
    assert_eq!(model.requests().len(), 1, "the trigger ran once");
    assert_eq!(
        state.claim(&bus, seq).await,
        TriggerClaim::Started {
            invocation_id: named.to_string()
        },
        "the claim records the first copy's start, under the id it named"
    );
    assert_eq!(
        drop_counts(&bus),
        (0, 1),
        "the first copy's own settlement is not a drop"
    );
}

/// **The held drop of a trigger's last delivery says it was the last.**
///
/// Past it no copy comes, and the trigger is dead-lettered unless the
/// delivery is acked, whatever the claim is waiting on. The first copy
/// parks in its setup; the broker's redeliveries are brought forward one
/// after another to the bound, and the last one is handed to `handle`.
/// The first copy then starts and acks the trigger, which the line did
/// not rule out.
#[tokio::test]
async fn the_held_drop_of_a_last_delivery_says_it_was_the_last() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("last-delivery-in-setup");
    let (_agents, registry) = registry_with(&agent);
    let state = StateDir::new().await;
    let setup = HeldSetup::new(0);
    let model = one_answer();
    let dispatcher = state
        .dispatcher(
            &bus,
            &registry,
            state.runner(&bus, "binary-1", Some(setup.clone())),
            model.clone(),
            "binary-1",
        )
        .await;
    let (mut consumer, msg) = one_delivery(&bus, &agent).await;
    let mut copy = msg.clone();
    let first = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move { dispatcher.handle(&msg).await }
    });
    setup.parked().await;

    let bound = crate::bus::TRIGGER_MAX_DELIVER;
    for delivery in 2..=bound {
        copy.ack_with(AckKind::Nak(None))
            .await
            .expect("redeliver the trigger");
        copy = next_delivery(&mut consumer).await;
        assert_eq!(copy.info().unwrap().delivered, delivery);
    }

    let log = Captured::default();
    {
        let _capture = log.install();
        tokio::time::timeout(Duration::from_secs(5), dispatcher.handle(&copy))
            .await
            .expect("the copy is refused, not parked behind the first");
    }
    let held = log.lines_with(HELD_DROP);
    assert_eq!(held.len(), 1, "one line per refused copy: {held:#?}");
    assert!(
        held[0].contains(&format!("delivered={bound}")) && held[0].contains(LAST_DELIVERY),
        "the last delivery's drop says it was the last: {}",
        held[0]
    );
    assert_eq!(drop_counts(&bus), (0, 1));

    setup.let_through();
    tokio::time::timeout(Duration::from_secs(20), first)
        .await
        .expect("the first copy runs to an end")
        .expect("task joins");
    settled(
        &mut consumer,
        "the first copy's durable start acks the trigger",
    )
    .await;
    assert_eq!(model.requests().len(), 1, "the trigger ran once");
}

/// **A delivery whose adoption loses to a changed row is logged, left
/// unanswered, and counted in neither count.**
///
/// The adoption is a compare-and-set on the row as it was read: still
/// `claimed`, by the claimant the read found. If the row changed in
/// between, the set changes nothing and the delivery is dropped. One
/// daemon has nothing that changes a dead process's row in that gap, so
/// the test stands in for the change: a SQLite trigger on the claim table
/// makes every update skip its row, which is what the set sees when the
/// row is no longer the one it read.
#[tokio::test]
async fn a_delivery_whose_adoption_loses_to_a_changed_row_is_logged() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("adoption-lost");
    let (_agents, registry) = registry_with(&agent);
    let state = StateDir::new().await;
    let model = one_answer();
    let dispatcher = state
        .dispatcher(
            &bus,
            &registry,
            state.runner(&bus, "binary-2", None),
            model.clone(),
            "binary-2",
        )
        .await;
    let (mut consumer, msg) = one_delivery(&bus, &agent).await;
    let seq = msg.info().unwrap().stream_sequence;
    let (claims, claim_db) = state.claim_store();
    let left_behind = uuid::Uuid::now_v7();
    assert_eq!(
        claims
            .claim_trigger(test_key(&bus, seq), "binary-1", &left_behind.to_string(), 0)
            .await
            .unwrap(),
        TriggerClaim::Won,
        "a process that has exited left the claim"
    );
    skip_every_claim_update(&claim_db).await;
    let (_listener, mut answers) = answers_to(server.url(), &msg).await;

    let log = Captured::default();
    {
        let _capture = log.install();
        tokio::time::timeout(Duration::from_secs(5), dispatcher.handle(&msg))
            .await
            .expect("the delivery is dropped");
    }

    assert!(model.requests().is_empty(), "nothing ran");
    assert_eq!(drop_counts(&bus), (0, 0), "a lost adoption is not counted");
    let lost = log.lines_with(ADOPTION_LOST);
    assert_eq!(
        lost.len(),
        1,
        "one line for the dropped delivery: {lost:#?}"
    );
    for field in [
        format!("stream_seq={seq}"),
        "delivered=1".to_string(),
        format!("subject={}", crate::events::subjects::trigger(&agent)),
        "claimant=binary-1".to_string(),
        "claim_age_ms=".to_string(),
        format!("claim_invocation_id={left_behind}"),
    ] {
        assert!(lost[0].contains(&field), "no `{field}` in {}", lost[0]);
    }
    ack_pending_stays(&mut consumer, 1, Duration::from_millis(300)).await;
    if let Ok(Some(answer)) = tokio::time::timeout(Duration::from_millis(300), answers.next()).await
    {
        panic!(
            "the dropped delivery was answered with {:?}",
            String::from_utf8_lossy(&answer.payload)
        );
    }
}

/// Make every update of a claim row skip its row from now on, through a
/// connection of its own to the store's file: the update succeeds and
/// changes nothing.
async fn skip_every_claim_update(claim_db: &std::path::Path) {
    use sqlx::{ConnectOptions, Connection};
    let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    sqlx::query(
        "CREATE TRIGGER skip_claim_updates BEFORE UPDATE ON trigger_claim \
         BEGIN SELECT RAISE(IGNORE); END",
    )
    .execute(&mut conn)
    .await
    .expect("install the skip");
    conn.close().await.expect("close the claim store's file");
}

/// **A copy that arrives after the durable start is counted as a copy of a
/// started run, and the held count does not move.**
///
/// The trigger runs and its claim is marked started. The same delivery,
/// handed to `handle` again, meets that claim: it is acked and dropped
/// with the started arm's line, and counted there only.
#[tokio::test]
async fn a_copy_after_the_durable_start_is_counted_as_started_not_held() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("copy-after-start");
    let (_agents, registry) = registry_with(&agent);
    let state = StateDir::new().await;
    let model = one_answer();
    let dispatcher = state
        .dispatcher(
            &bus,
            &registry,
            state.runner(&bus, "binary-1", None),
            model.clone(),
            "binary-1",
        )
        .await;
    let (mut consumer, msg) = one_delivery(&bus, &agent).await;
    let seq = msg.info().unwrap().stream_sequence;
    let copy = msg.clone();
    tokio::time::timeout(Duration::from_secs(20), dispatcher.handle(&msg))
        .await
        .expect("the trigger runs to an end");
    settled(&mut consumer, "the durable start acks the trigger").await;
    assert!(
        matches!(state.claim(&bus, seq).await, TriggerClaim::Started { .. }),
        "the claim records the durable start"
    );

    let log = Captured::default();
    {
        let _capture = log.install();
        tokio::time::timeout(Duration::from_secs(5), dispatcher.handle(&copy))
            .await
            .expect("the copy is answered");
    }

    assert_eq!(
        drop_counts(&bus),
        (1, 0),
        "the copy is counted as a copy of a started run, and not as held"
    );
    assert_eq!(
        log.lines_with(STARTED_DROP).len(),
        1,
        "one line for the refused copy"
    );
    assert!(
        log.lines_with(HELD_DROP).is_empty(),
        "a copy of a started run is not reported as held"
    );
    assert_eq!(model.requests().len(), 1, "the copy started nothing");
}
