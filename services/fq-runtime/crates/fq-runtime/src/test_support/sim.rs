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
    /// Costs are zero (empty pricing table) — budget never trips.
    pub async fn new(seed: u64, budget: f64) -> Self {
        Self::with_pricing(seed, budget, Arc::new(PricingTable::empty())).await
    }

    /// Build a world with real pricing, for the budget properties
    /// (reducer verification, slice 6).
    pub async fn with_pricing(seed: u64, budget: f64, pricing: Arc<PricingTable>) -> Self {
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

        let runner = Self::build_runner(&clock, &sink, &registry, &store, pricing);

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
        pricing: Arc<PricingTable>,
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
                    .pricing(pricing)
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
    pub(super) fn script(turns: usize) -> Vec<ChatResponse> {
        let mut responses: Vec<ChatResponse> = (0..turns)
            .map(|k| sim_tool_call(&format!("c{k}")))
            .collect();
        responses.push(end_turn("all-done"));
        responses
    }

    pub(super) fn load_fixture(llm: &FixtureClient, responses: &[ChatResponse]) {
        for r in responses {
            llm.push_response(r.clone());
        }
    }

    pub(super) fn queue_tool_outputs(world: &SimWorld, turns: usize) {
        for k in 0..turns {
            world.tool.push_output(ToolResult::ok(format!("out-{k}")));
        }
    }

    pub(super) struct RunResult {
        pub(super) observed: Vec<Value>,
        pub(super) summary: Option<String>,
        pub(super) dispatches: Vec<Value>,
    }

    pub(super) fn summary_of(outcome: &InvocationOutcome) -> Option<String> {
        match outcome {
            InvocationOutcome::Completed { response, .. } => response.content.clone(),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    pub(super) async fn run_reference(seed: u64, turns: usize) -> RunResult {
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

#[cfg(test)]
mod crash_dst {
    //! Slice 5 (claims R2, R3, R1-under-faults): the crash DST.
    //!
    //! Every publish index of a scripted run is a fault point. The
    //! WAL writes and publishes interleave strictly, so the fault
    //! index determines the WAL state the crash leaves behind:
    //! span-first publish → intent-only (SafeResume), span-middle →
    //! dispatched-without-completed (Ambiguous — on the tool path
    //! the side effect has already run), span-last →
    //! completed-with-events-lost (SafeReplay), terminal publishes →
    //! terminal row with the archive hand-off incomplete (healed by
    //! the sweeper). Each point is checked for: crash trace is a
    //! canonical prefix (R1-under-faults), `categorise` predicts
    //! resume's behaviour (R2), and tools execute at most once per
    //! logical call — exactly once wherever auto-recovery proceeds,
    //! never re-executed where it refuses (R3).

    use std::collections::HashSet;

    use super::resume_equivalence::{load_fixture, run_reference, script, summary_of};
    use super::*;
    use crate::llm::{ChatResponse, LlmError};
    use crate::test_support::oracle::{
        check_invocation_trace, check_invocation_trace_prefix, check_resume_trace,
    };
    use crate::worker::ArchiveRetrySweeper;
    use crate::worker::recovery::{RecoveryCategory, categorise};
    use crate::worker::store::DispatchStatus;

    /// What the fault-index geometry says the crash must leave.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum Expect {
        /// Fault at the `triggered` publish: nothing persisted.
        NothingPersisted,
        SafeResume,
        Ambiguous,
        SafeReplay,
        /// Fault at a terminal publish: the row is already terminal;
        /// resume refuses and the archive sweeper heals.
        AlreadyTerminal,
    }

    fn total_publishes(turns: usize) -> usize {
        6 * turns + 6
    }

    fn expect_for(fault: usize, turns: usize) -> Expect {
        let completed_idx = 6 * turns + 4;
        if fault == 0 {
            Expect::NothingPersisted
        } else if fault >= completed_idx {
            Expect::AlreadyTerminal
        } else {
            match (fault - 1) % 3 {
                0 => Expect::SafeResume,
                1 => Expect::Ambiguous,
                _ => Expect::SafeReplay,
            }
        }
    }

    fn queue_outputs(world: &SimWorld, turns: usize) {
        for k in 0..turns {
            world.tool.push_output(ToolResult::ok(format!("out-{k}")));
        }
    }

    /// Run to the injected crash; assert it surfaced as the fault.
    async fn crash_at(seed: u64, turns: usize, fault: usize) -> (SimWorld, Vec<ChatResponse>) {
        let world = SimWorld::new(seed, 5.0).await;
        queue_outputs(&world, turns);
        let responses = script(turns);
        world.sink.fail_publish_at(fault);
        let llm = FixtureClient::new();
        load_fixture(&llm, &responses);
        let err = world.run(&llm).await.expect_err("must crash at the fault");
        assert!(
            matches!(err, ExecutorError::Bus(_)),
            "expected the injected fault, got {err:?}"
        );
        (world, responses)
    }

    /// Categorise the (single) in-flight invocation's WAL directly.
    async fn wal_category(world: &SimWorld, inv: &str) -> RecoveryCategory {
        let state = world
            .store
            .get_invocation_state(inv)
            .await
            .unwrap()
            .expect("state row");
        let tools = world
            .store
            .list_tool_dispatches_for_invocation(inv)
            .await
            .unwrap();
        let llms = world
            .store
            .list_llm_dispatches_for_invocation(inv)
            .await
            .unwrap();
        categorise(&state, &tools, &llms)
    }

    async fn completed_llm_turns(world: &SimWorld, inv: &str) -> usize {
        world
            .store
            .list_llm_dispatches_for_invocation(inv)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.status == DispatchStatus::Completed)
            .count()
    }

    fn assert_prefix_canonical(events: &[Event], label: &str) {
        if let Err(violations) = check_invocation_trace_prefix(events) {
            let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
            panic!(
                "crash trace not a canonical prefix ({label}):\n{}",
                lines.join("\n")
            );
        }
    }

    /// The full matrix at one fault point. Returns a short label of
    /// the branch taken, for the sweep's coverage sanity check.
    async fn check_fault_point(
        seed: u64,
        turns: usize,
        fault: usize,
        reference_summary: &Option<String>,
    ) -> Expect {
        let label = format!("seed {seed}, turns {turns}, fault {fault}");
        let (world, responses) = crash_at(seed, turns, fault).await;
        let crash_events = world.sink.events();
        assert_eq!(
            crash_events.len(),
            fault,
            "sink must hold exactly the pre-fault publishes ({label})"
        );
        assert_prefix_canonical(&crash_events, &label);

        let expected = expect_for(fault, turns);
        match expected {
            Expect::NothingPersisted => {
                let in_flight = world.store.find_in_flight_invocations().await.unwrap();
                assert!(in_flight.is_empty(), "no residue expected ({label})");
            }
            Expect::AlreadyTerminal => {
                let inv = world.invocation_id().to_string();
                let err = world
                    .runner
                    .resume(&world.agent, &FixtureClient::new(), world.invocation_id())
                    .await
                    .expect_err("terminal rows must refuse resume");
                assert!(
                    err.to_string().contains("already terminal"),
                    "({label}) got: {err}"
                );
                // The archive hand-off is incomplete but healable —
                // the row is terminal and un-acked, so the sweeper
                // must pick it up (proved in the dedicated test).
                let pending = world.store.list_archive_pending().await.unwrap();
                assert_eq!(pending.len(), 1, "sweeper must see the row ({label})");
                assert_eq!(pending[0].invocation_id, inv);
            }
            Expect::Ambiguous => {
                let inv = world.invocation_id().to_string();
                assert_eq!(
                    wal_category(&world, &inv).await,
                    RecoveryCategory::Ambiguous,
                    "({label})"
                );
                let before = world.tool.dispatches().lock().unwrap().len();
                let err = world
                    .runner
                    .resume(&world.agent, &FixtureClient::new(), world.invocation_id())
                    .await
                    .expect_err("ambiguous WAL must refuse auto-resume");
                assert!(
                    err.to_string().contains("ambiguous"),
                    "({label}) got: {err}"
                );
                assert_eq!(
                    world.tool.dispatches().lock().unwrap().len(),
                    before,
                    "refusal must not re-dispatch tools ({label})"
                );
            }
            Expect::SafeResume | Expect::SafeReplay => {
                let inv = world.invocation_id().to_string();
                let category = wal_category(&world, &inv).await;
                let want = if expected == Expect::SafeResume {
                    RecoveryCategory::SafeResume
                } else {
                    RecoveryCategory::SafeReplay
                };
                assert_eq!(category, want, "({label})");

                let consumed = completed_llm_turns(&world, &inv).await;
                let resume_llm = FixtureClient::new();
                load_fixture(&resume_llm, &responses[consumed..]);
                let outcome = world.resume(&resume_llm).await.expect("auto-resume");
                assert_eq!(
                    &summary_of(&outcome),
                    reference_summary,
                    "outcome must match the uninterrupted run ({label})"
                );
                assert_eq!(
                    world.tool.dispatches().lock().unwrap().len(),
                    turns,
                    "each logical tool call exactly once across crash+resume ({label})"
                );
                let resume_events = &world.sink.events()[fault..];
                if let Err(violations) = check_resume_trace(resume_events) {
                    let lines: Vec<String> =
                        violations.iter().map(|v| format!("  - {v}")).collect();
                    panic!(
                        "resume trace not canonical ({label}):\n{}",
                        lines.join("\n")
                    );
                }
            }
        }
        expected
    }

    /// Every publish index of a fixed two-turn script.
    #[tokio::test]
    async fn exhaustive_fault_sweep_covers_the_wal_lattice() {
        let turns = 2;
        let reference = run_reference(77, turns).await;
        let mut seen = HashSet::new();
        for fault in 0..total_publishes(turns) {
            seen.insert(check_fault_point(77, turns, fault, &reference.summary).await);
        }
        // The sweep must have exercised every lattice class.
        assert_eq!(seen.len(), 5, "all five WAL classes covered: {seen:?}");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 24, ..Default::default()
        })]

        /// Random scripts × random fault points × random seeds.
        #[test]
        fn random_faults_recover_or_refuse(
            seed: u64,
            turns in 1usize..=3,
            fault in 0usize..24,
        ) {
            proptest::prop_assume!(fault < total_publishes(turns));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sim runtime");
            runtime.block_on(async {
                let reference = run_reference(seed, turns).await;
                check_fault_point(seed, turns, fault, &reference.summary).await;
            });
        }
    }

    /// Crash during recovery itself: a second fault lands mid-resume,
    /// and a second resume still completes with tools exactly-once.
    #[tokio::test]
    async fn crash_while_resuming_then_second_resume_completes() {
        let turns = 2;
        let reference = run_reference(11, turns).await;
        // First crash: llm turn 1's response publish is lost, but its
        // WAL row completed → SafeReplay.
        let (world, responses) = crash_at(11, turns, 3).await;
        let inv = world.invocation_id();
        assert_eq!(
            wal_category(&world, &inv.to_string()).await,
            RecoveryCategory::SafeReplay
        );

        // Second fault: the first publish the resume attempts (the
        // tool span opening) fails too. Call the runner directly —
        // SimWorld::resume would clear the injected fault.
        world.sink.fail_publish_at(world.sink.events().len());
        let resume_llm = FixtureClient::new();
        load_fixture(&resume_llm, &responses[1..]);
        let err = world
            .runner
            .resume(&world.agent, &resume_llm, inv)
            .await
            .expect_err("second crash mid-resume");
        assert!(matches!(err, ExecutorError::Bus(_)));
        assert_eq!(
            wal_category(&world, &inv.to_string()).await,
            RecoveryCategory::SafeResume,
            "second crash left an intent-only tool row"
        );

        // Second resume completes; the tool still ran exactly once
        // per logical call across all three attempts.
        let resume_llm2 = FixtureClient::new();
        load_fixture(&resume_llm2, &responses[1..]);
        let outcome = world.resume(&resume_llm2).await.expect("second resume");
        assert_eq!(summary_of(&outcome), reference.summary);
        assert_eq!(world.tool.dispatches().lock().unwrap().len(), turns);
    }

    /// Terminal publishes lost → the sweeper republishes
    /// `invocation.archived` until acked; an ack (simulated as the
    /// row deletion it causes) stops the republishing.
    #[tokio::test]
    async fn lost_terminal_publishes_heal_via_the_sweeper() {
        let turns = 1;
        for fault_offset in [0usize, 1] {
            let fault = 6 * turns + 4 + fault_offset; // completed / archived publish
            let (world, _) = crash_at(21, turns, fault).await;
            let inv = world.invocation_id().to_string();
            world.sink.clear_fault();

            let sweeper = ArchiveRetrySweeper::new_with_sink(
                Arc::clone(&world.sink) as Arc<dyn EventSink>,
                WorkerId::new("sim-worker").unwrap(),
                Arc::clone(&world.store),
            );
            let mut warned = HashSet::new();

            let archived_count = |events: &[Event]| {
                events
                    .iter()
                    .filter(|e| {
                        matches!(e.payload, EventPayload::InvocationArchived(_))
                            && e.envelope.invocation_id.to_string() == inv
                    })
                    .count()
            };

            // Ack never arrives: every sweep republishes.
            sweeper.sweep_once(&mut warned).await.unwrap();
            assert_eq!(archived_count(&world.sink.events()), 1, "fault {fault}");
            sweeper.sweep_once(&mut warned).await.unwrap();
            assert_eq!(archived_count(&world.sink.events()), 2, "fault {fault}");

            // The republished payload carries the row's truth.
            let row = world
                .store
                .get_invocation_state(&inv)
                .await
                .unwrap()
                .expect("terminal row survives until acked");
            let last = world.sink.events().into_iter().last().unwrap();
            match last.payload {
                EventPayload::InvocationArchived(p) => {
                    assert_eq!(p.final_phase, row.phase);
                    assert_eq!(p.final_state_blob, row.state_blob);
                }
                other => panic!("expected archived republish, got {other:?}"),
            }

            // Ack: the consumer deletes the row; the sweep goes quiet.
            world.store.delete_invocation_state(&inv).await.unwrap();
            sweeper.sweep_once(&mut warned).await.unwrap();
            assert_eq!(archived_count(&world.sink.events()), 2, "fault {fault}");
        }
    }

    /// LLM provider error: the invocation fails canonically — the
    /// trace ends in a `failed` terminal + archived (full-oracle
    /// valid), the WAL row completes with `is_error`, and nothing is
    /// left in flight.
    #[tokio::test]
    async fn llm_provider_error_fails_canonically() {
        let world = SimWorld::new(31, 5.0).await;
        let llm = FixtureClient::new();
        llm.push_error(LlmError::RateLimited);

        let err = world.run(&llm).await.expect_err("provider error");
        assert!(matches!(err, ExecutorError::Llm(_)), "got {err:?}");

        let events = world.sink.events();
        if let Err(violations) = check_invocation_trace(&events) {
            let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
            panic!("failed run's trace not canonical:\n{}", lines.join("\n"));
        }
        assert!(
            events
                .iter()
                .any(|e| matches!(e.payload, EventPayload::Failed(_)))
        );
        assert!(
            world
                .store
                .find_in_flight_invocations()
                .await
                .unwrap()
                .is_empty(),
            "a failed invocation must not linger in flight"
        );
    }
}

#[cfg(test)]
mod budget_properties {
    //! Slice 6 (claim R5): the budget ceiling, as properties.
    //!
    //! Enforcement semantics under test (from `run_model_with_llm`):
    //! the check is **post-call** — a call completes, its cost joins
    //! the totals, and only then is `total > budget` evaluated. So a
    //! `Completed` run's final cost is ≤ budget, a `BudgetExceeded`
    //! run's cost exceeds it by at most the crossing call, and no
    //! `llm.request` follows the trip. Crash/resume accumulation
    //! (pre-registered finding 1, fixed before the net existed) is
    //! closed here as a property: for any recoverable crash point,
    //! the resumed run trips at exactly the same total as the
    //! uninterrupted reference.

    use std::collections::HashMap;

    use super::resume_equivalence::load_fixture;
    use super::tests::{end_turn, sim_tool_call};
    use super::*;
    use crate::events::TokenUsage;
    use crate::llm::ChatResponse;
    use crate::pricing::ModelPricing;
    use crate::test_support::events::event_kind;
    use crate::test_support::oracle::check_invocation_trace;

    fn sim_rates() -> ModelPricing {
        ModelPricing {
            input_per_million: 10.0,
            output_per_million: 50.0,
            cache_read_per_million: Some(1.0),
            cache_write_per_million: Some(12.5),
        }
    }

    fn sim_pricing() -> Arc<PricingTable> {
        let mut entries = HashMap::new();
        entries.insert("claude-sim".to_string(), sim_rates());
        Arc::new(PricingTable::from_map(entries))
    }

    fn with_usage(mut response: ChatResponse, usage: TokenUsage) -> ChatResponse {
        response.usage = usage;
        response
    }

    fn usage(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    fn cost_of(u: &TokenUsage) -> f64 {
        sim_rates().calculate(u).2
    }

    /// A `turns`-tool-call script where every call carries `usages[k]`.
    fn priced_script(turns: usize, usages: &[TokenUsage]) -> Vec<ChatResponse> {
        assert_eq!(usages.len(), turns + 1);
        let mut responses: Vec<ChatResponse> = (0..turns)
            .map(|k| with_usage(sim_tool_call(&format!("c{k}")), usages[k]))
            .collect();
        responses.push(with_usage(end_turn("all-done"), usages[turns]));
        responses
    }

    fn llm_request_count(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|e| event_kind(e) == "llm_request")
            .count()
    }

    fn failed_kind(events: &[Event]) -> Option<crate::events::FailureKind> {
        events.iter().find_map(|e| match &e.payload {
            EventPayload::Failed(p) => Some(p.error_kind),
            _ => None,
        })
    }

    /// Deterministic anchor: three calls at $1.50 each against a
    /// $2.00 budget must trip after the second call, with the
    /// crossing call's cost included and the trace canonical.
    #[tokio::test]
    async fn budget_trips_after_the_crossing_call() {
        let world = SimWorld::with_pricing(51, 2.0, sim_pricing()).await;
        world.tool.push_output(ToolResult::ok("out-0"));
        let usages = vec![usage(100_000, 10_000); 3]; // $1.50 each
        let llm = FixtureClient::new();
        load_fixture(&llm, &priced_script(2, &usages));

        let outcome = world.run(&llm).await.expect("run");
        let InvocationOutcome::BudgetExceeded { cost, .. } = outcome else {
            panic!("expected BudgetExceeded, got {outcome:?}");
        };
        assert_eq!(cost, 3.0, "the crossing call's cost is included");

        let events = world.sink.events();
        assert_eq!(llm_request_count(&events), 2, "no request after the trip");
        assert_eq!(
            world.tool.dispatches().lock().unwrap().len(),
            1,
            "the crossing response's tool call must never dispatch"
        );
        assert!(matches!(
            failed_kind(&events),
            Some(crate::events::FailureKind::BudgetExceeded)
        ));
        if let Err(violations) = check_invocation_trace(&events) {
            panic!("budget-exceeded trace not canonical: {violations:?}");
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 24, ..Default::default()
        })]

        /// Random usages × random budget: cost accounting is exact
        /// (the outcome's total equals the independent recomputation
        /// from the pricing table), `Completed` never exceeds the
        /// budget, and `BudgetExceeded` exceeds it by at most the
        /// crossing call.
        #[test]
        fn ceiling_and_accounting_invariants(
            seed: u64,
            turns in 1usize..=3,
            per_call in proptest::collection::vec((1_000u32..500_000, 0u32..100_000), 4),
            budget in 0.5f64..12.0,
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sim runtime");
            runtime.block_on(async {
                let usages: Vec<TokenUsage> = per_call
                    .iter()
                    .take(turns + 1)
                    .map(|(i, o)| usage(*i, *o))
                    .collect();
                let world = SimWorld::with_pricing(seed, budget, sim_pricing()).await;
                for k in 0..turns {
                    world.tool.push_output(ToolResult::ok(format!("out-{k}")));
                }
                let llm = FixtureClient::new();
                load_fixture(&llm, &priced_script(turns, &usages));

                let outcome = world.run(&llm).await.expect("run");
                let events = world.sink.events();
                let k = llm_request_count(&events);
                let spent: f64 = usages[..k].iter().map(cost_of).sum();

                match outcome {
                    InvocationOutcome::Completed { .. } => {
                        assert_eq!(k, turns + 1, "completed runs make every call");
                        assert!(spent <= budget, "completed run overspent: {spent} > {budget}");
                    }
                    InvocationOutcome::BudgetExceeded { cost, .. } => {
                        assert_eq!(cost, spent, "outcome total must equal the recomputation");
                        assert!(cost > budget, "trip without crossing: {cost} <= {budget}");
                        let before: f64 = usages[..k - 1].iter().map(cost_of).sum();
                        assert!(
                            before <= budget,
                            "should have tripped a call earlier: {before} > {budget}"
                        );
                        if let Err(violations) = check_invocation_trace(&events) {
                            panic!("trace not canonical: {violations:?}");
                        }
                    }
                }
            });
        }

        /// Finding 1, closed as a property: a crash at any
        /// recoverable point must not reset the accumulator — the
        /// resumed run trips at exactly the reference's total.
        #[test]
        fn budget_accumulates_across_resume(
            seed: u64,
            turns in 2usize..=3,
            span in 0usize..4,
            span_first: bool,
        ) {
            proptest::prop_assume!(span < 2 * turns);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sim runtime");
            runtime.block_on(async {
                // Every call costs $1.50; the budget admits all but
                // the final llm call, which crosses it.
                let usages = vec![usage(100_000, 10_000); turns + 1];
                let budget = 1.5 * (turns as f64 + 1.0) - 0.1;
                let expected_cost = 1.5 * (turns as f64 + 1.0);

                // Reference: uninterrupted.
                let reference = SimWorld::with_pricing(seed, budget, sim_pricing()).await;
                for k in 0..turns {
                    reference.tool.push_output(ToolResult::ok(format!("out-{k}")));
                }
                let llm = FixtureClient::new();
                load_fixture(&llm, &priced_script(turns, &usages));
                let ref_outcome = reference.run(&llm).await.expect("reference");
                let InvocationOutcome::BudgetExceeded { cost: ref_cost, .. } = ref_outcome else {
                    panic!("reference must trip, got {ref_outcome:?}");
                };
                assert_eq!(ref_cost, expected_cost);

                // Interrupted at a recoverable point, then resumed.
                let world = SimWorld::with_pricing(seed, budget, sim_pricing()).await;
                for k in 0..turns {
                    world.tool.push_output(ToolResult::ok(format!("out-{k}")));
                }
                let fault = 1 + 3 * span + if span_first { 0 } else { 2 };
                world.sink.fail_publish_at(fault);
                let llm = FixtureClient::new();
                let responses = priced_script(turns, &usages);
                load_fixture(&llm, &responses);
                world.run(&llm).await.expect_err("must crash");

                let inv = world.invocation_id().to_string();
                let consumed = world
                    .store
                    .list_llm_dispatches_for_invocation(&inv)
                    .await
                    .unwrap()
                    .iter()
                    .filter(|r| r.status == crate::worker::store::DispatchStatus::Completed)
                    .count();
                let resume_llm = FixtureClient::new();
                load_fixture(&resume_llm, &responses[consumed..]);
                let outcome = world.resume(&resume_llm).await.expect("resume");
                let InvocationOutcome::BudgetExceeded { cost, .. } = outcome else {
                    panic!("resumed run must trip like the reference, got {outcome:?}");
                };
                assert_eq!(
                    cost, ref_cost,
                    "resume reset or double-counted the accumulator"
                );
                assert_eq!(world.tool.dispatches().lock().unwrap().len(), turns);
            });
        }
    }
}
