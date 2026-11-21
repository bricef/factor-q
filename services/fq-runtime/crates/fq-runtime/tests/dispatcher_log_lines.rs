//! What the trigger dispatcher logs when it drops a copy of a trigger,
//! cannot record or acknowledge a run's durable start, cannot write the
//! run's ownership row at that start, cannot record a
//! trigger it dealt with as consumed, cannot publish a trigger's dead
//! letter, or resumes a deferred run that has already ended, read off the
//! log of a dispatcher running its own consume loop; and what the
//! dead-letter watch's check, and the watch itself on a claim consumed as
//! dead-lettered or past its limit, log about an exhausted trigger's
//! consumed claim.
//!
//! **Why a binary of its own (#1031).** Whether a callsite is of interest
//! to a subscriber is cached once for the whole process, not per thread.
//! The dispatcher's unit tests used to read these lines through a capture
//! installed on their own thread (`tracing::subscriber::set_default`). A
//! test that reached the same `warn!` or `error!` without a capture could
//! leave it cached as of no interest, and the capturing test that reached
//! it next saw no line and failed on an empty capture: 27 failing runs in
//! 40 at `--test-threads=4` when the archive sweeper's tests copied the
//! capture (#1022), and a red `main` on 2026-11-17. Here one subscriber is
//! installed for the whole process before any test logs, as
//! `consumer_log_rate.rs` does, so every callsite has one answer and it is
//! yes. The tests in this binary share it, and each reads only the lines
//! that name its own trigger.
//!
//! **What guarantees the order is the constructor**, not a habit of the
//! tests: `Daemon::new` installs the subscriber before it starts a broker
//! or connects anything, and a test reads the log through the daemon it
//! got back (`Daemon::log`). A test that needs the log before it has a
//! daemon calls `Log::installed` itself, first.
//!
//! **What is pinned here.** Each of these lines is the only record an
//! operator has of the case it reports, and its level and the fields that
//! name the delivery are what make it findable in a daemon's journal, so
//! that is what these tests read: the level, the fields, the clause a
//! last delivery adds, and that a case is one line, not one per attempt.
//! What the dispatcher decided — which arm a copy took, what it counted,
//! whether a mark landed, what it sent the broker — is pinned where the
//! decision is made, without reading a log
//! (`control_plane::dispatcher::duplicate_drop_tests` and
//! `durable_start_signal_tests`). One test here reads the broker instead:
//! that the consume loop's hand-over of an unpublishable dead letter raises
//! the broker's advisory at once, which only a test running the real loop
//! can show.
//!
//! The dispatcher runs as the daemon runs it, [`TriggerDispatcher::run`]
//! on its own durable consumer, against a private broker and a real claim
//! store. The worker is a stub that scripts what the run does up to its
//! first WAL write.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_nats::jetstream::{self, AckKind};
use fq_runtime::agent::{Agent, AgentId, AgentRegistry};
use fq_runtime::bus::{
    ADVISORY_STREAM_NAME, EventBus, NATS_DEFAULT_MAX_ACK_PENDING, STREAM_NAME, TRIGGER_MAX_DELIVER,
    TRIGGER_STREAM_NAME,
};
use fq_runtime::control_plane::dispatcher::{
    CONSUMER_NAME, ClaimMark, ConsumedClaim, Exhausted, ExhaustionCheck, TriggerClaims,
};
use fq_runtime::control_plane::store::{ClaimState, ConsumedWhy, TriggerKey};
use fq_runtime::control_plane::{
    AdvisoryWatch, AdvisoryWatchError, ControlPlaneStore, TriggerClaim,
};
use fq_runtime::llm::LlmClient;
use fq_runtime::llm::fixture::FixtureClient;
use fq_runtime::trigger::{PublishedTrigger, Trigger};
use fq_runtime::worker::{
    DrainRequest, DrainState, DurableStart, DurableStartReport, ExecutorError, InvocationOutcome,
    ResumeError, WalRun, Worker, WorkerStore,
};
use fq_runtime::{DispatcherError, TriggerDispatcher, shared_registry};
use futures::StreamExt;
use serde_json::json;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use tokio::sync::{Notify, oneshot};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use uuid::Uuid;

/// The start of the line for a copy dropped unanswered because it met a
/// claim this daemon holds with no start recorded while another copy of
/// the trigger is still handling it (`own_claim`).
const HELD_DROP: &str = "dropping a redelivered trigger whose claim this daemon already holds";
/// The clause the held drop adds on a trigger's last delivery.
const LAST_DELIVERY: &str = "this was its last delivery, so no copy follows";
/// The start of the line for a copy of a run whose start is on its claim.
const STARTED_DROP: &str = "dropping duplicate trigger delivery after durable start";
/// What the line for a delivery whose adoption found the row changed says.
const ADOPTION_LOST: &str = "the claim it met changed before this daemon could adopt it";
/// The start of the line for a claim of this daemon's own taken again.
const RETAKEN: &str = "taking again a trigger claim this daemon holds";
/// The start of the line for a mark that landed after refusals.
const MARK_HEALED: &str = "marked trigger durably started after the claim store refused";
/// The start of the line for a mark the claim store refused every time.
const MARK_GAVE_UP: &str = "could not mark trigger durably started";
/// The line for an ack the client could not send.
const ACK_FAILED: &str = "failed to ack trigger message";
/// The line for a claim's release the claim store refused.
const RELEASE_FAILED: &str = "failed to release unfinished trigger claim";
/// The start of the line for a release that found no claim of its own to
/// give back.
const RELEASED_NOTHING: &str = "released no trigger claim";
/// The start of the line for a consume the claim store refused.
const CONSUME_FAILED: &str = "failed to record a trigger claim consumed";
/// The start of the line for a consume that found no claim of its own to
/// record consumed.
const CONSUMED_NOTHING: &str = "recorded no trigger claim consumed";
/// The start of the line for a mark that found its claim gone and wrote
/// one recording the start.
const CLAIM_WRITTEN: &str =
    "the claim of a trigger whose run started was gone when its start was marked";
/// The start of the line for a mark that found another invocation's
/// claim, not started, and recorded this run's start on it.
const CLAIM_WRITTEN_OVER: &str =
    "the claim of a trigger whose run started named another invocation, not started";
/// The start of the line for a mark that found another run's start on the
/// claim, and left it.
const CLAIM_STARTED_BY_ANOTHER: &str =
    "the claim of a trigger whose run started already records another run's start";
/// The start of the line for a start the store would not record on a
/// claim the mark did not find.
const START_NOT_RECORDED: &str =
    "could not record the durable start of a trigger whose claim does not name its run";
/// The start of the line for a run's ownership row the store would not
/// write at its durable start.
const ROW_NOT_WRITTEN: &str = "could not write the run's ownership row at its durable start";
/// The start of the line for a run's ownership row that already recorded
/// the run ended when its durable start came to write it.
const ROW_ALREADY_ENDED: &str = "the run's ownership row already records it ended";
/// The agent id an operator's drop of a run with no row names in the
/// worker column ([`Script::DroppedRow`]).
const DROPPED_BY_AGENT: &str = "the-dropped-runs-agent";
/// The invocation a changed claim names ([`ClaimChange`]).
const ANOTHER_RUN: &str = "another-run";
/// The line for a deferred run whose resume found it already ended.
const ENDED_BEFORE_RESUME: &str = "deferred invocation ended before its resume; nothing to resume";
/// The start of the line for a last delivery whose dead letter the
/// dispatcher could not publish.
const DEAD_LETTER_UNPUBLISHED: &str =
    "could not publish the dead letter of a trigger whose last delivery failed";
/// The start of the line for a start found over a consumed claim.
const START_OVER_CONSUMED: &str =
    "the claim of a trigger whose run started recorded it consumed without a run";
/// The start of the line for a copy that met a consumed claim.
const CONSUMED_DROP: &str = "dropping a redelivered trigger that was consumed without a run";
/// The start of the line for an exhausted trigger whose claim said
/// consumed and whose run the watch or a requeue found on record.
const EXHAUSTED_OVER_CONSUMED: &str =
    "the claim of an exhausted trigger said it was consumed without a run";

/// How long a test waits for a line, a delivery or the dispatcher.
const PATIENCE: Duration = Duration::from_secs(20);

// ---- The log ----

/// One event the dispatcher logged at `info` or above: its level, its
/// message and every other field, each as the text a formatter prints.
#[derive(Debug, Clone)]
struct Line {
    level: tracing::Level,
    message: String,
    fields: BTreeMap<&'static str, String>,
}

impl Line {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.record(field, format!("{value:?}"));
    }
}

impl Line {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.insert(field.name(), value);
        }
    }
}

/// Every line logged at `info` or above, by every test in this binary.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Line>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Log {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = Line {
            level: *event.metadata().level(),
            message: String::new(),
            fields: BTreeMap::new(),
        };
        event.record(&mut line);
        self.0.lock().unwrap().push(line);
    }
}

impl Log {
    /// The process's one subscriber, installed on the first call.
    /// `Daemon::new` makes that call before anything else, so a test with
    /// a daemon cannot log ahead of it.
    fn installed() -> &'static Log {
        static LOG: OnceLock<Log> = OnceLock::new();
        LOG.get_or_init(|| {
            let log = Log::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(log.clone().with_filter(LevelFilter::INFO)),
            )
            .expect("install the process's one subscriber");
            log
        })
    }

    /// The lines so far that `wanted` picks out.
    fn lines(&self, wanted: impl Fn(&Line) -> bool) -> Vec<Line> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|line| wanted(line))
            .cloned()
            .collect()
    }

    /// Wait until a line `wanted` picks out has been logged.
    async fn wait_for(&self, what: &str, wanted: impl Fn(&Line) -> bool) {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while self.lines(&wanted).is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no line {what} within {PATIENCE:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// Whether `line` says `message` and names `subject`.
fn about(line: &Line, message: &str, subject: &str) -> bool {
    line.message.contains(message) && line.field("subject") == Some(subject)
}

// ---- The worker ----

/// What the stub run does after its worker permit, before its first WAL
/// write.
#[derive(Clone, Copy)]
enum Script {
    /// Nothing.
    Nothing,
    /// Sets up until the test opens the gate.
    GatedSetup,
    /// Makes the claim store refuse marks as the [`Refusals`] say.
    RefusedMarks(Refusals),
    /// Makes the claim store refuse every mark, and holds its write lock
    /// from another connection for this long, so the first attempt to
    /// mark blocks.
    LockedStore(Duration),
    /// Makes the claim store refuse every mark, and closes the connection
    /// the dispatcher answers its deliveries on.
    RefusedMarksAndClosedConnection,
    /// Fails with a transient error before its first WAL write.
    FailsTransiently,
    /// Changes the claim row as the [`ClaimChange`] says, so the mark
    /// meets a claim that does not name the run.
    ChangedClaim(ClaimChange),
    /// Makes the claim store refuse every delete of a claim row, then ends
    /// the run with a transient error before its first WAL write: the
    /// delivery is NAKed, and the claim's release before the NAK is
    /// refused.
    FailsBeforeStartWithReleasesRefused,
    /// Makes its first WAL write and puts the run down for a moment, as a
    /// rate limit does; its resume then finds the run already ended, as
    /// an operator's drop folded while it slept leaves it.
    DeferredThenEndedBeforeItsResume,
    /// Names [`ANOTHER_RUN`] on the claim, still `claimed`, then ends the
    /// run with a transient error before its first WAL write: the claim's
    /// release before the NAK finds no claim of its own to give back.
    FailsBeforeStartWithTheClaimTakenAway,
    /// Makes the claim store refuse every update of a claim row, then fails
    /// for good before its first WAL write: the claim's consume before the
    /// ack is refused.
    FailsForGoodWithConsumesRefused,
    /// Names [`ANOTHER_RUN`] on the claim, still `claimed`, then fails for
    /// good before its first WAL write: the claim's consume before the ack
    /// finds no claim of its own to record consumed.
    FailsForGoodWithTheClaimTakenAway,
    /// Makes the store refuse every new ownership row: the mark lands and
    /// the run's row is not written.
    RefusedRows,
    /// Folds an operator's drop of the run onto its ownership row, as the
    /// coordination consumer would between the run's first WAL write and
    /// the dispatcher's write of the row.
    DroppedRow,
    /// Writes the run an ownership row with a status this build cannot
    /// read, as a newer daemon sharing the store could.
    UnreadableRow,
}

/// How [`Script::ChangedClaim`] changes the claim before the mark.
#[derive(Clone, Copy)]
enum ClaimChange {
    /// Deletes it.
    Gone,
    /// Deletes it, and makes the store refuse every insert and update of a
    /// claim row.
    GoneAndRefused,
    /// Names [`ANOTHER_RUN`] on it, still `claimed`.
    ClaimedByAnother,
    /// Names [`ANOTHER_RUN`] on it, marked `durably_started`.
    StartedByAnother,
    /// Consumes it, naming the run.
    ConsumedUnderTheRun,
    /// Names [`ANOTHER_RUN`] on it and consumes it as dead-lettered, as a
    /// copy that took it again and ran out of deliveries would.
    ConsumedUnderAnother,
    /// Makes the store refuse the first mark, naming [`ANOTHER_RUN`] on the
    /// claim, still `claimed`, as it does: the retry then finds no claim
    /// naming the run.
    RefusedOnceThenAnother,
}

/// What the stub run fails with under [`Script::FailsTransiently`].
const TRANSIENT_FAILURE: &str = "simulated transient store outage";

/// Which attempts to mark the claim the store refuses.
#[derive(Clone, Copy)]
enum Refusals {
    /// Every one.
    Every,
    /// The first `n`.
    First(u32),
}

/// A worker whose run does what its [`Script`] says, then makes its first
/// WAL write (fires its durable-start signal) and returns.
struct Stub {
    script: Script,
    claim_db: PathBuf,
    /// The dispatcher's own connection.
    connection: async_nats::Client,
    /// The trigger's id and the invocation id the run was handed.
    named: Mutex<Option<(Uuid, Uuid)>>,
    started: Notify,
    gate: Notify,
    ended: Notify,
    /// What the next reads of the WAL answer, in order: `Err(())` for a
    /// read that fails. Once they are used up, every read fails, as the
    /// worker trait's own default does.
    wal_answers: Mutex<std::collections::VecDeque<Result<Option<WalRun>, ()>>>,
    /// How the next read of the WAL changes the claim before it answers:
    /// a change between two reads of the claim by whoever reads the WAL.
    change_at_wal_read: Mutex<Option<ClaimChange>>,
}

#[async_trait::async_trait]
impl Worker for Stub {
    async fn wal_run(&self, invocation_id: &str) -> Result<Option<WalRun>, ExecutorError> {
        let change = self.change_at_wal_read.lock().unwrap().take();
        if let Some(change) = change {
            change_the_claim(&self.claim_db, change).await;
        }
        match self.wal_answers.lock().unwrap().pop_front() {
            Some(Ok(answer)) => Ok(answer),
            Some(Err(())) | None => Err(ExecutorError::WorkerStore(format!(
                "the stub cannot say whether its WAL holds invocation {invocation_id}"
            ))),
        }
    }

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
        self.started.notify_one();
        match self.script {
            Script::Nothing => {}
            Script::GatedSetup => self.gate.notified().await,
            Script::RefusedMarks(refusals) => refuse_claim_marks(&self.claim_db, refusals).await,
            Script::LockedStore(held_for) => {
                refuse_claim_marks(&self.claim_db, Refusals::Every).await;
                lock_claim_store(&self.claim_db, held_for).await;
            }
            Script::RefusedMarksAndClosedConnection => {
                refuse_claim_marks(&self.claim_db, Refusals::Every).await;
                close(&self.connection).await;
            }
            Script::ChangedClaim(change) => change_the_claim(&self.claim_db, change).await,
            Script::RefusedRows => refuse_ownership_rows(&self.claim_db).await,
            Script::DroppedRow => drop_the_run(&self.claim_db, invocation_id).await,
            Script::UnreadableRow => write_an_unreadable_row(&self.claim_db, invocation_id).await,
            Script::FailsTransiently => {
                drop(durable_start);
                self.ended.notify_one();
                return Err(ExecutorError::WorkerStore(TRANSIENT_FAILURE.to_string()));
            }
            Script::FailsForGoodWithConsumesRefused | Script::FailsForGoodWithTheClaimTakenAway => {
                if matches!(self.script, Script::FailsForGoodWithConsumesRefused) {
                    refuse_claim_marks(&self.claim_db, Refusals::Every).await;
                } else {
                    change_the_claim(&self.claim_db, ClaimChange::ClaimedByAnother).await;
                }
                drop(durable_start);
                self.ended.notify_one();
                return Err(ExecutorError::InvocationFailed {
                    kind: fq_runtime::events::FailureKind::RuntimeError,
                    message: "simulated failure for good before the first WAL write".to_string(),
                });
            }
            Script::FailsBeforeStartWithReleasesRefused
            | Script::FailsBeforeStartWithTheClaimTakenAway => {
                if matches!(self.script, Script::FailsBeforeStartWithReleasesRefused) {
                    refuse_claim_releases(&self.claim_db).await;
                } else {
                    change_the_claim(&self.claim_db, ClaimChange::ClaimedByAnother).await;
                }
                drop(durable_start);
                self.ended.notify_one();
                return Err(ExecutorError::WorkerStore(
                    "simulated transient store outage before the first WAL write".to_string(),
                ));
            }
            Script::DeferredThenEndedBeforeItsResume => {
                durable_start.fire(0);
                tokio::task::yield_now().await;
                self.ended.notify_one();
                return Ok(InvocationOutcome::Deferred {
                    invocation_id,
                    resume_after: Duration::from_millis(20),
                });
            }
        }
        durable_start.fire(0);
        // One more turn of `handle`'s loop, which marks and acks on the
        // turn the signal fires, before the run returns.
        tokio::task::yield_now().await;
        self.ended.notify_one();
        // What the run returns past its durable start is not logged at
        // `warn`, so it does not matter to these lines.
        Ok(InvocationOutcome::Suspended { invocation_id })
    }

    /// A resume finds the run already ended: only
    /// [`Script::DeferredThenEndedBeforeItsResume`] puts a run down.
    async fn resume_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        invocation_id: Uuid,
    ) -> Result<InvocationOutcome, ExecutorError> {
        Err(ExecutorError::Resume(ResumeError::AlreadyTerminal {
            invocation_id,
        }))
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> DrainState {
        DrainState::Running
    }
}

impl Stub {
    fn named(&self) -> (Uuid, Uuid) {
        self.named.lock().unwrap().expect("the run started")
    }

    async fn wait_until_started(&self) {
        tokio::time::timeout(PATIENCE, self.started.notified())
            .await
            .expect("the run starts");
    }

    async fn wait_until_ended(&self) {
        tokio::time::timeout(PATIENCE, self.ended.notified())
            .await
            .expect("the run reaches its end");
    }
}

/// Close `connection`, and wait until it refuses a publish: an answer to
/// a delivery sent on it then fails in the client.
async fn close(connection: &async_nats::Client) {
    connection.drain().await.expect("drain the connection");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while connection
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
}

// ---- The claim store, through connections of the test's own ----

async fn claim_db_connection(claim_db: &Path) -> sqlx::SqliteConnection {
    SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file")
}

/// Make updates of a claim row fail from now on, as `refusals` says. The
/// mark is the one update on the path from a claim to its ack. A counted
/// refusal spends one row of `injected_mark_refusals`, and raises `FAIL`
/// so the spent row stays spent.
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
    let mut conn = claim_db_connection(claim_db).await;
    for statement in statements {
        sqlx::query(statement)
            .execute(&mut conn)
            .await
            .expect("install the refusal");
    }
    conn.close().await.expect("close the claim store's file");
}

/// Make every insert of an ownership row fail from now on.
async fn refuse_ownership_rows(claim_db: &Path) {
    let mut conn = claim_db_connection(claim_db).await;
    sqlx::query(
        "CREATE TRIGGER refuse_ownership_rows BEFORE INSERT ON coordination_invocation_owner \
         BEGIN SELECT RAISE(ABORT, 'injected: the store refuses the ownership row'); END",
    )
    .execute(&mut conn)
    .await
    .expect("install the refusal");
    conn.close().await.expect("close the claim store's file");
}

/// Write `invocation_id` an ownership row whose status this build does
/// not know: the store's checks admit it, and every read of it fails.
async fn write_an_unreadable_row(claim_db: &Path, invocation_id: Uuid) {
    let mut conn = claim_db_connection(claim_db).await;
    sqlx::query(
        "INSERT INTO coordination_invocation_owner \
         (invocation_id, worker_id, assigned_at, status, archive_state) \
         VALUES (?, 'a-newer-daemon', 1, 'a_status_from_a_newer_daemon', 'not_due')",
    )
    .bind(invocation_id.to_string())
    .execute(&mut conn)
    .await
    .expect("write the row");
    conn.close().await.expect("close the claim store's file");
}

/// Fold an operator's drop of `invocation_id` onto its ownership row, as
/// the coordination consumer folds one for a run with no row: the agent's
/// id stands in the worker column.
async fn drop_the_run(claim_db: &Path, invocation_id: Uuid) {
    use fq_runtime::control_plane::ownership::{
        ArchiveExpected, OwnerFacts, OwnershipEvent, apply,
    };
    use fq_runtime::control_plane::store::TerminationRequest;
    let store = ControlPlaneStore::open(claim_db)
        .await
        .expect("open the claim store's file");
    let request = TerminationRequest {
        requested_at: 2,
        requested_by: Some("an-operator".to_string()),
        reason: None,
    };
    apply(
        &store,
        &invocation_id.to_string(),
        OwnershipEvent::TerminationRequested(ArchiveExpected::Yes),
        OwnerFacts {
            worker_id: DROPPED_BY_AGENT,
            at_ms: 2,
            ended_at_ms: 2,
            request: Some(&request),
        },
    )
    .await
    .expect("fold the drop");
}

/// Make every delete of a claim row fail from now on: the release is the
/// one delete on the path from a claim to its NAK.
async fn refuse_claim_releases(claim_db: &Path) {
    let mut conn = claim_db_connection(claim_db).await;
    sqlx::query(
        "CREATE TRIGGER refuse_claim_releases BEFORE DELETE ON trigger_claim \
         BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the release'); END",
    )
    .execute(&mut conn)
    .await
    .expect("install the refusal");
    conn.close().await.expect("close the claim store's file");
}

/// Change the one claim row as `change` says.
async fn change_the_claim(claim_db: &Path, change: ClaimChange) {
    let mut conn = claim_db_connection(claim_db).await;
    let refuse = [
        "CREATE TRIGGER refuse_claim_inserts BEFORE INSERT ON trigger_claim \
         BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the write'); END",
        "CREATE TRIGGER refuse_claim_marks BEFORE UPDATE ON trigger_claim \
         BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the mark'); END",
    ];
    let (statement, then): (&'static str, &[&'static str]) = match change {
        ClaimChange::Gone => ("DELETE FROM trigger_claim", &[]),
        ClaimChange::GoneAndRefused => ("DELETE FROM trigger_claim", &refuse),
        ClaimChange::ClaimedByAnother => ("UPDATE trigger_claim SET invocation_id = ?", &[]),
        ClaimChange::StartedByAnother => (
            "UPDATE trigger_claim SET invocation_id = ?, state = 'durably_started'",
            &[],
        ),
        ClaimChange::ConsumedUnderTheRun => (
            "UPDATE trigger_claim SET state = 'consumed', consumed_why = 'failed_for_good'",
            &[],
        ),
        ClaimChange::ConsumedUnderAnother => (
            "UPDATE trigger_claim SET invocation_id = ?, state = 'consumed', \
             consumed_why = 'dead_lettered'",
            &[],
        ),
        // `FAIL` keeps what the trigger did before it, the spent row and the
        // other invocation's name, where `ABORT` would roll them back.
        ClaimChange::RefusedOnceThenAnother => (
            "CREATE TABLE injected_once (spent INTEGER)",
            &[
                "INSERT INTO injected_once VALUES (0)",
                "CREATE TRIGGER refuse_once_naming_another BEFORE UPDATE ON trigger_claim \
                 WHEN EXISTS (SELECT 1 FROM injected_once) \
                 BEGIN DELETE FROM injected_once; \
                 UPDATE trigger_claim SET invocation_id = 'another-run'; \
                 SELECT RAISE(FAIL, 'injected: the claim store refuses the mark once'); END",
            ],
        ),
    };
    let query = sqlx::query(statement);
    let query = if statement.contains('?') {
        query.bind(ANOTHER_RUN)
    } else {
        query
    };
    query.execute(&mut conn).await.expect("change the claim");
    for statement in then {
        sqlx::query(*statement)
            .execute(&mut conn)
            .await
            .expect("install the refusal");
    }
    conn.close().await.expect("close the claim store's file");
}

/// Hold the claim store's write lock for `held_for`, from a task of its
/// own. A write meanwhile waits in the store's busy handler.
async fn lock_claim_store(claim_db: &Path, held_for: Duration) {
    let mut conn = claim_db_connection(claim_db).await;
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

/// Make every update of a claim row skip its row: the update succeeds and
/// changes nothing, which is what an adoption's compare-and-set sees when
/// the row is no longer the one it read.
async fn skip_every_claim_update(claim_db: &Path) {
    let mut conn = claim_db_connection(claim_db).await;
    sqlx::query(
        "CREATE TRIGGER skip_claim_updates BEFORE UPDATE ON trigger_claim \
         BEGIN SELECT RAISE(IGNORE); END",
    )
    .execute(&mut conn)
    .await
    .expect("install the skip");
    conn.close().await.expect("close the claim store's file");
}

// ---- The daemon ----

/// A private broker, one agent, a claim store, and a dispatcher that runs
/// its consume loop on them once [`Daemon::start`] is called.
struct Daemon {
    /// The process's one subscriber, installed before anything below was
    /// started.
    log: &'static Log,
    bus: EventBus,
    url: String,
    agent: AgentId,
    /// The agent's trigger subject, which every line about its triggers
    /// names except the started drop.
    subject: String,
    worker: Arc<Stub>,
    claims: Arc<ControlPlaneStore>,
    claim_db: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    running: Option<tokio::task::JoinHandle<Result<(), DispatcherError>>>,
    dirs: tempfile::TempDir,
    _server: fq_test_support::NatsServer,
}

impl Daemon {
    async fn new(label: &str, script: Script) -> Self {
        // First, before anything here can log (the module doc says why).
        let log = Log::installed();
        let server = fq_test_support::NatsServer::start();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let name = format!("log-{label}-{}", Uuid::now_v7().simple());
        let agent = AgentId::new(&name).expect("a legal agent id");
        let dirs = tempfile::tempdir().unwrap();
        let claim_db = dirs.path().join("control-plane.db");
        let claims = Arc::new(ControlPlaneStore::open(&claim_db).await.unwrap());
        let worker = Arc::new(Stub {
            script,
            claim_db: claim_db.clone(),
            connection: bus.jetstream().client(),
            named: Mutex::new(None),
            started: Notify::new(),
            gate: Notify::new(),
            ended: Notify::new(),
            wal_answers: Mutex::default(),
            change_at_wal_read: Mutex::default(),
        });
        Self {
            log,
            subject: fq_runtime::events::subjects::trigger(&name),
            bus,
            url,
            agent,
            worker,
            claims,
            claim_db,
            shutdown: None,
            running: None,
            dirs,
            _server: server,
        }
    }

    async fn publish(&self) -> PublishedTrigger {
        self.bus
            .publish_trigger(&self.agent, &json!({"input": "hi"}))
            .await
            .expect("publish")
    }

    /// The claim key the dispatcher uses for `stream_seq`.
    fn key(&self, stream_seq: u64) -> TriggerKey<'static> {
        TriggerKey {
            stream: TRIGGER_STREAM_NAME,
            stream_epoch: self.bus.trigger_stream_epoch(),
            stream_seq,
        }
    }

    /// Start the dispatcher's consume loop, claiming as `claimant`, with
    /// its claims recovered as the daemon recovers them, over an empty WAL.
    async fn start(&mut self, claimant: &str) {
        let dispatcher = self.dispatcher(claimant).await;
        let (shutdown, stop) = oneshot::channel();
        self.shutdown = Some(shutdown);
        self.running = Some(tokio::spawn(dispatcher.run(stop)));
    }

    /// What the dead-letter watch consults about this daemon's triggers:
    /// the claim store and the stub worker, through a dispatcher that is
    /// not running.
    async fn exhaustion_check(&self, claimant: &str) -> ExhaustionCheck {
        self.dispatcher(claimant)
            .await
            .exhaustion_check()
            .expect("the dispatcher holds a claim store")
    }

    /// The dispatcher [`Self::start`] runs, claiming as `claimant`.
    async fn dispatcher(&self, claimant: &str) -> TriggerDispatcher {
        let agents = self.dirs.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let definition = agents.join(format!("{}.md", self.agent));
        std::fs::write(
            &definition,
            format!(
                "---\nname: {}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent.",
                self.agent
            ),
        )
        .unwrap();
        let mut registry = AgentRegistry::new();
        registry.load_file(&definition);
        assert!(registry.errors().is_empty(), "{:?}", registry.errors());
        let wal = WorkerStore::open(&self.dirs.path().join("worker.db"))
            .await
            .unwrap();
        let starts = DurableStartReport::scan(&wal).await.unwrap();
        let claims = TriggerClaims::recover(self.claims.clone(), &starts)
            .await
            .expect("recover the claims");
        TriggerDispatcher::new(
            self.bus.clone(),
            shared_registry(registry),
            self.worker.clone() as Arc<dyn Worker>,
            Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
            1,
        )
        .with_trigger_claims(claims, claimant)
    }

    /// Stop the consume loop and wait for every delivery it was handling,
    /// so every line they log is in.
    async fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            // The loop may have ended already, on a closed connection.
            let _ = shutdown.send(());
        }
        let running = self.running.take().expect("the dispatcher was started");
        tokio::time::timeout(PATIENCE, running)
            .await
            .expect("the dispatcher stops")
            .expect("the dispatcher task joins")
            .expect("the dispatcher ran without error");
    }

    /// Spend all but the last of the published trigger's deliveries on the
    /// dispatcher's own durable before the dispatcher runs: each is taken
    /// here and NAKed with no delay, so the first delivery the dispatcher
    /// gets is the trigger's last.
    async fn spend_all_but_the_last_delivery(&self) {
        let consumer = self
            .bus
            .trigger_consumer(CONSUMER_NAME, NATS_DEFAULT_MAX_ACK_PENDING)
            .await
            .expect("the dispatcher's durable");
        for delivered in 1..TRIGGER_MAX_DELIVER {
            let mut batch = consumer
                .batch()
                .max_messages(1)
                .expires(PATIENCE)
                .messages()
                .await
                .expect("pull a delivery");
            let delivery = batch
                .next()
                .await
                .expect("a delivery within the pull's expiry")
                .expect("a delivery");
            assert_eq!(delivery.info().unwrap().delivered, delivered);
            delivery
                .ack_with(AckKind::Nak(None))
                .await
                .expect("redeliver the trigger");
        }
    }

    /// Publish a trigger, spend all but its last delivery
    /// ([`Self::spend_all_but_the_last_delivery`]) and delete the events
    /// stream. Once the consume loop starts, the dispatcher's first delivery
    /// is the trigger's last; with [`Script::FailsTransiently`] its run fails
    /// before its first WAL write and its dead letter finds no stream, so the
    /// dispatcher hands the trigger to the dead-letter watch.
    async fn an_unpublishable_last_delivery(&self) -> PublishedTrigger {
        let published = self.publish().await;
        self.spend_all_but_the_last_delivery().await;
        self.bus
            .jetstream()
            .delete_stream(STREAM_NAME)
            .await
            .expect("delete the events stream");
        published
    }

    /// Run one trigger whose run follows the worker's script, and return
    /// its trigger id and invocation id once every line is in.
    async fn run_one(&mut self) -> (Uuid, Uuid) {
        self.start("binary-1").await;
        self.publish().await;
        self.worker.wait_until_ended().await;
        self.stop().await;
        self.worker.named()
    }
}

/// Every delivery the broker makes to the dispatcher's consumer, seen by
/// a connection of the test's own. The dispatcher pulls into an inbox,
/// and the broker hands a message published to a subject to every
/// subscriber on it, so this sees each delivery the dispatcher gets, and
/// answering one here answers that delivery.
struct Deliveries {
    context: jetstream::Context,
    inboxes: async_nats::Subscriber,
}

impl Deliveries {
    async fn watch(url: &str) -> Self {
        let client = async_nats::connect(url).await.expect("connect a watcher");
        let inboxes = client.subscribe("_INBOX.>").await.expect("watch inboxes");
        client
            .flush()
            .await
            .expect("the watch is in place before anything is delivered");
        Self {
            context: jetstream::new(client),
            inboxes,
        }
    }

    async fn next(&mut self) -> jetstream::Message {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let message = tokio::time::timeout_at(deadline, self.inboxes.next())
                .await
                .expect("a delivery to the dispatcher")
                .expect("the watch is open");
            let delivery = jetstream::Message {
                message,
                context: self.context.clone(),
            };
            let ours = delivery.info().is_ok_and(|info| {
                info.stream == TRIGGER_STREAM_NAME && info.consumer == CONSUMER_NAME
            });
            if ours {
                return delivery;
            }
        }
    }
}

// ---- The lines ----

/// **Each copy dropped while its first copy sets up is one `warn` line,
/// which names the delivery, its attempt, its subject and the claim it
/// met; the last delivery's says it was the last** (#968).
///
/// The first copy parks in its setup. Its delivery is NAKed from here,
/// once per attempt, so the broker redelivers it to the dispatcher until
/// the bound. Each copy meets the claim the first copy holds, and is
/// dropped because the first copy is still handling it. The gate then
/// opens and the first copy starts and acks.
#[tokio::test(flavor = "multi_thread")]
async fn each_held_copy_is_one_warn_line_naming_its_delivery_and_the_last_says_so() {
    let mut daemon = Daemon::new("held", Script::GatedSetup).await;
    let log = daemon.log;
    let mut deliveries = Deliveries::watch(&daemon.url).await;
    daemon.start("binary-1").await;
    let published = daemon.publish().await;
    let mut latest = deliveries.next().await;
    daemon.worker.wait_until_started().await;
    let (_, named) = daemon.worker.named();

    for delivered in 2..=TRIGGER_MAX_DELIVER {
        latest
            .ack_with(AckKind::Nak(None))
            .await
            .expect("redeliver the trigger");
        latest = deliveries.next().await;
        assert_eq!(latest.info().unwrap().delivered, delivered);
        let attempt = delivered.to_string();
        log.wait_for(
            &format!("for the held copy of delivery {delivered}"),
            |line| {
                about(line, HELD_DROP, &daemon.subject)
                    && line.field("delivered") == Some(attempt.as_str())
            },
        )
        .await;
    }
    daemon.worker.gate.notify_one();
    daemon.worker.wait_until_ended().await;
    daemon.stop().await;

    let held = log.lines(|line| about(line, HELD_DROP, &daemon.subject));
    assert_eq!(
        held.len(),
        (TRIGGER_MAX_DELIVER - 1) as usize,
        "one line per dropped copy: {held:#?}"
    );
    for line in &held {
        assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
        assert_eq!(
            line.field("stream_seq"),
            Some(published.stream_seq.to_string().as_str())
        );
        assert_eq!(line.field("claimant"), Some("binary-1"), "{line:#?}");
        assert_eq!(
            line.field("claim_invocation_id"),
            Some(named.to_string().as_str()),
            "the line names the invocation the claim names: {line:#?}"
        );
        let age = line.field("claim_age_ms").expect("the claim's age");
        assert!(
            age.parse::<u64>().is_ok(),
            "an age in milliseconds: {line:#?}"
        );
        let last = line.field("delivered") == Some(TRIGGER_MAX_DELIVER.to_string().as_str());
        assert_eq!(
            line.message.contains(LAST_DELIVERY),
            last,
            "only the last delivery's line says it was the last: {line:#?}"
        );
    }
    assert!(
        log.lines(|line| line.message.starts_with(STARTED_DROP)
            && line.field("started_invocation_id") == Some(named.to_string().as_str()))
            .is_empty(),
        "no held copy is reported as a copy of a started run"
    );
}

/// **A delivery whose adoption of a dead process's claim finds the row
/// changed is one `warn` line, which names the delivery and the claim it
/// met.** Nothing else records it: it is not counted.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_adoption_is_one_warn_line_naming_the_delivery_and_the_claim_it_met() {
    let mut daemon = Daemon::new("adoption-lost", Script::Nothing).await;
    let log = daemon.log;
    let published = daemon.publish().await;
    let left_behind = Uuid::now_v7().to_string();
    assert_eq!(
        daemon
            .claims
            .claim_trigger(
                daemon.key(published.stream_seq),
                "binary-1",
                &left_behind,
                0
            )
            .await
            .unwrap(),
        TriggerClaim::Won,
        "a process that has exited left the claim"
    );
    skip_every_claim_update(&daemon.claim_db).await;
    daemon.start("binary-2").await;
    log.wait_for("for the lost adoption", |line| {
        about(line, ADOPTION_LOST, &daemon.subject)
    })
    .await;
    daemon.stop().await;

    let lost = log.lines(|line| about(line, ADOPTION_LOST, &daemon.subject));
    assert_eq!(
        lost.len(),
        1,
        "one line for the dropped delivery: {lost:#?}"
    );
    let line = &lost[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    for (field, value) in [
        ("stream_seq", published.stream_seq.to_string()),
        ("delivered", "1".to_string()),
        ("claimant", "binary-1".to_string()),
        ("claim_invocation_id", left_behind),
    ] {
        assert_eq!(
            line.field(field),
            Some(value.as_str()),
            "`{field}` in {line:#?}"
        );
    }
    let age = line.field("claim_age_ms").expect("the claim's age");
    assert!(
        age.parse::<u64>().is_ok(),
        "an age in milliseconds: {line:#?}"
    );
}

/// **A claim of this daemon's own that no copy is handling and whose run
/// has nothing on record is taken again with one `warn` line naming the
/// abandoned invocation** (issues 967 and 1055). The guide tells an
/// operator to match that invocation to the error line of the failure
/// before the re-take, so its `abandoned_invocation_id` is what the line is
/// for. The claim is left `claimed` under this daemon's own claimant, as a
/// copy whose task died, or whose release or consume failed before an
/// answer that was then lost, leaves it; the WAL holds nothing for that
/// run and it has no ownership row. The copy takes the claim again under a
/// new invocation id and runs the trigger.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_taken_again_is_one_warn_line_naming_the_abandoned_invocation() {
    let mut daemon = Daemon::new("retaken", Script::Nothing).await;
    let log = daemon.log;
    let published = daemon.publish().await;
    let abandoned = Uuid::now_v7().to_string();
    assert_eq!(
        daemon
            .claims
            .claim_trigger(daemon.key(published.stream_seq), "binary-1", &abandoned, 0)
            .await
            .unwrap(),
        TriggerClaim::Won,
        "a copy of this daemon's left the claim"
    );
    daemon
        .worker
        .wal_answers
        .lock()
        .unwrap()
        .push_back(Ok(None));
    daemon.start("binary-1").await;
    log.wait_for("for the re-take", |line| {
        about(line, RETAKEN, &daemon.subject)
    })
    .await;
    daemon.stop().await;

    let lines = log.lines(|line| about(line, RETAKEN, &daemon.subject));
    assert_eq!(lines.len(), 1, "one line for the re-take: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    assert_eq!(
        line.field("abandoned_invocation_id"),
        Some(abandoned.as_str()),
        "the line names the invocation the claim was abandoned under: {line:#?}"
    );
    assert!(
        line.field("invocation_id")
            .is_some_and(|retaken| retaken != abandoned),
        "and the new one it is taken again under: {line:#?}"
    );
    assert_eq!(
        line.field("stream_seq"),
        Some(published.stream_seq.to_string().as_str()),
        "{line:#?}"
    );
}

/// **A copy of a run whose start is on its claim is one `warn` line, which
/// names the delivery and the run that started**, and is not reported as
/// a held copy.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_of_a_started_run_is_one_warn_line_naming_the_start() {
    let mut daemon = Daemon::new("copy-after-start", Script::Nothing).await;
    let log = daemon.log;
    let published = daemon.publish().await;
    let started = Uuid::now_v7().to_string();
    let key = daemon.key(published.stream_seq);
    daemon
        .claims
        .claim_trigger(key, "binary-0", &started, 0)
        .await
        .unwrap();
    daemon
        .claims
        .mark_trigger_started(key, &started)
        .await
        .unwrap();
    daemon.start("binary-1").await;
    let names_the_start = |line: &Line| {
        line.message.starts_with(STARTED_DROP)
            && line.field("started_invocation_id") == Some(started.as_str())
    };
    log.wait_for("for the copy of a started run", names_the_start)
        .await;
    daemon.stop().await;

    let dropped = log.lines(names_the_start);
    assert_eq!(dropped.len(), 1, "one line for the copy: {dropped:#?}");
    let line = &dropped[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    assert_eq!(
        line.field("stream_seq"),
        Some(published.stream_seq.to_string().as_str()),
        "{line:#?}"
    );
    assert_eq!(line.field("delivered"), Some("1"), "{line:#?}");
    assert!(
        log.lines(|line| about(line, HELD_DROP, &daemon.subject))
            .is_empty(),
        "a copy of a started run is not reported as held"
    );
}

/// **A mark the claim store refused twice and then took is one `warn`
/// line, which names the run, the trigger and the refusals**, and nothing
/// says the mark was given up (#967).
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_that_heals_inside_its_retries_is_one_warn_line() {
    let mut daemon = Daemon::new("mark-heals", Script::RefusedMarks(Refusals::First(2))).await;
    let log = daemon.log;

    let (trigger_id, invocation_id) = daemon.run_one().await;

    let healed = log.lines(|line| about(line, MARK_HEALED, &daemon.subject));
    assert_eq!(healed.len(), 1, "the heal is logged once: {healed:#?}");
    let line = &healed[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    assert_eq!(line.field("refused"), Some("2"), "{line:#?}");
    assert_eq!(
        line.field("invocation_id"),
        Some(invocation_id.to_string().as_str())
    );
    assert_eq!(
        line.field("trigger_id"),
        Some(trigger_id.to_string().as_str())
    );
    assert!(
        log.lines(|line| about(line, MARK_GAVE_UP, &daemon.subject))
            .is_empty(),
        "the mark landed, so nothing says it was given up"
    );
}

/// **A mark the claim store refused on every attempt is one `error` line,
/// not one per attempt, which names the run and the trigger** (#967).
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_refused_past_its_retries_is_one_error_line_naming_the_run_and_the_trigger() {
    let mut daemon = Daemon::new("mark-gives-up", Script::RefusedMarks(Refusals::Every)).await;
    let log = daemon.log;

    let (trigger_id, invocation_id) = daemon.run_one().await;

    let gave_up = log.lines(|line| about(line, MARK_GAVE_UP, &daemon.subject));
    assert_eq!(gave_up.len(), 1, "the give-up is logged once: {gave_up:#?}");
    let line = &gave_up[0];
    assert_eq!(line.level, tracing::Level::ERROR, "{line:#?}");
    assert_eq!(
        line.field("invocation_id"),
        Some(invocation_id.to_string().as_str())
    );
    assert_eq!(
        line.field("trigger_id"),
        Some(trigger_id.to_string().as_str())
    );
}

/// **A mark whose one attempt blocked past the retries' time bound gives
/// up saying it had one attempt** (#967). Another connection holds the
/// claim store's write lock for longer than the retries are allowed (two
/// seconds, `MARK_RETRY_BUDGET`) and shorter than the store's busy timeout
/// (sqlx's five), so the first attempt waits it out and is refused, and no
/// second is offered.
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_that_blocks_past_the_time_bound_says_it_had_one_attempt() {
    let mut daemon = Daemon::new(
        "mark-blocks",
        Script::LockedStore(Duration::from_millis(2500)),
    )
    .await;
    let log = daemon.log;

    daemon.run_one().await;

    let gave_up = log.lines(|line| about(line, MARK_GAVE_UP, &daemon.subject));
    assert_eq!(gave_up.len(), 1, "the give-up is logged once: {gave_up:#?}");
    assert_eq!(
        gave_up[0].field("attempts"),
        Some("1"),
        "the give-up line says one attempt was offered: {:#?}",
        gave_up[0]
    );
}

/// **An ack that cannot be sent, after a mark refused on every attempt, is
/// one `error` line naming the trigger**, beside the one line for the
/// refused mark (#967). It is what an operator has to go on for a started
/// run whose delivery will come back, to be settled by the run's start on
/// record.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_ack_after_a_refused_mark_is_one_error_line_naming_the_trigger() {
    let mut daemon = Daemon::new("ack-fails", Script::RefusedMarksAndClosedConnection).await;
    let log = daemon.log;

    let (trigger_id, _) = daemon.run_one().await;

    let failed = log.lines(|line| about(line, ACK_FAILED, &daemon.subject));
    assert_eq!(
        failed.len(),
        1,
        "the failed ack is logged once: {failed:#?}"
    );
    assert_eq!(failed[0].level, tracing::Level::ERROR, "{:#?}", failed[0]);
    assert_eq!(
        failed[0].field("trigger_id"),
        Some(trigger_id.to_string().as_str()),
        "{:#?}",
        failed[0]
    );
    assert_eq!(
        log.lines(|line| about(line, MARK_GAVE_UP, &daemon.subject))
            .len(),
        1,
        "the refused mark is logged once"
    );
}

/// The one line `message` names for `daemon`'s trigger, checked to be an
/// `error` that names the run, the trigger and the sequence, and that no
/// line says the mark was given up: the mark landed, matching nothing.
fn the_one_error_line_naming_the_run(
    daemon: &Daemon,
    message: &str,
    (trigger_id, invocation_id): (Uuid, Uuid),
) -> Line {
    let lines = daemon
        .log
        .lines(|line| about(line, message, &daemon.subject));
    assert_eq!(lines.len(), 1, "the case is logged once: {lines:#?}");
    let line = lines[0].clone();
    assert_eq!(line.level, tracing::Level::ERROR, "{line:#?}");
    assert_eq!(
        line.field("invocation_id"),
        Some(invocation_id.to_string().as_str()),
        "{line:#?}"
    );
    assert_eq!(
        line.field("trigger_id"),
        Some(trigger_id.to_string().as_str()),
        "{line:#?}"
    );
    assert!(line.field("stream_seq").is_some(), "{line:#?}");
    assert!(
        daemon
            .log
            .lines(|line| about(line, MARK_GAVE_UP, &daemon.subject))
            .is_empty(),
        "the mark was not refused, so nothing says it was given up"
    );
    line
}

/// **A mark that found its claim gone, and wrote one recording the start,
/// is one `error` line naming the run, the trigger and the sequence**
/// (issue 1046). Something deleted a claim that a run which started still
/// held, and the line is the only record of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_that_found_its_claim_gone_is_one_error_line_naming_the_run() {
    let mut daemon = Daemon::new("claim-gone", Script::ChangedClaim(ClaimChange::Gone)).await;
    let named = daemon.run_one().await;

    the_one_error_line_naming_the_run(&daemon, CLAIM_WRITTEN, named);
}

/// **A mark that found another invocation's claim, not started, and
/// recorded this run's start on it, is one `error` line naming both
/// invocations** (issue 1046).
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_that_wrote_over_another_invocations_claim_is_one_error_line_naming_both() {
    let mut daemon = Daemon::new(
        "claim-of-another",
        Script::ChangedClaim(ClaimChange::ClaimedByAnother),
    )
    .await;
    let named = daemon.run_one().await;

    let line = the_one_error_line_naming_the_run(&daemon, CLAIM_WRITTEN_OVER, named);
    assert_eq!(
        line.field("claim_invocation_id"),
        Some(ANOTHER_RUN),
        "{line:#?}"
    );
}

/// **A mark that found another run's start on the claim is one `error`
/// line naming both runs**, since the trigger has run twice (issue 1046).
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_that_found_another_runs_start_is_one_error_line_naming_both() {
    let mut daemon = Daemon::new(
        "started-by-another",
        Script::ChangedClaim(ClaimChange::StartedByAnother),
    )
    .await;
    let named = daemon.run_one().await;

    let line = the_one_error_line_naming_the_run(&daemon, CLAIM_STARTED_BY_ANOTHER, named);
    assert_eq!(
        line.field("started_invocation_id"),
        Some(ANOTHER_RUN),
        "{line:#?}"
    );
}

/// **A start the claim store would not record, on a claim the mark did not
/// find, is one `error` line naming the run and the store's error** (issue
/// 1046). No claim records the run's start, and this line is what says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_that_could_not_be_recorded_is_one_error_line_naming_the_run() {
    let mut daemon = Daemon::new(
        "start-not-recorded",
        Script::ChangedClaim(ClaimChange::GoneAndRefused),
    )
    .await;
    let named = daemon.run_one().await;

    let line = the_one_error_line_naming_the_run(&daemon, START_NOT_RECORDED, named);
    assert!(
        line.field("error")
            .is_some_and(|error| error.contains("injected")),
        "{line:#?}"
    );
    assert_eq!(
        line.field("attempts"),
        Some("4"),
        "the record is offered on the mark's own schedule, four times for a store that \
         refuses at once: {line:#?}"
    );
    assert_eq!(line.field("refused_marks"), Some("0"), "{line:#?}");
}

/// **A mark refused once whose retry then finds no claim naming its run is
/// not logged as marked** (issue 1046). The first attempt is refused, and by
/// the retry the claim names another invocation. Nothing says the mark
/// landed: the one line is the unmarked start's, and it carries the
/// refused attempt.
#[tokio::test(flavor = "multi_thread")]
async fn a_mark_refused_then_matching_nothing_is_not_logged_as_marked() {
    let mut daemon = Daemon::new(
        "refused-then-unmatched",
        Script::ChangedClaim(ClaimChange::RefusedOnceThenAnother),
    )
    .await;
    let named = daemon.run_one().await;

    assert!(
        daemon
            .log
            .lines(|line| about(line, MARK_HEALED, &daemon.subject))
            .is_empty(),
        "the mark found no claim naming its run, so nothing may say it was marked"
    );
    let line = the_one_error_line_naming_the_run(&daemon, CLAIM_WRITTEN_OVER, named);
    assert_eq!(line.field("refused_marks"), Some("1"), "{line:#?}");
    assert_eq!(line.field("refused_records"), Some("0"), "{line:#?}");
}

/// **A last delivery whose dead letter cannot be published is one `error`
/// line, which names the trigger and carries the error that ended the
/// delivery** (issue 1062). The dispatcher NAKs such a delivery instead of
/// acking it, and the dead-letter watch writes the dead letter from the
/// broker's advisory; that dead letter does not carry the run's error, so
/// this line is the one record of it that names the trigger. Its
/// `stream_seq` is what matches it to that dead letter; its `trigger_id`
/// does too when the publisher named the trigger, as this test's does, and
/// otherwise names the id the dispatcher gave the delivery, which no dead
/// letter carries.
///
/// The trigger is set up by [`Daemon::an_unpublishable_last_delivery`].
#[tokio::test(flavor = "multi_thread")]
async fn an_unpublished_dead_letter_is_one_error_line_naming_the_trigger_and_its_last_error() {
    let mut daemon = Daemon::new("dead-letter-unpublished", Script::FailsTransiently).await;
    let log = daemon.log;
    let published = daemon.an_unpublishable_last_delivery().await;
    daemon.start("binary-1").await;
    let subject = daemon.subject.clone();
    let unpublished = |line: &Line| about(line, DEAD_LETTER_UNPUBLISHED, &subject);
    log.wait_for("for the unpublished dead letter", unpublished)
        .await;
    daemon.stop().await;

    let lines = log.lines(unpublished);
    assert_eq!(lines.len(), 1, "one line for the trigger: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::ERROR, "{line:#?}");
    for (field, value) in [
        ("agent_id", daemon.agent.to_string()),
        ("stream_seq", published.stream_seq.to_string()),
        ("trigger_id", published.id.to_string()),
        ("delivery_attempt", TRIGGER_MAX_DELIVER.to_string()),
    ] {
        assert_eq!(
            line.field(field),
            Some(value.as_str()),
            "`{field}` in {line:#?}"
        );
    }
    assert!(
        line.field("last_error")
            .is_some_and(|last| last.contains(TRANSIENT_FAILURE)),
        "the line carries the error that ended the last delivery: {line:#?}"
    );
    assert!(
        line.field("error").is_some_and(|error| !error.is_empty()),
        "the line carries why the publish failed: {line:#?}"
    );
}

/// How soon after the consume loop starts on a trigger's unpublishable last
/// delivery the broker has to emit its max-deliveries advisory. The run
/// fails at once and the dead letter finds no stream at once, so the
/// hand-over's NAK goes out within milliseconds; three seconds leaves room
/// for a loaded machine and stays far inside the twelve minutes of a last
/// delivery's ack window.
const HANDED_OVER_WITHIN: Duration = Duration::from_secs(3);

/// **The consume loop hands an unpublishable dead letter's trigger over at
/// once** (issue 1062): the broker's max-deliveries advisory for
/// `fq-dispatcher` reaches the advisory stream within [`HANDED_OVER_WITHIN`]
/// of the loop's start.
///
/// It reads the broker, not the log, and is here because this file is the
/// one that runs the dispatcher's own consume loop. The hand-over is a NAK
/// with no delay, which the broker answers at once only while a pull is
/// waiting on the consumer: the loop's pull of one message at a time is
/// re-issued as the loop goes round, so one is waiting whenever a delivery
/// is settled while the loop runs. A loop that stopped pulling while
/// deliveries settle would leave the advisory to its next pull. Without a
/// pull at all the broker waits, which
/// `durable_start_signal_tests::unpublished_dead_letter` pins on the
/// broker alone.
#[tokio::test(flavor = "multi_thread")]
async fn the_consume_loop_hands_an_unpublishable_dead_letter_over_at_once() {
    let mut daemon = Daemon::new("dead-letter-handed-over", Script::FailsTransiently).await;
    let published = daemon.an_unpublishable_last_delivery().await;
    let started = chrono::Utc::now();
    daemon.start("binary-1").await;
    let subject =
        format!("$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.{TRIGGER_STREAM_NAME}.{CONSUMER_NAME}");
    let advisories = daemon
        .bus
        .jetstream()
        .get_stream(ADVISORY_STREAM_NAME)
        .await
        .expect("the advisory stream");
    let deadline = tokio::time::Instant::now() + HANDED_OVER_WITHIN;
    let advisory = loop {
        if let Ok(raw) = advisories.get_last_raw_message_by_subject(&subject).await {
            break serde_json::from_slice::<serde_json::Value>(&raw.payload)
                .expect("the advisory parses");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the advisory stream held no max-deliveries advisory for `{CONSUMER_NAME}` within \
             {HANDED_OVER_WITHIN:?} of the consume loop's start"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    daemon.stop().await;

    let emitted = advisory["timestamp"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .expect("the broker stamps its advisory")
        .with_timezone(&chrono::Utc);
    let after = (emitted - started).to_std().unwrap_or_default();
    assert!(
        emitted >= started && after < HANDED_OVER_WITHIN,
        "the advisory was emitted {after:?} after the loop's start: {advisory}"
    );
    assert_eq!(
        advisory["stream_seq"],
        json!(published.stream_seq),
        "{advisory}"
    );
    assert_eq!(
        advisory["deliveries"],
        json!(TRIGGER_MAX_DELIVER),
        "{advisory}"
    );
}

/// **A claim's release the claim store refuses is one `error` line naming
/// the invocation whose claim it was** (issue 1061), beside the sequence
/// and the store's error. The guide tells an operator to match that
/// invocation to the `abandoned_invocation_id` of the copy that later takes
/// the claim again, so the field is what the line is for. The run ends with
/// a transient error before its first WAL write, and the store refuses the
/// release before the NAK. The line names no subject, so it is found by
/// the run's own invocation id, which no other test's run shares.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_release_is_one_error_line_naming_the_invocation_whose_claim_it_was() {
    let mut daemon = Daemon::new(
        "release-refused",
        Script::FailsBeforeStartWithReleasesRefused,
    )
    .await;
    let (_, invocation_id) = daemon.run_one().await;
    let invocation_id = invocation_id.to_string();
    let named = |line: &Line| {
        line.message.contains(RELEASE_FAILED)
            && line.field("invocation_id") == Some(invocation_id.as_str())
    };
    daemon.log.wait_for("for the refused release", named).await;

    let lines = daemon.log.lines(named);
    assert_eq!(lines.len(), 1, "the refusal is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::ERROR, "{line:#?}");
    assert!(line.field("stream_seq").is_some(), "{line:#?}");
    assert!(
        line.field("error")
            .is_some_and(|error| error.contains("injected")),
        "{line:#?}"
    );
}

/// **A deferred run that ended before its resume is one line at `info`**
/// (issue 1070). An operator's drop folded while the run slept finishes
/// its row, so the resume the dispatcher runs when the delay is out finds
/// it ended. That is the run behaving as the operating guide says, not a
/// failure: one `info` line names the invocation, and nothing about it is
/// logged at `warn` or above, as recovery's and the operator's resumes
/// already log the same case.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_run_that_ended_before_its_resume_is_one_info_line() {
    let mut daemon = Daemon::new(
        "ended-before-resume",
        Script::DeferredThenEndedBeforeItsResume,
    )
    .await;
    daemon.start("binary-1").await;
    daemon.publish().await;
    daemon.worker.wait_until_ended().await;
    let (_, invocation_id) = daemon.worker.named();
    let id = invocation_id.to_string();
    let names = |line: &Line| {
        line.field("invocation_id") == Some(id.as_str())
            || line
                .field("error")
                .is_some_and(|error| error.contains(id.as_str()))
    };
    let ended = |line: &Line| names(line) && line.message.contains(ENDED_BEFORE_RESUME);
    daemon
        .log
        .wait_for("saying the deferred run ended before its resume", ended)
        .await;
    daemon.stop().await;

    let lines = daemon.log.lines(ended);
    assert_eq!(lines.len(), 1, "one line: {lines:#?}");
    assert_eq!(lines[0].level, tracing::Level::INFO, "{:#?}", lines[0]);
    let alarms = daemon.log.lines(|line| {
        names(line) && matches!(line.level, tracing::Level::WARN | tracing::Level::ERROR)
    });
    assert!(alarms.is_empty(), "nothing at warn or above: {alarms:#?}");
}

/// **A release that finds no claim of its own to give back is one `warn`
/// line naming the invocation that gave it back, the delivery, and the
/// claim as it stands now** (issue 1055). Every release is made by a copy
/// whose claim is `claimed` under its own invocation, so one that removes
/// nothing means something changed the claim underneath it; after a
/// landed mark there is no release at all. Here the claim names another
/// invocation by the time the run fails before its first WAL write. What
/// the claim holds now is what tells the causes apart, so the line says
/// it, and it carries the subject, trigger and answer the NAK's lines
/// carry.
#[tokio::test(flavor = "multi_thread")]
async fn a_release_that_gives_nothing_back_is_one_warn_line_naming_the_invocation() {
    let mut daemon = Daemon::new(
        "release-gives-nothing",
        Script::FailsBeforeStartWithTheClaimTakenAway,
    )
    .await;
    let (_, invocation_id) = daemon.run_one().await;
    let invocation_id = invocation_id.to_string();
    let named = |line: &Line| {
        line.message.contains(RELEASED_NOTHING)
            && line.field("invocation_id") == Some(invocation_id.as_str())
    };
    daemon
        .log
        .wait_for("for the release that gave nothing back", named)
        .await;

    let lines = daemon.log.lines(named);
    assert_eq!(lines.len(), 1, "it is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    assert!(line.field("stream_seq").is_some(), "{line:#?}");
    assert_eq!(
        line.field("subject"),
        Some(daemon.subject.as_str()),
        "{line:#?}"
    );
    assert!(
        line.field("trigger_id").is_some_and(|id| id != "-"),
        "{line:#?}"
    );
    assert!(line.field("context").is_some(), "{line:#?}");
    assert_eq!(line.field("claim_state"), Some("claimed"), "{line:#?}");
    assert_eq!(
        line.field("claim_invocation_id"),
        Some(ANOTHER_RUN),
        "{line:#?}"
    );
    assert!(line.field("claimant").is_some(), "{line:#?}");
}

/// **A consume the claim store refuses is one `error` line naming the
/// invocation, and the delivery is still acked** (issue 1055). A run fails
/// for good before its first WAL write, so its claim is recorded consumed
/// before the ack; the store refuses the write. The claim stays `claimed`,
/// so a lost ack on top would let a copy handle the trigger again, which
/// the line says.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_consume_is_one_error_line_naming_the_invocation() {
    let mut daemon = Daemon::new("consume-refused", Script::FailsForGoodWithConsumesRefused).await;
    let (_, invocation_id) = daemon.run_one().await;
    let invocation_id = invocation_id.to_string();
    let named = |line: &Line| {
        line.message.contains(CONSUME_FAILED)
            && line.field("invocation_id") == Some(invocation_id.as_str())
    };
    daemon.log.wait_for("for the refused consume", named).await;

    let lines = daemon.log.lines(named);
    assert_eq!(lines.len(), 1, "the refusal is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::ERROR, "{line:#?}");
    assert!(line.field("stream_seq").is_some(), "{line:#?}");
    assert_eq!(
        line.field("consumed_why"),
        Some("failed_for_good"),
        "{line:#?}"
    );
    assert!(
        line.field("error")
            .is_some_and(|error| error.contains("injected")),
        "{line:#?}"
    );
}

/// **A consume that finds no claim of its own is one `warn` line naming
/// the invocation and the claim as it stands now** (issue 1055). The claim
/// names another invocation by the time the run fails for good before its
/// first WAL write, so the consume before the ack matches nothing; the
/// delivery is acked all the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_consume_that_finds_no_claim_of_its_own_is_one_warn_line_naming_the_invocation() {
    let mut daemon = Daemon::new(
        "consume-finds-nothing",
        Script::FailsForGoodWithTheClaimTakenAway,
    )
    .await;
    let (_, invocation_id) = daemon.run_one().await;
    let invocation_id = invocation_id.to_string();
    let named = |line: &Line| {
        line.message.contains(CONSUMED_NOTHING)
            && line.field("invocation_id") == Some(invocation_id.as_str())
    };
    daemon
        .log
        .wait_for("for the consume that found nothing", named)
        .await;

    let lines = daemon.log.lines(named);
    assert_eq!(lines.len(), 1, "it is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    assert_eq!(
        line.field("subject"),
        Some(daemon.subject.as_str()),
        "{line:#?}"
    );
    assert_eq!(
        line.field("consumed_why"),
        Some("failed_for_good"),
        "{line:#?}"
    );
    assert_eq!(line.field("claim_state"), Some("claimed"), "{line:#?}");
    assert_eq!(
        line.field("claim_invocation_id"),
        Some(ANOTHER_RUN),
        "{line:#?}"
    );
}

/// **A healthy start logs no release that gave nothing back** (issue
/// 1055). The same promise as the settling test
/// `the_ack_after_a_landed_mark_gives_nothing_back`, pinned in a state a
/// real store reaches: the run's mark lands, and the ack after it keeps the
/// claim, so there is no release at all; one that removed nothing would
/// now say so at warn.
#[tokio::test(flavor = "multi_thread")]
async fn a_healthy_start_logs_no_release_that_gave_nothing_back() {
    let mut daemon = Daemon::new("healthy-start-is-quiet", Script::Nothing).await;
    // `run_one` returns once the dispatcher has stopped and joined every
    // delivery it was handling, so every line of this trigger's is in.
    let (_, invocation_id) = daemon.run_one().await;
    let invocation_id = invocation_id.to_string();
    let lines = daemon.log.lines(|line: &Line| {
        line.message.contains(RELEASED_NOTHING)
            && line.field("invocation_id") == Some(invocation_id.as_str())
    });
    assert!(
        lines.is_empty(),
        "a healthy start released nothing and said so: {lines:#?}"
    );
}

/// **A run's ownership row the store refuses at its durable start is one
/// `error` line naming the run, the trigger and the sequence** (issue
/// 721), at the level of a mark the store refuses: the run goes on, and
/// the line is what says why `fq invocation list` leaves it out until its
/// ending writes the row. The mark landed, so nothing says it gave up.
#[tokio::test(flavor = "multi_thread")]
async fn a_row_the_store_refuses_is_one_error_line_naming_the_run() {
    let mut daemon = Daemon::new("row-refused", Script::RefusedRows).await;
    let named = daemon.run_one().await;

    let line = the_one_error_line_naming_the_run(&daemon, ROW_NOT_WRITTEN, named);
    assert!(line.field("error").is_some(), "{line:#?}");
}

/// **A run's ownership row an operator's drop already ended is one `warn`
/// line naming the run and the state the row records** (issue 721). The
/// drop was folded before the dispatcher wrote the row, so the start is
/// not written over it, and the row keeps the agent's id the drop wrote in
/// its worker column; the line is what says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_row_an_operators_drop_already_ended_is_one_warn_line_naming_the_run() {
    let mut daemon = Daemon::new("row-already-dropped", Script::DroppedRow).await;
    let (trigger_id, invocation_id) = daemon.run_one().await;

    let lines = daemon
        .log
        .lines(|line| about(line, ROW_ALREADY_ENDED, &daemon.subject));
    assert_eq!(lines.len(), 1, "the case is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    assert_eq!(
        line.field("invocation_id"),
        Some(invocation_id.to_string().as_str())
    );
    assert_eq!(
        line.field("trigger_id"),
        Some(trigger_id.to_string().as_str())
    );
    assert!(
        line.field("recorded")
            .is_some_and(|recorded| recorded.starts_with("failed/")),
        "the line names the drop's ending: {line:#?}"
    );
}

/// **A run's ownership row this build cannot read, met at its durable
/// start, is one `error` line naming the run** (issue 721): the row is
/// left as it is and the run goes on, and the line says why the run's
/// row does not read `in_flight`.
#[tokio::test(flavor = "multi_thread")]
async fn a_row_this_build_cannot_read_is_one_error_line_naming_the_run() {
    let mut daemon = Daemon::new("row-unreadable", Script::UnreadableRow).await;
    let named = daemon.run_one().await;

    let line = the_one_error_line_naming_the_run(&daemon, ROW_NOT_WRITTEN, named);
    assert!(line.field("detail").is_some(), "{line:#?}");
}

/// **A healthy start logs nothing about its run's ownership row** at
/// `info` or above (issue 721): the row is written, and only a row that
/// was not is worth an operator's attention.
#[tokio::test(flavor = "multi_thread")]
async fn a_healthy_start_logs_nothing_about_its_runs_ownership_row() {
    let mut daemon = Daemon::new("healthy-row-is-quiet", Script::Nothing).await;
    daemon.run_one().await;

    let lines = daemon.log.lines(|line: &Line| {
        (line.message.contains(ROW_NOT_WRITTEN) || line.message.contains(ROW_ALREADY_ENDED))
            && line.field("subject") == Some(daemon.subject.as_str())
    });
    assert!(
        lines.is_empty(),
        "a healthy start's row is not logged: {lines:#?}"
    );
}

/// **A start found over a consumed claim is one `error` line naming the
/// run and how the claim said it was consumed** (issue 1055). The run did
/// start, so the claim is put right, and the line is the record that it
/// read otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_over_a_consumed_claim_is_one_error_line_naming_the_run() {
    let mut daemon = Daemon::new(
        "start-over-consumed",
        Script::ChangedClaim(ClaimChange::ConsumedUnderTheRun),
    )
    .await;
    let named = daemon.run_one().await;

    let line = the_one_error_line_naming_the_run(&daemon, START_OVER_CONSUMED, named);
    assert_eq!(
        line.field("consumed_why"),
        Some("failed_for_good"),
        "{line:#?}"
    );
    assert_eq!(
        line.field("consumed_invocation_id"),
        Some(named.1.to_string().as_str()),
        "{line:#?}"
    );
}

/// **A copy that meets a consumed claim is one `warn` line naming the
/// delivery, the invocation that consumed it and how** (issue 1055).
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_of_a_consumed_trigger_is_one_warn_line_naming_how_it_was_consumed() {
    let mut daemon = Daemon::new("copy-of-consumed", Script::Nothing).await;
    let log = daemon.log;
    let published = daemon.publish().await;
    let consumed_by = Uuid::now_v7().to_string();
    let key = daemon.key(published.stream_seq);
    daemon
        .claims
        .claim_trigger(key, "binary-0", &consumed_by, 0)
        .await
        .unwrap();
    assert!(
        daemon
            .claims
            .consume_trigger_claim(
                key,
                &consumed_by,
                &fq_runtime::control_plane::store::ConsumedWhy::InvalidPayload,
            )
            .await
            .unwrap()
    );
    daemon.start("binary-1").await;
    let names_it = |line: &Line| {
        line.message.starts_with(CONSUMED_DROP)
            && line.field("consumed_invocation_id") == Some(consumed_by.as_str())
    };
    log.wait_for("for the copy of a consumed trigger", names_it)
        .await;
    daemon.stop().await;

    let dropped = log.lines(names_it);
    assert_eq!(dropped.len(), 1, "one line for the copy: {dropped:#?}");
    let line = &dropped[0];
    assert_eq!(line.level, tracing::Level::WARN, "{line:#?}");
    assert_eq!(
        line.field("consumed_why"),
        Some("invalid_payload"),
        "{line:#?}"
    );
    assert_eq!(
        line.field("stream_seq"),
        Some(published.stream_seq.to_string().as_str()),
        "{line:#?}"
    );
    assert_eq!(line.field("delivered"), Some("1"), "{line:#?}");
}

/// **The last look over a consumed claim whose run turns up corrects the
/// claim as the first look does, and is one `error` line naming the run**
/// (issue 1055). The claim says the trigger was consumed without a run.
/// When the verdict is first taken the worker cannot say what its WAL
/// holds, so the verdict is held; past the watch's limit the last look
/// (which `dead_letter.requeue` takes too) reads the WAL again and finds
/// the run in flight. The run started, so the claim is marked started for
/// it, the verdict says it marked it, and the line says how the claim had
/// said it was consumed.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_look_over_a_consumed_claim_marks_it_and_is_one_error_line() {
    let daemon = Daemon::new("last-look-over-consumed", Script::Nothing).await;
    let (seq, run) = consumed_with_its_run_found_late(&daemon).await;
    let key = daemon.key(seq);

    let verdict = daemon
        .exhaustion_check("binary-1")
        .await
        .last_look(seq)
        .await;

    assert!(
        matches!(
            &verdict,
            Exhausted::Started { invocation_id, mark: ClaimMark::Now, .. } if *invocation_id == run
        ),
        "a start, and the claim marked now: {verdict:?}"
    );
    let row = daemon.claims.trigger_claim(key).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.invocation_id.as_deref()),
        (ClaimState::DurablyStarted, Some(run.as_str())),
        "the claim is marked started for the run"
    );
    let lines = daemon.log.lines(|line: &Line| {
        line.message.starts_with(EXHAUSTED_OVER_CONSUMED)
            && line.field("invocation_id") == Some(run.as_str())
    });
    assert_eq!(lines.len(), 1, "it is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::ERROR, "{line:#?}");
    assert_eq!(
        line.field("consumed_why"),
        Some("failed_for_good"),
        "{line:#?}"
    );
    assert_eq!(line.field("mark"), Some("marked started now"), "{line:#?}");
    assert!(line.field("on_record").is_some(), "{line:#?}");
}

/// **The last look over a consumed claim it cannot mark says so** (issue
/// 1055). As above, but the claim store refuses every update. The run
/// started, so the verdict is still a start; the claim still says
/// consumed, and the verdict and the line say the mark failed and that
/// the claim still says consumed, since startup recovery will not mark it.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_look_over_a_consumed_claim_it_cannot_mark_says_so() {
    let daemon = Daemon::new("last-look-cannot-mark", Script::Nothing).await;
    let (seq, run) = consumed_with_its_run_found_late(&daemon).await;
    refuse_claim_marks(&daemon.claim_db, Refusals::Every).await;

    let verdict = daemon
        .exhaustion_check("binary-1")
        .await
        .last_look(seq)
        .await;

    assert!(
        matches!(
            &verdict,
            Exhausted::Started { invocation_id, mark: ClaimMark::ConsumedNotMarked(_), .. }
                if *invocation_id == run
        ),
        "a start, and the claim not marked: {verdict:?}"
    );
    let row = daemon.claims.trigger_claim(daemon.key(seq)).await.unwrap();
    assert_eq!(
        row.map(|row| row.state),
        Some(ClaimState::Consumed {
            why: ConsumedWhy::FailedForGood
        }),
        "the claim still says consumed"
    );
    let lines = daemon.log.lines(|line: &Line| {
        line.message.starts_with(EXHAUSTED_OVER_CONSUMED)
            && line.field("invocation_id") == Some(run.as_str())
    });
    assert_eq!(lines.len(), 1, "it is logged once: {lines:#?}");
    let mark = lines[0].field("mark").unwrap_or_default();
    assert!(
        mark.starts_with("not marked: the mark failed")
            && mark.ends_with("the claim still says consumed"),
        "{:#?}",
        lines[0]
    );
}

/// A trigger published, claimed as `binary-0` under a new run and consumed
/// as failed for good, with the stub's first WAL read failing and its
/// second finding the run in flight: what the last look meets when the
/// first look could not read the run's records. Returns the trigger's
/// sequence and the run.
async fn consumed_with_its_run_found_late(daemon: &Daemon) -> (u64, String) {
    let published = daemon.publish().await;
    let run = Uuid::now_v7().to_string();
    let key = daemon.key(published.stream_seq);
    daemon
        .claims
        .claim_trigger(key, "binary-0", &run, 0)
        .await
        .unwrap();
    assert!(
        daemon
            .claims
            .consume_trigger_claim(key, &run, &ConsumedWhy::FailedForGood)
            .await
            .unwrap()
    );
    daemon
        .worker
        .wal_answers
        .lock()
        .unwrap()
        .extend([Err(()), Ok(Some(WalRun::InFlight))]);
    (published.stream_seq, run)
}

/// **A consumed claim that records an operator's drop is not contradicted
/// by the drop's ownership row, whatever its reason** (issue 1055). An
/// operator's drop of a run that never made its first WAL write writes the
/// run's ownership row, by design, and the claim records the drop beside
/// its reason. So only a start in the worker's WAL contradicts it. Here the
/// WAL holds nothing, for a claim consumed for each reason: the verdict is
/// consumed, with the reason the claim keeps and the drop, the claim is
/// left as it is, and no line says it was contradicted.
#[tokio::test(flavor = "multi_thread")]
async fn a_drops_ownership_row_does_not_contradict_a_consumed_claim_that_records_it() {
    let daemon = Daemon::new("dropped-consumed", Script::Nothing).await;
    for why in every_reason() {
        let (seq, run) = dropped(&daemon, why.clone(), true, Ok(None)).await;

        let verdict = daemon
            .exhaustion_check("binary-1")
            .await
            .last_look(seq)
            .await;

        assert_eq!(
            verdict,
            Exhausted::Consumed(ConsumedClaim::Checked {
                invocation_id: Some(run.clone()),
                why: why.clone(),
                operator_dropped: true,
            }),
            "{why}: consumed, as the claim says, with the drop"
        );
        let row = daemon.claims.trigger_claim(daemon.key(seq)).await.unwrap();
        assert_eq!(
            row.map(|row| (row.state, row.operator_dropped_at.is_some())),
            Some((ClaimState::Consumed { why: why.clone() }, true)),
            "{why}: the claim is left as it was"
        );
        let lines = daemon.log.lines(|line: &Line| {
            line.message.starts_with(EXHAUSTED_OVER_CONSUMED)
                && line.field("invocation_id") == Some(run.as_str())
        });
        assert!(
            lines.is_empty(),
            "{why}: nothing contradicted the claim: {lines:#?}"
        );
    }
}

/// **A consumed claim that records no drop reads an ownership row as a
/// start, whatever its reason** (issue 1055): the rule keys on the recorded
/// drop, not on the reason, so a claim consumed as halted in setup with no
/// drop recorded is no exception. Its run's ownership row is a start, the
/// claim is marked started for the run, and the line says the claim was
/// contradicted.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumed_claim_that_records_no_drop_reads_an_ownership_row_as_a_start() {
    let daemon = Daemon::new("consumed-no-drop", Script::Nothing).await;
    for why in [ConsumedWhy::HaltedInSetup, ConsumedWhy::FailedForGood] {
        let (seq, run) = dropped(&daemon, why.clone(), false, Ok(None)).await;

        let verdict = daemon
            .exhaustion_check("binary-1")
            .await
            .last_look(seq)
            .await;

        assert!(
            matches!(
                &verdict,
                Exhausted::Started { invocation_id, mark: ClaimMark::Now, .. } if *invocation_id == run
            ),
            "{why}: a start, and the claim marked now: {verdict:?}"
        );
        let lines = daemon.log.lines(|line: &Line| {
            line.message.starts_with(EXHAUSTED_OVER_CONSUMED)
                && line.field("invocation_id") == Some(run.as_str())
        });
        assert_eq!(lines.len(), 1, "{why}: it is logged once: {lines:#?}");
    }
}

/// **A dropped run that is in the WAL is a start** (issue 1055): the drop
/// landed after the run's first WAL write, so the claim is wrong, and it is
/// corrected as any consumed claim with a start on record is, losing its
/// reason and the drop with its consumed state.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_run_in_the_wal_is_a_start() {
    let daemon = Daemon::new("dropped-after-start", Script::Nothing).await;
    let (seq, run) = dropped(
        &daemon,
        ConsumedWhy::HaltedInSetup,
        true,
        Ok(Some(WalRun::InFlight)),
    )
    .await;

    let verdict = daemon
        .exhaustion_check("binary-1")
        .await
        .last_look(seq)
        .await;

    assert!(
        matches!(
            &verdict,
            Exhausted::Started { invocation_id, mark: ClaimMark::Now, .. } if *invocation_id == run
        ),
        "a start, and the claim marked now: {verdict:?}"
    );
    let row = daemon.claims.trigger_claim(daemon.key(seq)).await.unwrap();
    assert_eq!(
        row.map(|row| (row.state, row.operator_dropped_at)),
        Some((ClaimState::DurablyStarted, None)),
        "marked started, and the drop goes with the consumed state"
    );
    let lines = daemon.log.lines(|line: &Line| {
        line.message.starts_with(EXHAUSTED_OVER_CONSUMED)
            && line.field("invocation_id") == Some(run.as_str())
    });
    assert_eq!(lines.len(), 1, "it is logged once: {lines:#?}");
}

/// Every reason a claim can be consumed for, a spelling of a newer build
/// included.
fn every_reason() -> [ConsumedWhy; 11] {
    [
        ConsumedWhy::RefusedSubject,
        ConsumedWhy::InvalidAgentId,
        ConsumedWhy::UnknownAgent,
        ConsumedWhy::InvalidPayload,
        ConsumedWhy::RequeuedFromHold,
        ConsumedWhy::AgentRemoved,
        ConsumedWhy::FailedForGood,
        ConsumedWhy::HaltedInSetup,
        ConsumedWhy::ReturnedUnstarted,
        ConsumedWhy::DeadLettered,
        ConsumedWhy::Other("a_reason_from_a_newer_build".to_string()),
    ]
}

/// A trigger published, claimed as `binary-0` under a new run and consumed
/// as `why`, with the run's ownership row `failed` as an operator's drop
/// leaves it, the drop recorded on the claim when `recorded`, and the
/// stub's WAL answering `wal` for the run. Returns the trigger's sequence
/// and the run.
async fn dropped(
    daemon: &Daemon,
    why: ConsumedWhy,
    recorded: bool,
    wal: Result<Option<WalRun>, ()>,
) -> (u64, String) {
    let published = daemon.publish().await;
    let run = Uuid::now_v7().to_string();
    let key = daemon.key(published.stream_seq);
    daemon
        .claims
        .claim_trigger(key, "binary-0", &run, 0)
        .await
        .unwrap();
    assert!(
        daemon
            .claims
            .consume_trigger_claim(key, &run, &why)
            .await
            .unwrap()
    );
    daemon
        .claims
        .upsert_invocation_ownership(
            &run,
            "binary-0",
            0,
            fq_runtime::control_plane::OwnerStatus::Failed,
        )
        .await
        .unwrap();
    if recorded {
        let mut conn = claim_db_connection(&daemon.claim_db).await;
        sqlx::query("UPDATE trigger_claim SET operator_dropped_at = 1 WHERE invocation_id = ?")
            .bind(&run)
            .execute(&mut conn)
            .await
            .expect("record the drop on the claim");
        conn.close().await.expect("close the claim store's file");
    }
    daemon.worker.wal_answers.lock().unwrap().push_back(wal);
    (published.stream_seq, run)
}

/// **The last look leaves a claim consumed under another run as it is**
/// (issue 1055). The check reads the claim `claimed` for a run and cannot
/// read the WAL for it; meanwhile the claim is taken again under another
/// run and consumed as dead-lettered. The last look finds the first run in
/// the WAL: a start, so the trigger is neither dead-lettered nor requeued.
/// The consumed claim names another run, whose start nothing records, so
/// nothing contradicts it: it is left as it is, and no line says a
/// consumed claim was wrong.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_look_leaves_a_claim_consumed_under_another_run_as_it_is() {
    let daemon = Daemon::new("last-look-consumed-by-another", Script::Nothing).await;
    let published = daemon.publish().await;
    let run = Uuid::now_v7().to_string();
    let key = daemon.key(published.stream_seq);
    daemon
        .claims
        .claim_trigger(key, "binary-0", &run, 0)
        .await
        .unwrap();
    *daemon.worker.change_at_wal_read.lock().unwrap() = Some(ClaimChange::ConsumedUnderAnother);
    daemon
        .worker
        .wal_answers
        .lock()
        .unwrap()
        .extend([Err(()), Ok(Some(WalRun::InFlight))]);

    let verdict = daemon
        .exhaustion_check("binary-1")
        .await
        .last_look(published.stream_seq)
        .await;

    assert!(
        matches!(
            &verdict,
            Exhausted::Started { invocation_id, mark: ClaimMark::Changed, .. } if *invocation_id == run
        ),
        "a start for the run in the WAL, the claim not marked: {verdict:?}"
    );
    let row = daemon.claims.trigger_claim(key).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.invocation_id.as_deref()),
        (
            ClaimState::Consumed {
                why: ConsumedWhy::DeadLettered
            },
            Some(ANOTHER_RUN)
        ),
        "the claim consumed under another run is left as it is"
    );
    let lines = daemon.log.lines(|line: &Line| {
        line.message.starts_with(EXHAUSTED_OVER_CONSUMED)
            && line.field("invocation_id") == Some(run.as_str())
    });
    assert!(
        lines.is_empty(),
        "nothing contradicted the consumed claim: {lines:#?}"
    );
}

/// The start of the dead-letter watch's line for an exhausted trigger whose
/// claim says consumed and which it could not check by its limit.
const CONSUMED_UNVERIFIED: &str = "not dead-lettering a trigger whose deliveries ran out: its \
     claim says it was consumed without a run, and the records that could say otherwise still \
     cannot be read";
/// The start of the dead-letter watch's line for an exhausted trigger whose
/// claim records that the dispatcher published its dead letter.
const CONSUMED_AS_DEAD_LETTERED: &str = "not dead-lettering a trigger whose deliveries ran out: \
     its claim records that the dispatcher published its dead letter";

/// **Past its limit, the dead-letter watch's word on a consumed claim it
/// could not check is one `error` line naming the run** (issue 1055). No
/// dead letter is written, since none is due either way, so this line is
/// the only record that a claim went unverified, and an operator reading
/// the journal at `error` has to find it. The real watch runs on the
/// daemon's advisory stream. The claim says consumed as failed for good
/// and the stub cannot say what its WAL holds. The advisory is the
/// broker's form with no timestamp: its age cannot be told, so the watch
/// holds it only for its first looks (about three seconds, under the NAK
/// delays) and its third delivery is past the limit.
#[tokio::test(flavor = "multi_thread")]
async fn past_the_watchs_limit_an_unverified_consumed_claim_is_one_error_line_naming_the_run() {
    let Watching {
        daemon,
        run,
        stream_seq,
        stop,
        watching,
    } = past_the_watchs_limit("consumed-unverified", ConsumedWhy::FailedForGood).await;
    let named = |line: &Line| {
        line.message.starts_with(CONSUMED_UNVERIFIED)
            && line.field("consumed_invocation_id") == Some(run.as_str())
    };
    daemon.log.wait_for("for the unverified claim", named).await;
    stop_watching(stop, watching).await;

    let lines = daemon.log.lines(named);
    assert_eq!(lines.len(), 1, "it is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::ERROR, "{line:#?}");
    assert_eq!(
        line.field("subject"),
        Some(daemon.subject.as_str()),
        "{line:#?}"
    );
    assert_eq!(
        line.field("stream_seq"),
        Some(stream_seq.to_string().as_str()),
        "{line:#?}"
    );
    assert_eq!(
        line.field("consumed_why"),
        Some("failed_for_good"),
        "{line:#?}"
    );
    assert!(
        line.field("reason")
            .is_some_and(|reason| reason.contains("could not be read")),
        "{line:#?}"
    );
}

/// **A consumed claim that says the trigger was dead-lettered is one `info`
/// line naming the run** (issue 1055). The dispatcher publishes that dead
/// letter as it consumes the claim, so the watch writes none and withholds
/// none. Its line says what it knows, that the claim records the dead
/// letter published, and names the run the claim names and the reason;
/// nothing is logged at `warn` or `error` for the run. The real watch runs
/// on the daemon's advisory stream, and the stub's WAL holds nothing for
/// the run, so the claim is checked on the advisory's first delivery.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_consumed_as_dead_lettered_is_one_info_line_naming_the_run() {
    let Watching {
        daemon,
        run,
        stream_seq,
        stop,
        watching,
    } = watching_a_consumed_claim(
        "consumed-dead-lettered-checked",
        ConsumedWhy::DeadLettered,
        Wal::HoldsNothing,
    )
    .await;
    one_line_for_a_claim_consumed_as_dead_lettered(&daemon, &run, stream_seq, stop, watching).await;
}

/// **Past its limit, a consumed claim that says the trigger was
/// dead-lettered is the same `info` line, unverified as it is** (issue
/// 1055). A dead letter is written either way, the dispatcher's own, so
/// the watch has nothing to raise: the line above, and nothing at `error`.
/// The same stub and advisory as the error line's.
#[tokio::test(flavor = "multi_thread")]
async fn past_the_watchs_limit_a_claim_consumed_as_dead_lettered_is_one_info_line_naming_the_run() {
    let Watching {
        daemon,
        run,
        stream_seq,
        stop,
        watching,
    } = past_the_watchs_limit("consumed-dead-lettered", ConsumedWhy::DeadLettered).await;
    one_line_for_a_claim_consumed_as_dead_lettered(&daemon, &run, stream_seq, stop, watching).await;
}

/// The watch's one line for a claim consumed as dead-lettered, waited for
/// and read: at `info`, naming the subject, the sequence, the run and the
/// reason, and no line for the run at `warn` or `error`.
async fn one_line_for_a_claim_consumed_as_dead_lettered(
    daemon: &Daemon,
    run: &str,
    stream_seq: u64,
    stop: oneshot::Sender<()>,
    watching: tokio::task::JoinHandle<Result<(), AdvisoryWatchError>>,
) {
    let named = |line: &Line| {
        line.message.starts_with(CONSUMED_AS_DEAD_LETTERED)
            && line.field("consumed_invocation_id") == Some(run)
    };
    daemon
        .log
        .wait_for("for the claim consumed as dead-lettered", named)
        .await;
    stop_watching(stop, watching).await;

    let lines = daemon.log.lines(named);
    assert_eq!(lines.len(), 1, "it is logged once: {lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::INFO, "{line:#?}");
    assert_eq!(
        line.field("subject"),
        Some(daemon.subject.as_str()),
        "{line:#?}"
    );
    assert_eq!(
        line.field("stream_seq"),
        Some(stream_seq.to_string().as_str()),
        "{line:#?}"
    );
    assert_eq!(
        line.field("consumed_why"),
        Some("dead_lettered"),
        "{line:#?}"
    );
    let raised = daemon.log.lines(|line: &Line| {
        matches!(line.level, tracing::Level::WARN | tracing::Level::ERROR)
            && line.field("consumed_invocation_id") == Some(run)
    });
    assert!(raised.is_empty(), "nothing at warn or error: {raised:#?}");
}

/// What [`watching_a_consumed_claim`] sets running.
struct Watching {
    daemon: Daemon,
    run: String,
    stream_seq: u64,
    stop: oneshot::Sender<()>,
    watching: tokio::task::JoinHandle<Result<(), AdvisoryWatchError>>,
}

/// What the stub answers when the watch reads its WAL for the run.
enum Wal {
    /// It cannot say what its WAL holds.
    CannotBeRead,
    /// Its WAL holds nothing for the run.
    HoldsNothing,
}

/// [`watching_a_consumed_claim`] with a WAL that cannot be read: past the
/// watch's limit by the advisory's third delivery.
async fn past_the_watchs_limit(label: &str, why: ConsumedWhy) -> Watching {
    watching_a_consumed_claim(label, why, Wal::CannotBeRead).await
}

/// A daemon whose trigger is claimed by an exited process and consumed as
/// `why`, its stub answering reads of its WAL as `wal` says, and the real
/// dead-letter watch running on its advisory stream with the broker's
/// advisory for the trigger, which has no timestamp. With a WAL that
/// cannot be read, the watch holds its verdict for its first looks and is
/// past its limit by the third delivery.
async fn watching_a_consumed_claim(label: &str, why: ConsumedWhy, wal: Wal) -> Watching {
    let daemon = Daemon::new(label, Script::Nothing).await;
    if let Wal::HoldsNothing = wal {
        daemon
            .worker
            .wal_answers
            .lock()
            .unwrap()
            .extend(std::iter::repeat_n(Ok(None), 16));
    }
    let published = daemon.publish().await;
    let run = Uuid::now_v7().to_string();
    let key = daemon.key(published.stream_seq);
    daemon
        .claims
        .claim_trigger(key, "binary-0", &run, 0)
        .await
        .unwrap();
    assert!(
        daemon
            .claims
            .consume_trigger_claim(key, &run, &why)
            .await
            .unwrap()
    );
    let dispatcher = daemon.dispatcher("binary-1").await;
    let (stop, stopped) = oneshot::channel();
    let watching = tokio::spawn(AdvisoryWatch::new(daemon.bus.clone(), &dispatcher).run(stopped));
    let advisory = json!({
        "type": "io.nats.jetstream.advisory.v1.max_deliver",
        "id": Uuid::now_v7().to_string(),
        "stream": TRIGGER_STREAM_NAME,
        "consumer": CONSUMER_NAME,
        "stream_seq": published.stream_seq,
        "deliveries": TRIGGER_MAX_DELIVER,
    });
    daemon
        .bus
        .jetstream()
        .publish(
            format!(
                "$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.{TRIGGER_STREAM_NAME}.{CONSUMER_NAME}"
            ),
            serde_json::to_vec(&advisory).unwrap().into(),
        )
        .await
        .expect("publish the advisory")
        .await
        .expect("the advisory stream holds it");
    Watching {
        daemon,
        run,
        stream_seq: published.stream_seq,
        stop,
        watching,
    }
}

/// Stop the watch [`watching_a_consumed_claim`] set running and wait for
/// it, which must have run without error.
async fn stop_watching(
    stop: oneshot::Sender<()>,
    watching: tokio::task::JoinHandle<Result<(), AdvisoryWatchError>>,
) {
    stop.send(()).expect("the watch is running");
    tokio::time::timeout(PATIENCE, watching)
        .await
        .expect("the watch stops")
        .expect("the watch task joins")
        .expect("the watch ran without error");
}
