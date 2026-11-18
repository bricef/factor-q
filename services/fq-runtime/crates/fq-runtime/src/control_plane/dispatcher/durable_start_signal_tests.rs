//! `handle` waits on two things at once: the durable-start signal and
//! the invocation itself, in a `biased` loop that polls the signal
//! first. Two ways that can go wrong are pinned here, and what a refused
//! claim mark does once the signal has fired.
//!
//! **A spent signal polled again (#987).** The signal is a oneshot
//! receiver, and polling one again after it has resolved panics with
//! "called after complete", so once the signal has resolved, whatever it
//! resolved to, `handle` must stop polling it. These tests resolve the
//! signal and then keep the invocation running for one more turn of
//! `handle`'s loop, by yielding once before it returns ([`Turn::Later`]).
//! That turn is the one on which the spent receiver used to be polled
//! again.
//!
//! **A signal fired in the poll that ends the invocation (#966).** An
//! invocation that fires the signal and returns without an await in
//! between ends on a turn whose signal poll had already come back empty,
//! so the loop ends on the result without having seen the signal. These
//! tests fire and return in one poll ([`Turn::Same`]), and `handle` must
//! still treat the run as durably started.
//!
//! **A refused mark (#967).** The run has made its first WAL write, so it
//! is never stopped and never redelivered: a mark the claim store refuses
//! is tried again, and one refused on every attempt is answered by acking
//! the delivery without releasing its claim. The claim store is made to
//! refuse marks through a SQLite trigger of the test's own
//! ([`refuse_claim_marks`]), for a set number of attempts or for good.
//! Such a refusal comes back at once, so one test also makes an attempt
//! block, on a write lock another connection holds ([`lock_claim_store`]),
//! which is what bounds the retries by time and what the delivery has to
//! be kept alive through. One test follows a refused mark through a
//! restart to a replayed copy of its trigger, where startup recovery's
//! marking of finished runs takes over.
//!
//! **A mark that finds its claim changed (#1046).** The stub changes the
//! claim row through a connection of its own just before the signal
//! fires ([`change_the_claim`]): it deletes it, or names another
//! invocation on it, claimed or started. The mark then matches no claim
//! naming its run, and these tests pin what the claim and the delivery
//! end as: a claim recording a start, which refuses a copy that comes
//! back, and one ack that releases nothing.
//!
//! **A long setup (#967).** From the worker permit to the durable start a
//! run provisions its workspace, starts its per-invocation MCP servers
//! and publishes `triggered`, and that can take longer than the
//! delivery's ack window. These tests give the stub a setup longer than a
//! short window ([`Setup`]) and hand any copy the broker redelivers
//! meanwhile to `handle` as well, as the consume loop would
//! ([`Pulled::handle_beside_copies`]): the delivery must be kept alive
//! through setup, and the keepalive must end where the setup does.
//!
//! **Settling (#1057).** A delivery's answer goes out only after its claim
//! is released, and on a last delivery after its dead letter is
//! published; the delivery is kept in progress through all of that. Tests
//! hold the release up past the ack window
//! ([`Pulled::setting_up_with_a_held_release`]), make the store refuse it,
//! cut it off at its bound, and make the dead letter's publish slow or time
//! out. Each checks that the delivery was renewed until its answer and
//! answered once, and that no copy came while it settled: by reading the
//! pull stream, where a copy would arrive, or, on a last delivery, which
//! no copy follows, by watching for the broker's max-deliveries advisory
//! instead. A last delivery whose dead letter was not published is NAKed
//! with no delay rather than acked, and then the advisory has to come
//! (issue 1062); [`unpublished_dead_letter`] follows that trigger to the
//! dead-letter watch.
//!
//! **Bounded (#1048).** Copies are driven beside the first copy, as the
//! consume loop's tasks are, never awaited inside its turn: a copy waiting
//! for the worker permit the first copy holds would otherwise stop the
//! first copy from ever finishing. Each wait that could last has a
//! deadline whose message names what was still pending, and every test
//! that drives copies runs under [`TEST_DEADLINE`] as a backstop.
//!
//! The tests run the real `handle` against a real broker and a real
//! control-plane store, with a stub worker. They pin what was decided,
//! from the claim, the refusals spent and the answers on the wire. The
//! lines a refused mark and a failed ack log are read in
//! `tests/dispatcher_log_lines.rs`, under one subscriber for the whole
//! process: a capture on a test's own thread could miss a line whose
//! callsite another test had reached first (#1031).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::jetstream::Message;
use async_nats::jetstream::consumer::{AckPolicy, PullConsumer, pull};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::json;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use uuid::Uuid;

use super::claim::{MARK_RETRY_BUDGET, MARK_RETRY_PAUSES, Refused, offer_with_retries};
use super::tests::{
    ack_pending_stays, canned_response, recovered_claims, registry_with, test_key, unique_agent_id,
    unique_consumer_name,
};
use super::{CONSUMER_NAME, TriggerDispatcher};
use crate::agent::{Agent, AgentId};
use crate::bus::{
    EventBus, NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_MAX_DELIVER, TRIGGER_RETRY_BACKOFF,
    TRIGGER_STREAM_NAME,
};
use crate::control_plane::{ControlPlaneStore, OwnerStatus, TriggerClaim};
use crate::llm::LlmClient;
use crate::llm::fixture::FixtureClient;
use crate::test_support::nats::{NatsServer, test_nats};
use crate::trigger::{Trigger, agent_id_from_subject};
use crate::worker::{
    BoundaryStop, DrainRequest, DrainState, DurableStart, ExecutorError, InvocationOutcome, Worker,
};

mod unpublished_dead_letter;

/// An ack window shorter than the pauses between a refused mark's
/// attempts ([`MARK_RETRY_PAUSES`], 1.5 seconds in all), for a consumer
/// of the test's own: a delivery that nothing kept alive while its mark
/// was retried would be redelivered underneath it. The production window
/// is 30 seconds (`TRIGGER_RETRY_BACKOFF[0]`).
const SHORT_ACK_WINDOW: Duration = Duration::from_secs(1);

/// How many attempts to mark a claim a store that refuses at once gets:
/// one, then one after each of [`MARK_RETRY_PAUSES`].
const ATTEMPTS_IF_REFUSED_AT_ONCE: u32 = MARK_RETRY_PAUSES.len() as u32 + 1;

/// How long another connection holds the claim store's write lock in
/// [`Resolve::FireIntoLockedStore`]: longer than the retries are allowed,
/// so the attempt that waits it out is the last one offered, and shorter
/// than the store's busy timeout (sqlx's default, five seconds), so that
/// attempt gets through the lock rather than failing on it.
const LOCK_HELD: Duration = Duration::from_millis(2500);

/// A setup longer than [`SHORT_ACK_WINDOW`] by more than a whole window:
/// a delivery nothing renews while it runs is redelivered at least once
/// underneath it.
const LONG_SETUP: Duration = Duration::from_millis(2500);

/// The backstop on a test that drives copies of a delivery
/// ([`within_the_test_deadline`]). Such a test waits on the broker, the
/// claim store and the dispatcher's tasks, and before issue 1048 one of
/// them could wait for ever. Longer than every deadline below, so one of
/// those, which says what it was waiting on, fails first.
const TEST_DEADLINE: Duration = Duration::from_secs(90);

/// How long [`Pulled::handle_beside_copies`] may take. The longest drive
/// is the held-up release's: a [`LONG_SETUP`], the release held for
/// [`RELEASE_HELD_FOR`], then the copy's own setup, about eight seconds;
/// five times that leaves room for a loaded machine.
const DRIVE_DEADLINE: Duration = Duration::from_secs(40);

/// How long [`Answers::sent`] keeps reading while answers keep arriving. A
/// delivery that is still being renewed this long after its test's drive
/// has ended is still in someone's hands, which is a failure to report
/// rather than a reason to keep reading.
const ANSWERS_DEADLINE: Duration = Duration::from_secs(10);

/// What the stub invocation does with its durable-start signal.
enum Resolve {
    /// Drop it unfired. `run_loop_for` does this when `run_loop_inner`
    /// returns before the first WAL write, and then still awaits the MCP
    /// shutdown and the workspace reclaim before its future finishes.
    Drop,
    /// Fire it: the first WAL write has landed.
    Fire,
    /// Make the claim store refuse marks as the [`Refusals`] say, then
    /// fire it: the first WAL write has landed and the claim's update is
    /// refused.
    FireIntoRefusedMarks(Refusals),
    /// Make the claim store refuse marks, one refusal for each attempt a
    /// store that refuses at once would get, and hold its write lock from
    /// another connection for this long; then fire it. The first attempt
    /// to mark the claim blocks on the lock and is refused once it gets
    /// through.
    FireIntoLockedStore(Duration),
    /// Change the claim row as the [`ClaimChange`] says, then fire it: the
    /// first WAL write has landed, and the mark meets a claim that no
    /// longer names the run.
    FireOnAChangedClaim(ClaimChange),
}

/// How [`Resolve::FireOnAChangedClaim`] changes the claim before the mark.
#[derive(Clone, Copy)]
enum ClaimChange {
    /// Delete it, as a release an earlier copy gave up on deletes it when
    /// it lands after this copy took the claim again (issue 1061).
    Gone,
    /// Delete it, and make the claim store refuse every insert and update
    /// of a claim row from then on.
    GoneAndRefused,
    /// Name [`ANOTHER_RUN`] on it, still `claimed`: another copy's claim.
    ClaimedByAnother,
    /// As [`Self::ClaimedByAnother`], and make the claim store refuse every
    /// update of a claim row from then on.
    ClaimedByAnotherAndRefused,
    /// Name [`ANOTHER_RUN`] on it, marked `durably_started`: another run's
    /// start.
    StartedByAnother,
}

/// The invocation a changed claim names ([`ClaimChange`]).
const ANOTHER_RUN: &str = "another-run";

/// Which attempts to mark the claim the store refuses.
#[derive(Clone, Copy)]
enum Refusals {
    /// Every one, for the rest of the test.
    Every,
    /// The first `n`, and none after: a store that heals.
    First(u32),
}

/// What the stub invocation does before it resolves its signal: the
/// workspace, the per-invocation MCP servers and the `triggered` publish
/// a real run does between its worker permit and its first WAL write.
#[derive(Clone, Copy)]
enum Setup {
    /// Nothing: it resolves the signal at once.
    Instant,
    /// Something that takes this long.
    Takes(Duration),
    /// Something that takes `takes`, with the worker draining from
    /// `drain_at` into it.
    DrainedPartWay { takes: Duration, drain_at: Duration },
}

/// On which turn of `handle`'s loop the stub invocation returns.
enum Turn {
    /// A later one than the turn that resolved the signal: the stub
    /// yields once before it returns, so `handle` sees the signal resolve
    /// on one turn of its loop and the invocation finish on a later one.
    Later,
    /// The same one: the stub returns in the poll that resolved the
    /// signal, with no await in between. The loop polled the signal
    /// before it polled the invocation, so that turn ends on the result
    /// with the signal unseen (#966).
    Same,
    /// This long after the turn that resolved the signal: a run that goes
    /// on after its durable start, or one that tears down its workspace
    /// and MCP servers after dropping the signal unfired.
    After(Duration),
}

/// What the stub invocation returns.
enum Returns {
    /// A transient error, which `handle` NAKs when no durable start was
    /// observed.
    TransientError,
    /// A completed run.
    Completed,
    /// A run a drain stopped at its first step boundary, before its first
    /// WAL write: the trigger is left for the next binary.
    Drained,
}

/// An invocation that resolves its durable-start signal and then
/// returns, on the turn of `handle`'s loop that its [`Turn`] says.
struct ResolvesThenReturns {
    setup: Setup,
    resolve: Resolve,
    turn: Turn,
    returns: Returns,
    /// The claim store's file, for [`Resolve::FireIntoRefusedMarks`] and
    /// [`Resolve::FireIntoLockedStore`].
    claim_db: PathBuf,
    ran_to_end: AtomicBool,
    /// The trigger's id and the invocation id its signal carries, as the
    /// dispatcher handed them over.
    named: Mutex<Option<(Uuid, Uuid)>>,
    /// How many times an invocation was started.
    starts: AtomicUsize,
    /// Whether the worker reports itself draining, and since when.
    draining: AtomicBool,
    drained_at: Mutex<Option<tokio::time::Instant>>,
    /// For how long the first run holds the claim store's write lock as it
    /// returns, holding up its claim's release, if it does
    /// ([`Pulled::setting_up_with_a_held_release`]).
    release_held_for: Option<Duration>,
    /// When each run returned, in order.
    run_returns: Mutex<Vec<tokio::time::Instant>>,
}

#[async_trait::async_trait]
impl Worker for ResolvesThenReturns {
    async fn run_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        trigger: Trigger,
        _delivery_attempt: Option<u32>,
        mut durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let invocation_id = durable_start
            .invocation_id()
            .expect("the dispatcher names the run it starts");
        *self.named.lock().unwrap() = Some((trigger.id, invocation_id));
        self.starts.fetch_add(1, Ordering::SeqCst);
        match self.setup {
            Setup::Instant => {}
            Setup::Takes(takes) => tokio::time::sleep(takes).await,
            Setup::DrainedPartWay { takes, drain_at } => {
                tokio::time::sleep(drain_at).await;
                *self.drained_at.lock().unwrap() = Some(tokio::time::Instant::now());
                self.draining.store(true, Ordering::SeqCst);
                tokio::time::sleep(takes.saturating_sub(drain_at)).await;
            }
        }
        match self.resolve {
            Resolve::Drop => drop(durable_start),
            Resolve::Fire => durable_start.fire(),
            Resolve::FireIntoRefusedMarks(refusals) => {
                refuse_claim_marks(&self.claim_db, refusals).await;
                durable_start.fire();
            }
            Resolve::FireIntoLockedStore(held_for) => {
                let refusals = Refusals::First(ATTEMPTS_IF_REFUSED_AT_ONCE);
                refuse_claim_marks(&self.claim_db, refusals).await;
                lock_claim_store(&self.claim_db, held_for).await;
                durable_start.fire();
            }
            Resolve::FireOnAChangedClaim(change) => {
                change_the_claim(&self.claim_db, change).await;
                durable_start.fire();
            }
        }
        match self.turn {
            Turn::Later => tokio::task::yield_now().await,
            Turn::Same => {}
            Turn::After(runs_on) => tokio::time::sleep(runs_on).await,
        }
        self.ran_to_end.store(true, Ordering::SeqCst);
        self.run_returns
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        if let Some(held_for) = self.release_held_for
            && self.starts.load(Ordering::SeqCst) == 1
        {
            lock_claim_store(&self.claim_db, held_for).await;
        }
        match self.returns {
            Returns::TransientError => Err(ExecutorError::WorkerStore(
                "simulated transient store outage".to_string(),
            )),
            Returns::Completed => Ok(InvocationOutcome::Completed {
                invocation_id: Uuid::now_v7(),
                response: canned_response(),
                cost: 0.0,
                duration_ms: 0,
            }),
            Returns::Drained => Ok(InvocationOutcome::Unstarted {
                invocation_id,
                stopped_by: BoundaryStop::Drain,
            }),
        }
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> DrainState {
        if self.draining.load(Ordering::SeqCst) {
            DrainState::Draining
        } else {
            DrainState::Running
        }
    }
}

/// Make updates of a claim row fail from now on, as `refusals` says,
/// through a connection of its own to the store's file. The claim this
/// delivery already holds was an insert and a release is a delete, so the
/// mark is the one write on this path the refusal meets.
///
/// A counted refusal spends one row of `injected_mark_refusals` per
/// refused update. It raises `FAIL` rather than `ABORT` because `FAIL`
/// keeps what the trigger did before it, the spent row, where `ABORT`
/// would roll that back with the refused update and refuse for ever.
async fn refuse_claim_marks(claim_db: &Path, refusals: Refusals) {
    let mut statements: Vec<&'static str> = Vec::new();
    match refusals {
        Refusals::Every => statements.push(
            "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
             BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the mark'); END",
        ),
        Refusals::First(n) => {
            statements.push("CREATE TABLE injected_mark_refusals (spent INTEGER)");
            statements.extend(std::iter::repeat_n(
                "INSERT INTO injected_mark_refusals VALUES (0)",
                n as usize,
            ));
            statements.push(
                "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
                 WHEN EXISTS (SELECT 1 FROM injected_mark_refusals) \
                 BEGIN DELETE FROM injected_mark_refusals \
                 WHERE rowid = (SELECT min(rowid) FROM injected_mark_refusals); \
                 SELECT RAISE(FAIL, 'injected: the claim store refuses the mark'); END",
            );
        }
    }
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    for statement in statements {
        sqlx::query(statement)
            .execute(&mut conn)
            .await
            .expect("install the refusal");
    }
    conn.close().await.expect("close the claim store's file");
}

/// Change the one claim row as `change` says, through a connection of its
/// own to the store's file.
async fn change_the_claim(claim_db: &Path, change: ClaimChange) {
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    let statements: Vec<sqlx::query::Query<'_, sqlx::Sqlite, _>> = match change {
        ClaimChange::Gone => vec![sqlx::query("DELETE FROM trigger_claim")],
        ClaimChange::GoneAndRefused => vec![
            sqlx::query("DELETE FROM trigger_claim"),
            sqlx::query(
                "CREATE TRIGGER refuse_claim_inserts BEFORE INSERT ON trigger_claim \
                 BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the write'); END",
            ),
            sqlx::query(
                "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
                 BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the mark'); END",
            ),
        ],
        ClaimChange::ClaimedByAnother => {
            vec![sqlx::query("UPDATE trigger_claim SET invocation_id = ?").bind(ANOTHER_RUN)]
        }
        ClaimChange::ClaimedByAnotherAndRefused => vec![
            sqlx::query("UPDATE trigger_claim SET invocation_id = ?").bind(ANOTHER_RUN),
            sqlx::query(
                "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
                 BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the mark'); END",
            ),
        ],
        ClaimChange::StartedByAnother => vec![
            sqlx::query("UPDATE trigger_claim SET invocation_id = ?, state = 'durably_started'")
                .bind(ANOTHER_RUN),
        ],
    };
    for statement in statements {
        statement
            .execute(&mut conn)
            .await
            .expect("change the claim");
    }
    conn.close().await.expect("close the claim store's file");
}

/// Take the claim store's write lock through a connection of its own and
/// hold it for `held_for`, from a task of its own. A write that needs the
/// lock meanwhile waits in the store's busy handler rather than failing.
async fn lock_claim_store(claim_db: &Path, held_for: Duration) {
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut conn)
        .await
        .expect("take the claim store's write lock");
    tokio::spawn(async move {
        tokio::time::sleep(held_for).await;
        sqlx::query("COMMIT")
            .execute(&mut conn)
            .await
            .expect("let the write lock go");
        conn.close().await.expect("close the claim store's file");
    });
}

/// How long [`Pulled::setting_up_with_a_held_release`] holds the claim
/// store's write lock from the run's return: three ack windows
/// ([`SHORT_ACK_WINDOW`]), so a delivery nothing renewed would lapse while
/// the release waits, with about two seconds to spare, and under the
/// store's busy timeout (sqlx's five seconds), so the dispatcher's `DELETE`
/// waits it out rather than failing on it.
const RELEASE_HELD_FOR: Duration = Duration::from_secs(3);

/// The claim store's busy timeout: sqlx's default, which
/// `ControlPlaneStore::open` does not change. A write that waits this long
/// for the lock fails.
const STORE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Watch for the first run's claim release to land, for a held-up
/// release ([`Pulled::setting_up_with_a_held_release`]): from the first
/// run's return, read the claim row until it is gone, and note when.
async fn watch_the_release(
    claims: &ControlPlaneStore,
    key: crate::control_plane::TriggerKey<'static>,
    run_returns: &Mutex<Vec<tokio::time::Instant>>,
    landed_at: &Mutex<Option<tokio::time::Instant>>,
) {
    loop {
        let returned = !run_returns.lock().unwrap().is_empty();
        if returned
            && claims
                .unsettled_trigger_claim(key)
                .await
                .expect("read the claim")
                .is_none()
        {
            *landed_at.lock().unwrap() = Some(tokio::time::Instant::now());
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The gate [`Pulled::handle_beside_copies`] holds a copy at, for a
/// held-up release: until [`watch_the_release`] has seen the dispatcher's
/// own release land. The daemon has no such gate; the constructor
/// ([`Pulled::setting_up_with_a_held_release`]) says why the test does.
async fn wait_for_the_release(landed_at: &Mutex<Option<tokio::time::Instant>>) {
    while landed_at.lock().unwrap().is_none() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Let the claim store take marks again: drop the refusal
/// [`refuse_claim_marks`] installed.
async fn heal_claim_marks(claim_db: &Path) {
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    sqlx::query("DROP TRIGGER refuse_claim_marks")
        .execute(&mut conn)
        .await
        .expect("drop the refusal");
    conn.close().await.expect("close the claim store's file");
}

/// How many of a [`Refusals::First`] count's refusals are still unspent.
async fn refusals_left(claim_db: &Path) -> i64 {
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    let (left,): (i64,) = sqlx::query_as("SELECT count(*) FROM injected_mark_refusals")
        .fetch_one(&mut conn)
        .await
        .expect("count the refusals left");
    conn.close().await.expect("close the claim store's file");
    left
}

/// One trigger pulled off a real broker and not yet handled, and a
/// dispatcher that arbitrates it through a real claim store as
/// `worker-a` and hands it to a [`ResolvesThenReturns`].
struct Pulled {
    bus: EventBus,
    dispatcher: TriggerDispatcher,
    worker: Arc<ResolvesThenReturns>,
    /// When the delivery was pulled, which the timelines in failure
    /// messages count from.
    pulled_at: tokio::time::Instant,
    /// When the first run's claim release landed, as
    /// [`Self::handle_beside_copies`] saw it, for a held-up release.
    release_landed_at: Mutex<Option<tokio::time::Instant>>,
    claims: Arc<ControlPlaneStore>,
    claim_db: PathBuf,
    url: String,
    consumer: PullConsumer,
    messages: pull::Stream,
    msg: Message,
    _held: (tempfile::TempDir, tempfile::TempDir, NatsServer),
}

impl Pulled {
    /// On a consumer configured as the dispatcher's own is, with the
    /// production ack window.
    async fn new(resolve: Resolve, turn: Turn, returns: Returns) -> Self {
        Self::on(Setup::Instant, resolve, turn, returns, None, None).await
    }

    /// With a setup before the signal resolves, on a consumer of the
    /// test's own whose ack window is [`SHORT_ACK_WINDOW`].
    async fn setting_up(setup: Setup, resolve: Resolve, turn: Turn, returns: Returns) -> Self {
        Self::on(setup, resolve, turn, returns, Some(SHORT_ACK_WINDOW), None).await
    }

    /// As [`Self::setting_up`], with the first run's claim release held
    /// up past the ack window (issue 1057). Before issue 1057 a copy of the
    /// delivery arrived while the release waited, and claimed once it had
    /// landed; now the delivery is kept alive until its answer, and none
    /// should.
    ///
    /// **The real interleaving.** As the first run returns, a connection of
    /// the test's own takes the claim store's write lock for
    /// [`RELEASE_HELD_FOR`] ([`lock_claim_store`]). The dispatcher's own
    /// release of the claim (`release_unstarted_claim`, a `DELETE`) waits
    /// behind it and then lands, and the NAK or ack goes out only after the
    /// release. Before issue 1057 nothing renewed the delivery while it
    /// waited, so once the ack window lapsed the broker redelivered.
    ///
    /// **The gate, which the daemon does not have.** A copy's claim insert
    /// would wait behind the same lock and then race the dispatcher's
    /// `DELETE` for it: when the insert won, the copy met the first copy's
    /// claim and was dropped as held, and when the `DELETE` won, it took
    /// the freed claim. So [`Self::handle_beside_copies`] holds a copy that
    /// arrived after the first run returned until the claim row is gone
    /// ([`wait_for_the_release`]), and only then hands it to `handle`. The
    /// daemon hands every copy to `handle` at once; the gate picks one of
    /// the two outcomes the daemon can reach, the one issue 1057 is about,
    /// and changes nothing the dispatcher does. A copy that arrives before
    /// the first run returns is not held.
    ///
    /// **Since issue 1057** the delivery is kept alive until its answer, so
    /// no copy arrives while the release is held up and the gate holds
    /// nothing; the tests that use this assert one run and no copy, and
    /// the gate is what makes a regression show as a second run.
    async fn setting_up_with_a_held_release(
        setup: Setup,
        resolve: Resolve,
        turn: Turn,
        returns: Returns,
    ) -> Self {
        Self::setting_up_with_the_release_held_for(setup, resolve, turn, returns, RELEASE_HELD_FOR)
            .await
    }

    /// As [`Self::setting_up_with_a_held_release`], holding the store's
    /// lock for `held_for` instead.
    async fn setting_up_with_the_release_held_for(
        setup: Setup,
        resolve: Resolve,
        turn: Turn,
        returns: Returns,
        held_for: Duration,
    ) -> Self {
        let window = Some(SHORT_ACK_WINDOW);
        Self::on(setup, resolve, turn, returns, window, Some(held_for)).await
    }

    /// On a consumer of the test's own whose ack window is `ack_window`.
    async fn with_ack_window(
        resolve: Resolve,
        turn: Turn,
        returns: Returns,
        ack_window: Duration,
    ) -> Self {
        Self::on(
            Setup::Instant,
            resolve,
            turn,
            returns,
            Some(ack_window),
            None,
        )
        .await
    }

    async fn on(
        setup: Setup,
        resolve: Resolve,
        turn: Turn,
        returns: Returns,
        ack_window: Option<Duration>,
        release_held_for: Option<Duration>,
    ) -> Self {
        let server = test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let agent_id = unique_agent_id("durable-start-signal");
        let (agent_dir, registry) = registry_with(&agent_id);

        let claim_dir = tempfile::tempdir().unwrap();
        let claim_db = claim_dir.path().join("control-plane.db");
        let claims = Arc::new(ControlPlaneStore::open(&claim_db).await.unwrap());
        let worker = Arc::new(ResolvesThenReturns {
            setup,
            resolve,
            turn,
            returns,
            claim_db: claim_db.clone(),
            ran_to_end: AtomicBool::new(false),
            named: Mutex::new(None),
            starts: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            drained_at: Mutex::new(None),
            release_held_for,
            run_returns: Mutex::new(Vec::new()),
        });
        let dispatcher = TriggerDispatcher::new(
            bus.clone(),
            registry,
            worker.clone() as Arc<dyn Worker>,
            Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
            1,
        )
        .with_trigger_claims(recovered_claims(&claims).await, "worker-a");

        let filter = crate::events::subjects::trigger(&agent_id);
        let consumer = match ack_window {
            None => bus
                .trigger_consumer_with_filter(
                    &unique_consumer_name(),
                    &filter,
                    NATS_DEFAULT_MAX_ACK_PENDING,
                )
                .await
                .expect("consumer"),
            Some(ack_wait) => bus
                .jetstream()
                .get_stream(TRIGGER_STREAM_NAME)
                .await
                .expect("the trigger stream")
                .create_consumer(pull::Config {
                    durable_name: Some(unique_consumer_name()),
                    filter_subject: filter,
                    ack_policy: AckPolicy::Explicit,
                    ack_wait,
                    max_deliver: TRIGGER_MAX_DELIVER,
                    ..Default::default()
                })
                .await
                .expect("consumer"),
        };
        bus.publish_trigger(&AgentId::new(&agent_id).unwrap(), &json!({"input": "hi"}))
            .await
            .expect("publish");
        let mut messages = consumer.messages().await.expect("messages");
        let msg = tokio::time::timeout(Duration::from_secs(5), messages.next())
            .await
            .expect("a message within 5s")
            .expect("stream open")
            .expect("message ok");
        let pulled_at = tokio::time::Instant::now();

        Self {
            bus,
            dispatcher,
            worker,
            pulled_at,
            release_landed_at: Mutex::new(None),
            claims,
            claim_db,
            url,
            consumer,
            messages,
            msg,
            _held: (agent_dir, claim_dir, server),
        }
    }

    fn stream_seq(&self) -> u64 {
        self.msg
            .info()
            .expect("a JetStream delivery")
            .stream_sequence
    }

    /// Hand the trigger to `handle` and wait up to `within` for it to
    /// return.
    async fn handle_within(&self, within: Duration) {
        tokio::time::timeout(within, self.dispatcher.handle(&self.msg))
            .await
            .expect("handle returns");
    }

    /// NAK the delivery in hand and take its redelivery, until it is the
    /// trigger's last delivery.
    async fn redeliver_until_the_last(&mut self) {
        while self.msg.info().expect("a JetStream delivery").delivered < TRIGGER_MAX_DELIVER {
            self.msg
                .ack_with(async_nats::jetstream::AckKind::Nak(None))
                .await
                .expect("redeliver the trigger");
            self.msg = tokio::time::timeout(Duration::from_secs(5), self.messages.next())
                .await
                .expect("a redelivery within 5s")
                .expect("stream open")
                .expect("message ok");
        }
    }

    /// Hand the trigger to `handle` and wait for it to return. Before
    /// #987 this is where the handler panicked.
    async fn handle(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.dispatcher.handle(&self.msg))
            .await
            .expect("handle returns");
    }

    /// Hand the trigger to `handle` as the consume loop does, and hand
    /// every copy the broker redelivers meanwhile to `handle` too, each
    /// driven beside the others as the loop's tasks are. Copies are taken
    /// for as long as the first copy or any copy is being handled; a copy
    /// delivered after all of them have returned is not looked at. Returns
    /// the moment each copy arrived.
    ///
    /// Never await a copy inside the first copy's turn: a copy that takes
    /// a fresh claim waits for the worker permit the first copy holds, and
    /// the first copy, no longer polled, never lets it go (#1048). Fails
    /// after [`DRIVE_DEADLINE`], naming what was still being handled.
    ///
    /// With a held-up release ([`Self::setting_up_with_a_held_release`]),
    /// this also watches for the release to land, holds each copy that
    /// arrived after the first run returned until it has
    /// ([`wait_for_the_release`]), and returns only once it has seen it.
    async fn handle_beside_copies(&mut self) -> Vec<tokio::time::Instant> {
        let held_release = self.worker.release_held_for.is_some();
        let key = test_key(&self.bus, self.stream_seq());
        let run_returns = &self.worker.run_returns;
        let landed_at = &self.release_landed_at;
        let landing = watch_the_release(&self.claims, key, run_returns, landed_at);
        tokio::pin!(landing);
        let mut landing_seen = !held_release;
        let dispatcher = &self.dispatcher;
        let messages = &mut self.messages;
        let first = dispatcher.handle(&self.msg);
        tokio::pin!(first);
        let deadline = tokio::time::sleep(DRIVE_DEADLINE);
        tokio::pin!(deadline);
        let mut first_done = false;
        let mut arrived = Vec::new();
        // The delivery attempt of each copy still being handled.
        let mut pending: Vec<i64> = Vec::new();
        let mut in_hand = FuturesUnordered::new();
        loop {
            if first_done && in_hand.is_empty() && landing_seen {
                return arrived;
            }
            tokio::select! {
                () = &mut first, if !first_done => first_done = true,
                Some(delivered) = in_hand.next(), if !in_hand.is_empty() => {
                    pending.retain(|attempt| *attempt != delivered);
                }
                () = &mut landing, if !landing_seen => landing_seen = true,
                next = messages.next(), if !first_done || !in_hand.is_empty() => match next {
                    Some(Ok(copy)) => {
                        arrived.push(tokio::time::Instant::now());
                        let delivered = copy.info().expect("a JetStream delivery").delivered;
                        pending.push(delivered);
                        let gated = held_release && !run_returns.lock().unwrap().is_empty();
                        in_hand.push(async move {
                            if gated {
                                wait_for_the_release(landed_at).await;
                            }
                            dispatcher.handle(&copy).await;
                            delivered
                        });
                    }
                    Some(Err(err)) => panic!("the pull stream failed while the trigger was handled: {err}"),
                    None => panic!("the pull stream closed while the trigger was handled"),
                },
                () = &mut deadline => panic!(
                    "the trigger was not handled within {DRIVE_DEADLINE:?}: the first copy's \
                     `handle` {}, and {} of the {} copies were still being handled (delivery \
                     attempts {pending:?}){}",
                    if first_done { "had returned" } else { "had not returned" },
                    pending.len(),
                    arrived.len(),
                    if landing_seen { "" } else { "; the held-up release had not landed" },
                ),
            }
        }
    }

    /// How many invocations the worker was asked to start.
    fn starts(&self) -> usize {
        self.worker.starts.load(Ordering::SeqCst)
    }

    /// When the stub worker began reporting itself draining.
    fn drained_at(&self) -> tokio::time::Instant {
        self.worker
            .drained_at
            .lock()
            .unwrap()
            .expect("the worker drained part-way through setup")
    }

    /// When the first run returned. Later runs, a copy's, return later
    /// and are not this.
    fn first_returned_at(&self) -> tokio::time::Instant {
        *self
            .worker
            .run_returns
            .lock()
            .unwrap()
            .first()
            .expect("the first run returned")
    }

    /// When a held-up release landed, if one did while copies were driven.
    fn release_landed_at(&self) -> Option<tokio::time::Instant> {
        *self.release_landed_at.lock().unwrap()
    }

    /// What happened when, for a failure message: runs started, and in
    /// milliseconds since the pull, when each run returned, when each copy
    /// arrived and when a held-up release landed.
    fn timeline(&self, copies: &[tokio::time::Instant]) -> String {
        let since = |at: &tokio::time::Instant| (*at - self.pulled_at).as_millis();
        let returns: Vec<_> = self
            .worker
            .run_returns
            .lock()
            .unwrap()
            .iter()
            .map(since)
            .collect();
        let arrivals: Vec<_> = copies.iter().map(since).collect();
        let landed = self.release_landed_at().as_ref().map(since);
        format!(
            "runs started {}; in ms since the pull, runs returned at {returns:?}, copies arrived \
             at {arrivals:?}, a held-up release landed at {landed:?}",
            self.starts()
        )
    }

    /// The dispatcher's count of copies it dropped before a start was
    /// recorded on their claim, which is this daemon's.
    fn held_drops(&self) -> u64 {
        self.bus
            .consumer_ledger()
            .record(CONSUMER_NAME)
            .held_duplicate_dropped
    }

    fn ran_to_end(&self) -> bool {
        self.worker.ran_to_end.load(Ordering::SeqCst)
    }

    /// The trigger's id and its invocation's id, once the invocation
    /// has started.
    fn named(&self) -> (Uuid, Uuid) {
        self.worker
            .named
            .lock()
            .unwrap()
            .expect("the invocation started")
    }

    /// What a fresh claim on this delivery's identity, by another
    /// claimant, gets.
    async fn probe_claim(&self) -> TriggerClaim {
        self.claims
            .claim_trigger(test_key(&self.bus, self.stream_seq()), "probe", "probe", 0)
            .await
            .expect("probe the claim")
    }

    /// Wait until the broker has the delivery acked: nothing is pending
    /// acknowledgement on the consumer.
    async fn wait_until_acked(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pending = self
                .consumer
                .info()
                .await
                .expect("consumer info")
                .num_ack_pending;
            if pending == 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the trigger was not acked (num_ack_pending={pending})"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Start reading every answer this delivery sends the broker.
    async fn watch_answers(&self) -> Answers {
        Answers::watch(&self.url, &self.msg).await
    }

    /// Make every answer this delivery sends fail in the client, before
    /// it can reach the broker: the delivery answers through a connection
    /// of its own that has been drained and closed. It still names the
    /// same message, because `info()` reads the reply subject alone.
    async fn refuse_acks(&mut self) {
        let client = async_nats::connect(self.url.as_str())
            .await
            .expect("connect a connection to close");
        client.drain().await.expect("drain it");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while client
            .publish("fq.test.closed-probe", "".into())
            .await
            .is_ok()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the drained connection still accepts a publish after 5s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.msg.context = async_nats::jetstream::new(client);
    }
}

/// Every answer one delivery sends the broker, read off its ack subject
/// by a connection of its own and stamped as it arrives. An ack is an
/// empty body, an in-progress ack `+WPI`, a NAK `-NAK`.
struct Answers {
    _client: async_nats::Client,
    /// Each answer, by kind (`ack`, `keepalive` or the body of another),
    /// with the moment it arrived.
    seen: Arc<Mutex<Vec<(tokio::time::Instant, String)>>>,
    collector: tokio::task::JoinHandle<()>,
}

impl Drop for Answers {
    fn drop(&mut self) {
        self.collector.abort();
    }
}

/// What [`Answers::sent`] read, counted by kind.
#[derive(Debug)]
struct Sent {
    acks: usize,
    keepalives: usize,
    other: Vec<String>,
    /// The last answer read: `ack`, `keepalive` or the body of another.
    last: Option<String>,
    /// Every answer, in order, with the moment it arrived.
    timeline: Vec<(tokio::time::Instant, String)>,
}

impl Sent {
    /// When the one answer that settled the delivery, an ack or a NAK,
    /// arrived.
    fn settled_at(&self) -> tokio::time::Instant {
        let mut settlements = self.timeline.iter().filter(|(_, kind)| kind != "keepalive");
        let (at, _) = settlements.next().expect("the delivery was answered");
        assert!(
            settlements.next().is_none(),
            "the delivery was answered more than once: {self:?}"
        );
        *at
    }

    /// The longest the delivery went without an answer reaching the
    /// broker, from `from` until it was settled.
    fn longest_silence_from(&self, from: tokio::time::Instant) -> Duration {
        let settled = self.settled_at();
        let mut marks: Vec<_> = self
            .timeline
            .iter()
            .map(|(at, _)| *at)
            .filter(|at| *at > from && *at <= settled)
            .collect();
        marks.insert(0, from);
        marks
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .max()
            .unwrap_or_default()
    }
}

impl Answers {
    async fn watch(url: &str, msg: &Message) -> Self {
        let client = async_nats::connect(url).await.expect("connect a watcher");
        let reply = msg
            .reply
            .clone()
            .expect("a JetStream delivery has an ack subject");
        let mut subscription = client.subscribe(reply).await.expect("watch the answers");
        client
            .flush()
            .await
            .expect("the watch is in place before the delivery is answered");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let collector = tokio::spawn({
            let seen = Arc::clone(&seen);
            async move {
                while let Some(answer) = subscription.next().await {
                    let kind = match answer.payload.as_ref() {
                        b"" | b"+ACK" => "ack".to_string(),
                        b"+WPI" => "keepalive".to_string(),
                        other => String::from_utf8_lossy(other).into_owned(),
                    };
                    seen.lock()
                        .unwrap()
                        .push((tokio::time::Instant::now(), kind));
                }
            }
        });
        Self {
            _client: client,
            seen,
            collector,
        }
    }

    /// The answers sent so far, once none has arrived for half a second.
    /// Fails if they are still arriving after [`ANSWERS_DEADLINE`].
    async fn sent(&mut self) -> Sent {
        let quiet = Duration::from_millis(500);
        let deadline = tokio::time::Instant::now() + ANSWERS_DEADLINE;
        loop {
            tokio::time::sleep(quiet).await;
            let sent = self.read();
            if sent
                .timeline
                .last()
                .is_none_or(|(at, _)| at.elapsed() >= quiet)
            {
                return sent;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "answers were still arriving after {ANSWERS_DEADLINE:?}, so something is still \
                 handling the delivery: {sent:?}"
            );
        }
    }

    /// The answers seen so far, counted by kind.
    fn read(&self) -> Sent {
        let timeline = self.seen.lock().unwrap().clone();
        let mut sent = Sent {
            acks: 0,
            keepalives: 0,
            other: Vec::new(),
            last: timeline.last().map(|(_, kind)| kind.clone()),
            timeline: Vec::new(),
        };
        for (_, kind) in &timeline {
            match kind.as_str() {
                "ack" => sent.acks += 1,
                "keepalive" => sent.keepalives += 1,
                other => sent.other.push(other.to_string()),
            }
        }
        sent.timeline = timeline;
        sent
    }
}

/// The run durably started, so its claim must read `durably_started`:
/// released, it would let the next delivery win a fresh claim and start
/// the work again.
fn assert_marked_started(claim: TriggerClaim) {
    assert!(
        matches!(claim, TriggerClaim::Started { .. }),
        "the run durably started, so its claim must be marked `durably_started`; \
         a fresh claim on the delivery got {claim:?}"
    );
}

/// A setup longer than the delivery's ack window is kept alive through,
/// and the trigger is delivered once. Before #967's setup keepalive
/// nothing renewed the delivery between the worker permit and the durable
/// start, so the broker redelivered it underneath its own setup, and each
/// copy met this daemon's claim and was dropped and counted as a held
/// duplicate, spending one of the trigger's deliveries. Now in-progress
/// acks are sent while the setup runs: no copy comes back, none is dropped,
/// the run starts once, and the durable start acks the delivery once.
#[tokio::test]
async fn a_setup_longer_than_the_ack_window_is_kept_alive_and_delivered_once() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::setting_up(
            Setup::Takes(LONG_SETUP),
            Resolve::Fire,
            Turn::Later,
            Returns::Completed,
        )
        .await;
        let mut answers = pulled.watch_answers().await;

        let copies = pulled.handle_beside_copies().await;

        assert!(
            copies.is_empty(),
            "the trigger was redelivered underneath its own setup {} time(s)",
            copies.len()
        );
        assert_eq!(
            pulled.held_drops(),
            0,
            "no copy was dropped as a held duplicate"
        );
        assert_eq!(pulled.starts(), 1, "the run started once");
        assert!(pulled.ran_to_end(), "the run reached its end");
        assert_marked_started(pulled.probe_claim().await);
        pulled.wait_until_acked().await;
        let sent = answers.sent().await;
        assert!(
            sent.keepalives > 0 && sent.acks == 1 && sent.other.is_empty(),
            "kept alive while it set up, then acked once at its durable start: {sent:?}"
        );
        assert_no_copy_comes_back(&mut pulled.messages, SHORT_ACK_WINDOW * 3).await;
    })
    .await;
}

/// The setup's keepalive ends with the durable start, and nothing is sent
/// for the delivery once it is acked. The run sets up for longer than the
/// window, makes its first WAL write, and then goes on for longer than the
/// window again: the mark lands at once, the delivery is acked, and the
/// ack is the last thing sent, however long the run goes on.
///
/// One in-progress ack can still go out after the first WAL write and
/// before the ack, when the keepalive's tick is due in the very poll that
/// fires the signal: `handle` sees the signal only on its loop's next
/// turn. That is harmless, and this test does not forbid it.
#[tokio::test]
async fn nothing_is_sent_after_the_ack_while_the_run_goes_on() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::setting_up(
            Setup::Takes(LONG_SETUP),
            Resolve::Fire,
            Turn::After(LONG_SETUP),
            Returns::Completed,
        )
        .await;
        let mut answers = pulled.watch_answers().await;

        let copies = pulled.handle_beside_copies().await;

        assert!(copies.is_empty(), "no copy came back: {} did", copies.len());
        assert!(pulled.ran_to_end(), "the run went on to its end");
        pulled.wait_until_acked().await;
        let sent = answers.sent().await;
        assert!(
            sent.keepalives > 0 && sent.acks == 1 && sent.other.is_empty(),
            "kept alive through setup, then acked once: {sent:?}"
        );
        assert_eq!(
            sent.last.as_deref(),
            Some("ack"),
            "nothing may follow the ack while the run goes on: {sent:?}"
        );
    })
    .await;
}

/// A run that fails during a long setup settles its delivery once, as it
/// did before the setup keepalive. The setup outlasts the window, then
/// drops its signal unfired and tears down for longer than the window
/// again before it returns a transient error. The delivery is kept alive
/// through both, since no durable start was made, and the pre-start
/// settlement then NAKs it once, releasing its claim, with nothing sent
/// after the NAK.
///
/// **Issue 1057 used to fail this test under load.** The claim's release
/// before the NAK can outlast the one-second window on a loaded machine;
/// before issue 1057 nothing renewed the delivery in that gap, so a copy
/// arrived, took the freed claim once the release landed, and the trigger
/// ran a second time (the review of PR 1058 measured 23 failures in 288
/// runs of this test alone under build-like load). The delivery is now
/// kept in progress until its answer is published, so that order no
/// longer happens. The message still counts the copies before and after
/// the first run's return and prints a timeline with the runs started, so
/// a failure here says which window it came from. The forced form of the
/// same order is
/// [`a_held_up_release_keeps_the_delivery_alive_until_its_answer`].
#[tokio::test]
async fn a_failure_during_a_long_setup_settles_the_delivery_once() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::setting_up(
            Setup::Takes(LONG_SETUP),
            Resolve::Drop,
            Turn::After(LONG_SETUP),
            Returns::TransientError,
        )
        .await;
        let mut answers = pulled.watch_answers().await;

        let copies = pulled.handle_beside_copies().await;

        let returned = pulled.first_returned_at();
        let after_return = copies.iter().filter(|arrived| **arrived > returned).count();
        assert!(
            copies.is_empty(),
            "the trigger was redelivered {} time(s): {} before the first run returned, while it was \
             kept alive, and {after_return} after it returned, while its claim was released and \
             before its NAK went out, which is issue 1057's window. {}",
            copies.len(),
            copies.len() - after_return,
            pulled.timeline(&copies)
        );
        assert_eq!(
            pulled.held_drops(),
            0,
            "no copy was dropped as a held duplicate"
        );
        assert_eq!(
            pulled.probe_claim().await,
            TriggerClaim::Won,
            "the NAK released the claim"
        );
        let sent = answers.sent().await;
        assert!(
            sent.keepalives > 0 && sent.acks == 0 && sent.other.len() == 1,
            "kept alive through setup and teardown, then settled by one NAK: {sent:?}"
        );
        assert!(
            sent.other[0].starts_with("-NAK")
                && sent.last.as_deref() == Some(sent.other[0].as_str()),
            "the one settlement is a NAK and nothing follows it: {sent:?}"
        );
    })
    .await;
}

/// A drain part-way through a long setup stops the keepalive, as it stops
/// every hold, and leaves the delivery as it was left before there was a
/// keepalive. The worker reports itself draining after more than a whole
/// window of setup; the run then sets up for longer than a window again
/// and returns stopped by the drain before its first WAL write.
///
/// Until the drain no copy comes back, because the setup was kept alive.
/// After it the keepalive stops, the window lapses, and the broker
/// redelivers (a draining daemon has stopped pulling, so in production the
/// copy waits for the next binary; this test's pull stream is still open,
/// which is how it sees the lapse). The delivery is neither acked nor
/// NAKed, and its claim stays this daemon's for the next binary to adopt.
#[tokio::test]
async fn a_drain_during_a_long_setup_stops_the_keepalive_and_leaves_the_delivery() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::setting_up(
            Setup::DrainedPartWay {
                takes: LONG_SETUP + LONG_SETUP / 2,
                drain_at: LONG_SETUP / 2 + SHORT_ACK_WINDOW / 4,
            },
            Resolve::Drop,
            Turn::Later,
            Returns::Drained,
        )
        .await;
        let mut answers = pulled.watch_answers().await;

        let copies = pulled.handle_beside_copies().await;

        let drained_at = pulled.drained_at();
        assert!(
            copies.iter().all(|arrived| *arrived > drained_at),
            "a copy came back before the drain, so the setup was not kept alive"
        );
        assert!(
            !copies.is_empty(),
            "no copy came back after the drain, so the keepalive did not stop with it"
        );
        assert_held_by_the_dispatcher(
            pulled.probe_claim().await,
            "a drain before the first WAL write leaves the claim for the next binary",
        );
        let sent = answers.sent().await;
        assert!(
            sent.keepalives > 0 && sent.acks == 0 && sent.other.is_empty(),
            "kept alive until the drain, and then neither acked nor NAKed: {sent:?}"
        );
    })
    .await;
}

/// **A run's claim release held up past the ack window keeps its
/// delivery alive until the delivery is answered, so no copy comes and the
/// trigger runs once (issue 1057).** Played after a run that fails before
/// its durable start (NAKed) and after one that returns without starting
/// (consumed, acked).
///
/// The run sets up for longer than the ack window, kept alive, and returns
/// before its first WAL write. Its claim's release is then held up for
/// [`RELEASE_HELD_FOR`], three windows
/// ([`Pulled::setting_up_with_a_held_release`] says which part of this is
/// real and what the helper's gate adds). Before issue 1057 nothing renewed
/// the delivery between the run's return and its answer, so the broker
/// redelivered, a copy took the freed claim once the release landed, and
/// the trigger ran again. Now the delivery is kept in progress from the
/// run's return until its answer is published.
///
/// Pinned: one run, asserted first, so that against the old dispatcher it
/// is this assertion that fails and names the issue; that the release
/// really was held past the window (else the test shows nothing); that no
/// copy arrived after the run returned; that in-progress acks reached the
/// broker during the settlement, never a whole window apart; that the
/// first copy was answered once and nothing followed; and that the claim
/// ends released.
async fn a_held_up_release_keeps_the_delivery_alive_until_its_answer(returns: Returns) {
    let naks = matches!(returns, Returns::TransientError);
    let mut pulled = Pulled::setting_up_with_a_held_release(
        Setup::Takes(LONG_SETUP),
        Resolve::Drop,
        Turn::Later,
        returns,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    let copies = pulled.handle_beside_copies().await;

    let timeline = pulled.timeline(&copies);
    assert_eq!(
        pulled.starts(),
        1,
        "the trigger ran again: the delivery was not kept alive between the run's return and \
         its answer, so a copy arrived while the claim's release was held up and took the freed \
         claim once it landed (issue 1057). {timeline}"
    );
    let returned = pulled.first_returned_at();
    let landed = pulled
        .release_landed_at()
        .unwrap_or_else(|| panic!("the held-up release never landed. {timeline}"));
    assert!(
        landed - returned > SHORT_ACK_WINDOW,
        "the release was not held up past the ack window, so this test shows nothing. {timeline}"
    );
    assert!(
        copies.iter().all(|arrived| *arrived < returned),
        "a copy arrived after the run returned. {timeline}"
    );
    assert_eq!(
        pulled.held_drops(),
        copies.len() as u64,
        "any copy that came during the setup was dropped as held. {timeline}"
    );
    let sent = answers.sent().await;
    assert!(
        sent.settled_at() - returned > SHORT_ACK_WINDOW,
        "the first copy was answered before its release, held up for longer than the ack \
         window, could have landed: the answer did not wait for the release. {timeline}: \
         {sent:?}"
    );
    let silence = sent.longest_silence_from(returned);
    assert!(
        silence < SHORT_ACK_WINDOW,
        "the delivery went {silence:?} without an answer between the run's return and its \
         settlement, as long as its ack window: {sent:?}"
    );
    let answered_once = if naks {
        sent.acks == 0 && sent.other.len() == 1 && sent.other[0].starts_with("-NAK")
    } else {
        sent.acks == 1 && sent.other.is_empty()
    };
    assert!(
        answered_once && sent.last.as_deref() != Some("keepalive"),
        "the first copy was answered once, with a {}, and nothing followed: {sent:?}",
        if naks { "NAK" } else { "ack" }
    );
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Won,
        "the claim ends released"
    );
}

#[tokio::test]
async fn a_held_up_release_after_a_failure_keeps_the_delivery_alive_until_its_nak() {
    within_the_test_deadline(a_held_up_release_keeps_the_delivery_alive_until_its_answer(
        Returns::TransientError,
    ))
    .await;
}

#[tokio::test]
async fn a_held_up_release_after_a_clean_return_keeps_the_delivery_alive_until_its_ack() {
    within_the_test_deadline(a_held_up_release_keeps_the_delivery_alive_until_its_answer(
        Returns::Completed,
    ))
    .await;
}

/// **A release the claim store refuses still answers the delivery, and the
/// keepalive stops with the answer (issue 1057).** The store's write lock
/// is held from the run's return for longer than its busy timeout (sqlx's
/// five seconds), so the release fails on the lock rather than landing.
/// The delivery is kept in progress the whole time, the NAK goes out after
/// the failure, nothing follows it, and the claim is left as the failed
/// release left it, `claimed` by the dispatcher. No copy comes: only the
/// first delivery is handed to `handle`, so a copy the broker sent would
/// be waiting on the pull stream, which the test reads.
#[tokio::test]
async fn a_release_the_store_refuses_still_naks_the_delivery_and_stops_the_keepalive() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::setting_up_with_the_release_held_for(
            Setup::Instant,
            Resolve::Drop,
            Turn::Later,
            Returns::TransientError,
            STORE_BUSY_TIMEOUT + Duration::from_secs(1),
        )
        .await;
        let mut answers = pulled.watch_answers().await;

        pulled.handle_within(STORE_BUSY_TIMEOUT * 2).await;

        let returned = pulled.first_returned_at();
        let sent = answers.sent().await;
        assert!(
            sent.settled_at() - returned >= STORE_BUSY_TIMEOUT,
            "the release did not wait out the store's busy timeout before failing: {sent:?}"
        );
        let silence = sent.longest_silence_from(returned);
        assert!(
            silence < SHORT_ACK_WINDOW,
            "the delivery went {silence:?} without an answer while its release waited and \
             failed, as long as its ack window: {sent:?}"
        );
        assert!(
            sent.acks == 0
                && sent.other.len() == 1
                && sent.other[0].starts_with("-NAK")
                && sent.last.as_deref() == sent.other.first().map(String::as_str),
            "the delivery was NAKed once and nothing followed the NAK: {sent:?}"
        );
        assert_no_copy_arrives(&mut pulled.messages, SHORT_ACK_WINDOW * 2).await;
        assert_held_by_the_dispatcher(
            pulled.probe_claim().await,
            "the release failed, so the claim is left `claimed` by the dispatcher",
        );
    })
    .await;
}

/// **A release that outlasts the settlement's own bound is given up on: the
/// delivery is answered anyway and the keepalive stops, and the release,
/// not cancelled, lands later (issue 1057).** The claim store's write lock
/// is held for twice the bound passed to `settle_within`, standing in for a
/// store stuck in a way it does not bound itself. The NAK goes out at the
/// bound, the delivery is renewed until then and nothing follows the NAK;
/// the claim is still `claimed` when settling returns, and goes once the
/// lock does. No copy comes, which the test reads the pull stream for.
///
/// The test's bound, 1.5 seconds, sits under the store's 5-second busy
/// timeout, so the release is still waiting for the lock when it is given
/// up on: a state the production bound excludes, since it stays above the
/// store's own waits (`settlement::RELEASE_BOUND` says why that matters).
/// What is pinned here holds wherever the release is stuck: settling stops
/// waiting at its bound, answers the delivery, stops renewing it, and does
/// not call the release back.
#[tokio::test]
async fn a_release_that_outlasts_its_bound_is_given_up_and_the_delivery_naked() {
    within_the_test_deadline(async {
        let mut pulled = Pulled::setting_up(
            Setup::Instant,
            Resolve::Drop,
            Turn::Later,
            Returns::TransientError,
        )
        .await;
        let named = Uuid::now_v7().to_string();
        pulled
            .claims
            .claim_trigger(
                test_key(&pulled.bus, pulled.stream_seq()),
                "worker-a",
                &named,
                0,
            )
            .await
            .expect("claim the trigger");
        let mut answers = pulled.watch_answers().await;
        let bound = Duration::from_millis(1500);
        lock_claim_store(&pulled.claim_db, bound * 2).await;

        let began = tokio::time::Instant::now();
        let answer = super::claim::Answer::Nak {
            delay: Duration::from_secs(30),
            context: "test",
        };
        tokio::time::timeout(
            bound * 2,
            pulled
                .dispatcher
                .settle_within(&pulled.msg, None, std::future::ready(answer), bound),
        )
        .await
        .expect("settling returns at its bound, before the lock goes");
        let key = test_key(&pulled.bus, pulled.stream_seq());
        let claim = pulled
            .claims
            .unsettled_trigger_claim(key)
            .await
            .expect("read the claim");
        assert!(
            claim.is_some(),
            "the claim was released before settling gave up on it; the lock should still be held"
        );

        let sent = answers.sent().await;
        let waited = sent.settled_at() - began;
        assert!(
            waited >= bound && waited < bound * 2,
            "the NAK went out after {waited:?}, not at the bound of {bound:?}: {sent:?}"
        );
        let silence = sent.longest_silence_from(began);
        assert!(
            silence < SHORT_ACK_WINDOW,
            "the delivery went {silence:?} without an answer while its release waited: {sent:?}"
        );
        assert!(
            sent.acks == 0
                && sent.other.len() == 1
                && sent.other[0].starts_with("-NAK")
                && sent.last.as_deref() == sent.other.first().map(String::as_str),
            "the delivery was NAKed once and nothing followed the NAK: {sent:?}"
        );
        assert_no_copy_arrives(&mut pulled.messages, SHORT_ACK_WINDOW).await;
        let lands_by = began + bound * 2 + Duration::from_secs(3);
        while pulled
            .claims
            .unsettled_trigger_claim(key)
            .await
            .expect("read the claim")
            .is_some()
        {
            assert!(
                tokio::time::Instant::now() < lands_by,
                "the release given up on never landed once the lock went"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

/// Wait `wait` on the pull stream, and fail if a copy of the delivery
/// arrives. A stream that closes or fails fails too, since it would read as
/// no copy without saying whether one was on its way.
async fn assert_no_copy_arrives(messages: &mut pull::Stream, wait: Duration) {
    match tokio::time::timeout(wait, messages.next()).await {
        Err(_elapsed) => {}
        Ok(None) => panic!("the pull stream closed while waiting for a copy"),
        Ok(Some(Err(err))) => panic!("the pull stream failed while waiting for a copy: {err}"),
        Ok(Some(Ok(copy))) => panic!(
            "a copy of the delivery arrived, as delivery {:?}: the delivery was not kept alive \
             until its answer",
            copy.info().map(|info| info.delivered).ok()
        ),
    }
}

/// How the events stream answers a dead letter's publish in
/// [`a_dead_letter_on_the_last_delivery_keeps_it_alive_until_its_answer`].
#[derive(Clone, Copy)]
enum DeadLetterPublish {
    /// The events stream itself takes it and acknowledges it at once.
    Lands,
    /// A stand-in never answers: the publish fails when its
    /// acknowledgement times out.
    TimesOut,
    /// A stand-in acknowledges it this long after the publish: it
    /// succeeds, slowly.
    SucceedsAfter(Duration),
}

/// The dead letter on a trigger's last delivery, its publish answered as
/// `publish` says, and the delivery kept alive until its answer (issue
/// 1057): an ack once the dead letter is published, and a NAK with no delay
/// when it is not (issue 1062).
///
/// On the last delivery a transient failure before the durable start is
/// dead-lettered by the dispatcher, which publishes the dead letter, then
/// releases the claim and answers. For a stand-in, the events stream is
/// deleted and a subscriber of the test's own takes its subjects, so the
/// publish waits on that subscriber. A last delivery is never redelivered,
/// but a window that lapses before its answer makes the broker give up on
/// the trigger and send its max-deliveries advisory, which the dead-letter
/// watch reads. With the delivery kept alive through the publish, a
/// published dead letter is acked and no advisory is sent; one that could
/// not be published is NAKed with no delay, and the broker sends its
/// advisory at once, for the watch to write the dead letter.
async fn a_dead_letter_on_the_last_delivery_keeps_it_alive_until_its_answer(
    publish: DeadLetterPublish,
) {
    let mut pulled = Pulled::setting_up(
        Setup::Instant,
        Resolve::Drop,
        Turn::Later,
        Returns::TransientError,
    )
    .await;
    pulled.redeliver_until_the_last().await;
    let watcher = async_nats::connect(pulled.url.as_str())
        .await
        .expect("connect a watcher");
    let publishes = Arc::new(AtomicUsize::new(0));
    let mut stand_ins = Vec::new();
    if !matches!(publish, DeadLetterPublish::Lands) {
        pulled
            .bus
            .jetstream()
            .delete_stream(crate::bus::STREAM_NAME)
            .await
            .expect("delete the events stream");
        for subject in crate::bus::EVENT_STREAM_SUBJECTS {
            let mut subscription = watcher
                .subscribe(subject.to_string())
                .await
                .expect("take the events stream's subjects");
            let (publishes, watcher) = (Arc::clone(&publishes), watcher.clone());
            stand_ins.push(tokio::spawn(async move {
                while let Some(published) = subscription.next().await {
                    publishes.fetch_add(1, Ordering::SeqCst);
                    let (DeadLetterPublish::SucceedsAfter(after), Some(reply)) =
                        (publish, published.reply)
                    else {
                        continue;
                    };
                    tokio::time::sleep(after).await;
                    let ack = r#"{"stream":"fq-events","seq":1}"#;
                    watcher
                        .publish(reply, ack.into())
                        .await
                        .expect("acknowledge the publish");
                }
            }));
        }
    }
    watcher.flush().await.expect("the watches are in place");
    let mut answers = pulled.watch_answers().await;

    pulled.handle_within(Duration::from_secs(20)).await;

    let returned = pulled.first_returned_at();
    let sent = answers.sent().await;
    let took = match publish {
        DeadLetterPublish::Lands => Duration::ZERO,
        DeadLetterPublish::TimesOut => Duration::from_secs(4),
        DeadLetterPublish::SucceedsAfter(after) => after,
    };
    let settled_in = sent.settled_at() - returned;
    assert!(
        settled_in >= took,
        "the dead letter's publish took less than {took:?}, so this test shows nothing: {sent:?}"
    );
    // No upper bound on a slow success: before issue 1062 a publish that
    // timed out was acked too, and only a settlement well inside the
    // client's five seconds told the two apart. Now a timed-out publish is
    // NAKed, so the answer itself says which it was (asserted below), and a
    // time bound would only fail on a loaded machine (it did, once in a
    // module run beside other suites: 4.8 s for a 2.5 s stand-in).
    let published = match publish {
        DeadLetterPublish::Lands => dead_letters_of(&pulled).await,
        DeadLetterPublish::TimesOut | DeadLetterPublish::SucceedsAfter(_) => {
            publishes.load(Ordering::SeqCst)
        }
    };
    assert_eq!(published, 1, "the dead letter was published once");
    let silence = sent.longest_silence_from(returned);
    assert!(
        silence < SHORT_ACK_WINDOW,
        "the last delivery went {silence:?} without an answer while its dead letter was \
         published, as long as its ack window: {sent:?}"
    );
    let advisory = captured_advisory_within(&pulled, SHORT_ACK_WINDOW * 2).await;
    if let DeadLetterPublish::TimesOut = publish {
        assert!(
            sent.acks == 0 && sent.other == ["-NAK"] && sent.last.as_deref() == Some("-NAK"),
            "the last delivery, its dead letter not published, was NAKed once with no delay, \
             not acked, and nothing followed: {sent:?}"
        );
        // How soon it comes is pinned on the dispatcher's own consumer, whose
        // last window is long enough to tell (`unpublished_dead_letter`).
        assert!(
            advisory.is_some(),
            "the broker raised no max-deliveries advisory after the NAK, so the dead-letter \
             watch never hears of the trigger: {sent:?}"
        );
    } else {
        assert!(
            sent.acks == 1 && sent.other.is_empty() && sent.last.as_deref() == Some("ack"),
            "the last delivery was acked once and nothing followed: {sent:?}"
        );
        if let Some(advisory) = advisory {
            panic!(
                "the broker gave up on the last delivery before its ack: {}",
                String::from_utf8_lossy(&advisory.payload)
            );
        }
    }
    for stand_in in stand_ins {
        stand_in.abort();
    }
}

/// A max-deliveries advisory, as the advisory stream captured it.
struct CapturedAdvisory {
    /// What the dead-letter watch is handed.
    payload: Vec<u8>,
    /// When the broker emitted it, by its own stamp.
    emitted: chrono::DateTime<chrono::Utc>,
}

/// The max-deliveries advisory the broker raised for `pulled`'s trigger on
/// `pulled`'s consumer, if the advisory stream captures one within `wait`.
///
/// Read off the advisory stream, which is what the dead-letter watch reads,
/// rather than off a subscription of the test's own. A subscription flushed
/// just before the NAK missed the advisory in about one run in fifteen of
/// the broker probe (`unpublished_dead_letter`), while the stream held it,
/// stamped just after the NAK: a flush does not mean the broker has the
/// subscription yet (issue 1045).
async fn captured_advisory_within(pulled: &Pulled, wait: Duration) -> Option<CapturedAdvisory> {
    let consumer = &pulled.consumer.cached_info().name;
    let subject =
        format!("$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.{TRIGGER_STREAM_NAME}.{consumer}");
    let advisories = pulled
        .bus
        .jetstream()
        .get_stream(crate::bus::ADVISORY_STREAM_NAME)
        .await
        .expect("the advisory stream");
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Ok(raw) = advisories.get_last_raw_message_by_subject(&subject).await {
            let fields: serde_json::Value =
                serde_json::from_slice(&raw.payload).expect("the advisory parses");
            let emitted = fields["timestamp"]
                .as_str()
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .expect("the broker stamps its advisory")
                .with_timezone(&chrono::Utc);
            return Some(CapturedAdvisory {
                payload: raw.payload.to_vec(),
                emitted,
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// How many dead letters, and other `failed` events, the events stream
/// holds for the agent `pulled` triggers.
async fn dead_letters_of(pulled: &Pulled) -> usize {
    let agent = agent_id_from_subject(&pulled.msg.subject).expect("a trigger subject");
    let subject = crate::events::subjects::agent_failed(agent);
    let stream = pulled
        .bus
        .jetstream()
        .get_stream(crate::bus::STREAM_NAME)
        .await
        .expect("the events stream");
    let mut counts = stream
        .info_with_subjects(&subject)
        .await
        .expect("count the events stream's subjects");
    let mut held = 0;
    while let Some(entry) = counts.next().await {
        let (_, count) = entry.expect("a subject's count");
        held += count;
    }
    held
}

/// **A dead letter whose publish times out on the trigger's last delivery
/// keeps the delivery alive until it is NAKed, and the broker hands the
/// trigger to the dead-letter watch (issues 1057 and 1062).** The publish
/// fails after async-nats' five seconds for its acknowledgement, and the
/// delivery is then NAKed with no delay rather than acked: the dead letter
/// was not published, so the broker has to report the trigger exhausted
/// for the watch to write one, and it does so at once. Acked, as it was
/// before issue 1062, the trigger would have been consumed with no dead
/// letter from either emitter. What the watch then writes is pinned in
/// `unpublished_dead_letter`.
#[tokio::test]
async fn a_dead_letter_whose_publish_times_out_keeps_the_last_delivery_alive_until_its_nak() {
    within_the_test_deadline(
        a_dead_letter_on_the_last_delivery_keeps_it_alive_until_its_answer(
            DeadLetterPublish::TimesOut,
        ),
    )
    .await;
}

/// **A dead letter published slowly on the trigger's last delivery keeps
/// the delivery alive until its ack (issue 1057).** The stand-in for the
/// events stream acknowledges the publish after two and a half seconds,
/// longer than two ack windows, so the publish succeeds, slowly, and the
/// delivery is renewed through the wait for its acknowledgement. Its ack
/// stops the broker's advisory, so the watch never writes a second dead
/// letter.
#[tokio::test]
async fn a_dead_letter_published_slowly_keeps_the_last_delivery_alive_until_its_ack() {
    within_the_test_deadline(
        a_dead_letter_on_the_last_delivery_keeps_it_alive_until_its_answer(
            DeadLetterPublish::SucceedsAfter(Duration::from_millis(2500)),
        ),
    )
    .await;
}

/// **A dead letter the events stream takes at once is acked once on the
/// trigger's last delivery, and the broker raises no advisory (issue
/// 1062).** The everyday path, unchanged: one dead letter, from the
/// dispatcher, and nothing for the watch.
#[tokio::test]
async fn a_dead_letter_published_at_once_acks_the_last_delivery_once_and_raises_no_advisory() {
    within_the_test_deadline(
        a_dead_letter_on_the_last_delivery_keeps_it_alive_until_its_answer(
            DeadLetterPublish::Lands,
        ),
    )
    .await;
}

/// Run a test's body against [`TEST_DEADLINE`], so that a hang is a
/// failure. Each step that can wait has a deadline of its own, shorter
/// than this one, whose message names what it was waiting on; this is the
/// backstop for a step that has none.
async fn within_the_test_deadline(body: impl std::future::Future<Output = ()>) {
    if tokio::time::timeout(TEST_DEADLINE, body).await.is_err() {
        panic!(
            "the test did not finish within {TEST_DEADLINE:?}: a step with no deadline of its own \
             was still waiting"
        );
    }
}

/// The signal is dropped unfired and the invocation then fails with a
/// transient error: `handle` must not poll the spent receiver again, and
/// the settlement after its loop NAKs the trigger for redelivery and
/// releases its claim.
#[tokio::test]
async fn a_dropped_signal_then_a_transient_error_naks_the_trigger() {
    let mut pulled = Pulled::new(Resolve::Drop, Turn::Later, Returns::TransientError).await;
    let seq = pulled.stream_seq();

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Won,
        "the NAK released the claim, so a fresh claim on the delivery wins"
    );
    // Only waits for a message to arrive, so a wide margin costs nothing
    // when the test passes.
    let redelivery = TRIGGER_RETRY_BACKOFF[0] + Duration::from_secs(10);
    let again = tokio::time::timeout(redelivery, pulled.messages.next())
        .await
        .expect("the NAKed trigger comes back for redelivery")
        .expect("stream open")
        .expect("message ok");
    let (again_seq, delivered) = {
        let info = again.info().expect("a JetStream delivery");
        (info.stream_sequence, info.delivered)
    };
    assert_eq!(
        (again_seq, delivered),
        (seq, 2),
        "the same trigger, delivered a second time"
    );
    let _ = again.ack().await;
}

/// The signal is dropped unfired and the invocation then completes:
/// `handle` must not poll the spent receiver again, and the settlement
/// after its loop acks the trigger and releases its claim.
#[tokio::test]
async fn a_dropped_signal_then_a_clean_return_acks_the_trigger() {
    let mut pulled = Pulled::new(Resolve::Drop, Turn::Later, Returns::Completed).await;

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    pulled.wait_until_acked().await;
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Won,
        "the ack released the claim, so a fresh claim on the delivery wins"
    );
}

/// The signal fires and the claim store refuses the first two attempts
/// to mark the claim, then takes the third: a store that heals inside the
/// retries. Nothing else may happen. The run is not stopped, the claim
/// ends `durably_started`, and the delivery is acked once and nothing else
/// is sent for it but keepalives. Before #967 the first refusal stopped
/// the run there, un-acked.
#[tokio::test]
async fn a_mark_refused_then_accepted_inside_its_retries_marks_the_claim_and_acks_once() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::First(2)),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on while its mark was retried"
    );
    assert_eq!(
        refusals_left(&pulled.claim_db).await,
        0,
        "both refusals were spent, so the mark that landed was a retry"
    );
    assert_marked_started(pulled.probe_claim().await);
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
}

/// The signal fires and the claim store refuses every attempt to mark the
/// claim. The run started, so it is neither stopped nor redelivered: the
/// delivery is acked without its claim being released, the claim stays
/// `claimed` by the dispatcher, and the run goes on to its end.
///
/// The consumer's ack window is shorter than the pauses between the
/// attempts ([`SHORT_ACK_WINDOW`]), so this also pins the keepalive that
/// runs alongside them: a delivery left to lapse while its mark was
/// retried would be redelivered underneath it. No copy comes back
/// either during the retries or for three windows after the ack, so none
/// can be dropped as a held duplicate or end in a dead letter.
#[tokio::test]
async fn a_mark_refused_past_its_retries_acks_keeping_the_claim_and_the_run_runs_on() {
    assert!(
        SHORT_ACK_WINDOW < MARK_RETRY_PAUSES.iter().sum::<Duration>(),
        "the window must lapse during the retries' pauses, or this test cannot see a missing \
         keepalive"
    );
    let mut pulled = Pulled::with_ack_window(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Later,
        Returns::Completed,
        SHORT_ACK_WINDOW,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on after its mark was refused"
    );
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the mark never landed, so the claim must be left `claimed` by the dispatcher, \
         not released",
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
    assert!(
        sent.keepalives > 0,
        "the delivery was kept alive while its mark was retried: {sent:?}"
    );
    assert_no_copy_comes_back(&mut pulled.messages, SHORT_ACK_WINDOW * 3).await;
}

/// Wait `wait` on the pull stream for a copy of the delivery, and fail if
/// one comes back. A stream that closes or fails fails too: it would read
/// as nothing having come back without saying whether a copy was on its
/// way.
async fn assert_no_copy_comes_back(messages: &mut pull::Stream, wait: Duration) {
    match tokio::time::timeout(wait, messages.next()).await {
        Err(_elapsed) => {}
        Ok(None) => panic!(
            "the pull stream closed while waiting, so it cannot say whether a copy came back"
        ),
        Ok(Some(Err(err))) => panic!(
            "the pull stream failed while waiting, so it cannot say whether a copy came back: \
             {err}"
        ),
        Ok(Some(Ok(copy))) => panic!(
            "a started run's delivery must not come back, and came back as delivery {:?}",
            copy.info().map(|info| info.delivered).ok()
        ),
    }
}

/// The signal fires and the first attempt to mark the claim blocks:
/// another connection holds the claim store's write lock for
/// [`LOCK_HELD`], longer than the retries are allowed, and the attempt is
/// refused once it gets through. A store that refuses at once cannot show
/// either of the two things this pins.
///
/// - **No attempt is offered once the time bound has passed.** The store
///   gets the one attempt it got before there were retries, and the
///   delivery is then acked keeping its claim: one refusal is spent.
/// - **The delivery is kept alive alongside an attempt, not only between
///   attempts.** There is no "between" here, and the consumer's ack
///   window ([`SHORT_ACK_WINDOW`]) is shorter than the lock: keepalives
///   are sent while the attempt waits, the delivery is acked once, and no
///   copy comes back.
#[tokio::test]
async fn a_mark_that_blocks_past_the_time_bound_is_offered_once_with_its_delivery_kept_alive() {
    assert!(
        LOCK_HELD > MARK_RETRY_BUDGET && LOCK_HELD > SHORT_ACK_WINDOW,
        "the lock must outlast the retry budget and the ack window, or this test pins neither"
    );
    let mut pulled = Pulled::with_ack_window(
        Resolve::FireIntoLockedStore(LOCK_HELD),
        Turn::Later,
        Returns::Completed,
        SHORT_ACK_WINDOW,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on after its mark was refused"
    );
    assert_eq!(
        refusals_left(&pulled.claim_db).await,
        i64::from(ATTEMPTS_IF_REFUSED_AT_ONCE - 1),
        "one attempt reached the store: none may be offered once the time bound has passed"
    );
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the mark never landed, so the claim must be left `claimed` by the dispatcher",
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
    assert!(
        sent.keepalives > 0,
        "the delivery was kept alive while its one attempt waited on the lock: {sent:?}"
    );
    assert_no_copy_comes_back(&mut pulled.messages, SHORT_ACK_WINDOW * 3).await;
}

/// A refused mark through a restart, end to end: the two halves of #967
/// together. The claim store refuses every attempt to mark the run's
/// start, so the delivery is acked keeping its claim and the run goes on
/// to its end. Then the store heals, the run is archived (the archive
/// hand-off's ownership row stands in for it), and the daemon restarts
/// under a new claimant id, its claims recovered over the worker's report
/// as the daemon recovers them. A copy of the trigger comes back as a
/// replay, from a durable recreated on the stream, which keeps the
/// message for a day whether or not it was acked.
///
/// Nothing may start a second time. Recovery has recorded the start on
/// the claim, so the replayed copy meets a started claim and is dropped as
/// a copy of a started run, and the claim reads `durably_started` under
/// the first run's invocation id. Before startup recovery marked finished
/// runs, the copy adopted the `claimed` row and ran the trigger again.
#[tokio::test]
async fn a_run_whose_mark_was_refused_is_not_started_again_by_a_replay_after_a_restart() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    pulled.handle().await;
    assert!(pulled.ran_to_end(), "the first run went on to its end");
    pulled.wait_until_acked().await;
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the mark never landed, so the first daemon left the claim `claimed`",
    );
    let invocation_id = pulled.named().1.to_string();

    heal_claim_marks(&pulled.claim_db).await;
    pulled
        .claims
        .upsert_invocation_ownership(&invocation_id, "worker-a", 2_000, OwnerStatus::Completed)
        .await
        .expect("the archive hand-off's ownership row");

    let agent_id = agent_id_from_subject(&pulled.msg.subject)
        .expect("a trigger subject")
        .to_string();
    let (_agent_dir, registry) = registry_with(&agent_id);
    let restarted = Arc::new(ResolvesThenReturns {
        setup: Setup::Instant,
        resolve: Resolve::Fire,
        turn: Turn::Later,
        returns: Returns::Completed,
        claim_db: pulled.claim_db.clone(),
        ran_to_end: AtomicBool::new(false),
        named: Mutex::new(None),
        starts: AtomicUsize::new(0),
        draining: AtomicBool::new(false),
        drained_at: Mutex::new(None),
        release_held_for: None,
        run_returns: Mutex::new(Vec::new()),
    });
    let dispatcher = TriggerDispatcher::new(
        pulled.bus.clone(),
        registry,
        restarted.clone() as Arc<dyn Worker>,
        Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
        1,
    )
    .with_trigger_claims(recovered_claims(&pulled.claims).await, "worker-b");

    let replayer = pulled
        .bus
        .trigger_consumer_with_filter(
            &unique_consumer_name(),
            pulled.msg.subject.as_str(),
            NATS_DEFAULT_MAX_ACK_PENDING,
        )
        .await
        .expect("a recreated durable");
    let mut replayed = replayer.messages().await.expect("messages");
    let copy = tokio::time::timeout(Duration::from_secs(5), replayed.next())
        .await
        .expect("the stream still holds the acked message, and replays it")
        .expect("stream open")
        .expect("message ok");
    assert_eq!(
        copy.info().expect("a JetStream delivery").stream_sequence,
        pulled.stream_seq(),
        "the same message, replayed"
    );
    tokio::time::timeout(Duration::from_secs(10), dispatcher.handle(&copy))
        .await
        .expect("handle returns");

    assert!(
        restarted.named.lock().unwrap().is_none(),
        "the replayed copy started the trigger a second time"
    );
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Started {
            invocation_id: invocation_id.clone()
        },
        "recovery recorded the first run's start on the claim"
    );
    let ledger = pulled.bus.consumer_ledger().record(CONSUMER_NAME);
    assert_eq!(
        (ledger.duplicate_dropped, ledger.held_duplicate_dropped),
        (1, 0),
        "the copy was dropped as one of a started run, not as one this daemon still holds"
    );
}

/// The signal fires, the claim store refuses every attempt to mark the
/// claim, and the ack sent in place of the mark cannot be sent either.
/// What follows is the risk #967 accepted: the delivery is left un-acked
/// with its claim `claimed`, so its redeliveries meet this daemon's own
/// claim and are settled by the run's start on record (`own_claim`). The
/// run must still go on to its end, with the delivery left pending and the
/// claim left as it was.
#[tokio::test]
async fn a_failed_ack_after_a_mark_refused_past_its_retries_leaves_the_delivery_and_the_run_runs_on()
 {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    pulled.refuse_acks().await;

    pulled.handle().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on after its ack failed"
    );
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the claim must be left `claimed` by the dispatcher, not released",
    );
    ack_pending_stays(&mut pulled.consumer, 1, Duration::from_millis(300)).await;
}

/// The signal fires and the invocation completes in the same poll, so
/// `handle`'s loop ends on the result without having seen the signal
/// (#966). The run durably started all the same: the claim must be
/// marked `durably_started` and the delivery acked, as the loop's own
/// signal branch would have done, not released as if nothing had run.
#[tokio::test]
async fn a_signal_fired_in_the_poll_that_completes_marks_the_claim_and_acks() {
    let mut pulled = Pulled::new(Resolve::Fire, Turn::Same, Returns::Completed).await;

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_marked_started(pulled.probe_claim().await);
    pulled.wait_until_acked().await;
}

/// The signal fires and the invocation fails with a transient error in
/// the same poll (#966). Before durable start a transient error is NAKed
/// for a retry, but this run had started: the claim must be marked
/// `durably_started` and the delivery acked, so that no redelivery comes
/// back to start the work a second time.
#[tokio::test]
async fn a_signal_fired_in_the_poll_that_fails_transiently_is_acked_not_retried() {
    let mut pulled = Pulled::new(Resolve::Fire, Turn::Same, Returns::TransientError).await;

    pulled.handle().await;

    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_marked_started(pulled.probe_claim().await);
    // Acked, and so not NAKed: a NAK with a delay keeps the delivery
    // pending until it is redelivered, so nothing pending means no retry
    // is on its way.
    pulled.wait_until_acked().await;
}

/// What a mark refused on every attempt must leave: the claim as it was,
/// `claimed` by the dispatcher, and the delivery acked.
///
/// The run started, so a redelivery is never wanted: each one would meet
/// the daemon's own `claimed` row and have to be settled by the run's
/// start on record (`own_claim`), spending one of the trigger's
/// deliveries. So the delivery is acked. The claim is not released first:
/// a lost ack would then let the redelivery win a fresh claim. A started
/// run under a claim that still says `claimed` is what is left (#967).
async fn assert_acked_with_the_claim_kept(pulled: &mut Pulled) {
    assert!(pulled.ran_to_end(), "the invocation ran to its end");
    assert_held_by_the_dispatcher(
        pulled.probe_claim().await,
        "the claim must be left as it was, `claimed` by the dispatcher, not released",
    );
    pulled.wait_until_acked().await;
}

/// The claim is still `claimed`, by `worker-a`, the dispatcher's id.
fn assert_held_by_the_dispatcher(claim: TriggerClaim, why: &str) {
    assert!(
        matches!(&claim, TriggerClaim::Held { claimant, .. } if claimant == "worker-a"),
        "{why}; a fresh claim on the delivery got {claim:?}"
    );
}

/// The signal fires and the invocation completes in the same poll, and
/// the claim store refuses every mark (#966): a finished run, acked with
/// its claim kept ([`assert_acked_with_the_claim_kept`]).
#[tokio::test]
async fn a_failed_mark_after_a_completion_in_the_same_poll_acks_and_keeps_the_claim() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Same,
        Returns::Completed,
    )
    .await;

    pulled.handle().await;

    assert_acked_with_the_claim_kept(&mut pulled).await;
}

/// The signal fires and the invocation fails with a transient error in
/// the same poll, and the claim store refuses every mark (#966). Before
/// durable start a transient error is NAKed for a retry; this run had
/// started, so it is acked with its claim kept instead
/// ([`assert_acked_with_the_claim_kept`]).
#[tokio::test]
async fn a_failed_mark_after_a_transient_error_in_the_same_poll_acks_and_keeps_the_claim() {
    let mut pulled = Pulled::new(
        Resolve::FireIntoRefusedMarks(Refusals::Every),
        Turn::Same,
        Returns::TransientError,
    )
    .await;

    pulled.handle().await;

    assert_acked_with_the_claim_kept(&mut pulled).await;
}

/// The claim on this delivery's identity, read without writing.
async fn the_claim(pulled: &Pulled) -> Option<crate::control_plane::store::TriggerClaimRow> {
    pulled
        .claims
        .trigger_claim(test_key(&pulled.bus, pulled.stream_seq()))
        .await
        .expect("read the claim")
}

/// The signal fires on a run whose claim the mark finds as it should: it
/// names the run and is `claimed`. The claim ends recording the run's
/// start under the run's own id, and the delivery is acked once with
/// nothing else sent but keepalives (#1046: what the mark's report must
/// not change).
#[tokio::test]
async fn a_mark_that_finds_its_claim_marks_it_for_its_run_and_acks_once() {
    let mut pulled = Pulled::new(Resolve::Fire, Turn::Later, Returns::Completed).await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    let (_, run) = pulled.named();
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Started {
            invocation_id: run.to_string()
        },
        "the claim records the run's start under the run's own id"
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
}

/// **A mark that finds its claim gone writes one recording the start
/// (issue 1046).** The run made its first WAL write, and by the time the
/// dispatcher marks it its claim is gone, as a release an earlier copy
/// gave up on leaves it when it lands after this copy took the claim again
/// (issue 1061). The mark matches nothing. The run started, and the claim
/// is what refuses a copy of the trigger that comes back, so the claim the
/// mark expected is written, recording the run's start under its id, and
/// the delivery is acked once, releasing nothing. Before issue 1046 the
/// mark reported success and the delivery was acked with no claim at all,
/// so a lost ack or a replay of the stream started the trigger again.
#[tokio::test]
async fn a_mark_that_finds_its_claim_gone_writes_one_recording_the_start_and_acks_once() {
    let mut pulled = Pulled::with_ack_window(
        Resolve::FireOnAChangedClaim(ClaimChange::Gone),
        Turn::Later,
        Returns::Completed,
        SHORT_ACK_WINDOW,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on"
    );
    let (_, run) = pulled.named();
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Started {
            invocation_id: run.to_string()
        },
        "the run started with its claim gone, so a claim recording its start must be written, \
         and a copy of the trigger that comes back refused by it"
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
    assert_no_copy_comes_back(&mut pulled.messages, SHORT_ACK_WINDOW * 2).await;
}

/// **A mark that finds its claim naming another invocation, not started,
/// records this run's start on it (issue 1046).** The run started, and by
/// the time the dispatcher marks it the claim names another invocation,
/// still `claimed`: a copy that claimed the trigger afresh after this
/// copy's claim was deleted underneath it. That copy's run may never
/// start, and its release would then delete the trigger's only claim, so
/// this run's start is recorded on the claim in that invocation's place,
/// and the delivery is acked once. The mark did the same before issue 1046,
/// without saying so; the test pins that its report does not lose it.
#[tokio::test]
async fn a_mark_that_finds_another_invocations_claim_records_this_start_on_it() {
    let mut pulled = Pulled::new(
        Resolve::FireOnAChangedClaim(ClaimChange::ClaimedByAnother),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    let (_, run) = pulled.named();
    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Started {
            invocation_id: run.to_string()
        },
        "the claim must record this run's start, which the other copy's release cannot delete"
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
}

/// **A mark that finds another run's start on its claim leaves it (issue
/// 1046).** Two runs of the trigger have started, and the other's start is
/// on the claim already, which is all a copy that comes back needs to be
/// refused. The claim is left naming that run, where the mark before issue
/// 1046 moved it to this one without saying so, and the delivery is acked
/// once, releasing nothing.
#[tokio::test]
async fn a_mark_that_finds_another_runs_start_leaves_the_claim_and_acks_once() {
    let mut pulled = Pulled::new(
        Resolve::FireOnAChangedClaim(ClaimChange::StartedByAnother),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    assert_eq!(
        pulled.probe_claim().await,
        TriggerClaim::Started {
            invocation_id: ANOTHER_RUN.to_string()
        },
        "the claim records another run's start, and must be left naming it"
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
}

/// **A start that cannot be recorded on a gone claim is still acked once,
/// and the run runs on (issue 1046).** The claim is gone when the mark
/// comes, and the claim store refuses the write that would record the
/// start. The record is offered again on the mark's own schedule, and the
/// delivery is kept alive while it is: the pauses outlast the consumer's
/// ack window ([`SHORT_ACK_WINDOW`]). The run started, so the delivery is
/// then acked, keeping it from being redelivered and run again now, and the
/// run is not stopped; no claim records the start, which only the log line
/// says.
#[tokio::test]
async fn a_start_that_cannot_be_recorded_on_a_gone_claim_is_still_acked_once() {
    let mut pulled = Pulled::with_ack_window(
        Resolve::FireOnAChangedClaim(ClaimChange::GoneAndRefused),
        Turn::Later,
        Returns::Completed,
        SHORT_ACK_WINDOW,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    assert!(
        pulled.ran_to_end(),
        "the invocation was stopped instead of driven on"
    );
    assert_eq!(
        the_claim(&pulled).await,
        None,
        "the store refused every write, so no claim is left"
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
    assert!(
        sent.keepalives > 0,
        "the delivery was kept alive while the record was offered again: {sent:?}"
    );
    assert_no_copy_comes_back(&mut pulled.messages, SHORT_ACK_WINDOW * 2).await;
}

/// **A start that cannot be recorded on another invocation's claim leaves
/// that claim, and the delivery is acked once (issue 1046).** The claim
/// names another invocation, still `claimed`, when the mark comes, and the
/// claim store refuses the update that would record this run's start on
/// it. The delivery is acked keeping the claim: an ack that released it
/// would delete the other invocation's claim, since a release deletes a
/// `claimed` row whatever invocation it names.
#[tokio::test]
async fn a_start_that_cannot_be_recorded_on_another_invocations_claim_leaves_it() {
    let mut pulled = Pulled::new(
        Resolve::FireOnAChangedClaim(ClaimChange::ClaimedByAnotherAndRefused),
        Turn::Later,
        Returns::Completed,
    )
    .await;
    let mut answers = pulled.watch_answers().await;

    pulled.handle().await;

    let claim = the_claim(&pulled)
        .await
        .expect("the other invocation's claim must be left, not released");
    assert!(
        !claim.durably_started && claim.invocation_id.as_deref() == Some(ANOTHER_RUN),
        "the other invocation's claim is left as it was: {claim:?}"
    );
    pulled.wait_until_acked().await;
    let sent = answers.sent().await;
    assert!(
        sent.acks == 1 && sent.other.is_empty(),
        "the delivery is acked once, and nothing but keepalives is sent besides: {sent:?}"
    );
}

/// A claim store's refusal, for the schedule's own tests.
fn refusal() -> crate::control_plane::ControlPlaneStoreError {
    crate::control_plane::ControlPlaneStoreError::Backend("injected: refused".to_string())
}

/// **A write of a run's start that the store refuses once is offered again
/// and lands (issue 1046).** The mark and the record of a start the mark did
/// not find share one schedule ([`offer_with_retries`]), so this is the
/// record refused once and then accepted. It is staged on the schedule
/// rather than through the claim store: the record runs in a transaction,
/// which rolls back whatever a SQLite trigger refusing it would count, so a
/// trigger can refuse it always or never, not once. The mark's own
/// refused-then-accepted test drives the same schedule through `handle`.
#[tokio::test]
async fn a_write_of_a_start_refused_once_is_offered_again_and_lands() {
    let attempts = AtomicUsize::new(0);
    let key = crate::control_plane::TriggerKey {
        stream: TRIGGER_STREAM_NAME,
        stream_epoch: 0,
        stream_seq: 1,
    };

    let landed = offer_with_retries("record", key, Uuid::now_v7(), || async {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(refusal())
        } else {
            Ok("recorded")
        }
    })
    .await;

    let Ok(landed) = landed else {
        panic!("the second attempt was accepted, so the write lands");
    };
    assert_eq!(landed.value, "recorded");
    assert_eq!(landed.refused.len(), 1, "one refusal before it landed");
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "offered twice");
}

/// **A write of a run's start that the store refuses throughout is offered
/// four times within the budget, and then given up (issue 1046).** Four is
/// what [`MARK_RETRY_PAUSES`] gives a store that refuses at once, the last
/// attempt beginning inside [`MARK_RETRY_BUDGET`].
#[tokio::test]
async fn a_write_of_a_start_refused_throughout_is_offered_on_the_schedule_and_given_up() {
    let attempts = AtomicUsize::new(0);
    let key = crate::control_plane::TriggerKey {
        stream: TRIGGER_STREAM_NAME,
        stream_epoch: 0,
        stream_seq: 1,
    };

    let refused = offer_with_retries("record", key, Uuid::now_v7(), || async {
        attempts.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(refusal())
    })
    .await;

    let Err(Refused {
        attempts: offered,
        elapsed,
        ..
    }) = refused
    else {
        panic!("every attempt was refused, so the write is given up");
    };
    assert_eq!(offered, ATTEMPTS_IF_REFUSED_AT_ONCE as usize);
    assert_eq!(attempts.load(Ordering::SeqCst), offered);
    assert!(
        elapsed >= MARK_RETRY_PAUSES.iter().sum::<Duration>() && elapsed < MARK_RETRY_BUDGET,
        "the attempts follow the pauses and end inside the budget: {elapsed:?}"
    );
}
