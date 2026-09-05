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
    ModelRequest, NextAction, Reducer, StepInput, StepOutput, ToolCallRequest, ToolCallResult,
    TriggerPayload,
};
use crate::events::{AssistantPart, Message, RequestParams, TaskStatus, ToolCallId, ToolResult};

/// Built-in fallback cap on LLM turns per invocation — a backstop
/// against a wedged agent, distinct from and well below the host's
/// `HOST_STEP_BUDGET` (1000). Raised from 20 (2026-07-06): 20 turns is
/// too few for a complex autonomous task — the M0 loop's first code task
/// (`fq reload`) exhausted it mid-implementation, before it could commit.
///
/// Since issue #9, `max_iterations` is configuration (Design Principle
/// 8): the effective cap is a per-agent definition override, else the
/// daemon config default ([`crate::config::Config::max_iterations`]),
/// else this constant — which is what the config default falls back to
/// when `fqd.toml` says nothing. It stays here so a runner built with no
/// explicit default (most tests) still gets a sane bound, itself
/// bounded in the large by the dollar budget and the host step budget.
pub const DEFAULT_MAX_ITERATIONS: u32 = 100;

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

/// Validate a persisted state blob without exposing the state type:
/// deserialises and runs the phase ↔ contents invariants. Used by the
/// verification soak (slice 7) to check every archived blob in volume.
/// Is this tool name the `report_outcome` declaration (#125)? Matches
/// the canonical registered name and, for one release alongside the
/// #177 legacy-grant mapping, the bare form.
fn is_report_outcome(name: &str) -> bool {
    name == crate::tools::REPORT_OUTCOME_CANONICAL_NAME
        || name == fq_tools::builtin::REPORT_OUTCOME_TOOL_NAME
}

#[cfg(test)]
pub(crate) fn validate_state_blob(bytes: &[u8]) -> Result<(), HarnessError> {
    HarnessState::load(bytes).map(|_| ())
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
                Some(Message::Assistant { .. }) => {
                    Some("phase AwaitingModel cannot follow an assistant message".to_string())
                }
                Some(_) => None,
            },
            Phase::DispatchingTools => match self.messages.last() {
                Some(Message::Assistant { parts }) => parts
                    .iter()
                    .all(|part| !matches!(part, AssistantPart::ToolCall(_)))
                    .then(|| {
                        "phase DispatchingTools requires the last assistant message to carry \
                         tool calls"
                            .to_string()
                    }),
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
                .filter(|m| matches!(m, Message::Assistant { .. }))
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

    state.messages.push(Message::System {
        text: input.config.system_prompt.clone(),
    });
    // Host-curated `static_resources` content, injected once right
    // after the system prompt and before the trigger. The runner
    // read the pins at invocation start (the reducer does no I/O);
    // `None` here means no pins, or this is a resumed invocation
    // where the content already lives in the persisted history.
    if let Some(context) = &input.static_resource_context {
        state.messages.push(Message::User {
            text: context.clone(),
        });
    }
    state.messages.push(Message::User {
        text: payload_to_user_message(&input.trigger),
    });

    append_host_notices(state, &input.host_notices);

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
    // Notices carried at this boundary fold in *before* the assistant
    // message: a tool-carrying response must keep its tool calls
    // adjacent to the tool results that answer them (providers reject
    // content between the two). The model first sees the notice in the
    // next request either way.
    append_host_notices(state, &input.host_notices);

    // The explicit, status-bearing terminal (#125): a turn that calls
    // `report_outcome` ends the invocation with the declared status —
    // a pure mapping to the terminal transition (ADR-0014), never a
    // dispatch. The declaration wins over any sibling tool calls in
    // the same turn (no post-declaration execution, applied within the
    // turn). An unparseable declaration (unknown status, non-object
    // args) is NOT terminal: it falls through to normal dispatch,
    // where the schema-only tool's execute() error teaches the model
    // to correct the call — a malformed declaration must never
    // mis-stamp a terminal status.
    if let Some(declared) = response.tool_calls().find_map(|call| {
        if !is_report_outcome(&call.tool_name) {
            return None;
        }
        let status = call.parameters.get("status")?.as_str()?;
        let status = TaskStatus::parse(status)?;
        let summary = call
            .parameters
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Some((status, summary))
    }) {
        let (task_status, summary) = declared;
        return terminal(
            state,
            NextAction::Complete {
                text: summary,
                task_status,
            },
        );
    }

    // The replay path. Every part the model produced is carried into the
    // conversation the next turn re-sends — which is what makes reasoning
    // round-trip once phase 3 starts producing reasoning parts. Nothing
    // here needs to know about reasoning specifically; it carries whatever
    // the turn contained.
    state.messages.push(Message::Assistant {
        parts: response.parts.clone(),
    });

    // Bare text is not a stop signal. Persist a host notice in the
    // transcript so replay produces the same corrective follow-up.
    if response.tool_calls().next().is_none() {
        append_host_notices(
            state,
            &["No tool calls were made and the run is not over — continue working, or end it by calling `report_outcome` with success, failed, blocked, or partial.".to_string()],
        );
    }

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

    if response.tool_calls().next().is_none() {
        let request = build_model_request(&input.config, &state.messages);
        state.phase = Phase::AwaitingModel;
        return Ok(StepOutput {
            next_action: NextAction::CallModel(request),
            state: state.save()?,
            logs: vec![log_info(
                "model made no tool calls; requesting another turn",
            )],
            events: vec![],
        });
    }

    let pending: Vec<ToolCallRequest> = response
        .tool_calls()
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

    // One turn answers one assistant turn (ADR-0034 D1b): every result of
    // the round rides in a single `Message::ToolResults`, in the order of
    // the calls that asked for them. That is Anthropic's documented shape
    // — one user message carrying N `tool_result` blocks — and the
    // OpenAI-compatible adapter unfolds the same message into its N
    // `tool` messages, so the reducer holds one shape and each adapter
    // renders its own (#511).
    let results = answer_in_call_order(state, results)?;
    state.messages.push(Message::ToolResults {
        results: results
            .into_iter()
            .map(|result| ToolResult {
                tool_call_id: result.tool_call_id,
                output: result.output,
                is_error: result.is_error,
            })
            .collect(),
    });

    // Notices carried at this boundary land after the tool results,
    // immediately before the request they are first seen in.
    append_host_notices(state, &input.host_notices);

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

/// Put the host's answer in the order of the calls the last assistant
/// turn made, and refuse an answer that does not match those calls.
///
/// The protocol says the host returns results in request order, and the
/// sequential host does. The reducer still orders them itself, from the
/// assistant message it recorded, so a concurrent host — or a resume
/// that regrouped WAL rows — cannot reorder the wire: a provider
/// verifies each `tool_result` against the `tool_use` it answers, and
/// the reducer is the only party holding both sides.
///
/// A result for a call the turn never made, or a call left without a
/// result, is a host protocol breach of the same class as answering
/// `CallModel` with a tool result, and fails the same way — rather than
/// going on to a provider that would reject the request with less
/// context.
fn answer_in_call_order(
    state: &HarnessState,
    mut results: Vec<ToolCallResult>,
) -> Result<Vec<ToolCallResult>, HarnessError> {
    let calls: Vec<&ToolCallId> = match state.messages.last() {
        Some(Message::Assistant { parts }) => parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(&call.tool_call_id),
                _ => None,
            })
            .collect(),
        _ => {
            return Err(internal_error(
                "tool results arrived with no assistant turn to answer",
            ));
        }
    };

    let mut called: Vec<&str> = calls.iter().map(|id| id.as_str()).collect();
    let mut answered: Vec<&str> = results.iter().map(|r| r.tool_call_id.as_str()).collect();
    called.sort_unstable();
    answered.sort_unstable();
    if called != answered {
        return Err(internal_error(&format!(
            "tool results do not answer the turn's calls: called {called:?}, answered {answered:?}"
        )));
    }

    // Stable, so two results for one repeated id keep their delivery order.
    results.sort_by_key(|result| calls.iter().position(|id| *id == &result.tool_call_id));
    Ok(results)
}

fn append_host_notices(state: &mut HarnessState, notices: &[String]) {
    for body in notices {
        state.messages.push(Message::User { text: body.clone() });
    }
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
            effort: config.effort,
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

#[cfg(test)]
mod tests {
    //! Unit tests for the reducer itself. These need no I/O,
    //! no async runtime, no event bus — they exercise the pure
    //! `step()` function directly.

    use super::*;
    use crate::agent::AgentId;
    use crate::events::{MessageToolCall, StopReason, TokenUsage, ToolSchema};
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
            effort: None,
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
            host_notices: vec![],
        }
    }

    fn end_turn_response(text: &str) -> ModelResponse {
        ModelResponse {
            parts: vec![AssistantPart::Text {
                text: text.to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    /// **The acceptance test for #437.** A reasoning-first model carries
    /// the substance of a turn in its reasoning, not its visible text.
    /// The reducer replays the whole conversation on every step, so if
    /// the assistant turn it stores drops reasoning, turn N+1 re-derives
    /// from a weaker base than the provider's contract assumes — silently,
    /// and worse the longer the loop runs.
    ///
    /// This drives two real turns through the harness and asserts the
    /// reasoning survives into the request for the second. Paired with
    /// `llm::genai`'s `reasoning_reaches_the_provider_on_the_next_turn`,
    /// which takes it from there to the wire, that is the full round trip
    /// the issue asks for.
    #[test]
    fn reasoning_survives_into_the_next_turn_of_the_conversation() {
        let h = Harness::new();
        let s0 = h.step(step_input(Vec::new(), None, 0)).unwrap();

        // Turn 1: the model reasons, then calls a tool.
        let turn_one = ModelResponse {
            parts: vec![
                AssistantPart::Reasoning(crate::events::Reasoning {
                    model: "kimi-k2".to_string(),
                    content: crate::events::ReasoningContent::Plain {
                        text: "The runbook is the cheapest place to start.".to_string(),
                    },
                }),
                AssistantPart::Text {
                    text: "Reading the runbook.".to_string(),
                },
                AssistantPart::ToolCall(crate::events::MessageToolCall {
                    tool_call_id: crate::events::ToolCallId::new("c1").unwrap(),
                    tool_name: "builtin__exec".to_string(),
                    parameters: json!({"command": ["true"]}),
                }),
            ],
            stop_reason: crate::events::StopReason::ToolUse,
            usage: crate::events::TokenUsage::default(),
        };
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(turn_one)),
                1,
            ))
            .unwrap();
        assert!(matches!(s1.next_action, NextAction::CallTool(_)));

        // The tool answers, and the reducer asks for turn 2.
        let s2 = h
            .step(step_input(
                s1.state,
                Some(CapabilityResult::ToolResult(ToolCallResult {
                    tool_call_id: crate::events::ToolCallId::new("c1").unwrap(),
                    output: "ok".to_string(),
                    is_error: false,
                    error_kind: None,
                    duration_ms: 0,
                })),
                2,
            ))
            .unwrap();
        let NextAction::CallModel(request) = s2.next_action else {
            panic!("expected another model turn after the tool result");
        };

        let replayed: Vec<&crate::events::Reasoning> = request
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { parts } => Some(parts),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                AssistantPart::Reasoning(r) => Some(r),
                _ => None,
            })
            .collect();

        assert_eq!(
            replayed.len(),
            1,
            "the model's own reasoning must be replayed to it on the next turn"
        );
        assert!(matches!(
            &replayed[0].content,
            crate::events::ReasoningContent::Plain { text }
                if text == "The runbook is the cheapest place to start."
        ));
        assert_eq!(
            replayed[0].model, "kimi-k2",
            "the part stays tagged, so a later cross-model edge can strip it"
        );

        // The rest of the turn is intact alongside it — this is addition,
        // not replacement.
        let assistant = request
            .messages
            .iter()
            .find(|m| matches!(m, Message::Assistant { .. }))
            .expect("the assistant turn is in the conversation");
        assert_eq!(assistant.text().as_deref(), Some("Reading the runbook."));
    }

    fn tool_use_response(name: &str, call_id: &str, params: Value) -> ModelResponse {
        ModelResponse {
            parts: crate::events::assistant_parts(
                None,
                vec![MessageToolCall {
                    tool_call_id: crate::events::ToolCallId::new(call_id).unwrap(),
                    tool_name: name.to_string(),
                    parameters: params,
                }],
            ),
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
                assert!(matches!(req.messages[0], Message::System { .. }));
                assert!(matches!(req.messages[1], Message::User { .. }));
                assert_eq!(req.messages[1].text().as_deref(), Some("hello"));
            }
            other => panic!("expected CallModel, got {other:?}"),
        }
        assert!(!out.state.is_empty());
    }

    #[test]
    fn end_turn_response_is_nudged_until_outcome_is_reported() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(end_turn_response("done."))),
                1,
            ))
            .unwrap();
        match &s1.next_action {
            NextAction::CallModel(request) => {
                assert!(matches!(
                    request.messages.last().unwrap(),
                    Message::User { .. }
                ));
                assert!(
                    request
                        .messages
                        .last()
                        .unwrap()
                        .text()
                        .as_deref()
                        .unwrap()
                        .contains("run is not over")
                );
            }
            other => panic!("expected corrective CallModel, got {other:?}"),
        }

        let s2 = h
            .step(step_input(
                s1.state,
                Some(CapabilityResult::ModelResult(tool_use_response(
                    crate::tools::REPORT_OUTCOME_CANONICAL_NAME,
                    "c1",
                    serde_json::json!({"status": "success", "summary": "done."}),
                ))),
                2,
            ))
            .unwrap();
        assert!(matches!(
            s2.next_action,
            NextAction::Complete {
                task_status: TaskStatus::Success,
                ..
            }
        ));
    }

    /// #125: a turn that calls `report_outcome` is terminal with the
    /// declared status — the only model-driven terminal.
    #[test]
    fn report_outcome_call_is_terminal_with_declared_status() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(tool_use_response(
                    crate::tools::REPORT_OUTCOME_CANONICAL_NAME,
                    "c1",
                    serde_json::json!({"status": "blocked", "summary": "CI is red and I cannot see why"}),
                ))),
                1,
            ))
            .unwrap();
        match s1.next_action {
            NextAction::Complete { text, task_status } => {
                assert_eq!(task_status, TaskStatus::Blocked);
                assert_eq!(text, "CI is red and I cannot see why");
            }
            other => panic!("expected terminal Complete, got {other:?}"),
        }
    }

    /// #125: the declaration wins over sibling tool calls in the same
    /// turn — no post-declaration execution, applied within the turn.
    #[test]
    fn report_outcome_wins_over_same_turn_siblings() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        let mut response = tool_use_response(
            "builtin__exec",
            "c1",
            serde_json::json!({"command": ["true"]}),
        );
        response
            .parts
            .push(AssistantPart::ToolCall(MessageToolCall {
                tool_call_id: crate::events::ToolCallId::new("c2").unwrap(),
                tool_name: crate::tools::REPORT_OUTCOME_CANONICAL_NAME.to_string(),
                parameters: serde_json::json!({"status": "failed", "summary": "gave up"}),
            }));
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(response)),
                1,
            ))
            .unwrap();
        match s1.next_action {
            NextAction::Complete { task_status, .. } => {
                assert_eq!(task_status, TaskStatus::Failed);
            }
            other => panic!("declaration must win over siblings, got {other:?}"),
        }
    }

    /// #125: an unparseable declaration is NOT terminal — it falls
    /// through to normal dispatch, where the schema-only tool's
    /// execute() error teaches the model. A malformed call must never
    /// mis-stamp a terminal status.
    #[test]
    fn malformed_report_outcome_falls_through_to_dispatch() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(tool_use_response(
                    crate::tools::REPORT_OUTCOME_CANONICAL_NAME,
                    "c1",
                    serde_json::json!({"status": "shrug"}),
                ))),
                1,
            ))
            .unwrap();
        match s1.next_action {
            NextAction::CallTool(req) => {
                assert_eq!(req.tool_name, crate::tools::REPORT_OUTCOME_CANONICAL_NAME);
            }
            NextAction::CallToolsParallel(reqs) => {
                assert_eq!(reqs.len(), 1);
            }
            other => panic!("malformed declaration must dispatch, got {other:?}"),
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
                    .filter(|m| matches!(m, Message::ToolResults { .. }))
                    .count();
                assert_eq!(tool_msgs, 1);
                let assistant_msgs = req
                    .messages
                    .iter()
                    .filter(|m| matches!(m, Message::Assistant { .. }))
                    .count();
                assert_eq!(assistant_msgs, 1);
            }
            other => panic!("expected CallModel after tool result, got {other:?}"),
        }
    }

    /// Step-0 notices fold exactly once (regression: an early draft
    /// appended them twice), in queue order, after the trigger and
    /// before the first request.
    #[test]
    fn host_notices_fold_once_after_the_trigger() {
        let h = Harness::new();
        let mut input = step_input(vec![], None, 0);
        input.host_notices = vec![
            "<host-notice>first</host-notice>".to_string(),
            "<host-notice>second</host-notice>".to_string(),
        ];
        let out = h.step(input).unwrap();
        match out.next_action {
            NextAction::CallModel(req) => {
                let contents: Vec<Option<String>> = req.messages.iter().map(|m| m.text()).collect();
                assert_eq!(
                    contents,
                    vec![
                        Some("You are a test agent.".to_string()),
                        Some("hello".to_string()),
                        Some("<host-notice>first</host-notice>".to_string()),
                        Some("<host-notice>second</host-notice>".to_string()),
                    ],
                    "each notice folds exactly once, in order, after the trigger"
                );
                assert!(
                    req.messages[2..]
                        .iter()
                        .all(|m| matches!(m, Message::User { .. })),
                    "notices are user-role messages"
                );
            }
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    /// A notice carried at a model-response boundary folds in before
    /// the assistant message, keeping the assistant's tool calls
    /// adjacent to the tool results that answer them.
    #[test]
    fn host_notices_on_model_response_precede_the_assistant_message() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();

        let mut s1_input = step_input(
            s0.state,
            Some(CapabilityResult::ModelResult(tool_use_response(
                "echo",
                "call_1",
                json!({}),
            ))),
            1,
        );
        s1_input.host_notices = vec!["<host-notice>mid</host-notice>".to_string()];
        let s1 = h.step(s1_input).unwrap();
        assert!(matches!(s1.next_action, NextAction::CallTool(_)));

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
                let roles: Vec<&str> = req
                    .messages
                    .iter()
                    .map(|m| match m {
                        Message::System { .. } => "system",
                        Message::User { .. } => "user",
                        Message::Assistant { .. } => "assistant",
                        Message::ToolResults { .. } => "tool",
                    })
                    .collect();
                assert_eq!(
                    roles,
                    vec!["system", "user", "user", "assistant", "tool"],
                    "notice sits before the assistant message; \
                     tool call and result stay adjacent"
                );
                assert_eq!(
                    req.messages[2].text().as_deref(),
                    Some("<host-notice>mid</host-notice>")
                );
            }
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    /// A notice carried at a tool-results boundary lands after the
    /// tool results, immediately before the request it is first seen
    /// in (regression: an early draft dropped it entirely).
    #[test]
    fn host_notices_on_tool_results_precede_the_request() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(tool_use_response(
                    "echo",
                    "call_1",
                    json!({}),
                ))),
                1,
            ))
            .unwrap();

        let mut s2_input = step_input(
            s1.state,
            Some(CapabilityResult::ToolResult(ToolCallResult {
                tool_call_id: crate::events::ToolCallId::new("call_1").unwrap(),
                output: "echoed".to_string(),
                is_error: false,
                error_kind: None,
                duration_ms: 1,
            })),
            2,
        );
        s2_input.host_notices = vec!["<host-notice>late</host-notice>".to_string()];
        let s2 = h.step(s2_input).unwrap();
        match s2.next_action {
            NextAction::CallModel(req) => {
                let last = req.messages.last().expect("non-empty request");
                assert!(matches!(last, Message::User { .. }));
                assert_eq!(
                    last.text().as_deref(),
                    Some("<host-notice>late</host-notice>"),
                    "notice is the final message before the request"
                );
                assert!(
                    matches!(
                        req.messages[req.messages.len() - 2],
                        Message::ToolResults { .. }
                    ),
                    "notice follows the tool results"
                );
            }
            other => panic!("expected CallModel, got {other:?}"),
        }
    }

    #[test]
    fn parallel_tool_calls_dispatch_in_parallel() {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();

        let two_call = ModelResponse {
            parts: crate::events::assistant_parts(
                None,
                vec![
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
            ),
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

    // --- #511: one turn answers one assistant turn

    /// A model turn calling `ids`, in that order, all on the `echo` tool.
    fn parallel_call_response(ids: &[&str]) -> ModelResponse {
        ModelResponse {
            parts: crate::events::assistant_parts(
                None,
                ids.iter()
                    .map(|id| MessageToolCall {
                        tool_call_id: crate::events::ToolCallId::new(*id).unwrap(),
                        tool_name: "echo".to_string(),
                        parameters: json!({"call": id}),
                    })
                    .collect(),
            ),
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }
    }

    fn tool_result(id: &str, output: &str) -> ToolCallResult {
        ToolCallResult {
            tool_call_id: crate::events::ToolCallId::new(id).unwrap(),
            output: output.to_string(),
            is_error: false,
            error_kind: None,
            duration_ms: 1,
        }
    }

    /// Drive one round: seed, a turn calling `ids`, the host's `answer`,
    /// and return the request the reducer builds for the next turn — or
    /// the error it raised integrating the answer.
    fn request_after_parallel_round(
        ids: &[&str],
        answer: Vec<ToolCallResult>,
    ) -> Result<ModelRequest, HarnessError> {
        let h = Harness::new();
        let s0 = h.step(step_input(vec![], None, 0)).unwrap();
        let s1 = h
            .step(step_input(
                s0.state,
                Some(CapabilityResult::ModelResult(parallel_call_response(ids))),
                1,
            ))
            .unwrap();
        assert!(
            matches!(
                s1.next_action,
                NextAction::CallTool(_) | NextAction::CallToolsParallel(_)
            ),
            "precondition: a tool-calling turn dispatches"
        );
        let s2 = h.step(step_input(
            s1.state,
            Some(CapabilityResult::ParallelToolResults(answer)),
            2,
        ))?;
        match s2.next_action {
            NextAction::CallModel(request) => Ok(request),
            other => panic!("expected the next model turn, got {other:?}"),
        }
    }

    /// The tool-results turns in a request: one entry per
    /// `Message::ToolResults`, each the `(call id, output)` pairs it
    /// carries in order.
    fn tool_turns(request: &ModelRequest) -> Vec<Vec<(String, String)>> {
        request
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResults { results } => Some(
                    results
                        .iter()
                        .map(|r| (r.tool_call_id.to_string(), r.output.clone()))
                        .collect(),
                ),
                _ => None,
            })
            .collect()
    }

    /// **The acceptance test for #511.** A turn that requests several
    /// tools is answered by *one* turn carrying every result, in the
    /// order of the calls — Anthropic's documented shape, and the one
    /// `Message::ToolResults` exists to express (ADR-0034 D1b). The host
    /// answers here in completion order, which is not call order: the
    /// sequential host happens to preserve it, a concurrent one would
    /// not, and the reducer must depend on neither.
    #[test]
    fn parallel_tool_results_are_one_turn_in_call_order() {
        let request = request_after_parallel_round(
            &["a", "b", "c"],
            vec![
                tool_result("c", "out-c"),
                tool_result("a", "out-a"),
                tool_result("b", "out-b"),
            ],
        )
        .expect("a complete answer is accepted");

        let pair = |id: &str, out: &str| (id.to_string(), out.to_string());
        assert_eq!(
            tool_turns(&request),
            vec![vec![
                pair("a", "out-a"),
                pair("b", "out-b"),
                pair("c", "out-c")
            ]],
            "one tool-results turn, ordered by the calls it answers"
        );
        // And it sits directly after the assistant turn that made the
        // calls — nothing may come between the two.
        let n = request.messages.len();
        assert!(matches!(request.messages[n - 2], Message::Assistant { .. }));
        assert!(matches!(
            request.messages[n - 1],
            Message::ToolResults { .. }
        ));
    }

    /// An answer that does not match the calls — a result for a call the
    /// turn never made, or a call left unanswered — is a host protocol
    /// breach. It is reported as the reducer's own internal error rather
    /// than sent on to a provider, which would reject the request with
    /// less context.
    #[test]
    fn tool_results_must_answer_the_turns_calls_exactly() {
        let stray = request_after_parallel_round(
            &["a", "b"],
            vec![tool_result("a", "x"), tool_result("zzz", "y")],
        )
        .expect_err("a result for a call the turn never made is refused");
        assert_eq!(stray.kind, HarnessErrorKind::InternalError);
        assert!(stray.message.contains("zzz"), "{}", stray.message);

        let short = request_after_parallel_round(&["a", "b"], vec![tool_result("a", "x")])
            .expect_err("an unanswered call is refused");
        assert_eq!(short.kind, HarnessErrorKind::InternalError);
        assert!(short.message.contains("\"b\""), "{}", short.message);
    }

    proptest::proptest! {
        /// For any number of parallel calls and any order the host
        /// completes them in, the reducer emits exactly one tool-results
        /// turn, in call order, with every result present once and its
        /// error flag intact.
        #[test]
        fn parallel_results_always_fold_to_one_ordered_turn(
            n in 1usize..=6,
            keys in proptest::collection::vec(proptest::num::u32::ANY, 6),
            errors in proptest::collection::vec(proptest::bool::ANY, 6),
        ) {
            let ids: Vec<String> = (0..n).map(|i| format!("call-{i}")).collect();
            let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            // The host's completion order: any permutation of the calls.
            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by_key(|&i| keys[i]);
            let answer: Vec<ToolCallResult> = order
                .iter()
                .map(|&i| {
                    let mut result = tool_result(&ids[i], &format!("out-{i}"));
                    result.is_error = errors[i];
                    result
                })
                .collect();

            let request = request_after_parallel_round(&id_refs, answer)
                .expect("a complete answer is accepted");
            let turns: Vec<&Vec<ToolResult>> = request
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::ToolResults { results } => Some(results),
                    _ => None,
                })
                .collect();
            proptest::prop_assert_eq!(turns.len(), 1, "exactly one tool-results turn");
            proptest::prop_assert_eq!(turns[0].len(), n, "every result, once");
            for (i, result) in turns[0].iter().enumerate() {
                proptest::prop_assert_eq!(result.tool_call_id.as_str(), ids[i].as_str());
                proptest::prop_assert_eq!(&result.output, &format!("out-{i}"));
                proptest::prop_assert_eq!(result.is_error, errors[i]);
            }
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
            host_notices: vec![],
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
            host_notices: vec![],
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
        let final_step = s2.step(Some(CapabilityResult::ModelResult(tool_use_response(
            crate::tools::REPORT_OUTCOME_CANONICAL_NAME,
            "final",
            json!({"status": "success", "summary": "after-resume."}),
        ))));

        match final_step.next_action {
            NextAction::Complete { text, .. } => assert_eq!(text, "after-resume."),
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
                host_notices: vec![],
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
