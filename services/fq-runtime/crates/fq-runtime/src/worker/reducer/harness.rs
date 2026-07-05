//! Native [`Reducer`] implementation as an explicit state
//! machine. Seed the conversation with system + user messages,
//! ask the model, dispatch any tool calls, feed results back,
//! repeat until the model declares an end turn or the iteration
//! cap is hit.
//!
//! The state enum is small on purpose. If it stays small as we
//! layer in retries, partial dispatch, multi-step reasoning, and
//! skill composition, that is positive evidence for the reducer
//! shape. If it balloons, that is data the design assessment
//! asked for.

use serde::{Deserialize, Serialize};

use super::types::{
    AgentConfig, CapabilityResult, HarnessError, HarnessErrorKind, LogEntry, LogLevel,
    ModelRequest, NextAction, Reducer, StepInput, StepOutput, ToolCallRequest, TriggerPayload,
};
use crate::events::{Message, MessageRole, MessageToolCall, RequestParams, StopReason};

/// Default maximum LLM turns. Mirrors `executor::MAX_ITERATIONS`.
pub const DEFAULT_MAX_ITERATIONS: u32 = 20;

/// Native, synchronous, stateless reducer. All state lives in
/// the opaque blob carried in [`StepInput::state`]; this struct
/// holds nothing.
#[derive(Debug, Clone, Default)]
pub struct Harness;

impl Harness {
    pub fn new() -> Self {
        Self
    }
}

impl Reducer for Harness {
    fn step(&self, input: StepInput) -> Result<StepOutput, HarnessError> {
        let mut state = HarnessState::load(&input.state)?;

        match state.phase {
            Phase::Initial => initial_step(input, &mut state),
            Phase::AwaitingModel => model_response_step(input, &mut state),
            Phase::DispatchingTools => tool_results_step(input, &mut state),
            Phase::Done => Err(internal_error(
                "step called after invocation already terminated",
            )),
        }
    }
}

/// Persistent state the reducer carries across steps. Round-
/// trips through `state: Vec<u8>` as JSON. Kept minimal:
/// everything else (`config`, `trigger`) arrives in `StepInput`
/// every call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HarnessState {
    phase: Phase,
    /// Conversation history accumulated across LLM turns. The
    /// system prompt + initial user message are seeded on step 0.
    #[serde(default)]
    messages: Vec<Message>,
    /// LLM-turn counter. Bounded by [`AgentConfig::max_iterations`].
    #[serde(default)]
    iteration: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, strum::EnumCount)]
#[serde(rename_all = "snake_case")]
enum Phase {
    /// Pre-step-0: nothing has happened yet. The reducer seeds
    /// the conversation and asks for the first model call.
    #[default]
    Initial,
    /// Last action was [`NextAction::CallModel`]; the next
    /// `last_result` should be a [`CapabilityResult::ModelResult`].
    AwaitingModel,
    /// Last action was a tool dispatch; the next `last_result`
    /// should be [`CapabilityResult::ToolResult`] or
    /// [`CapabilityResult::ParallelToolResults`].
    DispatchingTools,
    /// Terminal. `step` should not be called again.
    Done,
}

impl HarnessState {
    fn load(bytes: &[u8]) -> Result<Self, HarnessError> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let state: Self = serde_json::from_slice(bytes).map_err(|err| HarnessError {
            kind: HarnessErrorKind::InternalError,
            message: format!("state deserialise failed: {err}"),
        })?;
        // Serde catches structural malformation; this catches a
        // corrupt or stale persisted blob whose contents contradict
        // its phase.
        state.validate()?;
        Ok(state)
    }

    fn save(&self) -> Result<Vec<u8>, HarnessError> {
        // Validating on save catches a reducer bug that produced an
        // inconsistent state in-memory, before it can be persisted.
        self.validate()?;
        serde_json::to_vec(self).map_err(|err| HarnessError {
            kind: HarnessErrorKind::InternalError,
            message: format!("state serialise failed: {err}"),
        })
    }

    /// The phase ↔ contents invariants the state machine enforces
    /// (reducer verification plan, claim R7; written out from
    /// `initial_step` / `model_response_step` / `tool_results_step`):
    ///
    /// - `Initial` ⇒ the conversation is empty.
    /// - `AwaitingModel` ⇒ non-empty, and the last message is not an
    ///   assistant message (an assistant turn either completed the
    ///   invocation or moved it to `DispatchingTools`).
    /// - `DispatchingTools` ⇒ the last message is an assistant
    ///   message carrying at least one tool call.
    /// - `Done` ⇒ the conversation was seeded (only `Initial` may be
    ///   empty).
    /// - `iteration` is at least the number of assistant messages —
    ///   each assistant message in the history was one counted LLM
    ///   turn (turns that completed the invocation count without
    ///   appending a message, so this is a lower bound).
    fn validate(&self) -> Result<(), HarnessError> {
        let violation = match self.phase {
            Phase::Initial => (!self.messages.is_empty()).then(|| {
                format!(
                    "phase Initial requires an empty conversation, found {} message(s)",
                    self.messages.len()
                )
            }),
            Phase::AwaitingModel => match self.messages.last() {
                None => Some("phase AwaitingModel requires a seeded conversation".to_string()),
                Some(last) if matches!(last.role, MessageRole::Assistant) => {
                    Some("phase AwaitingModel cannot follow an assistant message".to_string())
                }
                Some(_) => None,
            },
            Phase::DispatchingTools => match self.messages.last() {
                Some(last) if matches!(last.role, MessageRole::Assistant) => {
                    last.tool_calls.is_empty().then(|| {
                        "phase DispatchingTools requires the last assistant message to carry \
                         tool calls"
                            .to_string()
                    })
                }
                Some(_) => Some(
                    "phase DispatchingTools requires the last message to be an assistant \
                     message"
                        .to_string(),
                ),
                None => Some("phase DispatchingTools requires a seeded conversation".to_string()),
            },
            Phase::Done => self
                .messages
                .is_empty()
                .then(|| "phase Done requires a seeded conversation".to_string()),
        };
        let violation = violation.or_else(|| {
            let assistant_count = self
                .messages
                .iter()
                .filter(|m| matches!(m.role, MessageRole::Assistant))
                .count();
            ((self.iteration as usize) < assistant_count).then(|| {
                format!(
                    "iteration {} is below the {} assistant message(s) in the history",
                    self.iteration, assistant_count
                )
            })
        });
        match violation {
            Some(message) => Err(HarnessError {
                kind: HarnessErrorKind::InternalError,
                message: format!("invalid harness state: {message}"),
            }),
            None => Ok(()),
        }
    }
}

fn initial_step(input: StepInput, state: &mut HarnessState) -> Result<StepOutput, HarnessError> {
    debug_assert_eq!(state.phase, Phase::Initial);

    state.messages.push(Message {
        role: MessageRole::System,
        content: Some(input.config.system_prompt.clone()),
        tool_calls: vec![],
        tool_call_id: None,
    });
    // Host-curated `static_resources` content, injected once right
    // after the system prompt and before the trigger. The runner
    // read the pins at invocation start (the reducer does no I/O);
    // `None` here means no pins, or this is a resumed invocation
    // where the content already lives in the persisted history.
    if let Some(context) = &input.static_resource_context {
        state.messages.push(Message {
            role: MessageRole::User,
            content: Some(context.clone()),
            tool_calls: vec![],
            tool_call_id: None,
        });
    }
    state.messages.push(Message {
        role: MessageRole::User,
        content: Some(payload_to_user_message(&input.trigger)),
        tool_calls: vec![],
        tool_call_id: None,
    });

    let request = build_model_request(&input.config, &state.messages);
    state.phase = Phase::AwaitingModel;

    Ok(StepOutput {
        next_action: NextAction::CallModel(request),
        state: state.save()?,
        logs: vec![log_info("initial step: requesting first model turn")],
        events: vec![],
    })
}

fn model_response_step(
    input: StepInput,
    state: &mut HarnessState,
) -> Result<StepOutput, HarnessError> {
    debug_assert_eq!(state.phase, Phase::AwaitingModel);
    let response = match input.last_result {
        Some(CapabilityResult::ModelResult(r)) => r,
        Some(CapabilityResult::Cancelled) => {
            return terminal(
                state,
                NextAction::Failed(HarnessError {
                    kind: HarnessErrorKind::InternalError,
                    message: "host cancelled model call".to_string(),
                }),
            );
        }
        Some(CapabilityResult::HostError(msg)) => {
            return terminal(
                state,
                NextAction::Failed(HarnessError {
                    kind: HarnessErrorKind::InternalError,
                    message: format!("host error during model call: {msg}"),
                }),
            );
        }
        other => {
            return Err(internal_error(&format!(
                "expected ModelResult after CallModel, got {other:?}"
            )));
        }
    };

    state.iteration = state.iteration.saturating_add(1);

    if response.tool_calls.is_empty() {
        let final_text = response.content.clone().unwrap_or_default();
        return terminal(state, NextAction::Complete(final_text));
    }

    state.messages.push(Message {
        role: MessageRole::Assistant,
        content: response.content.clone(),
        tool_calls: response.tool_calls.clone(),
        tool_call_id: None,
    });

    // `max_iterations` is literal. Zero is a valid stop signal —
    // the loop terminates immediately at iteration 1 (>= 0) and
    // the agent never runs another LLM turn. Producers that want
    // the harness default pass `DEFAULT_MAX_ITERATIONS` explicitly
    // rather than relying on a sentinel.
    let max_iter = input.config.max_iterations;
    if state.iteration >= max_iter {
        return terminal(
            state,
            NextAction::Failed(HarnessError {
                kind: HarnessErrorKind::MaxIterations,
                message: format!("exceeded max iterations ({max_iter})"),
            }),
        );
    }

    let pending: Vec<ToolCallRequest> = response
        .tool_calls
        .iter()
        .map(|tc| ToolCallRequest {
            tool_call_id: tc.tool_call_id.clone(),
            tool_name: tc.tool_name.clone(),
            parameters: tc.parameters.clone(),
        })
        .collect();

    state.phase = Phase::DispatchingTools;

    let action = if pending.len() == 1 {
        NextAction::CallTool(pending.into_iter().next().expect("len 1"))
    } else {
        NextAction::CallToolsParallel(pending)
    };

    Ok(StepOutput {
        next_action: action,
        state: state.save()?,
        logs: vec![log_info("model produced tool calls; dispatching")],
        events: vec![],
    })
}

fn tool_results_step(
    input: StepInput,
    state: &mut HarnessState,
) -> Result<StepOutput, HarnessError> {
    debug_assert_eq!(state.phase, Phase::DispatchingTools);

    let results = match input.last_result {
        Some(CapabilityResult::ToolResult(r)) => vec![r],
        Some(CapabilityResult::ParallelToolResults(rs)) => rs,
        Some(CapabilityResult::Cancelled) => {
            return terminal(
                state,
                NextAction::Failed(HarnessError {
                    kind: HarnessErrorKind::InternalError,
                    message: "host cancelled tool dispatch".to_string(),
                }),
            );
        }
        Some(CapabilityResult::HostError(msg)) => {
            return terminal(
                state,
                NextAction::Failed(HarnessError {
                    kind: HarnessErrorKind::InternalError,
                    message: format!("host error during tool dispatch: {msg}"),
                }),
            );
        }
        other => {
            return Err(internal_error(&format!(
                "expected ToolResult after CallTool, got {other:?}"
            )));
        }
    };

    for result in results {
        state.messages.push(Message {
            role: MessageRole::Tool,
            content: Some(result.output),
            tool_calls: vec![],
            tool_call_id: Some(result.tool_call_id),
        });
    }

    let request = build_model_request(&input.config, &state.messages);
    state.phase = Phase::AwaitingModel;

    Ok(StepOutput {
        next_action: NextAction::CallModel(request),
        state: state.save()?,
        logs: vec![log_info(
            "tool results integrated; requesting next model turn",
        )],
        events: vec![],
    })
}

fn terminal(state: &mut HarnessState, action: NextAction) -> Result<StepOutput, HarnessError> {
    state.phase = Phase::Done;
    Ok(StepOutput {
        next_action: action,
        state: state.save()?,
        logs: vec![],
        events: vec![],
    })
}

fn build_model_request(config: &AgentConfig, messages: &[Message]) -> ModelRequest {
    ModelRequest {
        model: config.model.clone(),
        messages: messages.to_vec(),
        tools: config.tools_available.clone(),
        params: RequestParams {
            temperature: None,
            max_tokens: Some(4096),
        },
    }
}

fn payload_to_user_message(trigger: &TriggerPayload) -> String {
    use serde_json::Value;
    match &trigger.payload {
        Value::Null => "(no input)".to_string(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn log_info(message: &str) -> LogEntry {
    LogEntry {
        level: LogLevel::Info,
        message: message.to_string(),
    }
}

fn internal_error(msg: &str) -> HarnessError {
    HarnessError {
        kind: HarnessErrorKind::InternalError,
        message: msg.to_string(),
    }
}

// Suppress dead-code warnings for fields used only via deserialise.
#[allow(dead_code)]
fn _force_use(_: MessageToolCall, _: StopReason) {}

#[cfg(test)]
mod tests {
    //! Unit tests for the reducer itself. These need no I/O,
    //! no async runtime, no event bus — they exercise the pure
    //! `step()` function directly.

    use super::*;
    use crate::agent::AgentId;
    use crate::events::{StopReason, TokenUsage, ToolSchema};
    use crate::worker::reducer::types::{
        ModelResponse, ToolCallResult, TriggerPayload, TriggerSourceKind,
    };
    use serde_json::{Value, json};
    use strum::EnumCount;

    /// Calibration test for the persistent state's variant count.
    ///
    /// ADR-0014 (agent harness as reducer) is load-bearing for
    /// large parts of the runtime; if the state machine balloons
    /// once retries, partial dispatch, skill composition, or
    /// other features get folded in, that's the signal the model
    /// is the wrong shape and ADR-0014 needs a re-read.
    ///
    /// Calibration thresholds (carried over from the May-5
    /// reducer-prototype plan, where they were the variant-count
    /// alarm convention):
    ///
    /// - **Under 10 variants** is comfortable — the current shape.
    /// - **Dozens** is tolerable — note the trend and consider a
    ///   refactor, but no architectural alarm yet.
    /// - **50+** is the alarm threshold — revisit ADR-0014.
    ///
    /// The count is derived from the enum via `strum::EnumCount`
    /// rather than pinned manually, so adding or removing a
    /// `Phase` variant automatically updates `Phase::COUNT` at
    /// compile time. The if/panic const-block pattern is used in
    /// place of `assert!` because clippy's
    /// `assertions_on_constants` (denied workspace-wide) flags
    /// any assertion whose result is compile-time constant —
    /// even when that's the whole point.
    #[test]
    fn phase_variant_count_is_within_comfort_threshold() {
        const _COMFORT: () = if Phase::COUNT >= 10 {
            panic!("Phase variant count exceeded the under-ten comfort threshold — note the trend");
        };
        const _ALARM: () = if Phase::COUNT >= 50 {
            panic!(
                "Phase variant count hit the alarm threshold; revisit ADR-0014 (agent harness as reducer)"
            );
        };
        // The const-blocks above already do the work at compile
        // time; the test function exists so the calibration is
        // explicit in the test runner's output. The body is
        // empty by design.
    }

    fn config() -> AgentConfig {
        AgentConfig {
            agent_id: AgentId::new("test").unwrap(),
            model: "claude-haiku".to_string(),
            system_prompt: "You are a test agent.".to_string(),
            tools_available: vec![ToolSchema {
                name: "echo".to_string(),
                description: "echo".to_string(),
                parameters_schema: json!({"type": "object"}),
            }],
            allowed_tool_names: vec!["echo".to_string()],
            max_iterations: 3,
        }
    }

    fn trigger(payload: Value) -> TriggerPayload {
        TriggerPayload {
            source: TriggerSourceKind::Manual,
            subject: None,
            payload,
        }
    }

    fn step_input(
        state: Vec<u8>,
        last_result: Option<CapabilityResult>,
        step_index: u32,
    ) -> StepInput {
        StepInput {
            config: config(),
            trigger: trigger(json!("hello")),
            state,
            last_result,
            now_ms: 1_000_000 + step_index as u64,
            random_seed: step_index as u64,
            step_index,
            static_resource_context: None,
        }
    }

    fn end_turn_response(text: &str) -> ModelResponse {
        ModelResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    fn tool_use_response(name: &str, call_id: &str, params: Value) -> ModelResponse {
        ModelResponse {
            content: None,
            tool_calls: vec![MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
                tool_name: name.to_string(),
                parameters: params,
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }
    }

    #[test]
    fn step_zero_seeds_conversation_and_asks_for_model() {
        let h = Harness::new();
        let out = h.step(step_input(vec![], None, 0)).unwrap();
        match out.next_action {
            NextAction::CallModel(req) => {
                assert_eq!(req.model, "claude-haiku");
                assert_eq!(req.messages.len(), 2, "system + user");
                assert!(matches!(req.messages[0].role, MessageRole::System));
                assert!(matches!(req.messages[1].role, MessageRole::User));
                assert_eq!(req.messages[1].content.as_deref(), Some("hello"));
            }
            other => panic!("expected CallModel, got {other:?}"),
        }
        assert!(!out.state.is_empty());
    }

    #[test]
    fn end_turn_response_completes_invocation() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(end_turn_response("done."))),
                1,
            ))
            .unwrap();
        match s1.next_action {
            NextAction::Complete(text) => assert_eq!(text, "done."),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_response_dispatches_then_continues() {
        let h = Harness::new();

        // Step 0: seed → ask model.
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        // Step 1: model returns a tool call → reducer asks for tool dispatch.
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(tool_use_response(
                    "echo",
                    "call_1",
                    json!({"x": 1}),
                ))),
                1,
            ))
            .unwrap();
        let call = match s1.next_action {
            NextAction::CallTool(c) => c,
            other => panic!("expected CallTool, got {other:?}"),
        };
        assert_eq!(call.tool_name, "echo");

        // Step 2: feed back the tool result → reducer asks for the next model turn,
        // and the conversation history now includes the tool message.
        let s2 = h
            .step(step_input(
                s1.state,
                Some(CapabilityResult::ToolResult(ToolCallResult {
                    tool_call_id: crate::events::ToolCallId::new("call_1").unwrap(),
                    output: "echoed".to_string(),
                    is_error: false,
                    error_kind: None,
                    duration_ms: 1,
                })),
                2,
            ))
            .unwrap();
        match s2.next_action {
            NextAction::CallModel(req) => {
                let tool_msgs = req
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, MessageRole::Tool))
                    .count();
                assert_eq!(tool_msgs, 1);
                let assistant_msgs = req
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, MessageRole::Assistant))
                    .count();
                assert_eq!(assistant_msgs, 1);
            }
            other => panic!("expected CallModel after tool result, got {other:?}"),
        }
    }

    #[test]
    fn parallel_tool_calls_dispatch_in_parallel() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();

        let two_call = ModelResponse {
            content: None,
            tool_calls: vec![
                MessageToolCall {
                    tool_call_id: crate::events::ToolCallId::new("a").unwrap(),
                    tool_name: "echo".to_string(),
                    parameters: json!({}),
                },
                MessageToolCall {
                    tool_call_id: crate::events::ToolCallId::new("b").unwrap(),
                    tool_name: "echo".to_string(),
                    parameters: json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        };

        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(two_call)),
                1,
            ))
            .unwrap();
        match s1.next_action {
            NextAction::CallToolsParallel(calls) => assert_eq!(calls.len(), 2),
            other => panic!("expected CallToolsParallel, got {other:?}"),
        }
    }

    #[test]
    fn max_iterations_zero_is_a_stop_signal() {
        // `max_iterations = 0` means "no LLM turns allowed". After
        // the harness handles a model response and is about to
        // dispatch a tool turn, the iteration counter is 1 which
        // is already past the 0 cap — the loop terminates with
        // `MaxIterations`. This pins the behaviour against
        // accidental regressions back to the old "0 means
        // default" sentinel pattern.
        let h = Harness::new();

        let mut cfg = config();
        cfg.max_iterations = 0;
        let trig = trigger(json!("loop"));

        let mk = |state, last, idx| StepInput {
            config: cfg.clone(),
            trigger: trig.clone(),
            state,
            last_result: last,
            now_ms: idx as u64,
            random_seed: idx as u64,
            step_index: idx,
            static_resource_context: None,
        };

        let s0 = h.step(mk(vec![], None, 0)).unwrap();
        let s1 = h
            .step(mk(
                s0.state,
                Some(CapabilityResult::ModelResult(tool_use_response(
                    "echo",
                    "c1",
                    json!({}),
                ))),
                1,
            ))
            .unwrap();
        match s1.next_action {
            NextAction::Failed(err) => assert_eq!(err.kind, HarnessErrorKind::MaxIterations),
            other => panic!("expected Failed(MaxIterations) for stop signal, got {other:?}"),
        }
    }

    #[test]
    fn max_iterations_yields_failed() {
        // Configure max_iterations = 1 and have the model loop on tools.
        let h = Harness::new();

        let mut cfg = config();
        cfg.max_iterations = 1;
        let trig = trigger(json!("loop"));

        let mk = |state, last, idx| StepInput {
            config: cfg.clone(),
            trigger: trig.clone(),
            state,
            last_result: last,
            now_ms: idx as u64,
            random_seed: idx as u64,
            step_index: idx,
            static_resource_context: None,
        };

        let s0 = h.step(mk(vec![], None, 0)).unwrap();
        let s1 = h
            .step(mk(
                s0.state,
                Some(CapabilityResult::ModelResult(tool_use_response(
                    "echo",
                    "c1",
                    json!({}),
                ))),
                1,
            ))
            .unwrap();
        match s1.next_action {
            NextAction::Failed(err) => assert_eq!(err.kind, HarnessErrorKind::MaxIterations),
            other => panic!("expected Failed(MaxIterations), got {other:?}"),
        }
    }

    #[test]
    fn state_round_trips_across_drop_and_resume() {
        // The crux of the suspend/resume claim: drop the reducer
        // mid-flight, recreate it, feed in the persisted state,
        // and continue with no observable difference.
        //
        // Implemented via the shared `ManualStepper` helper, which
        // is the same primitive crash-simulation tests will use
        // once the WAL lands.
        use crate::test_support::stepper::ManualStepper;

        let mut s1 = ManualStepper::new(Harness::new(), config(), trigger(json!("hello")));
        let _ = s1.step(None);
        let _ = s1.step(Some(CapabilityResult::ModelResult(tool_use_response(
            "echo",
            "call_1",
            json!({}),
        ))));

        let snapshot = s1.snapshot_state();
        let saved_index = s1.step_index();
        drop(s1);

        let mut s2 = ManualStepper::new(Harness::new(), config(), trigger(json!("hello")));
        s2.restore_state(snapshot);
        s2.set_step_index(saved_index);

        let _ = s2.step(Some(CapabilityResult::ToolResult(ToolCallResult {
            tool_call_id: crate::events::ToolCallId::new("call_1").unwrap(),
            output: "echoed".to_string(),
            is_error: false,
            error_kind: None,
            duration_ms: 1,
        })));
        let final_step = s2.step(Some(CapabilityResult::ModelResult(end_turn_response(
            "after-resume.",
        ))));

        match final_step.next_action {
            NextAction::Complete(text) => assert_eq!(text, "after-resume."),
            other => panic!("expected Complete after resume, got {other:?}"),
        }
    }

    // --- R7: state-blob integrity (reducer verification plan, slice 2)

    /// Drive the machine to each mid-flight phase and return the
    /// persisted blob. Uses a high iteration cap so property walks
    /// never trip the max-iterations terminal mid-construction.
    fn state_after_steps(tool_turns: usize, end_in_dispatch: bool) -> Vec<u8> {
        let mk = |state, last_result, step_index: u32| {
            let mut cfg = config();
            cfg.max_iterations = 1_000;
            StepInput {
                config: cfg,
                trigger: trigger(json!("hello")),
                state,
                last_result,
                now_ms: 1_000_000 + step_index as u64,
                random_seed: step_index as u64,
                step_index,
                static_resource_context: None,
            }
        };
        let h = Harness::new();
        let mut state = h.step(mk(vec![], None, 0)).unwrap().state;
        let mut idx = 1u32;
        for turn in 0..tool_turns {
            state = h
                .step(mk(
                    state,
                    Some(CapabilityResult::ModelResult(tool_use_response(
                        "echo",
                        &format!("c{turn}"),
                        json!({}),
                    ))),
                    idx,
                ))
                .unwrap()
                .state;
            idx += 1;
            if end_in_dispatch && turn == tool_turns - 1 {
                return state;
            }
            state = h
                .step(mk(
                    state,
                    Some(CapabilityResult::ToolResult(ToolCallResult {
                        tool_call_id: crate::events::ToolCallId::new(format!("c{turn}")).unwrap(),
                        output: "ok".to_string(),
                        is_error: false,
                        error_kind: None,
                        duration_ms: 1,
                    })),
                    idx,
                ))
                .unwrap()
                .state;
            idx += 1;
        }
        state
    }

    #[test]
    fn valid_states_round_trip_byte_identically() {
        for blob in [
            state_after_steps(0, false), // AwaitingModel, post-seed
            state_after_steps(2, false), // AwaitingModel, post-tools
            state_after_steps(1, true),  // DispatchingTools
        ] {
            let state = HarnessState::load(&blob).expect("valid state loads");
            assert_eq!(state.save().expect("valid state saves"), blob);
        }
    }

    #[test]
    fn unknown_fields_in_persisted_state_are_tolerated() {
        // Schema evolution: a blob written by a future version with an
        // extra field must still load (compaction will rely on this).
        let blob = state_after_steps(1, false);
        let mut value: serde_json::Value = serde_json::from_slice(&blob).unwrap();
        value["future_field"] = json!("from a newer version");
        let bytes = serde_json::to_vec(&value).unwrap();
        HarnessState::load(&bytes).expect("unknown fields tolerated");
    }

    /// Corrupt a valid blob by rewriting its phase, and expect load to
    /// name the invariant instead of continuing on nonsense state.
    #[test]
    fn load_rejects_phase_contradicting_contents() {
        let cases = [
            // AwaitingModel blob relabelled as fresh.
            (state_after_steps(0, false), "initial", "Initial requires"),
            // AwaitingModel blob (last message is a tool result)
            // relabelled as dispatching.
            (
                state_after_steps(1, false),
                "dispatching_tools",
                "assistant message",
            ),
            // DispatchingTools blob (last message is an assistant
            // tool call) relabelled as awaiting the model.
            (
                state_after_steps(1, true),
                "awaiting_model",
                "cannot follow an assistant message",
            ),
        ];
        for (blob, phase, expected) in cases {
            let mut value: serde_json::Value = serde_json::from_slice(&blob).unwrap();
            value["phase"] = json!(phase);
            let bytes = serde_json::to_vec(&value).unwrap();
            let err = HarnessState::load(&bytes).expect_err("corrupt state must not load");
            assert_eq!(err.kind, HarnessErrorKind::InternalError);
            assert!(
                err.message.contains(expected),
                "expected violation naming '{expected}', got: {}",
                err.message
            );
        }
    }

    #[test]
    fn load_rejects_iteration_below_assistant_count() {
        let blob = state_after_steps(2, false);
        let mut value: serde_json::Value = serde_json::from_slice(&blob).unwrap();
        value["iteration"] = json!(0);
        let bytes = serde_json::to_vec(&value).unwrap();
        let err = HarnessState::load(&bytes).expect_err("must reject");
        assert!(err.message.contains("assistant message"), "{}", err.message);
    }

    #[test]
    fn save_rejects_inconsistent_in_memory_state() {
        let state = HarnessState {
            phase: Phase::DispatchingTools,
            messages: vec![],
            iteration: 0,
        };
        let err = state.save().expect_err("must reject on save");
        assert_eq!(err.kind, HarnessErrorKind::InternalError);
    }

    proptest::proptest! {
        /// Every state the real machine can persist validates and
        /// round-trips byte-identically, for arbitrary interaction
        /// scripts (a random mix of tool turns ending mid-dispatch,
        /// awaiting the model, or terminal).
        #[test]
        fn machine_generated_states_always_validate(
            turns in 0usize..6,
            end_in_dispatch: bool,
            finish: bool,
        ) {
            let blob = state_after_steps(turns.max(usize::from(end_in_dispatch)), end_in_dispatch);
            let state = HarnessState::load(&blob).expect("machine state loads");
            proptest::prop_assert_eq!(state.save().expect("machine state saves"), blob.clone());

            if finish && !end_in_dispatch {
                // Drive to Done and check the terminal blob too.
                let h = Harness::new();
                let out = h
                    .step(step_input(
                        blob,
                        Some(CapabilityResult::ModelResult(end_turn_response("done."))),
                        99,
                    ))
                    .expect("terminal step");
                HarnessState::load(&out.state).expect("terminal state loads");
            }
        }

        /// Relabelling a mid-flight state with a random *different*
        /// phase is always caught by load — no phase confusion can
        /// slip through the boundary silently.
        #[test]
        fn phase_relabelling_never_loads(
            turns in 1usize..4,
            end_in_dispatch: bool,
            wrong_phase in proptest::sample::select(vec![
                "initial", "awaiting_model", "dispatching_tools",
            ]),
        ) {
            let blob = state_after_steps(turns, end_in_dispatch);
            let mut value: serde_json::Value = serde_json::from_slice(&blob).unwrap();
            let current = value["phase"].as_str().unwrap().to_string();
            proptest::prop_assume!(current != wrong_phase);
            value["phase"] = json!(wrong_phase);
            let bytes = serde_json::to_vec(&value).unwrap();
            proptest::prop_assert!(HarnessState::load(&bytes).is_err());
        }
    }

    #[test]
    fn pure_step_is_deterministic_for_equal_inputs() {
        let h = Harness::new();
        let inp = step_input(vec![], None, 0);
        let a = h.step(inp.clone()).unwrap();
        let b = h.step(inp).unwrap();
        // Pure function of inputs: structural equality of state + action.
        assert_eq!(a.state, b.state);
        match (&a.next_action, &b.next_action) {
            (NextAction::CallModel(r1), NextAction::CallModel(r2)) => {
                assert_eq!(r1.model, r2.model);
                assert_eq!(r1.messages.len(), r2.messages.len());
            }
            _ => panic!("non-CallModel or mismatched actions"),
        }
    }
}
