//! A copy of a trigger that meets a claim this daemon holds itself
//! (issue 967).
//!
//! A `claimed` row under this daemon's own claimant id was written by this
//! process. Either the copy that wrote it is still being handled (waiting in
//! a hold, setting up, or running with its mark not yet landed), or it is
//! not, and then the row is one of three things: a run that started and
//! whose claim was never marked (the mark refused past its retries and the
//! ack lost; or the task dying in the very poll that made its first WAL
//! write), or a delivery that never started (its task dying before its
//! first WAL write; or the claim release before a NAK failing). Each test
//! makes one of these and hands a copy of the trigger to `handle`.
//!
//! **What is real here.** The dispatcher's own `handle`, `claim_delivery`
//! and `TriggerClaims::recover`; the control-plane store, holding the
//! claims; a worker store, holding the WAL rows a run writes, which the
//! dispatcher reads through the `Worker` port; and a NATS connection that
//! every answer to a delivery goes out on, read back off the wire. Stubbed:
//! the run itself ([`Scripted`]), which writes its WAL rows and fires its
//! durable-start signal where the real runner does; and the broker's side
//! of a delivery. Deliveries are made here ([`Arena::delivery`]): the
//! message the broker would send, with its stream sequence and delivery
//! attempt in its ack subject. That puts every redelivery, restart and
//! replay in the test's hands, at no cost in ack windows, which is what
//! lets the property in [`proptests`] drive many lives of a trigger. The
//! broker-driven paths are covered beside this file
//! (`durable_start_signal_tests`, `duplicate_drop_tests`) and in the
//! incident replays.
//!
//! No test reads a log line (issue 1031): each asserts what was decided,
//! from the claim row, the WAL, the runs started and the answers on the
//! wire.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::jetstream::Message;
use futures::StreamExt;
use serde_json::json;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};
use tokio::sync::{Notify, Semaphore};
use uuid::Uuid;

use super::tests::{canned_response, registry_with, unique_agent_id};
use super::{CONSUMER_NAME, TriggerClaims, TriggerDispatcher};
use crate::agent::Agent;
use crate::bus::{EventBus, TRIGGER_STREAM_NAME};
use crate::control_plane::ControlPlaneStore;
use crate::llm::LlmClient;
use crate::llm::fixture::FixtureClient;
use crate::test_support::nats::{NatsServer, test_nats};
use crate::trigger::{TRIGGER_ID_HEADER, Trigger};
use crate::worker::{
    DrainRequest, DrainState, DurableStart, DurableStartReport, ExecutorError, InvocationOutcome,
    InvocationStateRow, WalRun, Worker, WorkerStore,
};

mod proptests;

/// What one run of the stub does between its start and its return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// Makes its first WAL write, fires its signal, and finishes: its WAL
    /// row ends terminal.
    Completes,
    /// Its task panics before its first WAL write.
    DiesBeforeStart,
    /// Makes its first WAL write, fires its signal, and panics in the same
    /// poll: `handle` never sees it start, yet the WAL holds its row.
    DiesAfterStart,
    /// Drops its signal unfired and fails with a transient error, before
    /// its first WAL write: `handle` NAKs the delivery.
    FailsTransiently,
    /// Waits at [`Scripted::setup_gate`] before its first WAL write, then
    /// completes.
    WaitsInSetup,
    /// Makes its first WAL write, fires its signal, then waits at
    /// [`Scripted::run_gate`] before it finishes.
    WaitsAfterStart,
}

/// The stub worker: each `run_invocation` takes the next [`Run`] from its
/// script (`Completes` when the script is empty) and keeps its WAL in a
/// real worker store.
struct Scripted {
    wal: WorkerStore,
    script: Mutex<VecDeque<Run>>,
    /// Every run started: its invocation id and the delivery attempt the
    /// dispatcher handed it.
    starts: Mutex<Vec<(Uuid, Option<u32>)>>,
    /// Runs started while the WAL already held a run of the trigger: the
    /// one thing that must never happen. Only meaningful in an arena that
    /// runs one trigger.
    started_over_a_start: Mutex<Vec<Uuid>>,
    setup_gate: Semaphore,
    run_gate: Semaphore,
    /// How many runs are waiting at either gate.
    waiting: AtomicUsize,
    /// Notified when a run reaches a gate.
    reached_gate: Notify,
}

impl Scripted {
    fn new(wal: WorkerStore) -> Arc<Self> {
        Arc::new(Self {
            wal,
            script: Mutex::new(VecDeque::new()),
            starts: Mutex::new(Vec::new()),
            started_over_a_start: Mutex::new(Vec::new()),
            setup_gate: Semaphore::new(0),
            run_gate: Semaphore::new(0),
            waiting: AtomicUsize::new(0),
            reached_gate: Notify::new(),
        })
    }

    fn queue(&self, run: Run) {
        self.script.lock().unwrap().push_back(run);
    }

    fn starts(&self) -> Vec<(Uuid, Option<u32>)> {
        self.starts.lock().unwrap().clone()
    }

    async fn wait(&self, gate: &Semaphore) {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.reached_gate.notify_waiters();
        gate.acquire()
            .await
            .expect("the gate is never closed")
            .forget();
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }

    /// Wait until `n` runs are waiting at a gate.
    async fn until_waiting(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.waiting.load(Ordering::SeqCst) < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no run reached its gate within 10s"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// A WAL row for `invocation_id`, in flight or terminal.
fn wal_row(invocation_id: Uuid, terminal: bool) -> InvocationStateRow {
    InvocationStateRow {
        invocation_id: invocation_id.to_string(),
        agent_id: "own-claim".to_string(),
        schema_version: 1,
        phase: if terminal {
            "completed"
        } else {
            "awaiting_model"
        }
        .to_string(),
        state_blob: vec![],
        step_index: 0,
        started_at: 1,
        updated_at: 2,
        terminal_at: terminal.then_some(2),
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
        terminal_outcome: None,
        terminal_published_at: None,
    }
}

#[async_trait::async_trait]
impl Worker for Scripted {
    async fn run_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        _trigger: Trigger,
        delivery_attempt: Option<u32>,
        mut durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let invocation_id = durable_start
            .invocation_id()
            .expect("the dispatcher names the run it starts");
        let on_record = self.wal.list_durable_starts().await.expect("read the WAL");
        if !on_record.is_empty() {
            self.started_over_a_start
                .lock()
                .unwrap()
                .push(invocation_id);
        }
        self.starts
            .lock()
            .unwrap()
            .push((invocation_id, delivery_attempt));
        let run = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Run::Completes);
        match run {
            Run::DiesBeforeStart => {
                panic!("injected: the invocation task dies before its first WAL write")
            }
            Run::FailsTransiently => {
                drop(durable_start);
                return Err(ExecutorError::WorkerStore(
                    "injected: a transient failure before the first WAL write".to_string(),
                ));
            }
            Run::WaitsInSetup => self.wait(&self.setup_gate).await,
            Run::Completes | Run::DiesAfterStart | Run::WaitsAfterStart => {}
        }
        self.wal
            .upsert_invocation_state(&wal_row(invocation_id, false))
            .await
            .expect("the first WAL write");
        durable_start.fire();
        if run == Run::DiesAfterStart {
            panic!("injected: the invocation task dies in the poll that made its first WAL write");
        }
        if run == Run::WaitsAfterStart {
            self.wait(&self.run_gate).await;
        }
        tokio::task::yield_now().await;
        self.wal
            .upsert_invocation_state(&wal_row(invocation_id, true))
            .await
            .expect("the terminal WAL write");
        Ok(InvocationOutcome::Completed {
            invocation_id,
            response: canned_response(),
            cost: 0.0,
            duration_ms: 0,
        })
    }

    async fn wal_run(&self, invocation_id: &str) -> Result<Option<WalRun>, ExecutorError> {
        self.wal
            .durable_start_of(invocation_id)
            .await
            .map(|terminal| terminal.map(WalRun::of_row))
            .map_err(|err| ExecutorError::WorkerStore(err.to_string()))
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> DrainState {
        DrainState::Running
    }
}

/// What the claim table holds for one stream sequence, read without
/// writing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaimRow {
    claimant: String,
    started: bool,
    invocation_id: Option<String>,
}

/// One copy of a trigger being handled: the task `handle` runs on, and the
/// copy's id in its ack subject.
struct Handling {
    task: tokio::task::JoinHandle<()>,
    copy: u64,
}

/// What one delivery answered the broker, read off its ack subject.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Answered {
    acks: usize,
    naks: usize,
    keepalives: usize,
    /// Whether `handle` panicked: its run's task died.
    panicked: bool,
}

/// A trigger, a dispatcher that arbitrates it through real stores, and a
/// connection its deliveries answer on.
struct Arena {
    bus: EventBus,
    agent: String,
    /// The token in every ack subject this arena makes, so arenas that
    /// share a broker cannot read each other's answers.
    consumer: String,
    answers_client: async_nats::Client,
    /// A connection that has been drained: a delivery made on it answers
    /// nothing the broker receives.
    closed_client: async_nats::Client,
    subscription: tokio::sync::Mutex<async_nats::Subscriber>,
    /// Every answer read so far, by the copy that sent it.
    answered: Mutex<HashMap<u64, Vec<String>>>,
    /// The id of the next copy made: it rides in the ack subject's
    /// consumer-sequence token, which `handle` never reads.
    copies: AtomicU64,
    /// The id of the next marker [`Self::answers`] sends.
    markers: AtomicU64,
    claims: Arc<ControlPlaneStore>,
    claim_db: PathBuf,
    worker: Arc<Scripted>,
    dispatcher: Arc<TriggerDispatcher>,
    claimant: String,
    restarts: u32,
    trigger_id: Uuid,
    _dirs: Vec<tempfile::TempDir>,
}

/// The trigger every arena runs sits at this stream sequence.
const SEQ: u64 = 1;

impl Arena {
    async fn new(url: &str, max_concurrent: usize) -> Self {
        let bus = EventBus::connect(url).await.expect("connect NATS");
        let agent = unique_agent_id("own-claim");
        let stores = tempfile::tempdir().unwrap();
        let claim_db = stores.path().join("control-plane.db");
        let claims = Arc::new(ControlPlaneStore::open(&claim_db).await.unwrap());
        let wal = WorkerStore::open(&stores.path().join("worker.db"))
            .await
            .unwrap();
        let worker = Scripted::new(wal);
        let answers_client = async_nats::connect(url).await.expect("connect");
        let consumer = format!("own-claim-{}", Uuid::now_v7().simple());
        let subscription = answers_client
            .subscribe(format!("$JS.ACK.{TRIGGER_STREAM_NAME}.{consumer}.>"))
            .await
            .expect("watch the answers");
        answers_client.flush().await.expect("the watch is in place");
        let closed_client = async_nats::connect(url).await.expect("connect");
        closed_client.drain().await.expect("drain it");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while closed_client
            .publish("fq.test.closed-probe", "".into())
            .await
            .is_ok()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "still open after 5s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (agent_dir, registry) = registry_with(&agent);
        let claimant = "daemon-0".to_string();
        let dispatcher =
            Self::dispatcher(&bus, registry, &worker, &claims, &claimant, max_concurrent).await;
        Self {
            bus,
            agent,
            consumer,
            answers_client,
            closed_client,
            subscription: tokio::sync::Mutex::new(subscription),
            answered: Mutex::new(HashMap::new()),
            copies: AtomicU64::new(1),
            markers: AtomicU64::new(1),
            claims,
            claim_db,
            worker,
            dispatcher,
            claimant,
            restarts: 0,
            trigger_id: Uuid::now_v7(),
            _dirs: vec![stores, agent_dir],
        }
    }

    /// A dispatcher claiming as `claimant`, its claims recovered over the
    /// worker's WAL as the daemon recovers them.
    async fn dispatcher(
        bus: &EventBus,
        registry: super::SharedRegistry,
        worker: &Arc<Scripted>,
        claims: &Arc<ControlPlaneStore>,
        claimant: &str,
        max_concurrent: usize,
    ) -> Arc<TriggerDispatcher> {
        let starts = DurableStartReport::scan(&worker.wal)
            .await
            .expect("scan the WAL");
        let recovered = TriggerClaims::recover(Arc::clone(claims), &starts)
            .await
            .expect("recover the claims");
        Arc::new(
            TriggerDispatcher::new(
                bus.clone(),
                registry,
                Arc::clone(worker) as Arc<dyn Worker>,
                Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
                max_concurrent,
            )
            .with_trigger_claims(recovered, claimant),
        )
    }

    /// The daemon restarts: a new process, claiming under a new id, whose
    /// claims are recovered over the WAL before it consumes anything. A run
    /// the WAL holds in flight is resumed by the new process and finishes
    /// (recovery's resume, played here by marking the row terminal), which
    /// is not a new start of its trigger.
    async fn restart(&mut self) {
        heal(&self.claim_db).await;
        for (id, terminal) in self.worker.wal.list_durable_starts().await.unwrap() {
            if !terminal {
                let id = Uuid::parse_str(&id).unwrap();
                self.worker
                    .wal
                    .upsert_invocation_state(&wal_row(id, true))
                    .await
                    .unwrap();
            }
        }
        self.restarts += 1;
        self.claimant = format!("daemon-{}", self.restarts);
        let (agent_dir, registry) = registry_with(&self.agent);
        self._dirs.push(agent_dir);
        self.dispatcher = Self::dispatcher(
            &self.bus,
            registry,
            &self.worker,
            &self.claims,
            &self.claimant,
            1,
        )
        .await;
    }

    /// The message the broker would send for delivery `delivered` of the
    /// trigger at `seq`, and the id of this copy. With `lost`, it answers on
    /// a closed connection, so none of its answers reaches the broker.
    fn delivery(&self, seq: u64, delivered: u32, lost: bool) -> (Message, u64) {
        let copy = self.copies.fetch_add(1, Ordering::SeqCst);
        let reply = format!(
            "$JS.ACK.{TRIGGER_STREAM_NAME}.{}.{delivered}.{seq}.{copy}.{}.0",
            self.consumer,
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let mut headers = async_nats::HeaderMap::new();
        let trigger_id = if seq == SEQ {
            self.trigger_id
        } else {
            Uuid::now_v7()
        };
        headers.insert(TRIGGER_ID_HEADER, trigger_id.to_string());
        let payload = serde_json::to_vec(&json!({ "input": "hi" })).unwrap();
        let client = if lost {
            self.closed_client.clone()
        } else {
            self.answers_client.clone()
        };
        let msg = Message {
            message: async_nats::Message {
                subject: crate::events::subjects::trigger(&self.agent).into(),
                reply: Some(reply.into()),
                length: payload.len(),
                payload: payload.into(),
                headers: Some(headers),
                status: None,
                description: None,
            },
            context: async_nats::jetstream::new(client),
        };
        (msg, copy)
    }

    /// Hand delivery `delivered` of the trigger at `seq` to `handle` on a
    /// task of its own, as the consume loop does.
    fn spawn_delivery(&self, seq: u64, delivered: u32, lost: bool) -> Handling {
        let dispatcher = Arc::clone(&self.dispatcher);
        let (msg, copy) = self.delivery(seq, delivered, lost);
        let task = tokio::spawn(async move { dispatcher.handle(&msg).await });
        Handling { task, copy }
    }

    /// Hand delivery `delivered` of this arena's trigger to `handle`, wait
    /// for it, and say what it answered.
    async fn deliver(&self, delivered: u32, lost: bool) -> Answered {
        let handling = self.spawn_delivery(SEQ, delivered, lost);
        self.finish(handling).await
    }

    /// Wait for a delivery's task and say what it answered.
    async fn finish(&self, handling: Handling) -> Answered {
        let joined = tokio::time::timeout(Duration::from_secs(20), handling.task)
            .await
            .expect("handle returns within 20s");
        let mut answered = self.answers(handling.copy).await;
        answered.panicked = joined.is_err();
        answered
    }

    /// What copy `copy` has answered so far.
    ///
    /// Every answer goes out on [`Self::answers_client`], which also reads
    /// them. A flush there only writes to the socket (it does not wait for
    /// the server), so this sends a marker on the same connection, under
    /// the same subscription, and reads until the marker comes back: the
    /// server returns one connection's messages in the order it sent them,
    /// so every answer sent before the marker has been read by then. No
    /// consumer bears this arena's consumer token, so the broker acts on
    /// none of it.
    async fn answers(&self, copy: u64) -> Answered {
        let marker = self.markers.fetch_add(1, Ordering::SeqCst);
        self.answers_client
            .publish(
                format!(
                    "$JS.ACK.{TRIGGER_STREAM_NAME}.{}.marker.{marker}",
                    self.consumer
                ),
                "".into(),
            )
            .await
            .expect("send the marker");
        let mut subscription = self.subscription.lock().await;
        loop {
            let answer = tokio::time::timeout(Duration::from_secs(10), subscription.next())
                .await
                .expect("the marker comes back within 10s")
                .expect("the subscription is open");
            let tokens: Vec<&str> = answer.subject.as_str().split('.').collect();
            if tokens[4] == "marker" {
                if tokens[5].parse::<u64>().unwrap() == marker {
                    break;
                }
                continue;
            }
            self.answered
                .lock()
                .unwrap()
                .entry(tokens[6].parse::<u64>().unwrap())
                .or_default()
                .push(String::from_utf8_lossy(&answer.payload).into_owned());
        }
        let answered = self.answered.lock().unwrap();
        let mut out = Answered::default();
        for payload in answered.get(&copy).into_iter().flatten() {
            match payload.as_str() {
                "" | "+ACK" => out.acks += 1,
                "+WPI" => out.keepalives += 1,
                naked if naked.starts_with("-NAK") => out.naks += 1,
                other => panic!("an answer this harness does not know: {other}"),
            }
        }
        out
    }

    /// The dispatcher's three counts of copies that met a claim, as
    /// `(duplicate_dropped, held_duplicate_dropped, abandoned_claim_retaken)`.
    fn counts(&self) -> (u64, u64, u64) {
        let record = self.bus.consumer_ledger().record(CONSUMER_NAME);
        (
            record.duplicate_dropped,
            record.held_duplicate_dropped,
            record.abandoned_claim_retaken,
        )
    }

    /// The claim row for the trigger at `seq`, read without writing.
    async fn claim(&self, seq: u64) -> Option<ClaimRow> {
        let mut conn = SqliteConnectOptions::new()
            .filename(&self.claim_db)
            .connect()
            .await
            .expect("open the claim store's file");
        let row: Option<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT claimant, state, invocation_id FROM trigger_claim WHERE stream_seq = ?",
        )
        .bind(seq as i64)
        .fetch_optional(&mut conn)
        .await
        .expect("read the claim");
        conn.close().await.expect("close the claim store's file");
        row.map(|(claimant, state, invocation_id)| ClaimRow {
            claimant,
            started: state == "durably_started",
            invocation_id,
        })
    }

    /// The runs the WAL holds, by invocation id.
    async fn wal(&self) -> Vec<(String, WalRun)> {
        let mut rows: Vec<(String, WalRun)> = self
            .worker
            .wal
            .list_durable_starts()
            .await
            .unwrap()
            .into_iter()
            .map(|(id, terminal)| (id, WalRun::of_row(terminal)))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }
}

/// Run `statements` against the claim store's file, through a connection of
/// its own.
async fn exec(claim_db: &Path, statements: &[&'static str]) {
    let mut conn = SqliteConnectOptions::new()
        .filename(claim_db)
        .connect()
        .await
        .expect("open the claim store's file");
    for statement in statements {
        sqlx::query(*statement)
            .execute(&mut conn)
            .await
            .expect("run the statement");
    }
    conn.close().await.expect("close the claim store's file");
}

/// From now on the claim store refuses every update of a claim row: marks,
/// re-takes and adoptions.
async fn refuse_updates(claim_db: &Path) {
    exec(
        claim_db,
        &[
            "CREATE TRIGGER IF NOT EXISTS refuse_claim_updates BEFORE UPDATE ON trigger_claim \
           BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the update'); END",
        ],
    )
    .await;
}

/// From now on the claim store refuses every delete of a claim row: the
/// release before an ack or a NAK.
async fn refuse_deletes(claim_db: &Path) {
    exec(
        claim_db,
        &[
            "CREATE TRIGGER IF NOT EXISTS refuse_claim_deletes BEFORE DELETE ON trigger_claim \
           BEGIN SELECT RAISE(ABORT, 'injected: the claim store refuses the delete'); END",
        ],
    )
    .await;
}

/// The claim store takes updates and deletes again.
async fn heal(claim_db: &Path) {
    exec(
        claim_db,
        &[
            "DROP TRIGGER IF EXISTS refuse_claim_updates",
            "DROP TRIGGER IF EXISTS refuse_claim_deletes",
        ],
    )
    .await;
}

async fn arena() -> (Arena, NatsServer) {
    let server = test_nats();
    let arena = Arena::new(server.url(), 1).await;
    (arena, server)
}

/// **A run whose task died before its first WAL write is taken again at
/// its trigger's next delivery, and starts once.** The first copy claims
/// the trigger and its task panics before the run makes its first WAL
/// write, so nothing is on record and nothing settles the delivery. Before
/// issue 967's last change every redelivery met this daemon's own claim
/// and was dropped, until the trigger was dead-lettered as
/// `trigger_exhausted`, with the work lost. Now the next delivery takes the
/// claim again under a new invocation id and runs the trigger, which starts
/// once and acks it.
#[tokio::test]
async fn a_panic_before_the_durable_start_is_taken_again_and_runs_once() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::DiesBeforeStart);

    let first = arena.deliver(1, false).await;
    assert!(first.panicked && first.acks + first.naks == 0, "{first:?}");
    let abandoned = arena.claim(SEQ).await.expect("the claim is left");
    assert!(!abandoned.started, "{abandoned:?}");

    let again = arena.deliver(2, false).await;

    let starts = arena.worker.starts();
    assert_eq!(starts.len(), 2, "the redelivery starts the trigger again");
    let (retaken, attempt) = starts[1];
    assert_eq!(attempt, Some(2));
    assert_ne!(
        Some(retaken.to_string()),
        abandoned.invocation_id,
        "the re-taken run is a new invocation under a new id"
    );
    assert_eq!(
        arena.wal().await,
        vec![(retaken.to_string(), WalRun::Finished)],
        "one run made its first WAL write: the trigger started once"
    );
    assert_eq!(
        arena.claim(SEQ).await,
        Some(ClaimRow {
            claimant: arena.claimant.clone(),
            started: true,
            invocation_id: Some(retaken.to_string()),
        }),
        "the claim names the re-taken run, marked started"
    );
    assert_eq!((again.acks, again.naks), (1, 0), "{again:?}");
    assert!(arena.worker.started_over_a_start.lock().unwrap().is_empty());
    assert_eq!(
        arena.counts(),
        (0, 0, 1),
        "counted as an abandoned claim taken again, and as no duplicate"
    );
}

/// **A delivery whose claim release failed before its NAK is taken again
/// when the NAK brings it back, and runs once.** The run fails with a
/// transient error before its first WAL write, so `handle` releases the
/// claim and NAKs the delivery; the store refuses the release, and the NAK
/// goes out anyway. The redelivery it asked for meets this daemon's own
/// claim with no copy handling it and nothing on record, and is taken
/// again and run.
#[tokio::test]
async fn a_nak_whose_release_failed_is_taken_again_and_runs_once() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::FailsTransiently);
    refuse_deletes(&arena.claim_db).await;

    let first = arena.deliver(1, false).await;
    assert_eq!((first.acks, first.naks), (0, 1), "{first:?}");
    let left = arena.claim(SEQ).await.expect("the release failed");
    assert!(!left.started);
    heal(&arena.claim_db).await;

    let again = arena.deliver(2, false).await;

    let starts = arena.worker.starts();
    assert_eq!(starts.len(), 2, "the redelivery runs the trigger");
    assert_eq!(
        arena.wal().await,
        vec![(starts[1].0.to_string(), WalRun::Finished)],
        "the trigger started once"
    );
    assert_eq!(
        arena
            .claim(SEQ)
            .await
            .map(|row| (row.started, row.invocation_id)),
        Some((true, Some(starts[1].0.to_string())))
    );
    assert_eq!((again.acks, again.naks), (1, 0), "{again:?}");
    assert_eq!(arena.counts(), (0, 0, 1));
}

/// **A run whose mark was refused and whose ack was lost is not run
/// again: its next delivery is acked and its claim marked.** The run makes
/// its first WAL write and finishes; the claim store refuses every attempt
/// to mark its start, and the ack sent in its place never reaches the
/// broker. Before issue 967's last change the redelivery met this
/// daemon's own claim and was dropped, every time, until a false
/// `trigger_exhausted` dead letter. Now the run's start is on record, in
/// the worker's WAL, so the redelivery is acked and the claim marked, and
/// nothing starts a second time.
#[tokio::test]
async fn a_mark_refused_whose_ack_was_lost_is_acked_and_marked_and_not_run_again() {
    let (arena, _server) = arena().await;
    refuse_updates(&arena.claim_db).await;

    let first = arena.deliver(1, true).await;
    assert!(!first.panicked);
    let first_run = arena.worker.starts()[0].0;
    assert_eq!(
        arena.wal().await,
        vec![(first_run.to_string(), WalRun::Finished)]
    );
    assert_eq!(
        arena.claim(SEQ).await.map(|row| row.started),
        Some(false),
        "the mark was refused"
    );
    heal(&arena.claim_db).await;

    let again = arena.deliver(2, false).await;

    assert_eq!(
        arena.worker.starts().len(),
        1,
        "the redelivery must not start the trigger again"
    );
    assert_eq!(
        arena.claim(SEQ).await,
        Some(ClaimRow {
            claimant: arena.claimant.clone(),
            started: true,
            invocation_id: Some(first_run.to_string()),
        }),
        "the claim is marked, naming the run that started"
    );
    assert_eq!((again.acks, again.naks), (1, 0), "{again:?}");
    assert_eq!(
        arena.counts(),
        (1, 0, 0),
        "counted as a copy of a run that started"
    );
}

/// **A run whose task died in the very poll that made its first WAL write
/// is not run again.** The trap the in-memory "being handled" test alone
/// would fall into: the run made its first WAL write and fired its
/// signal, and its task panicked before `handle` saw the signal. Nothing is
/// handling the delivery and `handle` never recorded a start, yet the WAL
/// holds the run in flight, and the next start's recovery resumes it. The
/// next delivery therefore finds a start on record: it is acked and the
/// claim marked, and the trigger is not started again. The run stays in
/// flight in the WAL until a resume picks it up.
#[tokio::test]
async fn a_panic_after_the_first_wal_write_is_acked_and_marked_and_not_run_again() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::DiesAfterStart);

    let first = arena.deliver(1, false).await;
    assert!(first.panicked && first.acks + first.naks == 0, "{first:?}");
    let first_run = arena.worker.starts()[0].0;
    assert_eq!(
        arena.wal().await,
        vec![(first_run.to_string(), WalRun::InFlight)]
    );

    let again = arena.deliver(2, false).await;

    assert_eq!(
        arena.worker.starts().len(),
        1,
        "the redelivery must not start the trigger again"
    );
    assert_eq!(
        arena
            .claim(SEQ)
            .await
            .map(|row| (row.started, row.invocation_id)),
        Some((true, Some(first_run.to_string()))),
        "the claim is marked, naming the run that started"
    );
    assert_eq!((again.acks, again.naks), (1, 0), "{again:?}");
    assert_eq!(
        arena.wal().await,
        vec![(first_run.to_string(), WalRun::InFlight)],
        "the run is left in flight for a resume"
    );
    assert_eq!(
        arena.counts(),
        (1, 0, 0),
        "counted as a copy of a run that started"
    );
}

/// **A copy that meets a claim whose first copy is still setting up is
/// dropped.** The first copy has its worker permit and waits in setup,
/// before its first WAL write, with nothing on record; a copy arrives. It
/// is answered with nothing and starts nothing: the first copy is still
/// being handled, and taking its claim would run the trigger twice. The
/// first copy then starts once.
#[tokio::test]
async fn a_copy_that_meets_a_first_copy_still_setting_up_is_dropped() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::WaitsInSetup);
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;

    let copy = arena.deliver(2, false).await;

    assert_eq!(copy, Answered::default(), "the copy is dropped, unanswered");
    assert_eq!(arena.worker.starts().len(), 1);
    assert_eq!(
        arena.counts(),
        (0, 1, 0),
        "counted as a copy of a claim in hand"
    );
    arena.worker.setup_gate.add_permits(1);
    let first = arena.finish(first).await;
    assert_eq!(first.acks, 1, "{first:?}");
    assert_eq!(arena.worker.starts().len(), 1, "the trigger starts once");
    assert_eq!(arena.claim(SEQ).await.map(|row| row.started), Some(true));
}

/// **A copy that meets a claim whose first copy is running with its mark
/// still pending is dropped.** The run made its first WAL write, so a
/// start is on record, and the claim store refuses the mark, so the first
/// copy is in its retries. Nothing may settle the delivery under it: the
/// copy is dropped unanswered, and the first copy's own fallback acks the
/// delivery when its retries run out.
#[tokio::test]
async fn a_copy_that_meets_a_first_copy_whose_mark_is_pending_is_dropped() {
    let (arena, _server) = arena().await;
    arena.worker.queue(Run::WaitsAfterStart);
    refuse_updates(&arena.claim_db).await;
    let first = arena.spawn_delivery(SEQ, 1, false);
    arena.worker.until_waiting(1).await;

    let copy = arena.deliver(2, false).await;

    assert_eq!(copy, Answered::default(), "the copy is dropped, unanswered");
    assert_eq!(arena.claim(SEQ).await.map(|row| row.started), Some(false));
    assert_eq!(
        arena.counts(),
        (0, 1, 0),
        "counted as a copy of a claim in hand"
    );
    arena.worker.run_gate.add_permits(1);
    let first = arena.finish(first).await;
    assert_eq!(first.acks, 1, "{first:?}");
    assert_eq!(arena.worker.starts().len(), 1, "the trigger starts once");
}

/// **A copy that meets a claim whose first copy is waiting for a worker
/// permit is dropped.** One worker permit, held by another trigger waiting
/// in setup; this trigger's first copy claims and waits for the permit, in
/// a hold that keeps its delivery alive. A copy arrives and is dropped
/// unanswered; once the permit frees, the first copy starts the trigger
/// once.
#[tokio::test]
async fn a_copy_that_meets_a_first_copy_waiting_for_a_permit_is_dropped() {
    let (arena, _server) = arena().await;
    const OTHER: u64 = 2;
    arena.worker.queue(Run::WaitsInSetup);
    let other = arena.spawn_delivery(OTHER, 1, false);
    arena.worker.until_waiting(1).await;
    let first = arena.spawn_delivery(SEQ, 1, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while arena.answers(first.copy).await.keepalives == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first copy never waited for the permit"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        arena.claim(SEQ).await.is_some(),
        "the first copy holds the claim"
    );

    let copy = arena.deliver(2, false).await;

    assert_eq!(copy, Answered::default(), "the copy is dropped, unanswered");
    assert_eq!(
        arena.worker.starts().len(),
        1,
        "only the other trigger runs"
    );
    assert_eq!(
        arena.counts(),
        (0, 1, 0),
        "counted as a copy of a claim in hand"
    );
    arena.worker.setup_gate.add_permits(1);
    arena.finish(other).await;
    let first = arena.finish(first).await;
    assert_eq!(first.acks, 1, "{first:?}");
    assert_eq!(arena.worker.starts().len(), 2, "each trigger starts once");
    assert_eq!(arena.claim(SEQ).await.map(|row| row.started), Some(true));
}
