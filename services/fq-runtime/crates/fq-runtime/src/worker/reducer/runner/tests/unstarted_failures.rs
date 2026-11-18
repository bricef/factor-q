//! A `failed` is in the `Setup` phase exactly when its run never made its
//! first WAL write (issue 1024). A run that did, and whose row a drop then
//! finished or handed off, publishes no `failed` at all (issue 1070;
//! `dropped_mid_step.rs`).
//!
//! Readers take `Setup` to mean the run has no start on record: the
//! control plane records no start for it, so its trigger's claim is not
//! marked started and a copy of the trigger can still run. Any other
//! phase on a run that never started would record a start that never
//! happened; `Setup` on a run with a row would hide one that did.
//!
//! Every test publishes through [`WalWitness`], which notes for each
//! event whether the run had a WAL row at that moment, and each `failed`
//! is checked against that note. Which test holds which caller of
//! `publish_failed`, so that bypassing it fails the test named:
//!
//! - `end_on_refused_publish` with no row (a host notice queued before
//!   the first step and refused): the sweep, at its second publish.
//! - `end_unstarted_on_refusal`: the unavailable-MCP test, when the
//!   refusal's own `failed` is refused. The sweep's first publish,
//!   `triggered`, comes here too, but a refused `triggered` is named
//!   `Setup` by its event type, so a bypass would not show there.
//! - `end_mid_step` with no row: the reducer whose first step errors. The
//!   sweep's refusals after the first WAL write come here with a row,
//!   where a bypass would not show.
//! - `refuse_for_unavailable_mcp`: builds its `failed` in `Setup`, so a
//!   bypass would publish the same event and nothing would show.
//!
//! The sweep also checks what each refusal ends the run in, which makes it
//! the gate for a publish added to a run: refused before the first WAL
//! write, a `failed` in `Setup`; refused while the row is in flight, a
//! `failed` in the phase of the refused event; refused once the row is
//! terminal, no `failed`. Beyond these tests, every event a runner
//! publishes in the test build passes a check that a `failed` in any
//! phase but `Setup` has a WAL row (`check_failed_phase` in
//! `publish_failure.rs`), so every test that drives a run to such a
//! `failed` fails, whichever path built it.

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

/// What the WAL held for an event's run when the event was offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAt {
    Absent,
    InFlight,
    Terminal,
}

/// A sink that notes, for every event it takes, whether the event's run
/// had a WAL row when it was published, and refuses one publish, chosen
/// by its index, as a broker refuses an event over its `max_payload`,
/// noting the refused event's type and the row it was offered with.
struct WalWitness {
    store: Arc<WorkerStore>,
    refuse_at: Option<usize>,
    next: AtomicUsize,
    published: Mutex<Vec<(Event, bool)>>,
    refused: Mutex<Option<(EventKind, RowAt)>>,
}

impl WalWitness {
    fn new(store: Arc<WorkerStore>, refuse_at: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            store,
            refuse_at,
            next: AtomicUsize::new(0),
            published: Mutex::new(Vec::new()),
            refused: Mutex::new(None),
        })
    }

    fn published(&self) -> Vec<(Event, bool)> {
        self.published.lock().unwrap().clone()
    }

    fn refused(&self) -> Option<(EventKind, RowAt)> {
        *self.refused.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl EventSink for WalWitness {
    async fn publish(&self, event: &Event) -> Result<u64, BusError> {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        let row = match self
            .store
            .get_invocation_state(&event.envelope.invocation_id.to_string())
            .await
            .expect("the WAL reads")
        {
            None => RowAt::Absent,
            Some(row) if row.terminal_at.is_some() => RowAt::Terminal,
            Some(_) => RowAt::InFlight,
        };
        let event_type = EventKind::from(&event.payload);
        if self.refuse_at == Some(index) {
            *self.refused.lock().unwrap() = Some((event_type, row));
            return Err(BusError::PayloadTooLarge {
                event_type,
                size: 2 * LIMIT,
                limit: LIMIT,
            });
        }
        self.published
            .lock()
            .unwrap()
            .push((event.clone(), row != RowAt::Absent));
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

/// The phases of the `failed` events in `published`, as the wire spells
/// them.
fn failed_phases(published: &[(Event, bool)]) -> Vec<String> {
    published
        .iter()
        .filter_map(|(event, _)| match &event.payload {
            EventPayload::Failed(failed) => Some(
                serde_json::to_value(failed.phase)
                    .expect("a phase serialises")
                    .as_str()
                    .expect("a phase is a string")
                    .to_string(),
            ),
            _ => None,
        })
        .collect()
}

/// The phase the schema gives the `failed` that ends a run in flight when
/// the broker refuses an event of this type, written out here rather than
/// taken from the runner, whose mapping is what is under test.
fn phase_when_refused_in_flight(event_type: EventKind) -> &'static str {
    match event_type {
        EventKind::LlmRequest => "llm_request",
        EventKind::LlmDispatched | EventKind::LlmResponse => "llm_response",
        EventKind::ToolCall | EventKind::ToolDispatched => "tool_call",
        EventKind::ToolResult => "tool_result",
        other => panic!(
            "the sweep refused {other:?} on a run in flight; say here what phase \
             its refusal ends the run in"
        ),
    }
}

/// What a sink took from one run, and what it refused.
struct Taken {
    published: Vec<(Event, bool)>,
    refused: Option<(EventKind, RowAt)>,
}

/// One fresh run of a two-turn script, with a host notice queued for it
/// before its first step, through a sink that refuses its `refuse_at`th
/// publish.
async fn a_fresh_run(refuse_at: Option<usize>) -> Taken {
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
    Taken {
        published: sink.published(),
        refused: sink.refused(),
    }
}

/// The gate: every publish of a fresh run refused in turn, from its
/// `triggered` to its archive. A refusal before the first WAL write ends
/// the run on one `failed`, in `Setup`; one while the row is in flight,
/// on one `failed` in the phase of the refused event
/// ([`phase_when_refused_in_flight`]); one once the row is terminal, on
/// none, since the outcome was already decided.
#[tokio::test]
async fn a_run_refused_at_each_publish_fails_in_setup_only_before_its_first_wal_write() {
    let clean = a_fresh_run(None).await;
    assert!(
        clean
            .published
            .iter()
            .any(|(event, _)| matches!(event.payload, EventPayload::Completed(_))),
        "the script completes when nothing is refused"
    );
    assert_eq!(check_failures(&clean.published, "nothing refused"), (0, 0));
    let (mut before, mut in_flight, mut terminal) = (0, 0, 0);
    for refuse_at in 0..clean.published.len() {
        let run = a_fresh_run(Some(refuse_at)).await;
        let context = format!("publish {refuse_at} refused");
        check_failures(&run.published, &context);
        let (event_type, row) = run
            .refused
            .unwrap_or_else(|| panic!("{context}: the sink refused nothing"));
        let expected: Vec<&str> = match row {
            RowAt::Absent => {
                before += 1;
                vec!["setup"]
            }
            RowAt::InFlight => {
                in_flight += 1;
                vec![phase_when_refused_in_flight(event_type)]
            }
            RowAt::Terminal => {
                terminal += 1;
                vec![]
            }
        };
        assert_eq!(
            failed_phases(&run.published),
            expected,
            "{context}: {event_type:?}, offered with the run's row {row:?}"
        );
    }
    // The sweep reached every side of the first WAL write: `triggered`
    // and the host notice before it, the events of both turns after it,
    // and the outcome and archive once the row is terminal.
    assert!(
        before >= 2,
        "{before} publishes refused before the first WAL write"
    );
    assert!(in_flight >= 2, "{in_flight} refused with the row in flight");
    assert!(terminal >= 1, "{terminal} refused with the row terminal");
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

/// A run that started and whose row a drop finished and handed off
/// publishes no `failed` at all. An operator drops the run while its model
/// call is in flight: the coordination consumer marks the row terminal,
/// the archive sweeper hands the terminal row off, and the archive's
/// acknowledgement deletes it. When the call then fails, the run finds its
/// row gone after having written it in this drive, so the drop ended it,
/// and it publishes no ending of its own (issue 1070). Its `llm.failure`,
/// the call's own record, still goes out.
///
/// Before that rule this `failed` went out with no row, in `Setup`, and
/// before the phase was read from the row, in `llm_request`.
#[tokio::test]
async fn a_run_whose_row_a_drop_handed_off_publishes_no_failed() {
    struct DroppedMidCall {
        store: Arc<WorkerStore>,
        invocation_id: Uuid,
    }
    #[async_trait::async_trait]
    impl crate::llm::LlmClient for DroppedMidCall {
        async fn chat(
            &self,
            _request: crate::llm::ChatRequest,
        ) -> Result<ChatResponse, crate::llm::LlmError> {
            let id = self.invocation_id.to_string();
            // What the coordination consumer does with the drop's
            // `invocation.operator_recovered`...
            let marked = self
                .store
                .mark_invocation_operator_terminal(&id, "failed", 1)
                .await
                .expect("the WAL writes");
            assert_eq!(marked, 1, "the run's row was there, in flight");
            // ...and the archive-ack consumer, once the sweeper's
            // `invocation.archived` for the terminal row is acknowledged.
            let deleted = self
                .store
                .delete_invocation_state(&id)
                .await
                .expect("the WAL writes");
            assert_eq!(deleted, 1);
            Err(crate::llm::LlmError::Auth(
                "provider refused the key".into(),
            ))
        }
    }

    let dir = tempdir().unwrap();
    let store = open_store(dir.path()).await;
    let sink = WalWitness::new(store.clone(), None);
    let runner = runner_over(
        &store,
        sink.clone(),
        Harness::new(),
        crate::mcp::McpServerStates::default(),
    );
    let invocation_id = Uuid::now_v7();
    let llm = DroppedMidCall {
        store: store.clone(),
        invocation_id,
    };
    let (durable_start, mut started) = DurableStart::channel(invocation_id);
    let err = runner
        .run_signalling(
            &agent("unstarted-dropped"),
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
        .await
        .expect_err("the run stops without an outcome of its own");
    assert!(!err.is_transient(), "nothing retries it: {err}");
    assert!(
        started.try_recv().is_ok(),
        "the dispatcher was told the run durably started"
    );
    let published = sink.published();
    assert!(
        published
            .iter()
            .any(
                |(event, had_row)| matches!(event.payload, EventPayload::LlmRequest(_)) && *had_row
            ),
        "the run made its first WAL write before its model call"
    );
    assert!(
        published
            .iter()
            .any(|(event, _)| matches!(event.payload, EventPayload::LlmFailure(_))),
        "the call's own record goes out"
    );
    let context = "dropped mid-call and handed off, then the call failed";
    assert_eq!(check_failures(&published, context), (0, 0), "{context}");
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
/// started, so the rule gives it the phase of the loop instead, and
/// `publish_failed` logs the mistake at warn.
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
