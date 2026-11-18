//! What the trigger dispatcher logs when it drops a copy of a trigger,
//! cannot record or acknowledge a run's durable start, or cannot publish a
//! trigger's dead letter, read off the log of a dispatcher running its own
//! consume loop.
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
use fq_runtime::control_plane::dispatcher::{CONSUMER_NAME, TriggerClaims};
use fq_runtime::control_plane::store::TriggerKey;
use fq_runtime::control_plane::{ControlPlaneStore, TriggerClaim};
use fq_runtime::llm::LlmClient;
use fq_runtime::llm::fixture::FixtureClient;
use fq_runtime::trigger::{PublishedTrigger, Trigger};
use fq_runtime::worker::{
    DrainRequest, DrainState, DurableStart, DurableStartReport, ExecutorError, InvocationOutcome,
    Worker, WorkerStore,
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
/// The start of the line for a mark that landed after refusals.
const MARK_HEALED: &str = "marked trigger durably started after the claim store refused";
/// The start of the line for a mark the claim store refused every time.
const MARK_GAVE_UP: &str = "could not mark trigger durably started";
/// The line for an ack the client could not send.
const ACK_FAILED: &str = "failed to ack trigger message";
/// The line for a claim's release the claim store refused.
const RELEASE_FAILED: &str = "failed to release unfinished trigger claim";
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
/// The invocation a changed claim names ([`ClaimChange`]).
const ANOTHER_RUN: &str = "another-run";
/// The start of the line for a last delivery whose dead letter the
/// dispatcher could not publish.
const DEAD_LETTER_UNPUBLISHED: &str =
    "could not publish the dead letter of a trigger whose last delivery failed";

/// How long a test waits for a line, a delivery or the dispatcher.
const PATIENCE: Duration = Duration::from_secs(20);

// ---- The log ----

/// One event the dispatcher logged at `warn` or above: its level, its
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

/// Every line logged at `warn` or above, by every test in this binary.
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
                tracing_subscriber::registry().with(log.clone().with_filter(LevelFilter::WARN)),
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
}

#[async_trait::async_trait]
impl Worker for Stub {
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
            Script::FailsTransiently => {
                drop(durable_start);
                self.ended.notify_one();
                return Err(ExecutorError::WorkerStore(TRANSIENT_FAILURE.to_string()));
            }
            Script::FailsBeforeStartWithReleasesRefused => {
                refuse_claim_releases(&self.claim_db).await;
                drop(durable_start);
                self.ended.notify_one();
                return Err(ExecutorError::WorkerStore(
                    "simulated transient store outage before the first WAL write".to_string(),
                ));
            }
        }
        durable_start.fire();
        // One more turn of `handle`'s loop, which marks and acks on the
        // turn the signal fires, before the run returns.
        tokio::task::yield_now().await;
        self.ended.notify_one();
        // What the run returns past its durable start is not logged at
        // `warn`, so it does not matter to these lines.
        Ok(InvocationOutcome::Suspended { invocation_id })
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
        let dispatcher = TriggerDispatcher::new(
            self.bus.clone(),
            shared_registry(registry),
            self.worker.clone() as Arc<dyn Worker>,
            Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
            1,
        )
        .with_trigger_claims(claims, claimant);
        let (shutdown, stop) = oneshot::channel();
        self.shutdown = Some(shutdown);
        self.running = Some(tokio::spawn(dispatcher.run(stop)));
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
