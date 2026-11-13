//! The trigger-claim window: the cutoff on its own, then the sweeper
//! against a real control-plane store, one test per rule of
//! [the window](super#the-trigger-claim-window).
//!
//! Every row is seeded through the dispatcher's own writes —
//! `claim_trigger` with a backdated clock, then `mark_trigger_started`
//! for a started row — so the rows are the shape the dispatcher leaves.
//! Each names an invocation id of its own, which is how a test asks
//! whether the row is still there without writing a claim to find out.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::oneshot;

use super::{MS_PER_DAY, RetentionSweeper, trigger_claim_cutoff_ms};
use crate::config::StateConfig;
use crate::control_plane::store::{ControlPlaneStore, InvocationArchiveRow, TriggerKey};

/// The worker id the sweeping daemon's dispatcher claims under.
const THIS_DAEMON: &str = "daemon-now";
/// A daemon that ran before this one and has exited.
const EXITED: &str = "daemon-before";
const STREAM: &str = "fq-triggers";
/// The incarnation of the trigger stream the daemon consumes now.
const EPOCH: i64 = 1_767_323_045_000_000_000;
/// An incarnation the stream had before it was recreated.
const OLD_EPOCH: i64 = 1_767_000_000_000_000_000;
/// The window every test sweeps with, unless it says otherwise.
const WINDOW_DAYS: i64 = 30;

/// A claim older than the window, and one inside it, by an hour each
/// way: the window is the only thing between them.
const PAST_WINDOW_MS: i64 = WINDOW_DAYS * MS_PER_DAY + 3_600_000;
const INSIDE_WINDOW_MS: i64 = WINDOW_DAYS * MS_PER_DAY - 3_600_000;

/// `[state]` with only the trigger-claim window on.
fn claims_only(trigger_claim_retention_days: i64) -> StateConfig {
    StateConfig {
        retention_days: -1,
        stale_worker_retention_days: -1,
        trigger_claim_retention_days,
        sweep_interval_seconds: 3_600,
        ..Default::default()
    }
}

/// The sweeper the daemon builds: told whose claims are live.
fn sweeper(store: &Arc<ControlPlaneStore>, days: i64) -> RetentionSweeper {
    RetentionSweeper::new(store.clone(), &claims_only(days)).with_claimant(THIS_DAEMON)
}

async fn fresh_store() -> (Arc<ControlPlaneStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        ControlPlaneStore::open(&dir.path().join("cp.db"))
            .await
            .unwrap(),
    );
    (store, dir)
}

/// One claim row as the dispatcher would leave it.
struct Seed {
    seq: u64,
    epoch: i64,
    claimant: &'static str,
    age_ms: i64,
    started: bool,
}

impl Seed {
    fn started(seq: u64, age_ms: i64) -> Self {
        Self {
            seq,
            epoch: EPOCH,
            claimant: EXITED,
            age_ms,
            started: true,
        }
    }

    fn claimed(seq: u64, claimant: &'static str, age_ms: i64) -> Self {
        Self {
            seq,
            epoch: EPOCH,
            claimant,
            age_ms,
            started: false,
        }
    }

    fn on_old_stream(self) -> Self {
        Self {
            epoch: OLD_EPOCH,
            ..self
        }
    }

    /// The invocation id the row names: unique per row, so it is how the
    /// row is found again.
    fn invocation(&self) -> String {
        format!("inv-{}-{}", self.epoch, self.seq)
    }

    async fn write(&self, store: &ControlPlaneStore) {
        let key = TriggerKey {
            stream: STREAM,
            stream_epoch: self.epoch,
            stream_seq: self.seq,
        };
        let claimed_at = Utc::now().timestamp_millis() - self.age_ms;
        store
            .claim_trigger(key, self.claimant, &self.invocation(), claimed_at)
            .await
            .unwrap();
        if self.started {
            store
                .mark_trigger_started(key, &self.invocation())
                .await
                .unwrap();
        }
    }

    async fn kept(&self, store: &ControlPlaneStore) -> bool {
        store
            .trigger_claim_for_invocation(&self.invocation())
            .await
            .unwrap()
            .is_some()
    }
}

async fn seed_all(store: &ControlPlaneStore, seeds: &[&Seed]) {
    for seed in seeds {
        seed.write(store).await;
    }
}

// ---- The cutoff ----

/// The window counts back from now in whole days, and `< 0` is no
/// cutoff at all: the sweep that keeps rows for ever has nothing to
/// compare against.
#[test]
fn the_claim_cutoff_is_the_window_back_from_now_and_none_when_kept_for_ever() {
    let now = 1_000 * MS_PER_DAY;
    assert_eq!(
        trigger_claim_cutoff_ms(now, 30),
        Some(now - 30 * MS_PER_DAY)
    );
    assert_eq!(trigger_claim_cutoff_ms(now, 2), Some(now - 2 * MS_PER_DAY));
    assert_eq!(trigger_claim_cutoff_ms(now, 0), Some(now));
    assert_eq!(trigger_claim_cutoff_ms(now, -1), None);
    assert_eq!(trigger_claim_cutoff_ms(now, -30), None);
}

// ---- What goes ----

/// A started row is the dedupe record of its run, and past the window
/// its message is gone: it goes. One inside the window stays, because
/// its message may not be.
#[tokio::test]
async fn a_started_claim_past_the_window_is_swept_and_one_inside_it_is_kept() {
    let (store, _dir) = fresh_store().await;
    let old = Seed::started(1, PAST_WINDOW_MS);
    let recent = Seed::started(2, INSIDE_WINDOW_MS);
    seed_all(&store, &[&old, &recent]).await;

    sweeper(&store, WINDOW_DAYS).sweep_now().await.unwrap();

    assert!(!old.kept(&store).await, "a started claim past the window");
    assert!(
        recent.kept(&store).await,
        "a started claim inside the window"
    );
}

/// A `claimed` row an exited process left, past the window: nothing can
/// redeliver its message, so nothing can adopt it, and it goes.
#[tokio::test]
async fn an_abandoned_claim_past_the_window_is_swept() {
    let (store, _dir) = fresh_store().await;
    let abandoned = Seed::claimed(1, EXITED, PAST_WINDOW_MS);
    seed_all(&store, &[&abandoned]).await;

    sweeper(&store, WINDOW_DAYS).sweep_now().await.unwrap();

    assert!(!abandoned.kept(&store).await);
}

/// A row of an earlier incarnation of the stream is inert, and goes on
/// the same schedule as the rest: past the window, not before.
#[tokio::test]
async fn an_earlier_stream_incarnation_goes_on_the_same_schedule() {
    let (store, _dir) = fresh_store().await;
    let old_started = Seed::started(1, PAST_WINDOW_MS).on_old_stream();
    let recent_started = Seed::started(2, INSIDE_WINDOW_MS).on_old_stream();
    let old_abandoned = Seed::claimed(3, EXITED, PAST_WINDOW_MS).on_old_stream();
    let recent_abandoned = Seed::claimed(4, EXITED, INSIDE_WINDOW_MS).on_old_stream();
    seed_all(
        &store,
        &[
            &old_started,
            &recent_started,
            &old_abandoned,
            &recent_abandoned,
        ],
    )
    .await;

    sweeper(&store, WINDOW_DAYS).sweep_now().await.unwrap();

    assert!(!old_started.kept(&store).await);
    assert!(!old_abandoned.kept(&store).await);
    assert!(recent_started.kept(&store).await);
    assert!(recent_abandoned.kept(&store).await);
}

// ---- What stays ----

/// A `claimed` row the sweeping daemon holds is never deleted, however
/// old: it may be a trigger the daemon is still holding behind a paused
/// model or a full agent, and nothing on the row says whether it is.
#[tokio::test]
async fn a_claim_this_daemon_holds_is_kept_at_any_age() {
    let (store, _dir) = fresh_store().await;
    let past = Seed::claimed(1, THIS_DAEMON, PAST_WINDOW_MS);
    let ancient = Seed::claimed(2, THIS_DAEMON, 3_650 * MS_PER_DAY);
    seed_all(&store, &[&past, &ancient]).await;

    sweeper(&store, WINDOW_DAYS).sweep_now().await.unwrap();

    assert!(past.kept(&store).await);
    assert!(ancient.kept(&store).await);
}

/// Inside the window a `claimed` row is kept whoever holds it: its
/// message may still be delivered, and a delivery of it adopts the row.
#[tokio::test]
async fn a_claimed_row_inside_the_window_is_kept_whoever_holds_it() {
    let (store, _dir) = fresh_store().await;
    let exited = Seed::claimed(1, EXITED, INSIDE_WINDOW_MS);
    let this = Seed::claimed(2, THIS_DAEMON, INSIDE_WINDOW_MS);
    seed_all(&store, &[&exited, &this]).await;

    sweeper(&store, WINDOW_DAYS).sweep_now().await.unwrap();

    assert!(exited.kept(&store).await);
    assert!(this.kept(&store).await);
}

/// `-1` keeps claim rows for ever: nothing goes, whatever its age, its
/// state or its holder.
#[tokio::test]
async fn minus_one_sweeps_no_claim() {
    let (store, _dir) = fresh_store().await;
    let ancient = 3_650 * MS_PER_DAY;
    let started = Seed::started(1, ancient);
    let abandoned = Seed::claimed(2, EXITED, ancient);
    let held = Seed::claimed(3, THIS_DAEMON, ancient);
    let old_stream = Seed::started(4, ancient).on_old_stream();
    seed_all(&store, &[&started, &abandoned, &held, &old_stream]).await;

    sweeper(&store, -1).sweep_now().await.unwrap();

    for seed in [&started, &abandoned, &held, &old_stream] {
        assert!(seed.kept(&store).await, "seq {}", seed.seq);
    }
}

/// With every `[state]` default, as a daemon with no `[state]` section
/// sweeps, a claim more than a month old is kept, started or left by an
/// exited process: the default keeps the link from a run to its trigger
/// for ninety days, three times the archive's thirty. Past ninety days the
/// claim goes, so the default is a window and not a keep-for-ever.
#[tokio::test]
async fn at_defaults_a_claim_is_kept_past_thirty_days_and_swept_past_ninety() {
    let (store, _dir) = fresh_store().await;
    let hour = 3_600_000;
    let month_started = Seed::started(1, 31 * MS_PER_DAY);
    let month_abandoned = Seed::claimed(2, EXITED, 31 * MS_PER_DAY);
    let quarter_started = Seed::started(3, 90 * MS_PER_DAY - hour);
    let quarter_abandoned = Seed::claimed(4, EXITED, 90 * MS_PER_DAY - hour);
    let past_started = Seed::started(5, 90 * MS_PER_DAY + hour);
    let past_abandoned = Seed::claimed(6, EXITED, 90 * MS_PER_DAY + hour);
    let kept = [
        &month_started,
        &month_abandoned,
        &quarter_started,
        &quarter_abandoned,
    ];
    let swept = [&past_started, &past_abandoned];
    seed_all(&store, &kept).await;
    seed_all(&store, &swept).await;

    RetentionSweeper::new(store.clone(), &StateConfig::default())
        .with_claimant(THIS_DAEMON)
        .sweep_now()
        .await
        .unwrap();

    for seed in kept {
        assert!(
            seed.kept(&store).await,
            "seq {}: inside the default window",
            seed.seq
        );
    }
    for seed in swept {
        assert!(
            !seed.kept(&store).await,
            "seq {}: past the default window",
            seed.seq
        );
    }
}

/// A sweeper that has not been told its daemon's claimant cannot tell a
/// live `claimed` row from an abandoned one, so it deletes none — and
/// still sweeps started rows, which are never live.
#[tokio::test]
async fn a_sweeper_not_told_its_claimant_deletes_no_claimed_row() {
    let (store, _dir) = fresh_store().await;
    let exited = Seed::claimed(1, EXITED, PAST_WINDOW_MS);
    let this = Seed::claimed(2, THIS_DAEMON, PAST_WINDOW_MS);
    let started = Seed::started(3, PAST_WINDOW_MS);
    seed_all(&store, &[&exited, &this, &started]).await;

    RetentionSweeper::new(store.clone(), &claims_only(WINDOW_DAYS))
        .sweep_now()
        .await
        .unwrap();

    assert!(exited.kept(&store).await);
    assert!(this.kept(&store).await);
    assert!(!started.kept(&store).await);
}

/// The log lines a test's own code emits, captured in plain text.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

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
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// A window below the floor never reaches the sweep through the config,
/// which refuses it at load; a `StateConfig` built in code can still
/// carry one. The sweep then deletes nothing, however old the claim, and
/// says why with the window and the floor — every tick, since nothing
/// else will.
#[tokio::test]
async fn a_window_below_the_floor_sweeps_no_claim_and_says_so() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (store, _dir) = fresh_store().await;
    let started = Seed::started(1, 3 * MS_PER_DAY);
    let abandoned = Seed::claimed(2, EXITED, 3 * MS_PER_DAY);
    seed_all(&store, &[&started, &abandoned]).await;

    for days in [0, 1] {
        sweeper(&store, days).sweep_now().await.unwrap();
        assert!(started.kept(&store).await, "window {days}: started claim");
        assert!(
            abandoned.kept(&store).await,
            "window {days}: abandoned claim"
        );
        let log = captured.text();
        assert!(
            log.contains(&format!("trigger_claim_retention_days={days} floor_days=2")),
            "window {days}: the warning names the window and the floor:\n{log}"
        );
    }

    // At the floor the window is honoured.
    sweeper(&store, 2).sweep_now().await.unwrap();
    assert!(!started.kept(&store).await);
    assert!(!abandoned.kept(&store).await);
}

// ---- The task ----

/// The claim window keeps the task running on its own, with the other
/// two off: the early exit is for "every window disabled". Driven
/// through `run` on a one-second tick, so a task that took the disabled
/// path would never sweep and this would time out.
#[tokio::test]
async fn the_claim_window_alone_keeps_the_task_sweeping() {
    let (store, _dir) = fresh_store().await;
    let old = Seed::started(1, PAST_WINDOW_MS);
    seed_all(&store, &[&old]).await;
    // Aged far past any window, and kept: the archive window is off.
    store
        .insert_archive(&InvocationArchiveRow {
            invocation_id: "archived".to_string(),
            agent_id: "a".to_string(),
            final_phase: "completed".to_string(),
            final_state_blob: vec![],
            started_at: 0,
            terminal_at: 0,
            archived_at: 0,
        })
        .await
        .unwrap();

    let state = StateConfig {
        sweep_interval_seconds: 1,
        ..claims_only(WINDOW_DAYS)
    };
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(
        RetentionSweeper::new(store.clone(), &state)
            .with_claimant(THIS_DAEMON)
            .run(rx),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while old.kept(&store).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the task never swept the old claim"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the sweeper joins after shutdown")
        .expect("task panic");
    assert!(store.get_archive("archived").await.unwrap().is_some());
}
