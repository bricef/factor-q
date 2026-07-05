//! The hermetic sim world (reducer verification plan, slice 3).
//!
//! Runs a real [`ReducerRunner`] with every platform dependency
//! swapped for a deterministic in-process double: a scripted LLM
//! ([`FixtureClient`]), a scripted recording tool, an in-memory
//! [`EventSink`] with publish-fault injection, a counter-based
//! [`Clock`], and a tempdir [`WorkerStore`]. No NATS, no network, no
//! wall clock — the same scripts and seed produce the same trace,
//! which is what slices 4–7 (resume equivalence, crash DST, budget
//! properties, soak) build on.
//!
//! A "crash" here is a publish fault at a chosen operation index:
//! the sink returns an error, the runner surfaces it, and
//! [`SimWorld::resume`] reopens the same store with fresh doubles and
//! drives recovery — fault points sit between our operations, exactly
//! as the plan's crash semantics prescribe.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::agent::Agent;
use crate::bus::BusError;
use crate::events::{Event, EventPayload, TriggerSource};
use crate::llm::fixture::FixtureClient;
use crate::pricing::PricingTable;
use crate::tools::ToolRegistry;
use crate::worker::reducer::runner::{
    Clock, EventSink, ReducerContext, ReducerRunner, RunnerConfig,
};
use crate::worker::store::WorkerStore;
use crate::worker::{ExecutorError, InvocationOutcome, WorkerId};
use fq_tools::{Tool, ToolContext, ToolError, ToolResult};

/// Deterministic time + entropy: a strictly monotonic millisecond
/// counter and a seeded xorshift stream.
pub struct SimClock {
    ms: AtomicU64,
    rng: AtomicU64,
}

impl SimClock {
    pub fn new(seed: u64) -> Self {
        Self {
            ms: AtomicU64::new(1_000_000),
            // Xorshift must not start at zero.
            rng: AtomicU64::new(seed.max(1)),
        }
    }
}

impl Clock for SimClock {
    fn now_ms(&self) -> u64 {
        self.ms.fetch_add(1, Ordering::SeqCst)
    }

    fn unix_now_ms(&self) -> i64 {
        self.ms.fetch_add(1, Ordering::SeqCst) as i64
    }

    fn rand_u64(&self) -> u64 {
        self.rng
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |mut x| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                Some(x)
            })
            .expect("fetch_update with Some never fails")
    }
}

/// In-memory [`EventSink`] that records every published event and can
/// inject a publish fault at a chosen operation index.
#[derive(Default)]
pub struct RecordingSink {
    events: Mutex<Vec<Event>>,
    fail_at: AtomicUsize,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            fail_at: AtomicUsize::new(usize::MAX),
        }
    }

    /// Inject a fault: the publish with this zero-based index (over
    /// the sink's lifetime) fails, simulating a crash at that
    /// operation boundary.
    pub fn fail_publish_at(&self, index: usize) {
        self.fail_at.store(index, Ordering::SeqCst);
    }

    /// Clear the fault so a resumed run can publish normally.
    pub fn clear_fault(&self) {
        self.fail_at.store(usize::MAX, Ordering::SeqCst);
    }

    /// Everything successfully published so far.
    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl EventSink for RecordingSink {
    async fn publish(&self, event: &Event) -> Result<(), BusError> {
        let mut events = self.events.lock().unwrap();
        if events.len() >= self.fail_at.load(Ordering::SeqCst) {
            return Err(BusError::Publish("sim: injected publish fault".to_string()));
        }
        events.push(event.clone());
        Ok(())
    }
}

/// A scripted tool that records every dispatch — the observable that
/// makes the tool-idempotency assumption checkable (claim R3's
/// at-most-once assertion).
pub struct ScriptedTool {
    name: String,
    outputs: Mutex<VecDeque<ToolResult>>,
    dispatches: Arc<Mutex<Vec<Value>>>,
}

impl ScriptedTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outputs: Mutex::new(VecDeque::new()),
            dispatches: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Queue the next output (defaults to `"sim-ok"` when the queue
    /// runs dry, so scripts only need to specify what matters).
    pub fn push_output(&self, result: ToolResult) {
        self.outputs.lock().unwrap().push_back(result);
    }

    /// Handle onto the dispatch log, for at-most-once assertions.
    pub fn dispatches(&self) -> Arc<Mutex<Vec<Value>>> {
        Arc::clone(&self.dispatches)
    }
}

#[async_trait::async_trait]
impl Tool for ScriptedTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "scripted sim tool"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(
        &self,
        _ctx: &ToolContext<'_>,
        params: Value,
    ) -> Result<ToolResult, ToolError> {
        self.dispatches.lock().unwrap().push(params);
        Ok(self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ToolResult::ok("sim-ok")))
    }
}

/// One hermetic invocation world: real runner, doubled platform.
pub struct SimWorld {
    pub clock: Arc<SimClock>,
    pub sink: Arc<RecordingSink>,
    pub tool: Arc<ScriptedTool>,
    store: Arc<WorkerStore>,
    agent: Agent,
    runner: ReducerRunner,
    // Keeps the store directory alive for the world's lifetime.
    _store_dir: TempDir,
}

/// The tool name every [`SimWorld`] agent declares.
pub const SIM_TOOL: &str = "sim_tool";

impl SimWorld {
    /// Build a world with the given entropy seed and per-invocation
    /// budget. The agent declares one scripted tool ([`SIM_TOOL`]).
    pub async fn new(seed: u64, budget: f64) -> Self {
        let clock = Arc::new(SimClock::new(seed));
        let sink = Arc::new(RecordingSink::new());
        let tool = Arc::new(ScriptedTool::new(SIM_TOOL));

        let mut registry = ToolRegistry::new();
        registry.register(Arc::clone(&tool) as Arc<dyn Tool>);

        let store_dir = TempDir::new().expect("sim store dir");
        let store = Arc::new(
            WorkerStore::open(&store_dir.path().join("sim.db"))
                .await
                .expect("sim worker store"),
        );

        let agent = Agent::builder()
            .id("sim-agent")
            .model("claude-sim")
            .system_prompt("You are the sim agent.")
            .tools([SIM_TOOL])
            .budget(budget)
            .build()
            .expect("sim agent");

        let runner = Self::build_runner(&clock, &sink, &registry, &store);

        Self {
            clock,
            sink,
            tool,
            store,
            agent,
            runner,
            _store_dir: store_dir,
        }
    }

    fn build_runner(
        clock: &Arc<SimClock>,
        sink: &Arc<RecordingSink>,
        registry: &ToolRegistry,
        store: &Arc<WorkerStore>,
    ) -> ReducerRunner {
        ReducerRunner::new(
            Arc::new(
                ReducerContext::builder()
                    .tools(Arc::new(registry.clone()))
                    .build(),
            ),
            Arc::new(
                RunnerConfig::builder()
                    .event_sink(Arc::clone(sink) as Arc<dyn EventSink>)
                    .clock(Arc::clone(clock) as Arc<dyn Clock>)
                    .pricing(Arc::new(PricingTable::empty()))
                    .store(Arc::clone(store))
                    .worker_id(WorkerId::new("sim-worker").expect("worker id"))
                    .build(),
            ),
            crate::worker::reducer::Harness::new(),
        )
    }

    /// Run one invocation against a scripted LLM.
    pub async fn run(&self, llm: &FixtureClient) -> Result<InvocationOutcome, ExecutorError> {
        self.runner
            .run(
                &self.agent,
                llm,
                TriggerSource::Manual,
                None,
                json!("sim-go"),
            )
            .await
    }

    /// Resume the single in-flight invocation after a crash, with a
    /// fresh LLM script for the remaining turns. Clears any injected
    /// sink fault first — the crash is over.
    pub async fn resume(&self, llm: &FixtureClient) -> Result<InvocationOutcome, ExecutorError> {
        self.sink.clear_fault();
        let in_flight = self
            .store
            .find_in_flight_invocations()
            .await
            .expect("query in-flight");
        assert_eq!(
            in_flight.len(),
            1,
            "expected exactly one crashed invocation"
        );
        let invocation_id = in_flight[0]
            .invocation_id
            .parse()
            .expect("invocation id parses");
        self.runner.resume(&self.agent, llm, invocation_id).await
    }

    /// The invocation id of the (single) trace captured so far.
    pub fn invocation_id(&self) -> uuid::Uuid {
        let events = self.sink.events();
        events
            .iter()
            .find(|e| matches!(e.payload, EventPayload::Triggered(_)))
            .map(|e| e.envelope.invocation_id)
            .expect("no triggered event captured yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MessageToolCall, StopReason, TokenUsage, ToolCallId};
    use crate::llm::ChatResponse;
    use crate::test_support::oracle;

    pub(super) fn sim_tool_call(call_id: &str) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: vec![MessageToolCall {
                tool_call_id: ToolCallId::new(call_id).unwrap(),
                tool_name: SIM_TOOL.to_string(),
                parameters: json!({"step": call_id}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    pub(super) fn end_turn(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 120,
                output_tokens: 8,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        }
    }

    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events
            .iter()
            .map(crate::test_support::events::event_kind)
            .collect()
    }

    /// The whole loop — trigger, two LLM turns, one tool span,
    /// terminal, archived — runs with no NATS, no network, no wall
    /// clock, and satisfies the slice-1 oracle.
    #[tokio::test]
    async fn scripted_run_is_hermetic_and_canonical() {
        let world = SimWorld::new(42, 5.0).await;
        let llm = FixtureClient::new();
        llm.push_response(sim_tool_call("c1"));
        llm.push_response(end_turn("done"));

        let outcome = world.run(&llm).await.expect("sim run");
        assert!(
            matches!(outcome, InvocationOutcome::Completed { .. }),
            "got {outcome:?}"
        );

        oracle::assert_valid_trace(&world.sink.events());
        assert_eq!(
            world.tool.dispatches().lock().unwrap().len(),
            1,
            "scripted tool must run exactly once"
        );
    }

    /// Same seed, same scripts → same observable trace: event kinds,
    /// tool dispatch parameters, and the final persisted state blob
    /// (extracted from the archived event) are all identical.
    #[tokio::test]
    async fn same_seed_produces_the_same_trace() {
        async fn run_world() -> (Vec<&'static str>, Vec<Value>, Vec<u8>) {
            let world = SimWorld::new(7, 5.0).await;
            let llm = FixtureClient::new();
            llm.push_response(sim_tool_call("c1"));
            llm.push_response(sim_tool_call("c2"));
            llm.push_response(end_turn("done"));
            world.run(&llm).await.expect("sim run");

            let events = world.sink.events();
            let blob = events
                .iter()
                .find_map(|e| match &e.payload {
                    EventPayload::InvocationArchived(p) => Some(p.final_state_blob.clone()),
                    _ => None,
                })
                .expect("archived event");
            let dispatches = world.tool.dispatches().lock().unwrap().clone();
            (kinds(&events), dispatches, blob)
        }

        let (kinds_a, dispatches_a, blob_a) = run_world().await;
        let (kinds_b, dispatches_b, blob_b) = run_world().await;
        assert_eq!(kinds_a, kinds_b);
        assert_eq!(dispatches_a, dispatches_b);
        assert_eq!(blob_a, blob_b, "final reducer state must be deterministic");
    }

    /// Crash at the `llm.response` publish (the LLM's WAL row is
    /// already completed), then resume: recovery replays the stored
    /// result, the tool executes exactly once across both attempts,
    /// and the invocation completes.
    #[tokio::test]
    async fn crash_at_publish_then_resume_completes_with_at_most_once_tools() {
        let world = SimWorld::new(9, 5.0).await;

        // Publishes: 0 triggered, 1 llm.request, 2 llm.dispatched,
        // 3 llm.response ← fault here.
        world.sink.fail_publish_at(3);
        let llm = FixtureClient::new();
        llm.push_response(sim_tool_call("c1"));

        let err = world.run(&llm).await.expect_err("must crash at publish");
        assert!(
            matches!(err, ExecutorError::Bus(_)),
            "expected the injected publish fault, got {err:?}"
        );
        assert_eq!(
            world.tool.dispatches().lock().unwrap().len(),
            0,
            "crash happened before the tool span began"
        );

        // Resume with the remaining script: the turn-1 response is
        // replayed from the WAL (no LLM call), the tool runs, and
        // turn 2 ends the invocation.
        let resume_llm = FixtureClient::new();
        resume_llm.push_response(end_turn("after-resume"));
        let outcome = world.resume(&resume_llm).await.expect("resume");
        assert!(
            matches!(outcome, InvocationOutcome::Completed { .. }),
            "got {outcome:?}"
        );

        assert_eq!(
            world.tool.dispatches().lock().unwrap().len(),
            1,
            "the tool must run exactly once across crash and resume"
        );

        // The invocation is terminal in the WAL.
        let in_flight = world
            .store
            .find_in_flight_invocations()
            .await
            .expect("query");
        assert!(in_flight.is_empty(), "no invocation left in flight");
    }
}

#[cfg(test)]
mod resume_equivalence {
    //! Slice 4 (claim R4): suspension is structural. For any script
    //! and any span boundary, interrupting at the boundary and
    //! resuming yields the same observational trace, outcome, and
    //! tool dispatches as the uninterrupted run.
    //!
    //! Boundaries are each span's *first publish* (`1 + 3·span` —
    //! `triggered` is publish 0 and every span emits a triple). The
    //! fault lands before the span does externally-visible work, so
    //! the recorded prefix is exactly the completed spans, and resume
    //! re-runs the interrupted span in full: the combined trace must
    //! equal the reference under the observational mask.

    use super::tests::{end_turn, sim_tool_call};
    use super::*;
    use crate::llm::ChatResponse;
    use crate::test_support::oracle::observational_trace;

    /// The LLM script for `turns` tool calls followed by an end turn.
    fn script(turns: usize) -> Vec<ChatResponse> {
        let mut responses: Vec<ChatResponse> = (0..turns)
            .map(|k| sim_tool_call(&format!("c{k}")))
            .collect();
        responses.push(end_turn("all-done"));
        responses
    }

    fn load_fixture(llm: &FixtureClient, responses: &[ChatResponse]) {
        for r in responses {
            llm.push_response(r.clone());
        }
    }

    fn queue_tool_outputs(world: &SimWorld, turns: usize) {
        for k in 0..turns {
            world.tool.push_output(ToolResult::ok(format!("out-{k}")));
        }
    }

    struct RunResult {
        observed: Vec<Value>,
        summary: Option<String>,
        dispatches: Vec<Value>,
    }

    fn summary_of(outcome: &InvocationOutcome) -> Option<String> {
        match outcome {
            InvocationOutcome::Completed { response, .. } => response.content.clone(),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    async fn run_reference(seed: u64, turns: usize) -> RunResult {
        let world = SimWorld::new(seed, 5.0).await;
        queue_tool_outputs(&world, turns);
        let llm = FixtureClient::new();
        load_fixture(&llm, &script(turns));
        let outcome = world.run(&llm).await.expect("reference run");
        RunResult {
            observed: observational_trace(&world.sink.events()),
            summary: summary_of(&outcome),
            dispatches: world.tool.dispatches().lock().unwrap().clone(),
        }
    }

    async fn run_interrupted(seed: u64, turns: usize, boundary_span: usize) -> RunResult {
        let world = SimWorld::new(seed, 5.0).await;
        queue_tool_outputs(&world, turns);
        let responses = script(turns);

        // Fault at the first publish of the chosen span.
        world.sink.fail_publish_at(1 + 3 * boundary_span);
        let llm = FixtureClient::new();
        load_fixture(&llm, &responses);
        let err = world
            .run(&llm)
            .await
            .expect_err("must crash at the boundary");
        assert!(
            matches!(err, ExecutorError::Bus(_)),
            "expected the injected fault, got {err:?}"
        );

        // Resume with the not-yet-consumed LLM turns: the fixture was
        // read once per *completed* LLM span, i.e. (span + 1) / 2.
        let consumed = boundary_span.div_ceil(2);
        let resume_llm = FixtureClient::new();
        load_fixture(&resume_llm, &responses[consumed..]);
        let outcome = world.resume(&resume_llm).await.expect("resume");

        RunResult {
            observed: observational_trace(&world.sink.events()),
            summary: summary_of(&outcome),
            dispatches: world.tool.dispatches().lock().unwrap().clone(),
        }
    }

    fn assert_equivalent(reference: &RunResult, resumed: &RunResult, label: &str) {
        for (i, (a, b)) in reference
            .observed
            .iter()
            .zip(resumed.observed.iter())
            .enumerate()
        {
            assert_eq!(a, b, "observational traces diverge at event {i} ({label})");
        }
        assert_eq!(
            reference.observed.len(),
            resumed.observed.len(),
            "trace lengths diverge ({label})"
        );
        assert_eq!(
            reference.summary, resumed.summary,
            "outcome diverges ({label})"
        );
        assert_eq!(
            reference.dispatches, resumed.dispatches,
            "tool dispatches diverge ({label})"
        );
    }

    /// Every boundary of a fixed two-tool-turn script, exhaustively.
    #[tokio::test]
    async fn every_boundary_of_a_fixed_script_is_equivalent() {
        let turns = 2;
        let reference = run_reference(1234, turns).await;
        for boundary in 0..=(2 * turns) {
            let resumed = run_interrupted(1234, turns, boundary).await;
            assert_equivalent(&reference, &resumed, &format!("boundary {boundary}"));
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 24, ..Default::default()
        })]

        /// Random scripts × random boundaries × random seeds: the
        /// interrupted-and-resumed run is observationally identical
        /// to the uninterrupted one.
        #[test]
        fn interrupted_runs_are_observationally_equivalent(
            seed: u64,
            turns in 1usize..=3,
            boundary in 0usize..=6,
        ) {
            proptest::prop_assume!(boundary <= 2 * turns);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sim runtime");
            runtime.block_on(async {
                let reference = run_reference(seed, turns).await;
                let resumed = run_interrupted(seed, turns, boundary).await;
                assert_equivalent(
                    &reference,
                    &resumed,
                    &format!("seed {seed}, turns {turns}, boundary {boundary}"),
                );
            });
        }
    }
}
