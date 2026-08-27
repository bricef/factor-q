//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use serde_json::json;

#[test]
fn round_trip_triggered_event() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("researcher").unwrap(),
        invocation_id,
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: None,
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: json!({"topic": "rust async"}),
            config_snapshot: ConfigSnapshot {
                name: "researcher".to_string(),
                model: "claude-haiku".to_string(),
                system_prompt: "You are a research agent.".to_string(),
                tools: vec!["read".to_string(), "web_search".to_string()],
                sandbox: SandboxSnapshot {
                    fs_read: vec!["/docs".to_string()],
                    fs_write: vec![],
                    network: vec![],
                    env: vec![],
                    exec_cwd: vec![],
                },
                budget: Some(0.50),
                ..Default::default()
            },
        }),
    );

    assert_eq!(event.subject(), "fq.agent.researcher.triggered");
    assert_eq!(event.envelope.schema_version, SCHEMA_VERSION);
    assert_eq!(event.envelope.agent_id, "researcher");
    assert_eq!(event.envelope.trace_id, event.envelope.invocation_id);
    assert!(event.envelope.parent_event_id.is_none());
    assert_eq!(event.envelope.schema_id, "factor-q/triggered@1");

    let json = serde_json::to_string(&event).unwrap();
    let round_tripped: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(round_tripped.envelope.agent_id, event.envelope.agent_id);
    assert_eq!(
        round_tripped.envelope.invocation_id,
        event.envelope.invocation_id
    );
    match round_tripped.payload {
        EventPayload::Triggered(p) => {
            assert!(matches!(p.trigger_source, TriggerSource::Manual));
            assert_eq!(p.config_snapshot.name, "researcher");
        }
        _ => panic!("wrong payload type"),
    }
}

/// A `triggered` event written before triggers had an identity — no
/// `trigger_id` key at all — still deserialises, and reads as "no
/// identity recorded" rather than failing.
///
/// This is the replay / older-peer case, and the reason the field is
/// optional: the log is append-only and full of events that predate it,
/// so a required field would break every reader of the existing log at
/// once (the required `EventView::seq` that broke the dashboard is the
/// same mistake, already paid for once).
#[test]
fn a_triggered_event_written_before_the_identity_still_deserialises() {
    let json = json!({
        "envelope": {
            "schema_version": SCHEMA_VERSION,
            "event_id": "01890000-0000-7000-8000-000000000001",
            "parent_event_id": null,
            "trace_id": "01890000-0000-7000-8000-000000000002",
            "agent_id": "researcher",
            "invocation_id": "01890000-0000-7000-8000-000000000002",
            "schema_id": "factor-q/triggered@1",
            "timestamp": "2026-01-02T03:04:05Z",
            "cost": null
        },
        "payload": {
            "event_type": "triggered",
            "payload": {
                "trigger_source": "subject",
                "trigger_subject": "fq.trigger.researcher",
                "trigger_payload": {"topic": "rust async"},
                "config_snapshot": {
                    "name": "researcher",
                    "model": "claude-haiku",
                    "system_prompt": "You are a research agent.",
                    "tools": [],
                    "sandbox": {
                        "fs_read": [], "fs_write": [], "network": [], "env": [], "exec_cwd": []
                    },
                    "budget": null
                }
            }
        }
    })
    .to_string();

    let event: Event = serde_json::from_str(&json).expect("an event without trigger_id parses");
    match event.payload {
        EventPayload::Triggered(p) => {
            assert!(
                p.trigger_id.is_none(),
                "an event that never carried an identity has none to report"
            );
            assert_eq!(p.trigger_subject.as_deref(), Some("fq.trigger.researcher"));
        }
        other => panic!("expected Triggered, got {other:?}"),
    }
}

#[test]
fn subjects_for_all_event_types() {
    let agent = "test-agent";
    assert_eq!(
        subjects::agent_triggered(agent),
        "fq.agent.test-agent.triggered"
    );
    assert_eq!(
        subjects::agent_llm_request(agent),
        "fq.agent.test-agent.llm.request"
    );
    assert_eq!(
        subjects::agent_llm_response(agent),
        "fq.agent.test-agent.llm.response"
    );
    assert_eq!(
        subjects::agent_tool_call(agent),
        "fq.agent.test-agent.tool.call"
    );
    assert_eq!(
        subjects::agent_tool_result(agent),
        "fq.agent.test-agent.tool.result"
    );
    assert_eq!(
        subjects::agent_completed(agent),
        "fq.agent.test-agent.completed"
    );
    assert_eq!(subjects::agent_failed(agent), "fq.agent.test-agent.failed");
    assert_eq!(
        subjects::agent_invocation_ambiguous(agent),
        "fq.agent.test-agent.invocation.ambiguous"
    );
    assert_eq!(subjects::worker_orphaned("w1"), "fq.worker.w1.orphaned");
}

#[test]
fn invocation_archived_subject_is_agent_scoped() {
    // `InvocationArchived` rides on the same agent-scoped
    // namespace as `InvocationAmbiguous` so the coordination
    // consumer's `fq.agent.*.invocation.*` filter catches it.
    let agent_id = AgentId::new("researcher").unwrap();
    let invocation_id = Uuid::now_v7();
    let worker_id = crate::worker::WorkerId::new("worker-007").unwrap();
    let event = Event::new(
        agent_id,
        invocation_id,
        EventPayload::InvocationArchived(InvocationArchivedPayload {
            worker_id: worker_id.clone(),
            final_phase: "completed".to_string(),
            final_state_blob: vec![1, 2, 3],
            started_at_ms: 1_700_000_000_000,
            terminal_at_ms: 1_700_000_001_000,
        }),
    );
    assert_eq!(event.subject(), "fq.agent.researcher.invocation.archived");
    assert_eq!(event.envelope.schema_id, "factor-q/invocation_archived@1");
    assert_eq!(event.envelope.invocation_id, invocation_id);
    match &event.payload {
        EventPayload::InvocationArchived(p) => {
            assert_eq!(p.worker_id, worker_id);
            assert_eq!(p.final_phase, "completed");
            assert_eq!(p.final_state_blob, vec![1, 2, 3]);
        }
        other => panic!("wrong payload variant: {other:?}"),
    }
}

#[test]
fn invocation_archive_acked_subject_is_worker_scoped() {
    // The ack rides on `fq.worker.{worker_id}.invocation.archive_acked`
    // so a worker can subscribe to its own acks with a
    // single filter. Coordination consumer's
    // `fq.agent.*.invocation.*` filter does not match.
    let agent_id = AgentId::new("researcher").unwrap();
    let invocation_id = Uuid::now_v7();
    let worker_id = crate::worker::WorkerId::new("worker-007").unwrap();
    let event = Event::new(
        agent_id,
        invocation_id,
        EventPayload::InvocationArchiveAcked(InvocationArchiveAckedPayload {
            worker_id: worker_id.clone(),
        }),
    );
    assert_eq!(
        event.subject(),
        "fq.worker.worker-007.invocation.archive_acked"
    );
    assert_eq!(
        event.envelope.schema_id,
        "factor-q/invocation_archive_acked@1"
    );
    // Envelope keeps the real invocation_id so the worker
    // can identify which row to delete.
    assert_eq!(event.envelope.invocation_id, invocation_id);
}

#[test]
fn invocation_operator_recovered_subject_is_agent_scoped() {
    // Operator-issued; rides on the same agent-scoped
    // namespace as `InvocationArchived` so the coordination
    // consumer's `fq.agent.*.invocation.*` filter catches it.
    let agent_id = AgentId::new("researcher").unwrap();
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        agent_id,
        invocation_id,
        EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: Some("stuck on flaky network call".to_string()),
        }),
    );
    assert_eq!(
        event.subject(),
        "fq.agent.researcher.invocation.operator_recovered"
    );
    assert_eq!(
        event.envelope.schema_id,
        "factor-q/invocation_operator_recovered@1"
    );
    assert_eq!(event.envelope.invocation_id, invocation_id);
    match &event.payload {
        EventPayload::InvocationOperatorRecovered(p) => {
            assert_eq!(p.action, "drop");
            assert_eq!(p.final_phase, "failed");
            assert_eq!(p.reason.as_deref(), Some("stuck on flaky network call"));
        }
        other => panic!("wrong payload variant: {other:?}"),
    }
}

#[test]
fn invocation_operator_recovered_payload_omits_reason_when_none() {
    // `reason` is operator-supplied; absence should
    // serialise as missing rather than `null`.
    let event = Event::new(
        AgentId::new("r").unwrap(),
        Uuid::now_v7(),
        EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: None,
        }),
    );
    let body = serde_json::to_value(&event.payload).unwrap();
    assert!(
        body.get("reason").is_none(),
        "reason should be omitted when None, got {body}"
    );
}

#[test]
fn worker_heartbeat_subject_reads_from_payload_not_envelope() {
    // The subject for a WorkerHeartbeat is built from the
    // payload's `worker_id`, not from `envelope.agent_id`
    // (which is the system sentinel for runtime-tier events).
    // This is the design call made on 2026-05-16 — worker is
    // its own scope, parallel to agent.
    let runtime_id = Uuid::now_v7();
    let worker_id = crate::worker::WorkerId::new("worker-007").unwrap();
    let event = Event::system(
        runtime_id,
        EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
            worker_id: worker_id.clone(),
        }),
    );
    assert_eq!(event.subject(), "fq.worker.worker-007.heartbeat");
    assert_eq!(event.envelope.schema_id, "factor-q/worker_heartbeat@1");
    // The envelope's agent_id remains the system sentinel —
    // worker events aren't tied to an agent. The payload is
    // where the worker_id lives.
    assert_eq!(event.envelope.agent_id.as_str(), "system");
    match &event.payload {
        EventPayload::WorkerHeartbeat(p) => assert_eq!(p.worker_id, worker_id),
        other => panic!("wrong payload variant: {other:?}"),
    }
}

#[test]
fn validate_token_accepts_typical_ids() {
    for ok in [
        "agent",
        "agent_1",
        "agent-1",
        "a",
        "system",
        "worker-001",
        "01HXJABC0123456789", // ulid-shaped
    ] {
        assert!(
            subjects::validate_token(ok).is_ok(),
            "expected {ok:?} to be a valid subject token"
        );
    }
}

#[test]
fn validate_token_rejects_dot_wildcard_whitespace_and_empty() {
    use subjects::SubjectTokenError;
    assert_eq!(
        subjects::validate_token(""),
        Err(SubjectTokenError::Empty),
        "empty token should be rejected"
    );
    for bad in ["foo.bar", "agent*", "agent>", "has space", "has\ttab"] {
        assert!(
            matches!(
                subjects::validate_token(bad),
                Err(SubjectTokenError::InvalidChar(_))
            ),
            "expected {bad:?} to be rejected as invalid"
        );
    }
}

#[test]
fn tool_result_error_kind_serialises() {
    let payload = ToolResultPayload {
        round: 0,
        tool_name: String::new(),
        tool_call_id: crate::events::ToolCallId::new("toolu_01ABC").unwrap(),
        output: "Path /etc/passwd is outside allowed scope".to_string(),
        is_error: true,
        error_kind: Some(ToolErrorKind::SandboxViolation),
        duration_ms: 1,
    };
    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(json["error_kind"], "sandbox_violation");
    assert_eq!(json["is_error"], true);
}

#[test]
fn tool_result_success_omits_error_kind() {
    let payload = ToolResultPayload {
        round: 0,
        tool_name: String::new(),
        tool_call_id: crate::events::ToolCallId::new("toolu_01ABC").unwrap(),
        output: "file contents".to_string(),
        is_error: false,
        error_kind: None,
        duration_ms: 12,
    };
    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(json.get("error_kind"), None);
    assert_eq!(json["is_error"], false);
}

#[test]
fn envelope_default_fields_on_new_event() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("test-agent").unwrap(),
        invocation_id,
        EventPayload::LlmDispatched(LlmDispatchedPayload {
            call_id: Uuid::now_v7(),
            model: "claude-haiku".to_string(),
        }),
    );
    assert!(event.envelope.parent_event_id.is_none());
    assert_eq!(event.envelope.trace_id, invocation_id);
    assert_eq!(event.envelope.invocation_id, invocation_id);
    assert_eq!(event.envelope.agent_id, "test-agent");
    assert_eq!(event.envelope.schema_id, "factor-q/llm_dispatched@1");
    assert!(event.annotations.is_empty());
}

#[test]
fn event_for_system_uses_runtime_id_as_trace_id() {
    let runtime_id = Uuid::now_v7();
    let event = Event::system(
        runtime_id,
        EventPayload::SystemStartup(SystemStartupPayload {
            runtime_id,
            version: "0.1.0".to_string(),
            nats_url: "nats://localhost:4222".to_string(),
            agents_loaded: 0,
            pricing_entries: 0,
        }),
    );
    assert_eq!(event.envelope.trace_id, runtime_id);
    assert_eq!(event.envelope.invocation_id, runtime_id);
    assert_eq!(event.envelope.agent_id, "system");
    assert!(event.envelope.parent_event_id.is_none());
}

#[test]
fn annotations_skip_serialise_when_empty() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("test-agent").unwrap(),
        invocation_id,
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: None,
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: json!({}),
            config_snapshot: ConfigSnapshot {
                name: "t".to_string(),
                model: "m".to_string(),
                system_prompt: String::new(),
                tools: vec![],
                sandbox: SandboxSnapshot::default(),
                budget: None,
                ..Default::default()
            },
        }),
    );
    let json = serde_json::to_value(&event).unwrap();
    assert!(json.get("annotations").is_none());
    assert!(json.get("envelope").is_some());
}

/// The version is pinned by a test so a bump is always a deliberate,
/// reviewable line in a diff rather than a side effect. v3 came with
/// ADR-0034: `Message` became an enum over turn kinds and `llm.response`
/// an ordered part list.
#[test]
fn schema_version_constant_is_three() {
    assert_eq!(SCHEMA_VERSION, 3);
}

/// The consumer barrier must strip reasoning from the **payload**, not
/// just the annotations.
///
/// Reasoning became a payload part in ADR-0034, which moved it from the
/// side of the barrier that was stripped to the side that was not. This
/// pins the rule while it is still cheap: `for_consumer_context` has no
/// production caller until multi-node lands (#414), so writing it now
/// costs nothing and writing it later means retrofitting a discipline
/// onto a shipped graph.
///
/// Fresh-context verification is the reason: a verifier that can see the
/// producer's working is no longer independent of the path that produced
/// it.
#[test]
fn the_consumer_barrier_strips_reasoning_from_a_response_payload() {
    let event = Event::new(
        AgentId::new("producer").unwrap(),
        Uuid::now_v7(),
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 1,
            origin: LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            parts: vec![
                AssistantPart::Reasoning(Reasoning {
                    model: "kimi-k2".to_string(),
                    content: ReasoningContent::Plain {
                        text: "the working a verifier must not see".to_string(),
                    },
                }),
                AssistantPart::Text {
                    text: "the answer a verifier may see".to_string(),
                },
            ],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    );

    let view = event.for_consumer_context();
    let EventPayload::LlmResponse(seen) = &view.payload else {
        panic!("variant must be preserved");
    };
    assert!(
        !seen
            .parts
            .iter()
            .any(|p| matches!(p, AssistantPart::Reasoning(_))),
        "a consuming agent must never see upstream reasoning"
    );
    assert_eq!(
        seen.text().as_deref(),
        Some("the answer a verifier may see"),
        "the barrier removes the working, not the output — a consumer is \
         entitled to the producer's typed result"
    );

    // The stored event is untouched: the barrier is a view, and the log
    // keeps everything for humans, debugging and the learning loop.
    let EventPayload::LlmResponse(stored) = &event.payload else {
        unreachable!()
    };
    assert!(
        stored
            .parts
            .iter()
            .any(|p| matches!(p, AssistantPart::Reasoning(_))),
        "stripping is for the consumer path only; the log retains it"
    );
}

/// The same rule on the request side, where prior assistant turns are
/// replayed — the larger leak, since a request carries the whole
/// conversation rather than one turn.
#[test]
fn the_consumer_barrier_strips_reasoning_from_replayed_messages() {
    let event = Event::new(
        AgentId::new("producer").unwrap(),
        Uuid::now_v7(),
        EventPayload::LlmRequest(LlmRequestPayload {
            call_id: Uuid::now_v7(),
            model: "kimi-k2".to_string(),
            messages: vec![
                Message::user("go"),
                Message::Assistant {
                    parts: vec![
                        AssistantPart::Reasoning(Reasoning {
                            model: "kimi-k2".to_string(),
                            content: ReasoningContent::Plain {
                                text: "private working".to_string(),
                            },
                        }),
                        AssistantPart::Text {
                            text: "public answer".to_string(),
                        },
                    ],
                },
            ],
            tools_available: vec![],
            request_params: RequestParams {
                effort: None,
                temperature: None,
                max_tokens: None,
            },
            origin: LlmCallOrigin::AgentTurn,
        }),
    );

    let view = event.for_consumer_context();
    let EventPayload::LlmRequest(seen) = &view.payload else {
        panic!("variant must be preserved");
    };
    let leaked = seen.messages.iter().any(|m| match m {
        Message::Assistant { parts } => parts
            .iter()
            .any(|p| matches!(p, AssistantPart::Reasoning(_))),
        _ => false,
    });
    assert!(!leaked, "replayed reasoning must not cross the barrier");
    assert_eq!(
        seen.messages[1].text().as_deref(),
        Some("public answer"),
        "the assistant turn survives without its working"
    );
}

/// #447: subject leaf, schema id and projection `event_type` are three
/// spellings of one name and are minted together. They are also easy
/// to desync later, so they are pinned here as a triple.
#[test]
fn llm_failure_subject_schema_and_type_agree() {
    let payload = EventPayload::LlmFailure(LlmFailurePayload {
        round: 3,
        call_id: Uuid::now_v7(),
        model: "claude-haiku".to_string(),
        error_kind: LlmErrorKind::RateLimited,
        error_message: "429".to_string(),
        duration_ms: 4_200,
        usage: None,
        origin: LlmCallOrigin::default(),
    });
    assert_eq!(
        payload.subject("researcher"),
        "fq.agent.researcher.llm.failure"
    );
    assert_eq!(payload.schema_id(), "factor-q/llm_failure@1");
    assert_eq!(payload.event_type(), "llm_failure");
    // Sibling of llm.response, so the existing wildcard still reaches it.
    assert!(
        payload
            .subject("researcher")
            .starts_with("fq.agent.researcher.llm.")
    );
}

/// `None` is not zero, on the wire as well as in the type: an unknown
/// usage is *absent*, not a zeroed `TokenUsage` that a reader would
/// take for "the provider billed nothing".
#[test]
fn llm_failure_omits_usage_when_unknown() {
    let payload = EventPayload::LlmFailure(LlmFailurePayload {
        round: 1,
        call_id: Uuid::now_v7(),
        model: "claude-haiku".to_string(),
        error_kind: LlmErrorKind::RequestFailed,
        error_message: "connection reset".to_string(),
        duration_ms: 10,
        usage: None,
        origin: LlmCallOrigin::default(),
    });
    let json = serde_json::to_value(&payload).unwrap();
    assert!(
        json["payload"].get("usage").is_none(),
        "unknown usage must be absent, not zeroed: {json}"
    );

    // And a recoverable one survives the round trip intact.
    let priced = EventPayload::LlmFailure(LlmFailurePayload {
        round: 1,
        call_id: Uuid::now_v7(),
        model: "claude-haiku".to_string(),
        error_kind: LlmErrorKind::EmptyResponse,
        error_message: "empty".to_string(),
        duration_ms: 10,
        usage: Some(TokenUsage {
            input_tokens: 1_200,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        }),
        origin: LlmCallOrigin::default(),
    });
    let wire = serde_json::to_string(&priced).unwrap();
    let back: EventPayload = serde_json::from_str(&wire).unwrap();
    let EventPayload::LlmFailure(p) = back else {
        panic!("wrong variant");
    };
    assert_eq!(p.usage.map(|u| u.input_tokens), Some(1_200));
    assert_eq!(p.error_kind, LlmErrorKind::EmptyResponse);
}

/// The forward-compatibility landing pad: an `event_type` this binary
/// has never heard of parses as `Unknown` instead of failing the whole
/// event. Without it, every new event type breaks every deployed
/// consumer for the length of the mixed-version window.
#[test]
fn unknown_event_type_parses_instead_of_failing() {
    let from_the_future = json!({
        "envelope": {
            "schema_version": SCHEMA_VERSION,
            "event_id": Uuid::now_v7(),
            "trace_id": Uuid::now_v7(),
            "agent_id": "researcher",
            "invocation_id": Uuid::now_v7(),
            "schema_id": "factor-q/telepathy@1",
            "timestamp": "2026-08-05T00:00:00Z"
        },
        "payload": {
            "event_type": "telepathy",
            "payload": { "thought": "a field no version of this binary knows" }
        }
    });

    let event: Event = serde_json::from_value(from_the_future).expect("unknown tag must not fail");
    assert!(matches!(event.payload, EventPayload::Unknown));
    // The envelope survives intact — which is the point: an old
    // consumer can still see whose invocation this was and when.
    assert_eq!(event.envelope.agent_id, "researcher");
}

/// The escape hatch must not swallow a *known* type whose payload is
/// malformed. A `triggered` event missing its config snapshot is a
/// real error, not an unknown event.
#[test]
fn malformed_known_payload_still_fails() {
    let broken = json!({
        "envelope": {
            "schema_version": SCHEMA_VERSION,
            "event_id": Uuid::now_v7(),
            "trace_id": Uuid::now_v7(),
            "agent_id": "researcher",
            "invocation_id": Uuid::now_v7(),
            "schema_id": "factor-q/triggered@1",
            "timestamp": "2026-08-05T00:00:00Z"
        },
        "payload": {
            "event_type": "triggered",
            "payload": { "trigger_source": "manual" }
        }
    });
    assert!(serde_json::from_value::<Event>(broken).is_err());
}

#[test]
fn tool_call_id_round_trips_as_bare_string() {
    let id = ToolCallId::new("toolu_01ABC").unwrap();
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, "\"toolu_01ABC\"");
    let parsed: ToolCallId = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, id);
}

#[test]
fn tool_call_id_rejects_empty_input() {
    assert!(ToolCallId::new("").is_err());
}

#[test]
fn tool_call_id_deserialise_rejects_empty_string() {
    // Wire-boundary check: an event arriving with an empty
    // tool_call_id fails to parse rather than landing in the
    // runtime where downstream code assumes non-empty.
    let result: Result<ToolCallId, _> = serde_json::from_str("\"\"");
    assert!(result.is_err());
}

#[test]
fn event_with_parent_sets_envelope_field() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::LlmDispatched(LlmDispatchedPayload {
            call_id: Uuid::now_v7(),
            model: "m".to_string(),
        }),
    );
    let parent = Uuid::now_v7();
    let event = event.with_parent(parent);
    assert_eq!(event.envelope.parent_event_id, Some(parent));
}

#[test]
fn system_events_have_null_parent() {
    // Resolved decision from step 2 of the envelope-refactor
    // plan: SystemStartup, SystemRecovery, SystemShutdown,
    // SystemTaskFailed are not part of any invocation chain.
    let runtime_id = Uuid::now_v7();
    let cases = vec![
        EventPayload::SystemStartup(SystemStartupPayload {
            runtime_id,
            version: String::new(),
            nats_url: String::new(),
            agents_loaded: 0,
            pricing_entries: 0,
        }),
        EventPayload::SystemShutdown(SystemShutdownPayload {
            runtime_id,
            reason: String::new(),
            clean: true,
        }),
        EventPayload::SystemTaskFailed(SystemTaskFailedPayload {
            runtime_id,
            task_name: String::new(),
            error_message: String::new(),
        }),
        EventPayload::SystemRecovery(SystemRecoveryPayload {
            runtime_id,
            worker_id: String::new(),
            safe_resume: 0,
            safe_replay: 0,
            ambiguous: 0,
            total: 0,
        }),
    ];
    for p in cases {
        let event = Event::system(runtime_id, p);
        assert!(
            event.envelope.parent_event_id.is_none(),
            "system events must not chain to a parent: schema_id={}",
            event.envelope.schema_id
        );
    }
}

#[test]
fn event_with_cost_sets_envelope_cost() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            parts: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    );
    let cost = CostMetadata {
        call_id: Uuid::now_v7(),
        model: "claude-haiku".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: 0.0001,
        output_cost: 0.0005,
        total_cost: 0.0006,
        cumulative_invocation_cost: 0.0006,
        cumulative_agent_cost: 0.0006,
        origin: LlmCallOrigin::AgentTurn,
        reasoning_tokens: 0,
    };
    let event = event.with_cost(cost.clone());
    assert_eq!(event.envelope.cost.as_ref(), Some(&cost));
}

#[test]
fn cost_metadata_round_trips_on_envelope() {
    let invocation_id = Uuid::now_v7();
    let cost = CostMetadata {
        call_id: Uuid::now_v7(),
        model: "m".to_string(),
        input_tokens: 1,
        output_tokens: 2,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        input_cost: 0.1,
        output_cost: 0.2,
        total_cost: 0.3,
        cumulative_invocation_cost: 0.3,
        cumulative_agent_cost: 0.3,
        origin: LlmCallOrigin::AgentTurn,
        reasoning_tokens: 0,
    };
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            parts: vec![AssistantPart::Text {
                text: "ok".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    )
    .with_cost(cost.clone());
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.envelope.cost.as_ref(), Some(&cost));
}

#[test]
fn envelope_cost_omits_when_none() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            parts: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    );
    let json = serde_json::to_value(&event).unwrap();
    let envelope = json.get("envelope").expect("envelope present");
    assert!(envelope.get("cost").is_none());
}

#[test]
fn event_annotate_inserts_key() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: None,
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: json!({}),
            config_snapshot: ConfigSnapshot {
                name: "t".to_string(),
                model: "m".to_string(),
                system_prompt: String::new(),
                tools: vec![],
                sandbox: SandboxSnapshot::default(),
                budget: None,
                ..Default::default()
            },
        }),
    )
    .annotate(annotation_keys::NOTES, json!("hello"))
    .annotate(annotation_keys::CONFIDENCE, json!(0.7));
    assert_eq!(
        event.annotations.0.get(annotation_keys::NOTES),
        Some(&json!("hello"))
    );
    assert_eq!(
        event.annotations.0.get(annotation_keys::CONFIDENCE),
        Some(&json!(0.7))
    );
}

#[test]
fn event_annotate_replaces_existing_key() {
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::LlmDispatched(LlmDispatchedPayload {
            call_id: Uuid::now_v7(),
            model: "m".to_string(),
        }),
    )
    .annotate(annotation_keys::NOTES, json!("first"))
    .annotate(annotation_keys::NOTES, json!("second"));
    assert_eq!(
        event.annotations.0.get(annotation_keys::NOTES),
        Some(&json!("second"))
    );
    assert_eq!(event.annotations.0.len(), 1);
}

#[test]
fn unknown_annotation_keys_permitted() {
    // The registry is advisory; arbitrary keys are still legal.
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::LlmDispatched(LlmDispatchedPayload {
            call_id: Uuid::now_v7(),
            model: "m".to_string(),
        }),
    )
    .annotate("my_custom_key", json!({"shape": "blob"}));
    assert!(event.annotations.0.contains_key("my_custom_key"));
}

#[test]
fn well_known_annotation_keys_are_constants() {
    assert_eq!(annotation_keys::NOTES, "notes");
    assert_eq!(annotation_keys::CONFIDENCE, "confidence");
    assert_eq!(annotation_keys::REASONING, "reasoning");
    assert_eq!(annotation_keys::SOURCES_CONSIDERED, "sources_considered");
    assert_eq!(annotation_keys::FLAGS, "flags");
}

#[test]
fn consumer_view_strips_annotations_round_trip() {
    // Step 4 acceptance test: an event with payload + two
    // annotations serialises via for_consumer_context with
    // envelope and payload but no annotations field.
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            parts: vec![AssistantPart::Text {
                text: "hello".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    )
    .annotate(annotation_keys::NOTES, json!("thinking aloud"))
    .annotate(annotation_keys::CONFIDENCE, json!(0.9));

    let view = event.for_consumer_context();
    let json = serde_json::to_value(&view).unwrap();
    assert!(json.get("envelope").is_some(), "envelope present");
    assert!(json.get("payload").is_some(), "payload present");
    assert!(
        json.get("annotations").is_none(),
        "annotations must be stripped from consumer view"
    );
    // Original event still has the annotations — the barrier is
    // a serialisation property of the view, not a mutation of
    // the source.
    assert_eq!(event.annotations.0.len(), 2);
}

#[test]
fn consumer_view_serialises_without_annotations_field_even_with_annotations() {
    // Same property as above, but with the most common attack
    // path: a producer trying to smuggle a reasoning trace
    // through the consumer barrier.
    let invocation_id = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("producer").unwrap(),
        invocation_id,
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: LlmCallOrigin::AgentTurn,
            call_id: Uuid::now_v7(),
            parts: vec![AssistantPart::Text {
                text: "answer: 42".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    )
    .annotate(
        annotation_keys::REASONING,
        json!("I tried 41, then 42, and decided 42"),
    );

    let view = event.for_consumer_context();
    let serialised = serde_json::to_string(&view).unwrap();
    // Assert on the annotation's *value*, not on the substring
    // "reasoning". The payload legitimately carries `usage.
    // reasoning_tokens` — a cost figure, not a trace — and a name-based
    // check cannot tell the two apart. What must never appear is the
    // trace itself.
    assert!(
        !serialised.contains("I tried 41"),
        "reasoning trace must not leak through consumer view"
    );
    assert!(
        !serialised.contains("\"annotations\""),
        "the annotations layer must not appear at all"
    );
    assert!(
        !serialised.contains("I tried 41"),
        "annotation value must not leak through consumer view"
    );
}

#[test]
fn event_with_parent_round_trips_through_serde() {
    let invocation_id = Uuid::now_v7();
    let parent = Uuid::now_v7();
    let event = Event::new(
        AgentId::new("agent").unwrap(),
        invocation_id,
        EventPayload::ToolDispatched(ToolDispatchedPayload {
            tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
            tool_name: "t".to_string(),
        }),
    )
    .with_parent(parent);
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Event = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.envelope.parent_event_id, Some(parent));
}

#[test]
fn schema_id_for_every_payload_variant() {
    // Exhaustive check that every payload variant resolves to a
    // non-empty `factor-q/<name>@<v>` schema_id. The match in
    // `schema_id_for` is exhaustive, so adding a new payload
    // variant without a schema_id mapping will fail to compile.
    let inv = Uuid::now_v7();
    let cases: Vec<EventPayload> = vec![
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: None,
            trigger_source: TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: json!({}),
            config_snapshot: ConfigSnapshot {
                name: "t".into(),
                model: "m".into(),
                system_prompt: String::new(),
                tools: vec![],
                sandbox: SandboxSnapshot::default(),
                budget: None,
                ..Default::default()
            },
        }),
        EventPayload::LlmRequest(LlmRequestPayload {
            origin: LlmCallOrigin::AgentTurn,
            call_id: inv,
            model: "m".into(),
            messages: vec![],
            tools_available: vec![],
            request_params: RequestParams {
                effort: None,
                temperature: None,
                max_tokens: None,
            },
        }),
        EventPayload::LlmDispatched(LlmDispatchedPayload {
            call_id: inv,
            model: "m".into(),
        }),
        EventPayload::LlmResponse(LlmResponsePayload {
            round: 0,
            origin: LlmCallOrigin::AgentTurn,
            call_id: inv,
            parts: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
        EventPayload::ToolCall(ToolCallPayload {
            round: 0,
            tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
            tool_name: "n".into(),
            parameters: json!({}),
        }),
        EventPayload::ToolDispatched(ToolDispatchedPayload {
            tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
            tool_name: "n".into(),
        }),
        EventPayload::ToolResult(ToolResultPayload {
            round: 0,
            tool_name: String::new(),
            tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
            output: String::new(),
            is_error: false,
            error_kind: None,
            duration_ms: 0,
        }),
        EventPayload::Completed(CompletedPayload {
            task_status: TaskStatus::default(),
            result_summary: None,
            total_llm_calls: 0,
            total_tool_calls: 0,
            total_cost: 0.0,
            total_duration_ms: 0,
        }),
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::RuntimeError,
            error_message: String::new(),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
        }),
        EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
            stuck_entity: "tool_dispatch".into(),
            stuck_call_id: "tc".into(),
            note: String::new(),
        }),
        EventPayload::SystemStartup(SystemStartupPayload {
            runtime_id: inv,
            version: String::new(),
            nats_url: String::new(),
            agents_loaded: 0,
            pricing_entries: 0,
        }),
        EventPayload::SystemShutdown(SystemShutdownPayload {
            runtime_id: inv,
            reason: String::new(),
            clean: true,
        }),
        EventPayload::SystemTaskFailed(SystemTaskFailedPayload {
            runtime_id: inv,
            task_name: String::new(),
            error_message: String::new(),
        }),
        EventPayload::SystemRecovery(SystemRecoveryPayload {
            runtime_id: inv,
            worker_id: String::new(),
            safe_resume: 0,
            safe_replay: 0,
            ambiguous: 0,
            total: 0,
        }),
        EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
            worker_id: crate::worker::WorkerId::new("w").unwrap(),
        }),
        EventPayload::WorkerOrphaned(WorkerOrphanedPayload {
            worker_id: crate::worker::WorkerId::new("w").unwrap(),
            last_heartbeat_ms: 0,
        }),
        EventPayload::McpServerLog(McpServerLogPayload {
            server: String::new(),
            level: String::new(),
            logger: None,
            data: serde_json::Value::Null,
        }),
    ];
    for payload in cases {
        let id = schema_id_for(&payload);
        assert!(
            id.starts_with("factor-q/") && id.ends_with("@1"),
            "schema_id_for produced {id:?}"
        );
    }
}
