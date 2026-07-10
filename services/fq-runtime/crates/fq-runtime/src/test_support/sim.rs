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
use crate::worker::{DrainSignal, ExecutorError, InvocationOutcome, WorkerId};
use fq_tools::{Tool, ToolContext, ToolError, ToolResult};

tokio::task_local! {
    /// The invocation ordinal a sim task runs under (parallel-workers
    /// Phase 2, audit H1). [`SimWorld::run_many`] scopes each spawned
    /// invocation with its ordinal so [`SimClock::rand_u64`] draws from
    /// a per-invocation stream derived from `(seed, ordinal)` — an
    /// invocation's entropy sequence is then independent of how the
    /// scheduler interleaves its siblings, so its per-invocation trace
    /// signature reproduces even when the global interleaving doesn't.
    static SIM_INVOCATION_ORDINAL: usize;
}

/// Deterministic time + entropy: a strictly monotonic millisecond
/// counter and a seeded xorshift stream.
pub struct SimClock {
    /// Global monotonic milliseconds. Deliberately shared across
    /// concurrent invocations: the draw *order* is the interleaving
    /// record the concurrent oracle's overlap gauge reads.
    ms: AtomicU64,
    /// The un-scoped entropy stream — serial tests draw from here,
    /// bit-identical to the pre-Phase-2 behavior.
    rng: AtomicU64,
    seed: u64,
    /// Per-invocation xorshift states, keyed by ordinal, derived
    /// lazily from `(seed, ordinal)`.
    streams: Mutex<std::collections::HashMap<usize, u64>>,
}

impl SimClock {
    pub fn new(seed: u64) -> Self {
        Self {
            ms: AtomicU64::new(1_000_000),
            // Xorshift must not start at zero.
            rng: AtomicU64::new(seed.max(1)),
            seed,
            streams: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn derive_stream(seed: u64, ordinal: usize) -> u64 {
        // SplitMix-style spread so adjacent ordinals land far apart;
        // xorshift must not start at zero.
        (seed ^ (ordinal as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)).max(1)
    }

    fn step_xorshift(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
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
        // Inside a `run_many` invocation task: draw from that
        // invocation's own stream. Outside (every serial test): the
        // shared stream, unchanged.
        if let Ok(ordinal) = SIM_INVOCATION_ORDINAL.try_with(|o| *o) {
            let mut streams = self.streams.lock().unwrap();
            let state = streams
                .entry(ordinal)
                .or_insert_with(|| Self::derive_stream(self.seed, ordinal));
            return Self::step_xorshift(state);
        }
        self.rng
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |mut x| {
                Some(Self::step_xorshift(&mut x))
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
    /// One-shot fault keyed by per-invocation publish count (see
    /// [`Self::fail_publish_at_invocation_count`]); the per-invocation
    /// counts live in `per_invocation_published`.
    fail_at_invocation_count: Mutex<Option<usize>>,
    per_invocation_published: Mutex<std::collections::HashMap<uuid::Uuid, usize>>,
    /// Armed graceful drain (ADR-0027): when the publish count first
    /// reaches the index, request the `DrainSignal` once, then disarm.
    /// Distinct from `fail_at` — the publish still succeeds; the drain
    /// takes effect at the reducer's next step-boundary poll.
    drain_arm: Mutex<Option<(usize, DrainSignal)>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            fail_at: AtomicUsize::new(usize::MAX),
            fail_at_invocation_count: Mutex::new(None),
            per_invocation_published: Mutex::new(std::collections::HashMap::new()),
            drain_arm: Mutex::new(None),
        }
    }

    /// Inject a fault: the publish with this zero-based index (over
    /// the sink's lifetime) fails, simulating a crash at that
    /// operation boundary.
    pub fn fail_publish_at(&self, index: usize) {
        self.fail_at.store(index, Ordering::SeqCst);
    }

    /// Inject a fault keyed by *per-invocation* publish count
    /// (parallel-workers Phase 2): the first invocation to attempt its
    /// `nth` (zero-based) publish fails that one publish, exactly once.
    /// Under concurrency a global publish index lands on a
    /// nondeterministic invocation; a per-invocation count pins the
    /// crash to a well-defined point in *some* invocation's own arc,
    /// which is what the per-invocation recovery invariants need.
    pub fn fail_publish_at_invocation_count(&self, nth: usize) {
        *self.fail_at_invocation_count.lock().unwrap() = Some(nth);
    }

    /// Clear the fault so a resumed run can publish normally.
    pub fn clear_fault(&self) {
        self.fail_at.store(usize::MAX, Ordering::SeqCst);
        *self.fail_at_invocation_count.lock().unwrap() = None;
    }

    /// Arm a graceful drain (ADR-0027): when the publish count first
    /// reaches `index`, request `signal` once and disarm. Unlike
    /// [`Self::fail_publish_at`] that publish still *succeeds* — the
    /// drain is observed at the reducer loop's next step-boundary poll,
    /// suspending the invocation there.
    pub fn drain_at_publish(&self, index: usize, signal: DrainSignal) {
        *self.drain_arm.lock().unwrap() = Some((index, signal));
    }

    /// Disarm any pending drain injection.
    pub fn clear_drain(&self) {
        *self.drain_arm.lock().unwrap() = None;
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
        // Per-invocation-count fault (Phase 2): fire once, on the first
        // invocation whose own publish count reaches the armed value.
        {
            let mut count_arm = self.fail_at_invocation_count.lock().unwrap();
            if let Some(nth) = *count_arm {
                let mut counts = self.per_invocation_published.lock().unwrap();
                let count = counts.entry(event.envelope.invocation_id).or_insert(0);
                if *count >= nth {
                    *count_arm = None;
                    return Err(BusError::Publish(
                        "sim: injected per-invocation publish fault".to_string(),
                    ));
                }
                *count += 1;
            }
        }
        // Graceful-drain injection (ADR-0027): at the armed publish
        // index request the drain once (the publish still succeeds); the
        // reducer loop suspends at its next step-boundary poll.
        let mut arm = self.drain_arm.lock().unwrap();
        let ready = arm
            .as_ref()
            .is_some_and(|(index, _)| events.len() >= *index);
        if ready && let Some((_, signal)) = arm.take() {
            signal.request();
        }
        drop(arm);
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
        // `cwd`/`path` declare `format: path` so sim tests exercise the
        // declared-path-only `${workspace}` substitution; every other
        // property is deliberately un-annotated (passed verbatim).
        json!({
            "type": "object",
            "properties": {
                "cwd":  { "type": "string", "format": "path" },
                "path": { "type": "string", "format": "path" },
                "step": { "type": "string" },
                "content": { "type": "string" }
            }
        })
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
    // Arc'd for the same reason as production: `run_many` spawns N
    // concurrent invocations through the one shared runner.
    runner: Arc<ReducerRunner>,
    // The `${workspace}` binding, if any — the fresh-binary resume path
    // re-wires it so re-association is exercised across the handoff.
    workspace: Option<Arc<dyn crate::worker::workspace::WorkspaceProvider>>,
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

    /// Build a world with a `${workspace}` binding (parallel-workers
    /// Phase 0): zero-cost pricing, the given provider wired into the
    /// runner.
    pub async fn with_workspace(
        seed: u64,
        budget: f64,
        workspace: Arc<dyn crate::worker::workspace::WorkspaceProvider>,
    ) -> Self {
        Self::build(
            seed,
            budget,
            Arc::new(PricingTable::empty()),
            Some(workspace),
        )
        .await
    }

    /// Build a world with real pricing, for the budget properties
    /// (reducer verification, slice 6).
    pub async fn with_pricing(seed: u64, budget: f64, pricing: Arc<PricingTable>) -> Self {
        Self::build(seed, budget, pricing, None).await
    }

    async fn build(
        seed: u64,
        budget: f64,
        pricing: Arc<PricingTable>,
        workspace: Option<Arc<dyn crate::worker::workspace::WorkspaceProvider>>,
    ) -> Self {
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

        let runner = Arc::new(Self::build_runner(
            &clock,
            &sink,
            &registry,
            &store,
            pricing,
            workspace.clone(),
        ));

        Self {
            clock,
            sink,
            tool,
            store,
            agent,
            runner,
            workspace,
            _store_dir: store_dir,
        }
    }

    fn build_runner(
        clock: &Arc<SimClock>,
        sink: &Arc<RecordingSink>,
        registry: &ToolRegistry,
        store: &Arc<WorkerStore>,
        pricing: Arc<PricingTable>,
        workspace: Option<Arc<dyn crate::worker::workspace::WorkspaceProvider>>,
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
                    .workspace(workspace)
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

    /// Resume the single in-flight invocation on a **fresh runner** —
    /// the next-binary handoff a graceful drain relies on (ADR-0027).
    /// A `DrainSignal` is monotonic, so resuming on the original runner
    /// (whose flag is still set) would re-suspend immediately with no
    /// progress; the new runner starts `Running` over the same
    /// store/sink/clock. Clears any drain arming first — the drain is
    /// over.
    pub async fn resume_on_fresh_binary(
        &self,
        llm: &FixtureClient,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.sink.clear_drain();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::clone(&self.tool) as Arc<dyn Tool>);
        let fresh = Self::build_runner(
            &self.clock,
            &self.sink,
            &registry,
            &self.store,
            Arc::new(PricingTable::empty()),
            self.workspace.clone(),
        );
        let in_flight = self
            .store
            .find_in_flight_invocations()
            .await
            .expect("query in-flight");
        assert_eq!(
            in_flight.len(),
            1,
            "expected exactly one suspended invocation"
        );
        let invocation_id = in_flight[0]
            .invocation_id
            .parse()
            .expect("invocation id parses");
        fresh.resume(&self.agent, llm, invocation_id).await
    }

    /// Run N invocations **concurrently** through the one shared
    /// runner (parallel-workers Phase 2), each with its own scripted
    /// LLM, its own entropy stream (scoped by ordinal — see
    /// [`SIM_INVOCATION_ORDINAL`]), and a trigger payload
    /// `{"sim": ordinal}` so tests can map invocation ids back to
    /// scripts through the `Triggered` event. Returns outcomes in
    /// ordinal order.
    pub async fn run_many(
        &self,
        scripts: Vec<FixtureClient>,
    ) -> Vec<Result<InvocationOutcome, ExecutorError>> {
        let mut set = tokio::task::JoinSet::new();
        for (ordinal, llm) in scripts.into_iter().enumerate() {
            let runner = Arc::clone(&self.runner);
            let agent = self.agent.clone();
            set.spawn(SIM_INVOCATION_ORDINAL.scope(ordinal, async move {
                let outcome = runner
                    .run(
                        &agent,
                        &llm,
                        TriggerSource::Manual,
                        None,
                        json!({"sim": ordinal}),
                    )
                    .await;
                (ordinal, outcome)
            }));
        }
        let mut outcomes: Vec<Option<Result<InvocationOutcome, ExecutorError>>> = Vec::new();
        while let Some(joined) = set.join_next().await {
            let (ordinal, outcome) = joined.expect("sim invocation task panicked");
            if outcomes.len() <= ordinal {
                outcomes.resize_with(ordinal + 1, || None);
            }
            outcomes[ordinal] = Some(outcome);
        }
        outcomes
            .into_iter()
            .map(|o| o.expect("every ordinal reports an outcome"))
            .collect()
    }

    /// Resume **every** in-flight invocation concurrently on a fresh
    /// runner — the next-binary handoff with N suspended (plan §3).
    /// `scripts` maps invocation id → continuation script. Clears any
    /// armed drain/fault first. Returns `(invocation_id, outcome)`
    /// pairs, one per in-flight invocation.
    pub async fn resume_all_on_fresh_binary(
        &self,
        mut scripts: std::collections::HashMap<uuid::Uuid, FixtureClient>,
    ) -> Vec<(uuid::Uuid, Result<InvocationOutcome, ExecutorError>)> {
        self.sink.clear_drain();
        self.sink.clear_fault();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::clone(&self.tool) as Arc<dyn Tool>);
        let fresh = Arc::new(Self::build_runner(
            &self.clock,
            &self.sink,
            &registry,
            &self.store,
            Arc::new(PricingTable::empty()),
            self.workspace.clone(),
        ));
        let mut in_flight: Vec<uuid::Uuid> = self
            .store
            .find_in_flight_invocations()
            .await
            .expect("query in-flight")
            .iter()
            .map(|row| row.invocation_id.parse().expect("invocation id parses"))
            .collect();
        // Deterministic resume order (and ordinal scoping) regardless
        // of map iteration order.
        in_flight.sort();

        let mut set = tokio::task::JoinSet::new();
        for (ordinal, invocation_id) in in_flight.into_iter().enumerate() {
            let llm = scripts
                .remove(&invocation_id)
                .unwrap_or_else(|| panic!("no continuation script for {invocation_id}"));
            let runner = Arc::clone(&fresh);
            let agent = self.agent.clone();
            set.spawn(SIM_INVOCATION_ORDINAL.scope(ordinal, async move {
                let outcome = runner.resume(&agent, &llm, invocation_id).await;
                (invocation_id, outcome)
            }));
        }
        let mut results = Vec::new();
        while let Some(joined) = set.join_next().await {
            results.push(joined.expect("sim resume task panicked"));
        }
        results.sort_by_key(|(id, _)| *id);
        results
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
        sim_tool_call_with(call_id, json!({"step": call_id}))
    }

    pub(super) fn sim_tool_call_with(call_id: &str, parameters: Value) -> ChatResponse {
        ChatResponse {
            content: None,
            tool_calls: vec![MessageToolCall {
                tool_call_id: ToolCallId::new(call_id).unwrap(),
                tool_name: SIM_TOOL.to_string(),
                parameters,
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

    /// A single model turn that emits *two* tool calls. The harness
    /// answers this with `CallToolsParallel`, so it drives the
    /// parallel-batch path — and, once persisted, its resume/replay.
    pub(super) fn sim_two_tool_calls(first: &str, second: &str) -> ChatResponse {
        let call = |id: &str| MessageToolCall {
            tool_call_id: ToolCallId::new(id).unwrap(),
            tool_name: SIM_TOOL.to_string(),
            parameters: json!({ "step": id }),
        };
        ChatResponse {
            content: None,
            tool_calls: vec![call(first), call(second)],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 10,
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

    /// Phase 0 of the parallel-workers plan: with a workspace provider
    /// wired, `${workspace}` is substituted in the tool call's
    /// *declared path parameters only* (before the intent is
    /// persisted); non-path parameters — file contents, arbitrary
    /// strings — pass through **verbatim**, because silently rewriting
    /// agent output is undebuggable. The binding lands in the state
    /// row's `workspace_ref` so recovery can re-associate, and the
    /// step-0 preamble tells the agent where `${workspace}` points.
    #[tokio::test]
    async fn workspace_binding_substitutes_path_params_only_and_persists_ref() {
        let ws_dir = tempfile::tempdir().expect("workspace dir");
        let ws_path = ws_dir.path().to_path_buf();
        let provider = Arc::new(crate::worker::workspace::StaticWorkspace::new(
            ws_path.clone(),
        ));
        let world = SimWorld::with_workspace(7, 5.0, provider).await;

        let llm = FixtureClient::new();
        llm.push_response(sim_tool_call_with(
            "c1",
            json!({
                "cwd": "${workspace}",
                "path": "${workspace}/notes.md",
                "content": "I am writing ${workspace} into a file",
                "step": "echo ${workspace}"
            }),
        ));
        llm.push_response(end_turn("done"));
        let outcome = world.run(&llm).await.expect("sim run");
        assert!(matches!(outcome, InvocationOutcome::Completed { .. }));

        let dispatches = world.tool.dispatches().lock().unwrap().clone();
        let ws = ws_path.to_string_lossy();
        assert_eq!(dispatches[0]["cwd"], json!(ws));
        assert_eq!(dispatches[0]["path"], json!(format!("{ws}/notes.md")));
        assert_eq!(
            dispatches[0]["content"],
            json!("I am writing ${workspace} into a file"),
            "non-path parameters must never be rewritten"
        );
        assert_eq!(
            dispatches[0]["step"],
            json!("echo ${workspace}"),
            "non-path parameters must never be rewritten"
        );

        let row = world
            .store
            .get_invocation_state(&world.invocation_id().to_string())
            .await
            .expect("state row query")
            .expect("state row");
        assert_eq!(
            row.workspace_ref.as_deref(),
            Some(ws.as_ref()),
            "the workspace binding must be persisted for resume re-association"
        );

        // The step-0 preamble tells the agent authoritatively where
        // `${workspace}` points — first user message of the first
        // request, ahead of the trigger payload.
        let requests = llm.requests();
        let first_user = requests[0]
            .messages
            .iter()
            .find(|m| matches!(m.role, crate::events::MessageRole::User))
            .expect("a user message");
        let preamble = first_user.content.as_deref().unwrap_or_default();
        assert!(
            preamble.contains(ws.as_ref()) && preamble.contains("${workspace}"),
            "preamble must name the real path and the token; got: {preamble}"
        );
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

    /// Regression (durable-execution replay): a model turn that fires
    /// **parallel** tool calls must survive suspend + resume. Recovery
    /// records one `tool_dispatch` row per call, but the harness answers
    /// the turn with a single `CallToolsParallel` / `ParallelToolResults`.
    /// Replaying the rows individually desynced the reducer — it consumed
    /// the first result, returned to `AwaitingModel`, then rejected the
    /// second ("expected ModelResult after CallModel, got ToolResult") —
    /// so *any* invocation that batched tool calls was unrecoverable and
    /// boot-looped forever on each restart.
    #[tokio::test]
    async fn parallel_tool_batch_survives_suspend_and_resume() {
        let world = SimWorld::new(11, 5.0).await;
        world.tool.push_output(ToolResult::ok("out-a"));
        world.tool.push_output(ToolResult::ok("out-b"));

        // Turn 0 fires two tool calls; turn 1 ends. Crash at turn 1's
        // first publish (publish 10): triggered(0) + llm turn 0 (1-3) +
        // tool c0a (4-6) + tool c0b (7-9). Both tool results are durable
        // by then, so resume must *replay* the batch, not re-run it.
        let llm = FixtureClient::new();
        llm.push_response(sim_two_tool_calls("c0a", "c0b"));
        llm.push_response(end_turn("done"));
        world.sink.fail_publish_at(10);

        let err = world
            .run(&llm)
            .await
            .expect_err("must crash after the parallel batch is recorded");
        assert!(
            matches!(err, ExecutorError::Bus(_)),
            "expected the injected publish fault, got {err:?}"
        );
        assert_eq!(
            world.tool.dispatches().lock().unwrap().len(),
            2,
            "both parallel tools ran before the crash"
        );

        // Resume drives the same `runner.resume` replay path a
        // fresh-binary handoff uses. Before the fix this returned Err at
        // the second tool result ("expected ModelResult after
        // CallModel"); now the batch replays as one `ParallelToolResults`
        // and the invocation completes.
        let resume_llm = FixtureClient::new();
        resume_llm.push_response(end_turn("done"));
        let outcome = world
            .resume(&resume_llm)
            .await
            .expect("resume must replay the parallel tool batch");
        assert!(
            matches!(outcome, InvocationOutcome::Completed { .. }),
            "got {outcome:?}"
        );

        // Tools were replayed, not re-run (still 2 total), and nothing
        // is left in flight to boot-loop on the next start.
        assert_eq!(
            world.tool.dispatches().lock().unwrap().len(),
            2,
            "resume replays the recorded tools; it must not re-run them"
        );
        let in_flight = world
            .store
            .find_in_flight_invocations()
            .await
            .expect("query");
        assert!(in_flight.is_empty(), "no zombie invocation left in flight");
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

    pub(super) fn assert_equivalent(reference: &RunResult, resumed: &RunResult, label: &str) {
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
    pub(super) async fn wal_category(world: &SimWorld, inv: &str) -> RecoveryCategory {
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

    pub(super) async fn completed_llm_turns(world: &SimWorld, inv: &str) -> usize {
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
                    InvocationOutcome::Suspended { .. } => {
                        panic!("unexpected drain-suspend in budget-accounting run");
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

#[cfg(test)]
mod soak {
    //! Slice 7: everything, in volume.
    //!
    //! One seeded lifecycle driver composes the whole net: random
    //! scripts (plain tool turns, error-result tools, unknown-tool
    //! turns that take the synthetic-error path, LLM provider
    //! errors), real pricing with budgets that sometimes trip, and up
    //! to two crash/resume cycles at random publish indices. The
    //! checks are deliberately *universal invariants* — things that
    //! must hold on every path without the checker predicting the
    //! path, so it never becomes a second implementation:
    //!
    //! 1. Every crash segment is a canonical prefix; every resumed
    //!    segment is resume-canonical (prefix form if it crashed
    //!    again); at most one terminal event across the whole life.
    //! 2. Every logical tool call executes at most once, ever.
    //! 3. The WAL cost triangle: each completed LLM row's stored
    //!    `cost_usd` equals the price recomputed from its stored
    //!    usage, and any terminal totals equal the rows' sum.
    //! 4. Every persisted state blob (rows and archived payloads)
    //!    passes the phase ↔ contents invariants.
    //! 5. The budget ceiling: `Completed` ⇒ cost ≤ budget;
    //!    `BudgetExceeded` ⇒ cost > budget.
    //!
    //! CI runs a fixed seed range; `just soak` scales iterations via
    //! `FQ_SOAK_ITERS` for deep local runs.

    use std::collections::{HashMap, HashSet};

    use super::resume_equivalence::load_fixture;
    use super::tests::{end_turn, sim_tool_call};
    use super::*;
    use crate::events::TokenUsage;
    use crate::llm::{ChatResponse, LlmError};
    use crate::pricing::ModelPricing;
    use crate::test_support::oracle::{
        check_invocation_trace_prefix, check_resume_trace, check_resume_trace_prefix,
    };
    use crate::worker::recovery::{RecoveryCategory, categorise};
    use crate::worker::store::DispatchStatus;

    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn pick(state: &mut u64, bound: u64) -> u64 {
        xorshift(state) % bound
    }

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

    #[derive(Debug)]
    struct Scenario {
        turns: usize,
        usages: Vec<TokenUsage>,
        /// Per tool turn: Some(false) = ok result, Some(true) =
        /// error result, None = unknown tool (synthetic error path).
        tool_kinds: Vec<Option<bool>>,
        llm_error_at: Option<usize>,
        budget: f64,
        /// Absolute (sink-lifetime) publish indices to fault, sorted.
        faults: Vec<usize>,
    }

    fn generate(seed: u64) -> Scenario {
        let mut rng = seed.max(1);
        let turns = pick(&mut rng, 4) as usize; // 0..=3
        let usages: Vec<TokenUsage> = (0..=turns)
            .map(|_| TokenUsage {
                input_tokens: 1_000 + pick(&mut rng, 400_000) as u32,
                output_tokens: pick(&mut rng, 80_000) as u32,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            })
            .collect();
        let tool_kinds: Vec<Option<bool>> = (0..turns)
            .map(|_| match pick(&mut rng, 10) {
                0 => None,           // unknown tool
                1 | 2 => Some(true), // error result
                _ => Some(false),    // ok
            })
            .collect();
        let llm_error_at = if pick(&mut rng, 5) == 0 {
            Some(pick(&mut rng, (turns + 1) as u64) as usize)
        } else {
            None
        };
        // Budgets: mostly generous, sometimes guaranteed to trip.
        let max_cost: f64 = usages.iter().map(|u| sim_rates().calculate(u).2).sum();
        let budget = if pick(&mut rng, 3) == 0 {
            (max_cost * (pick(&mut rng, 80) as f64 + 10.0) / 100.0).max(0.01)
        } else {
            max_cost + 1.0
        };
        // Up to two faults inside the maximum possible trace.
        let ceiling = (6 * turns + 6) as u64;
        let mut faults: Vec<usize> = (0..pick(&mut rng, 3))
            .map(|_| pick(&mut rng, ceiling) as usize)
            .collect();
        faults.sort_unstable();
        faults.dedup();
        Scenario {
            turns,
            usages,
            tool_kinds,
            llm_error_at,
            budget,
            faults,
        }
    }

    fn build_responses(s: &Scenario) -> Vec<ChatResponse> {
        let mut responses = Vec::new();
        for k in 0..s.turns {
            let mut r = sim_tool_call(&format!("c{k}"));
            if s.tool_kinds[k].is_none() {
                r.tool_calls[0].tool_name = "ghost_tool".to_string();
            }
            r.usage = s.usages[k];
            responses.push(r);
        }
        let mut last = end_turn("soak-done");
        last.usage = s.usages[s.turns];
        responses.push(last);
        responses
    }

    fn assert_no_violations(
        result: Result<(), Vec<crate::test_support::oracle::TraceViolation>>,
        what: &str,
        seed: u64,
    ) {
        if let Err(violations) = result {
            let lines: Vec<String> = violations.iter().map(|v| format!("  - {v}")).collect();
            panic!("{what} not canonical (seed {seed}):\n{}", lines.join("\n"));
        }
    }

    async fn check_wal_cost_triangle(world: &SimWorld, inv: &str, seed: u64) -> f64 {
        let rows = world
            .store
            .list_llm_dispatches_for_invocation(inv)
            .await
            .unwrap();
        let mut sum = 0.0;
        for row in rows
            .iter()
            .filter(|r| r.status == DispatchStatus::Completed && r.is_error != Some(true))
        {
            let response: ChatResponse =
                serde_json::from_str(row.response.as_deref().expect("completed row has response"))
                    .expect("stored response parses");
            let expected = sim_rates().calculate(&response.usage).2;
            let stored = row.cost_usd.unwrap_or(0.0);
            assert!(
                (stored - expected).abs() < 1e-12,
                "WAL cost {stored} != recomputed {expected} (seed {seed})"
            );
            sum += stored;
        }
        sum
    }

    async fn check_blobs(world: &SimWorld, seed: u64) {
        for event in world.sink.events() {
            if let EventPayload::InvocationArchived(p) = &event.payload {
                crate::worker::reducer::harness::validate_state_blob(&p.final_state_blob)
                    .unwrap_or_else(|e| panic!("archived blob invalid (seed {seed}): {e}"));
            }
        }
        for row in world.store.find_in_flight_invocations().await.unwrap() {
            crate::worker::reducer::harness::validate_state_blob(&row.state_blob)
                .unwrap_or_else(|e| panic!("in-flight blob invalid (seed {seed}): {e}"));
        }
    }

    /// Drive one full lifecycle from a seed; panic on any violated
    /// invariant. Returns a coarse label for coverage accounting.
    async fn run_scenario(seed: u64) -> &'static str {
        let s = generate(seed);
        let world = SimWorld::with_pricing(seed, s.budget, sim_pricing()).await;
        for (k, kind) in s.tool_kinds.iter().enumerate() {
            match kind {
                Some(false) => world.tool.push_output(ToolResult::ok(format!("out-{k}"))),
                Some(true) => world.tool.push_output(ToolResult {
                    output: format!("err-{k}"),
                    is_error: true,
                }),
                None => {} // unknown tool never reaches the registry
            }
        }
        let responses = build_responses(&s);

        let mut segment_starts = vec![0usize];
        let mut faults = s.faults.clone().into_iter();
        let mut next_fault = faults.next();
        if let Some(f) = next_fault {
            world.sink.fail_publish_at(f);
        }

        let llm = FixtureClient::new();
        if let Some(k) = s.llm_error_at {
            load_fixture(&llm, &responses[..k]);
            llm.push_error(LlmError::RateLimited);
        } else {
            load_fixture(&llm, &responses);
        }

        let mut attempt = world.run(&llm).await;
        let label;

        loop {
            match attempt {
                Ok(outcome) => {
                    // Terminal via the normal path: ceiling + totals.
                    let inv = world.invocation_id().to_string();
                    let wal_sum = check_wal_cost_triangle(&world, &inv, seed).await;
                    match outcome {
                        InvocationOutcome::Completed { cost, .. } => {
                            assert!(
                                cost <= s.budget,
                                "overspend: cost {cost} > budget {} (seed {seed}, wal_sum \
                                 {wal_sum}, scenario {s:?})",
                                s.budget
                            );
                            assert!((cost - wal_sum).abs() < 1e-9, "seed {seed}");
                            label = "completed";
                        }
                        InvocationOutcome::BudgetExceeded { cost, .. } => {
                            assert!(
                                cost > s.budget,
                                "phantom trip: cost {cost} <= budget {} (seed {seed})",
                                s.budget
                            );
                            assert!((cost - wal_sum).abs() < 1e-9, "seed {seed}");
                            label = "budget_exceeded";
                        }
                        InvocationOutcome::Suspended { .. } => {
                            panic!("unexpected drain-suspend at seed {seed}");
                        }
                    }
                    break;
                }
                Err(ExecutorError::Bus(_)) => {
                    // Our injected crash. Segment bookkeeping, then
                    // recover by category.
                    let events = world.sink.events();
                    segment_starts.push(events.len());
                    let in_flight = world.store.find_in_flight_invocations().await.unwrap();
                    if in_flight.is_empty() {
                        // Crash at the triggered publish (or the row
                        // went terminal before the fault): if a
                        // terminal row exists the sweeper owns it.
                        let pending = world.store.list_archive_pending().await.unwrap();
                        label = if pending.is_empty() {
                            "no_residue"
                        } else {
                            "terminal_crash"
                        };
                        break;
                    }
                    let inv = in_flight[0].invocation_id.clone();
                    let tools = world
                        .store
                        .list_tool_dispatches_for_invocation(&inv)
                        .await
                        .unwrap();
                    let llms = world
                        .store
                        .list_llm_dispatches_for_invocation(&inv)
                        .await
                        .unwrap();
                    match categorise(&in_flight[0], &tools, &llms) {
                        RecoveryCategory::Ambiguous => {
                            let err = world
                                .runner
                                .resume(&world.agent, &FixtureClient::new(), world.invocation_id())
                                .await
                                .expect_err("ambiguous must refuse");
                            assert!(err.to_string().contains("ambiguous"), "seed {seed}");
                            label = "ambiguous_refused";
                            break;
                        }
                        RecoveryCategory::SafeResume | RecoveryCategory::SafeReplay => {
                            let consumed = llms
                                .iter()
                                .filter(|r| r.status == DispatchStatus::Completed)
                                .count();
                            let resume_llm = FixtureClient::new();
                            match s.llm_error_at {
                                Some(k) if consumed < k + 1 => {
                                    load_fixture(&resume_llm, &responses[consumed..k]);
                                    resume_llm.push_error(LlmError::RateLimited);
                                }
                                _ => load_fixture(&resume_llm, &responses[consumed..]),
                            }
                            next_fault = faults.next();
                            match next_fault {
                                Some(f) if f > world.sink.events().len() => {
                                    world.sink.fail_publish_at(f);
                                    attempt = world
                                        .runner
                                        .resume(&world.agent, &resume_llm, world.invocation_id())
                                        .await;
                                }
                                _ => {
                                    attempt =
                                        world.resume(&resume_llm).await.map(Ok).unwrap_or_else(Err);
                                }
                            }
                        }
                    }
                }
                Err(ExecutorError::Llm(_)) => {
                    // The scheduled provider error: terminal failed.
                    assert!(
                        s.llm_error_at.is_some(),
                        "unscheduled llm error (seed {seed})"
                    );
                    let failed = world
                        .sink
                        .events()
                        .iter()
                        .filter(|e| matches!(e.payload, EventPayload::Failed(_)))
                        .count();
                    assert!(failed >= 1, "no failed event (seed {seed})");
                    label = "llm_failed";
                    break;
                }
                Err(other) => panic!("unexpected error (seed {seed}): {other}"),
            }
        }

        // --- Universal invariants over the whole life. ---
        let events = world.sink.events();

        // Segment canonicality: the first segment is a run prefix;
        // later segments are resume traces. Only lives that ended
        // through the normal path (terminal outcome or provider
        // failure) demand completeness of their final segment —
        // crash-ended lives are all-prefix by construction.
        if segment_starts.last() != Some(&events.len()) {
            segment_starts.push(events.len());
        }
        let life_completed = matches!(label, "completed" | "budget_exceeded" | "llm_failed");
        for (i, pair) in segment_starts.windows(2).enumerate() {
            let segment = &events[pair[0]..pair[1]];
            let is_last = i + 2 == segment_starts.len();
            let result = match (i == 0, is_last && life_completed) {
                (true, true) => crate::test_support::oracle::check_invocation_trace(segment),
                (true, false) => check_invocation_trace_prefix(segment),
                (false, true) => check_resume_trace(segment),
                (false, false) => check_resume_trace_prefix(segment),
            };
            assert_no_violations(result, &format!("segment {i} ({label})"), seed);
        }

        // At most one terminal event, ever.
        let terminals = events
            .iter()
            .filter(|e| {
                matches!(
                    e.payload,
                    EventPayload::Completed(_) | EventPayload::Failed(_)
                )
            })
            .count();
        assert!(terminals <= 1, "{terminals} terminals (seed {seed})");

        // Each logical tool call executed at most once.
        let dispatches = world.tool.dispatches().lock().unwrap().clone();
        let mut seen = HashSet::new();
        for params in &dispatches {
            assert!(
                seen.insert(params.to_string()),
                "logical tool call ran twice: {params} (seed {seed})"
            );
        }

        // Every persisted blob still satisfies the state invariants.
        check_blobs(&world, seed).await;

        label
    }

    /// The CI tier: a fixed seed range, every invariant on.
    #[tokio::test]
    async fn soak_fixed_seed_range() {
        let iters: u64 = std::env::var("FQ_SOAK_ITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(48);
        let mut coverage: HashMap<&'static str, usize> = HashMap::new();
        for seed in 1..=iters {
            let label = run_scenario(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)).await;
            *coverage.entry(label).or_default() += 1;
        }
        eprintln!("soak coverage over {iters} scenarios: {coverage:?}");
        // The fixed CI range must at least exercise the two dominant
        // classes; deep runs (just soak) cover the rest by volume.
        assert!(coverage.get("completed").copied().unwrap_or(0) > 0);
        assert!(coverage.len() >= 3, "coverage collapsed: {coverage:?}");
    }
}

#[cfg(test)]
mod drain_equivalence {
    //! ADR-0027 PR-1: a drain requested at a step boundary suspends the
    //! invocation with its state already checkpointed, and the next
    //! binary's recovery resumes it. Held to the same bar as the crash
    //! DST (`crash_dst`) / `resume_equivalence`: at every drain point we
    //! assert (a) the suspend prefix is a canonical trace prefix, (b) the
    //! WAL categorises the suspend as cleanly resumable — **never
    //! `Ambiguous`** (the load-bearing safety property: a drain always
    //! lands at a resumable boundary), (c) the resume leg is a valid
    //! headless resume, (d) the tool runs at most once per logical call
    //! across drain+resume, and (e) the suspended-then-resumed run is
    //! observationally identical to the uninterrupted reference. Proven
    //! for a fixed script exhaustively and over random seeds/scripts.
    //!
    //! The drain rides the same shared-sink publish counter the crash
    //! tests use, but gracefully: `drain_at_publish` lets the publish
    //! succeed and the loop suspends at its next step boundary. The
    //! resume leg runs on a *fresh* runner — the "next binary" — because
    //! a `DrainSignal` is monotonic.

    use super::crash_dst::{completed_llm_turns, wal_category};
    use super::resume_equivalence::{
        RunResult, assert_equivalent, load_fixture, queue_tool_outputs, run_reference, script,
        summary_of,
    };
    use super::*;
    use crate::test_support::oracle::{
        check_invocation_trace_prefix, check_resume_trace, observational_trace,
    };
    use crate::worker::recovery::RecoveryCategory;

    /// Drain at the first publish of `boundary_span`, assert the full
    /// suspend/resume invariant stack, then resume on a fresh binary with
    /// exactly the LLM turns the WAL says are still outstanding. Returns
    /// the combined-run result for the observational-equivalence check.
    async fn run_drained(seed: u64, turns: usize, boundary_span: usize) -> RunResult {
        let world = SimWorld::new(seed, 5.0).await;
        queue_tool_outputs(&world, turns);
        let responses = script(turns);

        // Graceful drain at the span boundary: the publish still
        // succeeds (unlike a crash) and the loop suspends at the next
        // step boundary.
        world
            .sink
            .drain_at_publish(1 + 3 * boundary_span, world.runner.drain_signal());
        let llm = FixtureClient::new();
        load_fixture(&llm, &responses);
        let outcome = world.run(&llm).await.expect("drained run returns Ok");
        assert!(
            matches!(outcome, InvocationOutcome::Suspended { .. }),
            "expected Suspended at span {boundary_span}, got {outcome:?}"
        );

        // (a) The captured prefix is a canonical trace prefix — the run
        // did nothing illegal before suspending.
        let prefix = world.sink.events();
        if let Err(violations) = check_invocation_trace_prefix(&prefix) {
            panic!("suspend prefix not canonical (span {boundary_span}): {violations:?}");
        }
        // Non-terminal: the row stays in-flight for recovery to resume.
        let in_flight = world.store.find_in_flight_invocations().await.unwrap();
        assert_eq!(
            in_flight.len(),
            1,
            "a suspended invocation must stay in-flight (span {boundary_span})"
        );
        // (b) The load-bearing safety property: a drain suspends at a
        // cleanly-resumable boundary, never `Ambiguous`.
        let inv = world.invocation_id().to_string();
        let category = wal_category(&world, &inv).await;
        assert!(
            matches!(
                category,
                RecoveryCategory::SafeResume | RecoveryCategory::SafeReplay
            ),
            "a drain-suspend must categorise SafeResume/SafeReplay, got {category:?} \
             (span {boundary_span})"
        );
        let prefix_len = prefix.len();

        // Resume on a fresh binary with the not-yet-consumed turns — the
        // WAL is the source of truth for how many LLM turns completed.
        let consumed = completed_llm_turns(&world, &inv).await;
        let resume_llm = FixtureClient::new();
        load_fixture(&resume_llm, &responses[consumed..]);
        let outcome = world
            .resume_on_fresh_binary(&resume_llm)
            .await
            .expect("fresh-binary resume completes");

        // (c) The resume leg is a valid headless resume: starts mid-
        // stream (no re-`triggered`) and runs to one terminal + archived.
        let all_events = world.sink.events();
        if let Err(violations) = check_resume_trace(&all_events[prefix_len..]) {
            panic!("resume leg not a valid headless resume (span {boundary_span}): {violations:?}");
        }
        // (d) At most once per logical tool call across drain+resume.
        assert_eq!(
            world.tool.dispatches().lock().unwrap().len(),
            turns,
            "each logical tool call must run exactly once across drain+resume (span {boundary_span})"
        );

        RunResult {
            observed: observational_trace(&all_events),
            summary: summary_of(&outcome),
            dispatches: world.tool.dispatches().lock().unwrap().clone(),
        }
    }

    /// Every interior step boundary of a fixed two-tool-turn script,
    /// exhaustively: drain there and match the uninterrupted reference
    /// (plus the invariant stack asserted inside `run_drained`).
    #[tokio::test]
    async fn every_drain_boundary_of_a_fixed_script_is_equivalent() {
        let turns = 2;
        let reference = run_reference(1234, turns).await;
        // Interior boundaries only: draining during the *final* step
        // lets it complete rather than suspend, so stop before it.
        for boundary_span in 0..(2 * turns) {
            let drained = run_drained(1234, turns, boundary_span).await;
            assert_equivalent(&reference, &drained, &format!("drain span {boundary_span}"));
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 24, ..Default::default()
        })]

        /// Random scripts × random interior boundaries × random seeds:
        /// the drained-and-resumed run holds the full invariant stack
        /// (asserted in `run_drained`) and is observationally identical
        /// to the uninterrupted one.
        #[test]
        fn drained_runs_are_observationally_equivalent(
            seed: u64,
            turns in 1usize..=3,
            boundary_span in 0usize..6,
        ) {
            // Interior boundaries only (draining the final step completes).
            proptest::prop_assume!(boundary_span < 2 * turns);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sim runtime");
            runtime.block_on(async {
                let reference = run_reference(seed, turns).await;
                let drained = run_drained(seed, turns, boundary_span).await;
                assert_equivalent(
                    &reference,
                    &drained,
                    &format!("seed {seed}, turns {turns}, drain span {boundary_span}"),
                );
            });
        }
    }
}

/// Parallel-workers Phase 2: N invocations concurrently through the
/// one shared runner, validated by the partitioned oracle
/// (per-invocation grammar + Triggered roots + the D1 overlap gauge)
/// and the cross-invocation conservation/isolation invariants
/// (E2/E3/E4) from the plan's verification design.
#[cfg(test)]
mod concurrency {
    use std::collections::HashMap;

    use super::tests::{end_turn, sim_tool_call_with};
    use super::*;
    use crate::events::EventPayload;
    use crate::test_support::oracle;
    use crate::worker::workspace::PerInvocationWorkspace;

    /// Script for ordinal `i`: `len` tool spans whose `path` parameter
    /// targets `${workspace}` (so recorded dispatches attribute to the
    /// invocation's own directory), then an end turn. `from` offsets
    /// the call ids so a continuation script never collides with the
    /// WAL rows of already-completed spans.
    fn script_for(i: usize, from: usize, len: usize) -> FixtureClient {
        let llm = FixtureClient::new();
        for k in from..from + len {
            llm.push_response(sim_tool_call_with(
                &format!("i{i}c{k}"),
                json!({
                    "step": format!("{i}-{k}"),
                    "path": format!("${{workspace}}/out-{k}.txt"),
                }),
            ));
        }
        llm.push_response(end_turn(&format!("done-{i}")));
        llm
    }

    /// Map invocation id → ordinal via the `{"sim": ordinal}` trigger
    /// payload each `run_many` invocation carries in its Triggered root.
    fn ordinals_by_invocation(events: &[Event]) -> HashMap<uuid::Uuid, usize> {
        events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Triggered(t) => Some((
                    e.envelope.invocation_id,
                    t.trigger_payload["sim"].as_u64().expect("sim ordinal") as usize,
                )),
                _ => None,
            })
            .collect()
    }

    /// Happy path: N invocations with distinct script lengths, each in
    /// its own provisioned workspace. Every arc passes the unchanged
    /// grammar; dispatches conserve per invocation and never touch a
    /// sibling's directory; terminal workspaces are reclaimed.
    #[tokio::test]
    async fn concurrent_happy_path_passes_the_partitioned_oracle() {
        let root = tempfile::tempdir().expect("workspace root");
        let provider = Arc::new(PerInvocationWorkspace::new(root.path().to_path_buf()));
        let world = SimWorld::with_workspace(11, 50.0, provider).await;

        let lens = [1usize, 2, 3];
        let scripts = lens
            .iter()
            .enumerate()
            .map(|(i, &len)| script_for(i, 0, len))
            .collect();
        let outcomes = world.run_many(scripts).await;
        for outcome in &outcomes {
            assert!(
                matches!(outcome, Ok(InvocationOutcome::Completed { .. })),
                "expected Completed, got {outcome:?}"
            );
        }

        let events = world.sink.events();
        oracle::assert_valid_concurrent_trace(&events, lens.len());

        let ordinals = ordinals_by_invocation(&events);
        assert_eq!(ordinals.len(), lens.len(), "one arc per invocation");

        let dispatches = world.tool.dispatches().lock().unwrap().clone();
        assert_eq!(
            dispatches.len(),
            lens.iter().sum::<usize>(),
            "global tool-dispatch conservation"
        );
        for (id, ordinal) in &ordinals {
            let row = world
                .store
                .get_invocation_state(&id.to_string())
                .await
                .expect("state row query")
                .expect("state row");
            let ws = row.workspace_ref.expect("workspace_ref persisted");
            assert!(
                ws.ends_with(&id.to_string()),
                "workspace dir is named by the invocation id"
            );
            let own = dispatches
                .iter()
                .filter(|p| p["path"].as_str().unwrap_or("").starts_with(&ws))
                .count();
            assert_eq!(
                own, lens[*ordinal],
                "invocation {id}: every dispatch lands in its own workspace, \
                 and exactly as many as its script issued"
            );
            assert!(
                !std::path::Path::new(&ws).exists(),
                "terminal invocation's workspace is reclaimed"
            );
        }
    }

    /// Graceful drain with N in flight: every invocation suspends at a
    /// step boundary (or had already completed), suspended workspaces
    /// survive, and a fresh binary resumes each exactly once to
    /// completion with per-invocation continuation scripts sized from
    /// the WAL.
    #[tokio::test]
    async fn concurrent_drain_suspends_all_and_resume_completes_each() {
        let root = tempfile::tempdir().expect("workspace root");
        let provider = Arc::new(PerInvocationWorkspace::new(root.path().to_path_buf()));
        let world = SimWorld::with_workspace(13, 50.0, provider).await;

        let len = 3usize;
        // Arm the drain early: the publish still succeeds, and every
        // in-flight invocation observes the (global) signal at its own
        // next step boundary.
        world.sink.drain_at_publish(4, world.runner.drain_signal());

        let scripts = (0..3).map(|i| script_for(i, 0, len)).collect();
        let outcomes = world.run_many(scripts).await;
        let mut suspended: Vec<uuid::Uuid> = Vec::new();
        for outcome in &outcomes {
            match outcome {
                Ok(InvocationOutcome::Suspended { invocation_id }) => {
                    suspended.push(*invocation_id)
                }
                Ok(InvocationOutcome::Completed { .. }) => {}
                other => panic!("expected Suspended or Completed, got {other:?}"),
            }
        }
        assert!(
            !suspended.is_empty(),
            "an early drain must suspend at least one invocation"
        );

        // Mid-drain the trace is a set of canonical prefixes.
        oracle::assert_valid_concurrent_trace_prefix(&world.sink.events(), 3);

        // Suspended workspaces persist across the "restart".
        let ordinals = ordinals_by_invocation(&world.sink.events());
        let mut refs: HashMap<uuid::Uuid, String> = HashMap::new();
        for id in &suspended {
            let row = world
                .store
                .get_invocation_state(&id.to_string())
                .await
                .expect("state row query")
                .expect("state row");
            let ws = row.workspace_ref.expect("workspace_ref persisted");
            assert!(
                std::path::Path::new(&ws).exists(),
                "suspended invocation keeps its workspace"
            );
            refs.insert(*id, ws);
        }

        // Continuation scripts sized from each invocation's own WAL.
        let mut continuations: HashMap<uuid::Uuid, FixtureClient> = HashMap::new();
        for id in &suspended {
            let done = super::crash_dst::completed_llm_turns(&world, &id.to_string()).await;
            let ordinal = ordinals[id];
            // `done` counts completed LLM turns; each tool span is one
            // turn, the end turn is one more. Remaining spans resume
            // at call-id offset `done`.
            let remaining = len.saturating_sub(done);
            continuations.insert(*id, script_for(ordinal, done, remaining));
        }
        let resumed = world.resume_all_on_fresh_binary(continuations).await;
        assert_eq!(
            resumed.len(),
            suspended.len(),
            "each suspended invocation resumes once"
        );
        for (id, outcome) in &resumed {
            assert!(
                matches!(outcome, Ok(InvocationOutcome::Completed { .. })),
                "invocation {id}: expected Completed after resume, got {outcome:?}"
            );
            assert!(
                !std::path::Path::new(&refs[id]).exists(),
                "invocation {id}: workspace reclaimed after terminal resume"
            );
        }

        // The combined pre-drain + post-resume trace: every arc is a
        // full canonical trace again.
        oracle::assert_valid_concurrent_trace(&world.sink.events(), 3);
    }

    /// A publish fault pinned to one invocation's own publish count:
    /// exactly one invocation crashes, its siblings complete
    /// untouched, and resume brings the crashed one to completion.
    #[tokio::test]
    async fn concurrent_crash_isolates_to_one_invocation() {
        let world = SimWorld::new(17, 50.0).await;
        let len = 2usize;
        world.sink.fail_publish_at_invocation_count(3);

        let scripts = (0..2).map(|i| script_for(i, 0, len)).collect();
        let outcomes = world.run_many(scripts).await;
        let crashed: Vec<usize> = outcomes
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.is_err().then_some(i))
            .collect();
        assert_eq!(
            crashed.len(),
            1,
            "the per-invocation fault crashes exactly one invocation: {outcomes:?}"
        );

        let events_at_crash = world.sink.events().len();
        oracle::assert_valid_concurrent_trace_prefix(&world.sink.events(), 2);

        let in_flight = world
            .store
            .find_in_flight_invocations()
            .await
            .expect("query in-flight");
        assert_eq!(in_flight.len(), 1, "one crashed invocation to recover");
        let id: uuid::Uuid = in_flight[0].invocation_id.parse().expect("id parses");
        let ordinal = ordinals_by_invocation(&world.sink.events())[&id];

        let done = super::crash_dst::completed_llm_turns(&world, &id.to_string()).await;
        let remaining = len.saturating_sub(done);
        let mut continuation = HashMap::new();
        continuation.insert(id, script_for(ordinal, done, remaining));
        let resumed = world.resume_all_on_fresh_binary(continuation).await;
        assert_eq!(resumed.len(), 1);
        assert!(
            matches!(resumed[0].1, Ok(InvocationOutcome::Completed { .. })),
            "crashed invocation completes on resume: {:?}",
            resumed[0].1
        );

        // A crash can land mid-triple, so the crashed arc's combined
        // trace is validated in two segments — pre-crash as a prefix,
        // the continuation with the resume grammar (headless, runs to
        // terminal) — exactly as the single-invocation crash DST does.
        // The untouched sibling must still be one full canonical arc.
        let events = world.sink.events();
        let crashed_pre: Vec<Event> = events[..events_at_crash]
            .iter()
            .filter(|e| e.envelope.invocation_id == id)
            .cloned()
            .collect();
        let crashed_post: Vec<Event> = events[events_at_crash..]
            .iter()
            .filter(|e| e.envelope.invocation_id == id)
            .cloned()
            .collect();
        if let Err(violations) = oracle::check_invocation_trace_prefix(&crashed_pre) {
            panic!("crashed arc's pre-crash segment not a canonical prefix: {violations:?}");
        }
        if let Err(violations) = oracle::check_resume_trace(&crashed_post) {
            panic!("crashed arc's resumed segment not a canonical resume trace: {violations:?}");
        }
        for (sibling, _) in ordinals_by_invocation(&events)
            .iter()
            .filter(|(other, _)| **other != id)
        {
            let arc: Vec<Event> = events
                .iter()
                .filter(|e| e.envelope.invocation_id == *sibling)
                .cloned()
                .collect();
            oracle::assert_valid_trace(&arc);
        }
    }

    /// E2's sharpest case: one invocation blowing its budget never
    /// touches a concurrent sibling's spend.
    #[tokio::test]
    async fn concurrent_budgets_do_not_cross_contaminate() {
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            "claude-sim".to_string(),
            crate::pricing::ModelPricing {
                // Each scripted turn reads 100 input tokens → $1.00.
                input_per_million: 10_000.0,
                output_per_million: 0.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
        );
        let pricing = Arc::new(PricingTable::from_map(entries));
        let world = SimWorld::with_pricing(19, 5.0, pricing).await;

        // Ordinal 0: six $1 turns — crosses the $5 budget mid-run.
        // Ordinal 1: one turn plus the end turn — comfortably under.
        let scripts = vec![script_for(0, 0, 6), script_for(1, 0, 1)];
        let outcomes = world.run_many(scripts).await;

        let ordinals = ordinals_by_invocation(&world.sink.events());
        for (id, ordinal) in &ordinals {
            let outcome = &outcomes[*ordinal];
            match ordinal {
                0 => assert!(
                    matches!(outcome, Ok(InvocationOutcome::BudgetExceeded { .. })),
                    "invocation {id} (big spender): expected BudgetExceeded, got {outcome:?}"
                ),
                _ => assert!(
                    matches!(outcome, Ok(InvocationOutcome::Completed { .. })),
                    "invocation {id} (frugal): its sibling's overspend must not \
                     trip this budget; got {outcome:?}"
                ),
            }
        }
    }
}

/// The seeded-random-interleaving sweep from the plan's Phase 2
/// verification design: many (seed, N, script-shape) combinations, each
/// run concurrently and held to the partitioned oracle plus
/// conservation. Per-invocation entropy is ordinal-derived, so a
/// failure's per-invocation signature reproduces even where the global
/// interleaving doesn't.
#[cfg(test)]
mod concurrency_properties {
    use proptest::prelude::*;

    use super::tests::{end_turn, sim_tool_call_with};
    use super::*;
    use crate::events::EventPayload;
    use crate::test_support::oracle;

    fn script_for(i: usize, len: usize) -> FixtureClient {
        let llm = FixtureClient::new();
        for k in 0..len {
            llm.push_response(sim_tool_call_with(
                &format!("i{i}c{k}"),
                json!({"step": format!("{i}-{k}")}),
            ));
        }
        llm.push_response(end_turn(&format!("done-{i}")));
        llm
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 16,
            ..ProptestConfig::default()
        })]

        #[test]
        fn concurrent_happy_sweep(
            seed in 1u64..=u64::MAX / 2,
            lens in proptest::collection::vec(1usize..=3, 2..=4),
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let world = SimWorld::new(seed, 500.0).await;
                let scripts = lens
                    .iter()
                    .enumerate()
                    .map(|(i, &len)| script_for(i, len))
                    .collect();
                let outcomes = world.run_many(scripts).await;
                for outcome in &outcomes {
                    prop_assert!(
                        matches!(outcome, Ok(InvocationOutcome::Completed { .. })),
                        "expected Completed, got {:?}",
                        outcome
                    );
                }

                let events = world.sink.events();
                if let Err(violations) =
                    oracle::check_concurrent_trace(&events, lens.len())
                {
                    let lines: Vec<String> =
                        violations.iter().map(|v| format!("  - {v}")).collect();
                    prop_assert!(false, "oracle violations:\n{}", lines.join("\n"));
                }

                // Conservation: per-invocation LLM turns match each
                // script exactly (len tool turns + 1 end turn).
                let mut seen = 0usize;
                for event in &events {
                    if let EventPayload::Triggered(t) = &event.payload {
                        let ordinal =
                            t.trigger_payload["sim"].as_u64().expect("ordinal") as usize;
                        let turns = super::crash_dst::completed_llm_turns(
                            &world,
                            &event.envelope.invocation_id.to_string(),
                        )
                        .await;
                        prop_assert_eq!(
                            turns,
                            lens[ordinal] + 1,
                            "invocation {} (ordinal {}) LLM-turn conservation",
                            event.envelope.invocation_id,
                            ordinal
                        );
                        seen += 1;
                    }
                }
                prop_assert_eq!(seen, lens.len(), "one Triggered root per script");
                Ok(())
            })?;
        }
    }
}
