//! A `failed` is in the `Setup` phase exactly when its run never made its
//! first WAL write (issue 1024).
//!
//! Readers take `Setup` to mean the run never started: the control plane
//! records no start for it, so its trigger's claim is not marked started
//! and a copy of the trigger can still run. Any other phase on a run with
//! no WAL row would record a start that never happened; `Setup` on a run
//! with a row would hide one that did.
//!
//! Every test publishes through [`WalWitness`], which notes for each
//! event whether the run had a WAL row at that moment, and each `failed`
//! is checked against that note. The first test is the gate: it refuses
//! every publish of a fresh run in turn, a host notice queued before the
//! first step included, so a publish added before the first WAL write is
//! refused by it too, and a `failed` that does not follow the rule fails
//! it. The other two are the failures that are not size refusals: a
//! reducer whose first step errors, and a refusal for an unavailable MCP
//! server, whose own events are refused in turn as well.

use super::*;
use crate::bus::{BusError, EventSink};
use crate::events::{EventKind, FailurePhase};
use crate::trigger::Trigger;
use crate::worker::DurableStart;
use crate::worker::reducer::types::StepOutput;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The `max_payload` a refused publish is reported against.
const LIMIT: usize = 4 * 1024;

/// A sink that notes, for every event it takes, whether the event's run
/// had a WAL row when it was published, and refuses one publish, chosen
/// by its index, as a broker refuses an event over its `max_payload`.
struct WalWitness {
    store: Arc<WorkerStore>,
    refuse_at: Option<usize>,
    next: AtomicUsize,
    published: Mutex<Vec<(Event, bool)>>,
}

impl WalWitness {
    fn new(store: Arc<WorkerStore>, refuse_at: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            store,
            refuse_at,
            next: AtomicUsize::new(0),
            published: Mutex::new(Vec::new()),
        })
    }

    fn published(&self) -> Vec<(Event, bool)> {
        self.published.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl EventSink for WalWitness {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        if self.refuse_at == Some(index) {
            return Err(BusError::PayloadTooLarge {
                event_type: EventKind::from(&event.payload),
                size: 2 * LIMIT,
                limit: LIMIT,
            });
        }
        let started = self
            .store
            .get_invocation_state(&event.envelope.invocation_id.to_string())
            .await
            .expect("the WAL reads")
            .is_some();
        self.published
            .lock()
            .unwrap()
            .push((event.clone(), started));
        Ok(index as u64)
    }
}

/// How many `failed` events `published` holds for runs that had not
/// started and for runs that had, after checking each against the rule.
fn check_failures(published: &[(Event, bool)], context: &str) -> (usize, usize) {
    let (mut unstarted, mut started) = (0, 0);
    for (event, had_row) in published {
        let EventPayload::Failed(failed) = &event.payload else {
            continue;
        };
        let setup = matches!(failed.phase, FailurePhase::Setup);
        assert_eq!(
            setup,
            !had_row,
            "{context}: a `failed` with phase {:?} for a run {} ({})",
            failed.phase,
            if *had_row {
                "with a WAL row"
            } else {
                "with no WAL row"
            },
            failed.error_message
        );
        if *had_row {
            started += 1;
        } else {
            unstarted += 1;
        }
    }
    (unstarted, started)
}

/// A runner over `store` driving `reducer`, publishing through `sink`,
/// with `states` as its table of shared MCP servers.
fn runner_over<R: Reducer + Send + Sync>(
    store: &Arc<WorkerStore>,
    sink: Arc<WalWitness>,
    reducer: R,
    states: crate::mcp::McpServerStates,
) -> ReducerRunner<R> {
    ReducerRunner::new(
        Arc::new(
            ReducerContext::builder()
                .tools(Arc::new(ToolRegistry::with_builtins()))
                .build(),
        ),
        Arc::new(
            RunnerConfig::builder()
                .event_sink(sink as Arc<dyn EventSink>)
                .pricing(test_pricing())
                .store(store.clone())
                .worker_id(test_worker_id())
                .mcp_states(states)
                .build(),
        ),
        reducer,
    )
}

async fn open_store(dir: &std::path::Path) -> Arc<WorkerStore> {
    Arc::new(WorkerStore::open(&dir.join("events.db")).await.unwrap())
}

fn agent(prefix: &str) -> Agent {
    Agent::builder()
        .id(unique_agent_id(prefix))
        .model("claude-haiku")
        .system_prompt("be brief")
        .budget(1.0)
        .build()
        .unwrap()
}

/// One fresh run of a two-turn script, with a host notice queued for it
/// before its first step, through a sink that refuses its `refuse_at`th
/// publish. Returns what the sink took.
async fn a_fresh_run(refuse_at: Option<usize>) -> Vec<(Event, bool)> {
    let dir = tempdir().unwrap();
    let store = open_store(dir.path()).await;
    let sink = WalWitness::new(store.clone(), refuse_at);
    let runner = runner_over(
        &store,
        sink.clone(),
        Harness::new(),
        crate::mcp::McpServerStates::default(),
    );
    let invocation_id = Uuid::now_v7();
    runner.queue_host_notice(
        invocation_id,
        "test",
        format!(
            "{} queued before the first step",
            crate::events::HOST_NOTICE_SENTINEL
        ),
    );
    let llm = FixtureClient::new();
    llm.push_response(tool_use(
        "builtin__self_inspect",
        "tc-0",
        json!({}),
        (10, 5),
    ));
    llm.push_response(canned("done", 10, 5));
    let (durable_start, _started) = DurableStart::channel(invocation_id);
    let _ = runner
        .run_signalling(
            &agent("unstarted-sweep"),
            &llm,
            Trigger::named(
                Uuid::now_v7(),
                TriggerSource::Manual,
                None,
                json!({"input": "go"}),
            ),
            Some(1),
            durable_start,
        )
        .await;
    sink.published()
}

/// The gate: every publish of a fresh run refused in turn, from its
/// `triggered` to its archive. A refusal before the first WAL write ends
/// the run on a `failed` in `Setup`; one after it, on a `failed` in the
/// phase of what was refused.
#[tokio::test]
async fn a_run_refused_at_each_publish_fails_in_setup_only_before_its_first_wal_write() {
    let clean = a_fresh_run(None).await;
    assert!(
        clean
            .iter()
            .any(|(event, _)| matches!(event.payload, EventPayload::Completed(_))),
        "the script completes when nothing is refused"
    );
    let (mut unstarted, mut started) = check_failures(&clean, "nothing refused");
    for refuse_at in 0..clean.len() {
        let published = a_fresh_run(Some(refuse_at)).await;
        let (u, s) = check_failures(&published, &format!("publish {refuse_at} refused"));
        unstarted += u;
        started += s;
    }
    // The sweep reached both sides of the first WAL write: `triggered`
    // and the host notice before it, and the events of the first turn
    // after it.
    assert!(
        unstarted >= 2,
        "{unstarted} runs failed before their first WAL write"
    );
    assert!(started >= 2, "{started} runs failed after it");
}

/// A reducer that errors on its first step fails before the step's row is
/// written.
#[tokio::test]
async fn a_reducer_failing_its_first_step_fails_in_setup() {
    struct FailingReducer;
    impl Reducer for FailingReducer {
        fn step(&self, _input: StepInput) -> Result<StepOutput, HarnessError> {
            Err(HarnessError {
                kind: crate::worker::reducer::types::HarnessErrorKind::InternalError,
                message: "synthetic reducer defect".to_string(),
            })
        }
    }

    let dir = tempdir().unwrap();
    let store = open_store(dir.path()).await;
    let sink = WalWitness::new(store.clone(), None);
    let runner = runner_over(
        &store,
        sink.clone(),
        FailingReducer,
        crate::mcp::McpServerStates::default(),
    );
    runner
        .run(
            &agent("unstarted-reducer"),
            &FixtureClient::new(),
            TriggerSource::Manual,
            None,
            json!("x"),
        )
        .await
        .expect_err("the reducer's error ends the run");
    let (unstarted, started) = check_failures(&sink.published(), "the first step errors");
    assert_eq!((unstarted, started), (1, 0));
}

/// A run refused for an unavailable MCP server never starts, and its
/// `failed` is in `Setup`; so is the `failed` it ends on when the
/// refusal's own `triggered`, or its own `failed`, is refused for size.
#[tokio::test]
async fn a_refusal_for_an_unavailable_mcp_server_fails_in_setup_at_each_publish() {
    for refuse_at in [None, Some(0), Some(1)] {
        let dir = tempdir().unwrap();
        let store = open_store(dir.path()).await;
        let sink = WalWitness::new(store.clone(), refuse_at);
        let states = crate::mcp::McpServerStates::default();
        states.unavailable(
            "wedged",
            "no initialize response within the start-up deadline".to_string(),
            2,
            None,
        );
        let runner = runner_over(&store, sink.clone(), Harness::new(), states);
        runner
            .run(
                &agent_declaring("unstarted-mcp", "wedged"),
                &FixtureClient::new(),
                TriggerSource::Manual,
                None,
                json!("x"),
            )
            .await
            .expect_err("the run is refused");
        let (unstarted, started) =
            check_failures(&sink.published(), &format!("publish {refuse_at:?} refused"));
        assert_eq!(
            (unstarted, started),
            (1, 0),
            "publish {refuse_at:?} refused"
        );
    }
}

/// The rule itself, both ways round. No caller builds a `Setup` failure
/// for a run with a row today; if one did, the run would read as never
/// started, so the rule gives it the phase of the loop instead.
#[test]
fn the_phase_is_setup_exactly_when_the_run_has_no_wal_row() {
    use super::super::publish_failure::phase_for;
    for doing in [
        FailurePhase::Setup,
        FailurePhase::Reducer,
        FailurePhase::HostStepBudget,
        FailurePhase::Budget,
        FailurePhase::LlmRequest,
        FailurePhase::LlmResponse,
        FailurePhase::ToolCall,
        FailurePhase::ToolResult,
    ] {
        assert!(matches!(phase_for(false, doing), FailurePhase::Setup));
        assert!(!matches!(phase_for(true, doing), FailurePhase::Setup));
    }
    assert!(matches!(
        phase_for(true, FailurePhase::ToolCall),
        FailurePhase::ToolCall
    ));
}
