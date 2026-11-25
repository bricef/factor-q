//! A billed model call that fails, and a stop before the run's `failed`
//! is out (issue 1108).
//!
//! A call's `llm.failure` is the only event that carries what the call
//! cost: its usage on the payload, its cost on the envelope. The runner
//! closes the call's row in the worker's store first, then publishes the
//! call's `llm.dispatched` and `llm.failure`, then marks the run's row
//! finished with its `failed` stored. A stop anywhere between the close
//! and the mark leaves the run in flight, and the next start reproduces
//! the failure from the call's row. Three places the stop can fall:
//!
//! - (a) at `llm.dispatched`: neither of the call's events went out;
//! - (b) at `llm.failure`: `llm.dispatched` went out, `llm.failure` did
//!   not;
//! - (c) after both, before the run's mark: the daemon stopped between.
//!
//! After the resume, in each: the call's usage and cost are on the stream
//! exactly once, and the run's `failed` carries the run's real totals.
//!
//! The recording sink keeps every copy it is given, as the stream does
//! once the broker's duplicate window has passed, so (c) is checked where
//! the window no longer helps: in the projection the cost reports read.

use super::*;
use crate::control_plane::projection::store::ProjectionStore;
use crate::test_support::oracle::check_resume_trace;
use crate::test_support::sim::{RecordingSink, ScriptedTool};
use fq_tools::ToolResult;

/// The tool the run's first turn asks for.
const TOOL: &str = "lost_failure_tool";

/// The run's first call asks for the tool and is billed for this many
/// tokens in and out.
const FIRST: (u32, u32) = (10_000, 2_000);

/// The second call comes back empty and is billed for its prompt.
const EMPTY_PROMPT: u32 = 1_000;

/// The publishes of a run whose second call fails: `triggered`, the first
/// call's three events, the tool's three, then the second call's
/// `llm.request` (7), `llm.dispatched` (8) and `llm.failure` (9), then the
/// run's `failed` and `invocation.archived`.
const AT_DISPATCHED: usize = 8;
const AT_FAILURE: usize = 9;

fn usage(input: u32, output: u32) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        reasoning_tokens: None,
    }
}

/// What `usage` costs at the test's rates.
fn cost_of(usage: TokenUsage) -> f64 {
    test_pricing()
        .lookup("claude-haiku")
        .expect("priced")
        .calculate(&usage)
        .2
}

/// What the failing call was billed.
fn failed_call_cost() -> f64 {
    cost_of(usage(EMPTY_PROMPT, 0))
}

/// What the run was billed: both calls, summed in the order the run
/// summed them.
fn billed() -> f64 {
    cost_of(usage(FIRST.0, FIRST.1)) + failed_call_cost()
}

/// The run's model: a billed turn that asks for the tool, then an empty
/// answer billed for its prompt.
fn model() -> FixtureClient {
    let llm = FixtureClient::new();
    llm.push_response(tool_use(TOOL, "tc-0", json!({}), FIRST));
    llm.push_response(ChatResponse {
        parts: vec![],
        stop_reason: StopReason::EndTurn,
        usage: usage(EMPTY_PROMPT, 0),
        reported_cost_usd: None,
    });
    llm
}

/// One run's runner, store and agent, publishing through a recording sink.
struct World {
    runner: ReducerRunner<Harness>,
    store: Arc<WorkerStore>,
    sink: Arc<RecordingSink>,
    agent: Agent,
    dir: tempfile::TempDir,
}

async fn a_world() -> World {
    let dir = tempdir().unwrap();
    let store = Arc::new(
        WorkerStore::open(&dir.path().join("worker.db"))
            .await
            .unwrap(),
    );
    let sink = Arc::new(RecordingSink::new());
    let tool = Arc::new(ScriptedTool::new(TOOL));
    tool.push_output(ToolResult::ok("the tool ran"));
    let mut tools = ToolRegistry::new();
    tools.register_fixture(tool);
    let runner = ReducerRunner::new(
        Arc::new(ReducerContext::builder().tools(Arc::new(tools)).build()),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(sink.clone() as Arc<dyn EventSink>)
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .build(),
        ),
        Harness::new(),
    );
    let agent = Agent::builder()
        .id(unique_agent_id("lost-failure"))
        .model("claude-haiku")
        .system_prompt("be brief")
        .tools([TOOL])
        .budget(1.0)
        .build()
        .unwrap();
    World {
        runner,
        store,
        sink,
        agent,
        dir,
    }
}

impl World {
    async fn run(&self) -> Result<InvocationOutcome, ExecutorError> {
        self.run_with(&model()).await
    }

    async fn run_with(&self, llm: &FixtureClient) -> Result<InvocationOutcome, ExecutorError> {
        self.runner
            .run(&self.agent, llm, TriggerSource::Manual, None, json!("go"))
            .await
    }

    fn invocation_id(&self) -> Uuid {
        self.sink
            .events()
            .iter()
            .find(|event| matches!(event.payload, EventPayload::Triggered(_)))
            .map(|event| event.envelope.invocation_id)
            .expect("the run was triggered")
    }

    /// The next start: the fault is over, and the run is resumed. The
    /// reproduction asks the model nothing.
    async fn resume(&self) -> Result<InvocationOutcome, ExecutorError> {
        self.sink.clear_fault();
        let in_flight = self.store.find_in_flight_invocations().await.unwrap();
        assert_eq!(in_flight.len(), 1, "the stop left the run in flight");
        self.runner
            .resume(&self.agent, &FixtureClient::new(), self.invocation_id())
            .await
    }

    /// Stop the run when its publish number `index` is made, and check
    /// that publish is the one meant.
    async fn stopped_at_publish(&self, index: usize, kind: &str) {
        self.sink.fail_publish_at(index);
        let outcome = self.run().await;
        assert!(
            matches!(outcome, Err(ExecutorError::Bus(_))),
            "the stop: {outcome:?}"
        );
        let refused = self.sink.refused();
        assert_eq!(
            refused
                .first()
                .map(|event| event.payload.event_type())
                .unwrap_or("nothing"),
            kind,
            "the publish the stop falls at"
        );
    }

    /// The worker's store as a second connection sees it, for the stop
    /// that falls between two writes of the run's own.
    async fn worker_db(&self) -> sqlx::SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(self.dir.path().join("worker.db")),
            )
            .await
            .unwrap()
    }

    /// Stop the run after both of the failing call's events are out and
    /// before the run marks its row finished: the write of the mark fails,
    /// as it does when the daemon stops first, and the row stays in
    /// flight. Then the daemon starts again.
    async fn stopped_before_the_runs_mark(&self) {
        let db = self.worker_db().await;
        sqlx::query(
            "CREATE TRIGGER the_daemon_stops \
             BEFORE UPDATE OF terminal_outcome ON invocation_state \
             WHEN NEW.terminal_at IS NOT NULL \
             BEGIN SELECT RAISE(ABORT, 'the daemon stopped'); END",
        )
        .execute(&db)
        .await
        .unwrap();
        let outcome = self.run().await;
        assert!(
            matches!(outcome, Err(ExecutorError::WorkerStore(_))),
            "the stop: {outcome:?}"
        );
        sqlx::query("DROP TRIGGER the_daemon_stops")
            .execute(&db)
            .await
            .unwrap();
        db.close().await;
        let kinds: Vec<&str> = self
            .sink
            .events()
            .iter()
            .map(|event| event.payload.event_type())
            .collect();
        assert_eq!(
            kinds.last(),
            Some(&"llm_failure"),
            "both of the call's events are out, and nothing after them: {kinds:?}"
        );
    }

    /// What the stopped and resumed run left: its resume's outcome, every
    /// event on the stream, and the publishes the resume made.
    async fn left(self, resumed: Result<InvocationOutcome, ExecutorError>, from: usize) -> Left {
        let events = self.sink.events();
        let resume_events = events[from..].to_vec();
        Left {
            resumed,
            invocation_id: self.invocation_id(),
            agent_id: self.agent.id().as_str().to_string(),
            events,
            resume_events,
            refused: self.sink.refused(),
            store: self.store,
            dir: self.dir,
        }
    }
}

struct Left {
    resumed: Result<InvocationOutcome, ExecutorError>,
    invocation_id: Uuid,
    agent_id: String,
    events: Vec<Event>,
    resume_events: Vec<Event>,
    refused: Vec<Event>,
    store: Arc<WorkerStore>,
    dir: tempfile::TempDir,
}

/// Two copies of one event, compared in full: id, time, parent, payload
/// and annotations, as a reader of either would see them.
fn assert_same_event(left: &Event, right: &Event, what: &str) {
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap(),
        "{what}"
    );
}

impl Left {
    fn of_kind(&self, kind: &str) -> Vec<&Event> {
        self.events
            .iter()
            .filter(|event| event.payload.event_type() == kind)
            .collect()
    }

    /// The failing call's `llm.failure`, as the stream holds it: every copy
    /// is the one event, with the call's usage and cost. Returns it.
    fn the_calls_failure(&self) -> &Event {
        let failures = self.of_kind("llm_failure");
        let Some(first) = failures.first() else {
            panic!(
                "the failing call's llm.failure, with its usage and cost, is on the stream: {:?}",
                self.events
                    .iter()
                    .map(|event| event.payload.event_type())
                    .collect::<Vec<_>>()
            );
        };
        for copy in &failures[1..] {
            assert_same_event(copy, first, "every copy on the stream is the one event");
        }
        let EventPayload::LlmFailure(payload) = &first.payload else {
            unreachable!()
        };
        assert_eq!(
            payload.usage.map(|usage| usage.input_tokens),
            Some(EMPTY_PROMPT),
            "the call's usage"
        );
        let cost = first.envelope.cost.as_ref().expect("the call is priced");
        assert!(
            (cost.total_cost - failed_call_cost()).abs() < 1e-12,
            "the call's cost: {cost:?}"
        );
        first
    }

    /// One `failed`, carrying what the run's rows record: both calls'
    /// cost, the one call that produced an outcome, and the tool call.
    fn assert_the_failed_carries_the_runs_totals(&self) {
        let failed = self.of_kind("failed");
        let [failed] = &failed[..] else {
            panic!("one failed: {failed:?}");
        };
        let EventPayload::Failed(payload) = &failed.payload else {
            unreachable!()
        };
        let totals = payload.partial_totals;
        assert!(
            (totals.total_cost - billed()).abs() < 1e-12,
            "the run's cost, {}, is on its failed: {totals:?}",
            billed()
        );
        assert_eq!(
            (totals.total_llm_calls, totals.total_tool_calls),
            (1, 1),
            "the run's call counts: {totals:?}"
        );
        assert!(
            matches!(&self.resumed, Err(ExecutorError::Llm(_))),
            "the resume ends on the reproduced failure: {:?}",
            self.resumed
        );
    }

    /// What an operator reads: the projection, folded from every event on
    /// the stream, copies included, gives the run the cost it was billed,
    /// once, from its two calls' events.
    async fn assert_the_projection_counts_the_cost_once(&self) {
        let projection = ProjectionStore::open(&self.dir.path().join("projection.db"))
            .await
            .unwrap();
        for (seq, event) in self.events.iter().enumerate() {
            projection
                .insert_event(event, Some(seq as u64))
                .await
                .unwrap();
        }
        let run = projection
            .cost_of_invocation(&self.invocation_id.to_string())
            .await
            .unwrap()
            .expect("the run's cost is on record");
        assert!(
            (run.total_cost - billed()).abs() < 1e-12,
            "the run's cost in the projection is {}, billed {}",
            run.total_cost,
            billed()
        );
        assert_eq!(
            (run.event_count, run.total_input_tokens),
            (2, i64::from(FIRST.0 + EMPTY_PROMPT)),
            "one row for each call"
        );
        let agents = projection
            .cost_summary(Some(&self.agent_id), None)
            .await
            .unwrap();
        let [agent] = &agents[..] else {
            panic!("one agent: {agents:?}");
        };
        assert!(
            (agent.total_cost - billed()).abs() < 1e-12,
            "fq costs reads {} for the agent, billed {}",
            agent.total_cost,
            billed()
        );
    }

    /// The resume's own publishes are a resume trace, and the run is over:
    /// nothing is left in flight for the next start.
    async fn assert_the_run_ended(&self) {
        if let Err(violations) = check_resume_trace(&self.resume_events) {
            let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
            panic!("the resume's trace:\n{}", lines.join("\n"));
        }
        assert!(
            self.store
                .find_in_flight_invocations()
                .await
                .unwrap()
                .is_empty(),
            "nothing for the next start to resume"
        );
    }
}

/// **Red before issue 1108.** (a) The stop falls at the failing call's
/// `llm.dispatched`: neither of the call's events went out. The resume
/// publishes the call's `llm.failure`, then the run's `failed` with the
/// run's totals.
#[tokio::test]
async fn a_stop_at_the_calls_dispatched_keeps_its_cost_on_the_stream() {
    let world = a_world().await;
    world
        .stopped_at_publish(AT_DISPATCHED, "llm_dispatched")
        .await;
    let from = world.sink.events().len();
    let resumed = world.resume().await;
    let left = world.left(resumed, from).await;
    left.the_calls_failure();
    assert_eq!(left.of_kind("llm_failure").len(), 1, "one copy");
    left.assert_the_failed_carries_the_runs_totals();
    left.assert_the_projection_counts_the_cost_once().await;
    left.assert_the_run_ended().await;
}

/// **Red before issue 1108.** (b) The stop falls at the failing call's
/// `llm.failure`: its `llm.dispatched` went out. The resume publishes the
/// very event the stop lost, chained to that `llm.dispatched`, then the
/// run's `failed` with the run's totals.
#[tokio::test]
async fn a_stop_at_the_calls_failure_keeps_its_cost_on_the_stream() {
    let world = a_world().await;
    world.stopped_at_publish(AT_FAILURE, "llm_failure").await;
    let from = world.sink.events().len();
    let resumed = world.resume().await;
    let left = world.left(resumed, from).await;
    let failure = left.the_calls_failure();
    assert_eq!(left.of_kind("llm_failure").len(), 1, "one copy");
    assert_same_event(
        failure,
        &left.refused[0],
        "the resume publishes the llm.failure the stop lost, itself",
    );
    let dispatched = left.of_kind("llm_dispatched");
    assert_eq!(
        failure.envelope.parent_event_id,
        dispatched.last().map(|event| event.envelope.event_id),
        "chained to the call's llm.dispatched, which is on the stream"
    );
    left.assert_the_failed_carries_the_runs_totals();
    left.assert_the_projection_counts_the_cost_once().await;
    left.assert_the_run_ended().await;
}

/// **Red before issue 1108.** (c) Both of the failing call's events went
/// out, and the daemon stopped before the run marked its row finished.
/// The resume cannot tell this from (b), and publishes the call's
/// `llm.failure` again: the same event, with the same `event_id`, which a
/// broker inside its duplicate window drops and which the projection, an
/// upsert on `event_id`, counts once past it. The spend is not counted
/// twice, and the run's `failed` carries the run's totals.
#[tokio::test]
async fn a_stop_after_both_of_the_calls_events_does_not_count_its_cost_twice() {
    let world = a_world().await;
    world.stopped_before_the_runs_mark().await;
    let from = world.sink.events().len();
    let resumed = world.resume().await;
    let left = world.left(resumed, from).await;
    left.the_calls_failure();
    assert_eq!(
        left.of_kind("llm_failure").len(),
        2,
        "the resume's copy is the second on the stream, so the projection below meets both"
    );
    left.assert_the_failed_carries_the_runs_totals();
    left.assert_the_projection_counts_the_cost_once().await;
    left.assert_the_run_ended().await;
}

/// A row that keeps no `llm.failure` (one closed before worker schema
/// v13), or one that is not this call's (only a hand edit makes one):
/// nothing is published in its name. The run still ends on its `failed`,
/// which carries the run's totals, the failed call's cost among them.
#[tokio::test]
async fn a_row_without_the_calls_own_failure_publishes_none() {
    for planted in ["none", "another call's"] {
        let world = a_world().await;
        world.stopped_at_publish(AT_FAILURE, "llm_failure").await;
        let stored = match planted {
            "none" => None,
            _ => {
                let mut other = world.sink.refused()[0].clone();
                let EventPayload::LlmFailure(payload) = &mut other.payload else {
                    unreachable!()
                };
                payload.call_id = Uuid::now_v7();
                Some(serde_json::to_string(&other).unwrap())
            }
        };
        let db = world.worker_db().await;
        let planted_rows = sqlx::query(
            "UPDATE llm_dispatch SET failure_event = ? WHERE failure_event IS NOT NULL",
        )
        .bind(stored)
        .execute(&db)
        .await
        .unwrap()
        .rows_affected();
        assert_eq!(
            planted_rows, 1,
            "the failed call's row kept its llm.failure"
        );
        db.close().await;
        let from = world.sink.events().len();
        let resumed = world.resume().await;
        let left = world.left(resumed, from).await;
        assert_eq!(
            left.of_kind("llm_failure").len(),
            0,
            "{planted}: nothing goes out in the call's name"
        );
        left.assert_the_failed_carries_the_runs_totals();
        left.assert_the_run_ended().await;
    }
}

/// A provider error whose text is longer than the broker takes. The stop
/// fell at the call's `llm.dispatched`, so the live run never measured its
/// `llm.failure`; the resume does, before it publishes it, and ends the
/// run on the refusal's `failed`, as the live run would have (#828),
/// instead of returning the refusal and leaving the run in flight for
/// every start to refuse again. That `failed` carries the run's totals.
#[tokio::test]
async fn a_reproduced_failure_too_large_for_the_broker_ends_the_run_on_the_refusal() {
    const LIMIT: usize = 64 * 1024;
    let world = a_world().await;
    world.sink.set_max_payload(LIMIT);
    let llm = FixtureClient::new();
    llm.push_response(tool_use(TOOL, "tc-0", json!({}), FIRST));
    llm.push_error(crate::llm::LlmError::RequestFailed("x".repeat(2 * LIMIT)));
    world.sink.fail_publish_at(AT_DISPATCHED);
    let outcome = world.run_with(&llm).await;
    assert!(
        matches!(outcome, Err(ExecutorError::Bus(_))),
        "the stop: {outcome:?}"
    );
    let from = world.sink.events().len();
    let resumed = world.resume().await;
    let left = world.left(resumed, from).await;
    assert!(
        matches!(
            &left.resumed,
            Err(ExecutorError::InvocationFailed {
                kind: crate::events::FailureKind::PayloadTooLarge,
                ..
            })
        ),
        "the resume ends on the refusal: {:?}",
        left.resumed
    );
    let kinds: Vec<&str> = left
        .resume_events
        .iter()
        .map(|event| event.payload.event_type())
        .collect();
    assert_eq!(
        kinds,
        ["failed", "invocation_archived"],
        "the resume's events"
    );
    let EventPayload::Failed(failed) = &left.resume_events[0].payload else {
        unreachable!()
    };
    assert!(
        matches!(
            failed.detail,
            Some(crate::events::FailureDetail::PayloadTooLarge {
                event_type: crate::events::EventKind::LlmFailure,
                ..
            })
        ),
        "the refused event is the call's llm.failure: {:?}",
        failed.detail
    );
    let totals = failed.partial_totals;
    assert!(
        (totals.total_cost - cost_of(usage(FIRST.0, FIRST.1))).abs() < 1e-12
            && (totals.total_llm_calls, totals.total_tool_calls) == (1, 1),
        "the run's totals: {totals:?}"
    );
    assert!(
        left.store
            .find_in_flight_invocations()
            .await
            .unwrap()
            .is_empty(),
        "nothing for the next start to resume"
    );
}
