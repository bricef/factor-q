//! The Turn atom: one action in an invocation's conversation — the
//! opening prompt, an assistant output, or a tool result — as an
//! immutable, event-log-backed fact
//! (`docs/design/committed/operator-surface-domain-model.md`). The
//! transcript is a *rendering* composed over turns (plus the
//! invocation's outcome); the dependency runs that way, never the
//! reverse.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

// The atom itself is `fq_ops::turn`, re-exported so a caller reaches
// it by the same path as before. What stays is the fold: it reads the
// event vocabulary, which is this crate's.
pub use fq_ops::turn::*;

use crate::events::{Event, EventPayload, LlmCallOrigin, LlmRequestPayload, Message};
use crate::transcript::AssistantToolCall;

/// The fold from events to turns. Stateful across a window so a tool
/// result joins its initiating call (name, parameters, and the
/// assistant turn's sequence), and so an invocation's opening prompt
/// is yielded once; the initiating events always precede the result in
/// the log, so folding forward suffices.
#[derive(Default)]
pub struct TurnFold {
    calls: HashMap<String, PendingCall>,
    /// Invocations whose opening prompt this window has already
    /// yielded. An invocation has exactly one opening prompt, but it
    /// can publish more than one opening `llm.request`: `resume`
    /// replays only *completed* WAL rows, so a crash between the
    /// request publish and the response leaves nothing to replay and
    /// the reducer re-issues the same opening call. Keyed by
    /// invocation because one fold serves many — `list_turns` walks
    /// the whole agent subject.
    prompted: HashSet<String>,
}

struct PendingCall {
    tool_name: String,
    parameters: Value,
    assistant_seq: Option<u64>,
}

/// Is this request the invocation's *opening* one — the prompt the
/// agent was launched with, rather than a later round's replay of the
/// accumulated conversation?
///
/// Decided from the event's own content, never from the fold's
/// position in the log. A fold is built fresh per read window:
/// `turn.list` walks the whole agent subject (many invocations share
/// one fold) and `turn.stream` starts at the caller's cursor, usually
/// mid-invocation. A "the first one I saw" rule would answer
/// differently per window — crowning whichever invocation the window
/// opened on, and minting a bogus prompt out of round N's replayed
/// history for anyone joining a run in progress. Two intrinsic tests,
/// each stable under any window:
///
/// * The call is an agent turn. Server-initiated sampling and
///   elicitation calls, and the evaluator completions that gate them
///   (ADR-0018), also publish `llm.request`, and their message lists
///   are system+user shaped — they are not this invocation's prompt.
/// * The message list carries no assistant and no tool message. The
///   harness's opening request is exactly system + user
///   (`reducer::harness::initial_step`); every later round re-sends
///   the whole conversation, which by then contains the preceding
///   assistant turn.
fn is_opening_request(payload: &LlmRequestPayload) -> bool {
    matches!(payload.origin, LlmCallOrigin::AgentTurn)
        && !payload
            .messages
            .iter()
            .any(|m| matches!(m, Message::Assistant { .. } | Message::ToolResults { .. }))
}

impl TurnFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event at `seq`. Returns the turn it yields, if any —
    /// most events (dispatch middle states, lifecycle, heartbeats)
    /// yield none.
    pub fn apply(&mut self, seq: u64, event: &Event) -> Option<TurnState> {
        let envelope = &event.envelope;
        let base = |round, initiating_turn, action| TurnState {
            seq,
            invocation_id: envelope.invocation_id.to_string(),
            agent_id: envelope.agent_id.to_string(),
            round,
            timestamp_ms: envelope.timestamp.timestamp_millis(),
            initiating_turn,
            action,
        };
        match &event.payload {
            EventPayload::LlmRequest(p) if is_opening_request(p) => {
                // The prompt is not an action *within* a Round: it is
                // what the first Round answers. `round: 0` is the
                // ledger's own reading — `RoundLedger::current` is 0
                // until the first response advances it, and this event
                // is published before that (`runner::dispatch_llm`
                // publishes, then `rounds.next()` stamps the
                // response). `initiating_turn: None`: no turn caused
                // it — the trigger did, and a trigger is not a Turn.
                let prompt = crate::transcript::prompt_from_messages(
                    envelope.timestamp.timestamp_millis(),
                    &p.messages,
                )?;
                if !self.prompted.insert(envelope.invocation_id.to_string()) {
                    return None;
                }
                Some(base(
                    0,
                    None,
                    TurnAction::Prompt {
                        system: prompt.system,
                        user: prompt.user,
                    },
                ))
            }
            EventPayload::LlmResponse(p) => {
                for call in p.tool_calls() {
                    self.calls.insert(
                        call.tool_call_id.to_string(),
                        PendingCall {
                            tool_name: call.tool_name.clone(),
                            parameters: call.parameters.clone(),
                            assistant_seq: Some(seq),
                        },
                    );
                }
                let cost = envelope.cost.as_ref();
                Some(base(
                    p.round,
                    None,
                    TurnAction::Assistant {
                        model: cost
                            .map(|c| c.model.clone())
                            .unwrap_or_else(|| "?".to_string()),
                        content: p.text(),
                        tool_calls: p
                            .tool_calls()
                            .map(|c| AssistantToolCall {
                                tool_call_id: c.tool_call_id.to_string(),
                                tool_name: c.tool_name.clone(),
                                parameters: c.parameters.clone(),
                            })
                            .collect(),
                        cost_usd: cost.map(|c| c.total_cost),
                        // Not unknown — false. An `llm.response` event
                        // exists only for a call that returned a
                        // response; a call that did not publishes
                        // `llm.failure` instead, folded below. That is
                        // the same distinction the WAL-backed
                        // transcript drew with its `is_error` column,
                        // so the bridge's byte-identity contract needs
                        // the value, not a null standing in for it.
                        is_error: Some(false),
                    },
                ))
            }
            // The failure fold (#447). No new `TurnAction` variant:
            // `is_error` already exists and the transcript already
            // renders `" [error]"` from it — precisely the entry the
            // WAL-backed transcript produced and the Turn-backed one
            // lost when the read path flipped. The message is the
            // content, because on a failed call it is all there is.
            EventPayload::LlmFailure(p) => Some(base(
                p.round,
                None,
                TurnAction::Assistant {
                    model: p.model.clone(),
                    content: Some(p.error_message.clone()),
                    tool_calls: Vec::new(),
                    cost_usd: envelope.cost.as_ref().map(|c| c.total_cost),
                    is_error: Some(true),
                },
            )),
            EventPayload::ToolCall(p) => {
                // Enrich (or establish) the pending-call record; the
                // event itself is part of the assistant turn, not a
                // turn of its own.
                self.calls
                    .entry(p.tool_call_id.to_string())
                    .or_insert(PendingCall {
                        tool_name: p.tool_name.clone(),
                        parameters: p.parameters.clone(),
                        assistant_seq: None,
                    });
                None
            }
            EventPayload::ToolResult(p) => {
                let pending = self.calls.remove(&p.tool_call_id.to_string());
                let tool_name = pending
                    .as_ref()
                    .map(|c| c.tool_name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| p.tool_name.clone());
                Some(base(
                    p.round,
                    pending.as_ref().and_then(|c| c.assistant_seq),
                    TurnAction::ToolResult {
                        tool_call_id: p.tool_call_id.to_string(),
                        tool_name,
                        parameters: pending.map(|c| c.parameters).unwrap_or(Value::Null),
                        output: Some(p.output.clone()),
                        is_error: Some(p.is_error),
                    },
                ))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::events::{
        CostMetadata, LlmResponsePayload, Message, MessageToolCall, RequestParams, StopReason,
        TokenUsage, ToolCallId, ToolResultPayload,
    };
    use crate::transcript::TranscriptEntry;
    use uuid::Uuid;

    fn user_msg(content: &str) -> Message {
        Message::user(content)
    }

    /// An `llm.request` event for one invocation, carrying `messages`
    /// verbatim — the fold's only input for the prompt turn.
    fn request_event(invocation: Uuid, origin: LlmCallOrigin, messages: Vec<Message>) -> Event {
        Event::new(
            AgentId::new("fold-probe").unwrap(),
            invocation,
            EventPayload::LlmRequest(LlmRequestPayload {
                call_id: Uuid::now_v7(),
                model: "claude-haiku".into(),
                messages,
                tools_available: Vec::new(),
                request_params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: Some(4096),
                },
                origin,
            }),
        )
    }

    /// The shape the harness's `initial_step` builds: system, then the
    /// trigger's user message. Nothing else.
    fn opening_messages() -> Vec<Message> {
        vec![
            Message::system("You are a deterministic fixture."),
            user_msg("Summarise the fixture."),
        ]
    }

    /// Round 2's request: the whole conversation replayed, assistant
    /// turn and tool result included.
    fn continued_messages() -> Vec<Message> {
        let mut messages = opening_messages();
        messages.push(Message::assistant_text("Reading the file first."));
        messages.push(Message::tool_result(
            crate::events::ToolCallId::new("call_1").unwrap(),
            "deterministic",
        ));
        messages
    }

    fn assistant_event(round: u64, with_call: Option<&str>) -> Event {
        Event::new(
            AgentId::new("fold-probe").unwrap(),
            Uuid::now_v7(),
            EventPayload::LlmResponse(LlmResponsePayload {
                parts: crate::events::assistant_parts(
                    Some("thinking".into()),
                    with_call
                        .map(|id| {
                            vec![MessageToolCall {
                                tool_call_id: ToolCallId::new(id).unwrap(),
                                tool_name: "read_file".into(),
                                parameters: serde_json::json!({"path": "x"}),
                            }]
                        })
                        .unwrap_or_default(),
                ),
                round,
                call_id: Uuid::now_v7(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                origin: LlmCallOrigin::default(),
            }),
        )
        .with_cost(CostMetadata {
            call_id: Uuid::now_v7(),
            model: "claude-haiku".into(),
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_cost: 0.0,
            output_cost: 0.0,
            total_cost: 0.01,
            cumulative_invocation_cost: 0.01,
            cumulative_agent_cost: 0.01,
            origin: Default::default(),
        })
    }

    fn result_event(round: u64, call_id: &str, tool_name: &str) -> Event {
        Event::new(
            AgentId::new("fold-probe").unwrap(),
            Uuid::now_v7(),
            EventPayload::ToolResult(ToolResultPayload {
                round,
                tool_name: tool_name.into(),
                tool_call_id: ToolCallId::new(call_id).unwrap(),
                output: "ok".into(),
                is_error: false,
                error_kind: None,
                duration_ms: 5,
            }),
        )
    }

    /// The join: a result folded after its assistant turn carries the
    /// call's name and parameters, and names the assistant turn as
    /// its initiator in log coordinates.
    #[test]
    fn result_joins_its_initiating_turn() {
        let mut fold = TurnFold::new();
        let assistant = fold.apply(41, &assistant_event(3, Some("tc-1"))).unwrap();
        assert_eq!(assistant.round, 3);
        assert!(matches!(assistant.action, TurnAction::Assistant { .. }));

        let result = fold.apply(45, &result_event(3, "tc-1", "")).unwrap();
        assert_eq!(result.round, 3);
        assert_eq!(result.initiating_turn, Some(41));
        let TurnAction::ToolResult {
            tool_name,
            parameters,
            ..
        } = &result.action
        else {
            panic!("expected a tool-result turn");
        };
        assert_eq!(tool_name, "read_file");
        assert_eq!(parameters, &serde_json::json!({"path": "x"}));
    }

    /// A lone result (window starts after its call) renders from the
    /// event's restated tool_name, parameters null, no initiator.
    #[test]
    fn lone_result_stands_on_its_restated_name() {
        let mut fold = TurnFold::new();
        let result = fold.apply(90, &result_event(2, "tc-9", "exec")).unwrap();
        assert_eq!(result.initiating_turn, None);
        let TurnAction::ToolResult {
            tool_name,
            parameters,
            ..
        } = &result.action
        else {
            panic!("expected a tool-result turn");
        };
        assert_eq!(tool_name, "exec");
        assert_eq!(parameters, &serde_json::Value::Null);
    }

    /// The opening `llm.request` is a turn, and it is the first one:
    /// the runner publishes it before the provider call, so it sorts
    /// ahead of round 1's response on sequence alone.
    #[test]
    fn opening_request_becomes_the_first_turn() {
        let invocation = Uuid::now_v7();
        let mut fold = TurnFold::new();
        let prompt = fold
            .apply(
                10,
                &request_event(invocation, LlmCallOrigin::AgentTurn, opening_messages()),
            )
            .expect("the opening request yields a turn");
        let assistant = fold.apply(11, &assistant_event(1, None)).unwrap();

        assert!(prompt.seq < assistant.seq, "the prompt sorts first");
        // Not an action within a Round — it is what Round 1 answers.
        assert_eq!(prompt.round, 0);
        assert_eq!(prompt.initiating_turn, None);
        assert_eq!(prompt.invocation_id, invocation.to_string());
        let TurnAction::Prompt { system, user } = &prompt.action else {
            panic!("expected a prompt turn, got {:?}", prompt.action);
        };
        assert_eq!(system.as_deref(), Some("You are a deterministic fixture."));
        assert_eq!(user.as_deref(), Some("Summarise the fixture."));
    }

    /// The negative control the whole rule turns on: a later round's
    /// request re-sends the accumulated conversation, and must not
    /// produce a second prompt. Folded *alone* — no earlier request in
    /// the window — so nothing but the event's own content can be
    /// telling the fold "not the opening one". This is the case a
    /// `--follow` that joins a run in progress actually sees.
    #[test]
    fn a_later_rounds_replayed_history_is_not_a_prompt() {
        let invocation = Uuid::now_v7();
        let mut fold = TurnFold::new();
        assert!(
            fold.apply(
                77,
                &request_event(invocation, LlmCallOrigin::AgentTurn, continued_messages()),
            )
            .is_none(),
            "round N's replayed history is not the opening prompt"
        );
    }

    /// Sampling and elicitation calls (ADR-0018) publish `llm.request`
    /// too, and their message lists are system+user shaped. They are
    /// somebody else's prompt, not this invocation's.
    #[test]
    fn server_initiated_calls_mint_no_prompt() {
        let invocation = Uuid::now_v7();
        for origin in [
            LlmCallOrigin::Sampling {
                server: "docs".into(),
            },
            LlmCallOrigin::Elicitation {
                server: "docs".into(),
            },
        ] {
            let mut fold = TurnFold::new();
            assert!(
                fold.apply(
                    5,
                    &request_event(invocation, origin.clone(), opening_messages())
                )
                .is_none(),
                "{origin:?} must not mint a prompt turn"
            );
        }
    }

    /// An invocation has one opening prompt however many opening
    /// requests it published. `resume` replays only *completed* WAL
    /// rows, so a crash between the request publish and the response
    /// leaves nothing to replay and the reducer re-issues the same
    /// opening call — two identical events, one turn.
    #[test]
    fn one_prompt_per_invocation_however_many_opening_requests() {
        let invocation = Uuid::now_v7();
        let mut fold = TurnFold::new();
        assert!(
            fold.apply(
                3,
                &request_event(invocation, LlmCallOrigin::AgentTurn, opening_messages())
            )
            .is_some()
        );
        assert!(
            fold.apply(
                9,
                &request_event(invocation, LlmCallOrigin::AgentTurn, opening_messages())
            )
            .is_none(),
            "the re-issued opening call is the same prompt, not a second one"
        );
    }

    /// The dedup is per invocation, not per fold: `list_turns` walks
    /// the whole agent subject, so one fold routinely sees several
    /// invocations and each is owed its own prompt.
    #[test]
    fn one_fold_serves_every_invocations_prompt() {
        let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
        let mut fold = TurnFold::new();
        assert!(
            fold.apply(
                1,
                &request_event(first, LlmCallOrigin::AgentTurn, opening_messages())
            )
            .is_some()
        );
        assert!(
            fold.apply(
                2,
                &request_event(second, LlmCallOrigin::AgentTurn, opening_messages())
            )
            .is_some(),
            "a second invocation in the same window keeps its prompt"
        );
    }

    /// The rendering bridge maps a prompt turn onto the same
    /// `TranscriptEntry::Prompt` the WAL path emits, timestamped from
    /// the request event's envelope.
    #[test]
    fn prompt_turn_bridges_to_the_prompt_entry() {
        let mut fold = TurnFold::new();
        let turn = fold
            .apply(
                4,
                &request_event(Uuid::now_v7(), LlmCallOrigin::AgentTurn, opening_messages()),
            )
            .unwrap();
        match turn.transcript_entry() {
            TranscriptEntry::Prompt {
                timestamp_ms,
                system,
                user,
            } => {
                assert_eq!(timestamp_ms, turn.timestamp_ms);
                assert_eq!(system.as_deref(), Some("You are a deterministic fixture."));
                assert_eq!(user.as_deref(), Some("Summarise the fixture."));
            }
            other => panic!("expected a prompt entry, got {other:?}"),
        }
    }

    /// #447: a failed call folds to an assistant turn flagged as an
    /// error — the `[error]` entry the WAL-backed transcript produced
    /// and the Turn-backed one lost. The fold's catch-all arm compiles
    /// happily without this, so the assertion is the only thing
    /// holding it.
    #[test]
    fn failure_folds_to_an_error_assistant_turn() {
        let failure = Event::new(
            AgentId::new("fold-probe").unwrap(),
            Uuid::now_v7(),
            EventPayload::LlmFailure(crate::events::LlmFailurePayload {
                round: 4,
                call_id: Uuid::now_v7(),
                model: "claude-haiku".into(),
                error_kind: crate::events::LlmErrorKind::RateLimited,
                error_message: "rate limited".into(),
                duration_ms: 12,
                usage: None,
                origin: Default::default(),
            }),
        );

        let turn = TurnFold::new()
            .apply(12, &failure)
            .expect("a failed call is a turn");
        assert_eq!(turn.round, 4);
        let TurnAction::Assistant {
            model,
            content,
            is_error,
            cost_usd,
            ..
        } = &turn.action
        else {
            panic!("expected an assistant turn");
        };
        assert_eq!(is_error, &Some(true));
        assert_eq!(model, "claude-haiku");
        assert_eq!(content.as_deref(), Some("rate limited"));
        assert_eq!(cost_usd, &None, "no cost metadata, no cost claim");

        // And it renders as the operator saw it before the flip.
        let rendered = crate::transcript::render_pretty(&[turn.transcript_entry()], None);
        assert!(rendered.contains("[error]"), "got:\n{rendered}");
    }

    /// The rendering bridge maps turns onto the exact transcript
    /// entries the WAL path produces — the flip's byte contract.
    #[test]
    fn transcript_entry_bridge_matches_shapes() {
        let mut fold = TurnFold::new();
        let turn = fold.apply(7, &assistant_event(1, None)).unwrap();
        match turn.transcript_entry() {
            TranscriptEntry::Assistant {
                model,
                cost_usd,
                is_error,
                ..
            } => {
                assert_eq!(model, "claude-haiku");
                assert_eq!(cost_usd, Some(0.01));
                // The WAL path renders a completed call as
                // `is_error: false`; a response event means exactly
                // that, so the bridge must not weaken it to `null`.
                assert_eq!(is_error, Some(false));
            }
            other => panic!("expected an assistant entry, got {other:?}"),
        }
    }
}
