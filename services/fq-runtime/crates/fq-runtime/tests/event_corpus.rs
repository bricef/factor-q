//! The golden event corpus
//! (<https://github.com/bricef/factor-q/issues/409>): one file per event
//! type this build writes, serialised by the real serialisers and
//! committed under `tests/corpus/events/v3/`, plus hand-written v1 and
//! v2 envelopes under `v1/` and `v2/`, taken from the schema changelog.
//! Replayed through the projection consumer's parse boundary on every
//! CI run.
//!
//! Two properties. **Every current file projects**: it is admitted at
//! the boundary, inserts into a projection store, and re-serialises to
//! the committed JSON value — so a breaking change to a payload shape
//! without a version bump fails here, which is exactly what passed CI
//! before, because every other test constructs events with the current
//! serialisers on both sides. **Every older file halts**: it is refused
//! for its version, never as malformed, and never admitted — so a reader
//! that stopped checking the version, or a loop that went back to acking
//! what it could not read, goes red on committed history rather than on
//! a rebuild in production.
//!
//! `UPDATE_CORPUS=1 cargo test -p fq-runtime --test event_corpus`
//! rewrites the v3 files from the exemplars below and is the one way to
//! add or change one; the v1 and v2 files are hand-written history and
//! are never regenerated.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use fq_runtime::ProjectionStore;
use fq_runtime::agent::AgentId;
use fq_runtime::control_plane::durable_consumer::{Admission, admit};
use fq_runtime::events::*;
use fq_runtime::worker::WorkerId;
use serde_json::{Value, json};
use strum::IntoEnumIterator;
use uuid::Uuid;

fn corpus_dir(version: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus/events")
        .join(version)
}

/// Every `*.json` in `dir`, sorted, so the corpus is read in one order
/// everywhere.
fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("corpus dir {dir:?}: {e}"))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

// ------------------------------------------------------------------
// The exemplars: one event per type, with fixed ids and timestamps so
// the files are a function of the serialisers and nothing else.
// ------------------------------------------------------------------

const AGENT: &str = "corpus-agent";
const WORKER: &str = "corpus-worker";
const MODEL: &str = "claude-haiku-4-5";

fn fixed(n: u32) -> Uuid {
    Uuid::parse_str(&format!("01990000-0000-7000-8000-{n:012}")).unwrap()
}

fn invocation() -> Uuid {
    fixed(1000)
}

fn runtime() -> Uuid {
    fixed(2000)
}

fn llm_call() -> Uuid {
    fixed(4001)
}

fn at(n: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 7, 12, 0, 0).unwrap() + chrono::Duration::seconds(n.into())
}

/// Pin the envelope fields `Event::new` mints from the clock and the
/// id generator; everything else is what the constructor wrote.
fn stamp(mut event: Event, n: u32) -> Event {
    event.envelope.event_id = fixed(n);
    event.envelope.timestamp = at(n);
    event
}

fn agent_event(n: u32, payload: EventPayload) -> Event {
    stamp(
        Event::new(AgentId::new(AGENT).unwrap(), invocation(), payload),
        n,
    )
}

/// An agent event chained to the one before it, as the runner chains
/// every event after `triggered`.
fn chained(n: u32, payload: EventPayload) -> Event {
    agent_event(n, payload).with_parent(fixed(n - 1))
}

fn system_event(n: u32, payload: EventPayload) -> Event {
    stamp(Event::system(runtime(), payload), n)
}

fn worker() -> WorkerId {
    WorkerId::new(WORKER).unwrap()
}

fn tool_call_id() -> ToolCallId {
    ToolCallId::new("toolu_corpus_01").unwrap()
}

fn read_call() -> MessageToolCall {
    MessageToolCall {
        tool_call_id: tool_call_id(),
        tool_name: "read".to_string(),
        parameters: json!({"path": "/docs/README.md"}),
    }
}

fn usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 120,
        output_tokens: 30,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: Some(12),
    }
}

/// One exemplar per event type. Adding a variant to `EventPayload`
/// fails the coverage assertion below until it has one here.
fn exemplars() -> Vec<Event> {
    vec![
        agent_event(
            1,
            EventPayload::Triggered(TriggeredPayload {
                trigger_id: Some(fixed(3000)),
                trigger_source: TriggerSource::Subject,
                trigger_subject: Some("fq.trigger.corpus-agent".to_string()),
                trigger_payload: json!({"topic": "schema versions"}),
                config_snapshot: ConfigSnapshot {
                    name: AGENT.to_string(),
                    model: MODEL.to_string(),
                    system_prompt: "You are the corpus agent.".to_string(),
                    tools: vec!["read".to_string()],
                    sandbox: SandboxSnapshot {
                        fs_read: vec!["/docs".to_string()],
                        ..Default::default()
                    },
                    budget: Some(1.0),
                    ..Default::default()
                },
            }),
        )
        .annotate(annotation_keys::FLAGS, json!(["corpus"])),
        chained(
            2,
            EventPayload::LlmRequest(LlmRequestPayload {
                call_id: llm_call(),
                model: MODEL.to_string(),
                messages: vec![
                    Message::system("You are the corpus agent."),
                    Message::user("Read the docs."),
                    Message::Assistant {
                        parts: vec![
                            AssistantPart::Text {
                                text: "Reading.".to_string(),
                            },
                            AssistantPart::Reasoning(Reasoning {
                                model: MODEL.to_string(),
                                content: ReasoningContent::Plain {
                                    text: "The docs first.".to_string(),
                                },
                            }),
                            AssistantPart::ToolCall(read_call()),
                        ],
                    },
                    Message::ToolResults {
                        results: vec![ToolResult {
                            tool_call_id: tool_call_id(),
                            output: "# Docs".to_string(),
                            is_error: false,
                        }],
                    },
                ],
                tools_available: vec![ToolSchema {
                    name: "read".to_string(),
                    description: "Read a file.".to_string(),
                    parameters_schema: json!({
                        "type": "object",
                        "properties": {"path": {"type": "string"}}
                    }),
                }],
                request_params: RequestParams {
                    effort: Some(Effort::Low),
                    temperature: Some(0.0),
                    max_tokens: Some(256),
                },
                origin: LlmCallOrigin::AgentTurn,
            }),
        ),
        chained(
            3,
            EventPayload::LlmDispatched(LlmDispatchedPayload {
                call_id: llm_call(),
                model: MODEL.to_string(),
            }),
        ),
        chained(
            4,
            EventPayload::LlmResponse(LlmResponsePayload {
                round: 1,
                call_id: llm_call(),
                parts: vec![
                    AssistantPart::Reasoning(Reasoning {
                        model: MODEL.to_string(),
                        content: ReasoningContent::Signed {
                            text: "Then the tool.".to_string(),
                            token: json!({"type": "thinking", "signature": "corpus-signature"}),
                        },
                    }),
                    AssistantPart::ToolCall(read_call()),
                ],
                stop_reason: StopReason::ToolUse,
                usage: usage(),
                origin: LlmCallOrigin::AgentTurn,
            }),
        )
        .with_cost(CostMetadata {
            call_id: llm_call(),
            model: MODEL.to_string(),
            input_tokens: 120,
            output_tokens: 30,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: Some(12),
            input_cost: 0.000096,
            output_cost: 0.00012,
            total_cost: 0.000216,
            cumulative_invocation_cost: 0.000216,
            cumulative_agent_cost: 0.000216,
            origin: LlmCallOrigin::AgentTurn,
            reported_cost: None,
        }),
        chained(
            5,
            EventPayload::LlmFailure(LlmFailurePayload {
                round: 2,
                call_id: fixed(4002),
                model: MODEL.to_string(),
                error_kind: LlmErrorKind::RateLimited,
                error_message: "429 Too Many Requests".to_string(),
                duration_ms: 1500,
                usage: None,
                origin: LlmCallOrigin::AgentTurn,
            }),
        ),
        chained(
            6,
            EventPayload::ToolCall(ToolCallPayload {
                round: 1,
                tool_call_id: tool_call_id(),
                tool_name: "read".to_string(),
                parameters: json!({"path": "/docs/README.md"}),
            }),
        ),
        chained(
            7,
            EventPayload::ToolDispatched(ToolDispatchedPayload {
                tool_call_id: tool_call_id(),
                tool_name: "read".to_string(),
            }),
        ),
        chained(
            8,
            EventPayload::ToolResult(ToolResultPayload {
                round: 1,
                tool_name: "read".to_string(),
                tool_call_id: tool_call_id(),
                output: "# Docs".to_string(),
                is_error: false,
                error_kind: None,
                duration_ms: 3,
            }),
        ),
        chained(
            9,
            EventPayload::HostNotice(HostNoticePayload {
                kind: "resume".to_string(),
                body: format!("{HOST_NOTICE_SENTINEL}Resumed after a restart.</host-notice>"),
            }),
        ),
        chained(
            10,
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".to_string(),
                stuck_call_id: "toolu_corpus_01".to_string(),
                note: "dispatched without a result".to_string(),
            }),
        ),
        chained(
            11,
            EventPayload::InvocationStuck(InvocationStuckPayload {
                last_step_at_ms: 1_788_000_000_000,
                stuck_after_ms: 4_210_000,
                phase: "awaiting_model".to_string(),
                step_index: 7,
            }),
        ),
        chained(
            12,
            EventPayload::InvocationSummary(InvocationSummaryPayload {
                kind: SummaryKind::Outcome,
                summary: "Read the docs and reported success.".to_string(),
            }),
        ),
        chained(
            13,
            EventPayload::Completed(CompletedPayload {
                task_status: TaskStatus::Success,
                result_summary: Some("Read the docs.".to_string()),
                total_llm_calls: 2,
                total_tool_calls: 1,
                total_cost: 0.000216,
                total_duration_ms: 4200,
            }),
        ),
        chained(
            14,
            EventPayload::Failed(FailedPayload {
                error_kind: FailureKind::ToolError,
                error_message: "read: permission denied".to_string(),
                phase: FailurePhase::ToolCall,
                partial_totals: InvocationTotals {
                    total_llm_calls: 2,
                    total_tool_calls: 1,
                    total_cost: 0.000216,
                    total_duration_ms: 4200,
                    sampling_cost: 0.0,
                    elicitation_cost: 0.0,
                },
            }),
        ),
        chained(
            15,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: worker(),
                final_phase: "completed".to_string(),
                final_state_blob: vec![123, 125],
                started_at_ms: 1_788_000_000_000,
                terminal_at_ms: 1_788_000_004_200,
            }),
        ),
        chained(
            16,
            EventPayload::InvocationArchiveAcked(InvocationArchiveAckedPayload {
                worker_id: worker(),
            }),
        ),
        chained(
            17,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: Some("operator: stale".to_string()),
            }),
        ),
        chained(
            18,
            EventPayload::InvocationOperatorResumed(InvocationOperatorResumedPayload {
                completed_call_ids: vec!["toolu_corpus_01".to_string()],
                reason: Some("operator: result recovered".to_string()),
            }),
        ),
        system_event(
            19,
            EventPayload::SystemStartup(SystemStartupPayload {
                runtime_id: runtime(),
                version: "0.1.0".to_string(),
                nats_url: "nats://127.0.0.1:4222".to_string(),
                agents_loaded: 1,
                pricing_entries: 12,
            }),
        ),
        system_event(
            20,
            EventPayload::SystemShutdown(SystemShutdownPayload {
                runtime_id: runtime(),
                reason: "ctrl_c".to_string(),
                clean: true,
            }),
        ),
        system_event(
            21,
            EventPayload::SystemTaskFailed(SystemTaskFailedPayload {
                runtime_id: runtime(),
                task_name: "projection_consumer".to_string(),
                error_message: "store error: disk full".to_string(),
            }),
        ),
        system_event(
            22,
            EventPayload::SystemRecovery(SystemRecoveryPayload {
                runtime_id: runtime(),
                worker_id: WORKER.to_string(),
                safe_resume: 1,
                safe_replay: 0,
                ambiguous: 0,
                total: 1,
            }),
        ),
        system_event(
            23,
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: worker(),
                last_step_at_ms: Some(1_788_000_000_000),
            }),
        ),
        system_event(
            24,
            EventPayload::WorkerOrphaned(WorkerOrphanedPayload {
                worker_id: worker(),
                last_heartbeat_ms: 1_788_000_000_000,
            }),
        ),
        system_event(
            25,
            EventPayload::McpServerLog(McpServerLogPayload {
                server: "docs".to_string(),
                level: "info".to_string(),
                logger: Some("docs.index".to_string()),
                data: json!({"message": "indexed 3 files"}),
            }),
        ),
    ]
}

fn file_for(event: &Event) -> String {
    format!("{}.json", event.payload.event_type())
}

/// Every event type this build writes has a committed file, the file is
/// what the serialisers write today, and the consumer's parse boundary
/// admits it into a projection store.
#[tokio::test]
async fn every_current_event_type_has_a_file_that_projects() {
    let dir = corpus_dir("v3");
    let exemplars = exemplars();

    // Coverage: one exemplar per kind the vocabulary declares, the
    // landing pad excepted (nothing writes it). A new variant fails
    // here until it has an exemplar, and so a file.
    let covered: BTreeSet<EventKind> = exemplars
        .iter()
        .map(|e| EventKind::from(&e.payload))
        .collect();
    let declared: BTreeSet<EventKind> = EventKind::iter()
        .filter(|k| *k != EventKind::Unknown)
        .collect();
    let missing: Vec<&EventKind> = declared.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "every event type needs a corpus exemplar; add one for {missing:?} and run \
         UPDATE_CORPUS=1 cargo test -p fq-runtime --test event_corpus"
    );
    assert_eq!(covered.len(), exemplars.len(), "one exemplar per kind");

    if std::env::var_os("UPDATE_CORPUS").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        for event in &exemplars {
            let json = serde_json::to_string_pretty(&serde_json::to_value(event).unwrap()).unwrap();
            std::fs::write(dir.join(file_for(event)), format!("{json}\n")).unwrap();
        }
    }

    // The committed set is exactly the exemplar set: a stale file for a
    // type that no longer exists is as wrong as a missing one.
    let expected_files: BTreeSet<String> = exemplars.iter().map(file_for).collect();
    let actual_files: BTreeSet<String> = json_files(&dir)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        actual_files, expected_files,
        "tests/corpus/events/v3 must hold one file per event type; run \
         UPDATE_CORPUS=1 cargo test -p fq-runtime --test event_corpus"
    );

    let scratch = tempfile::tempdir().unwrap();
    let store = ProjectionStore::open(&scratch.path().join("projection.db"))
        .await
        .expect("open a projection store");
    for (i, exemplar) in exemplars.iter().enumerate() {
        let seq = i as u64 + 1;
        let path = dir.join(file_for(exemplar));
        let bytes = std::fs::read(&path).unwrap();
        let event = match admit(&bytes, &exemplar.subject(), Some(seq)) {
            Admission::Event(event) => event,
            other => {
                panic!("{path:?} must be admitted by the consumer's parse boundary, got {other:?}")
            }
        };
        // The file is what the serialisers write today, compared as
        // values because key order depends on which packages were
        // compiled in. A shape change without a version bump lands
        // here, as a diff against committed history.
        let committed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            committed,
            "{path:?} no longer round-trips through the current serialisers; a deliberate \
             shape change needs a schema version bump, and then UPDATE_CORPUS=1"
        );
        assert_eq!(event.envelope.schema_version, SCHEMA_VERSION);
        assert_eq!(
            EventKind::from(&event.payload),
            EventKind::from(&exemplar.payload),
            "{path:?} reads back as the type it was written as"
        );
        store
            .insert_event(&event, Some(seq))
            .await
            .unwrap_or_else(|e| panic!("{path:?} must project: {e}"));
    }
}

/// Every v1 and v2 file is refused for its version — never as
/// malformed, and never admitted. Either of those would be the silent
/// ack: malformed is acked as poison, and admitted is projected as
/// if it were current history.
#[test]
fn every_older_version_is_refused_for_its_version_and_never_admitted() {
    for (version, expected) in [("v1", 1u32), ("v2", 2u32)] {
        let files = json_files(&corpus_dir(version));
        assert!(!files.is_empty(), "the {version} corpus is empty");
        for path in files {
            let bytes = std::fs::read(&path).unwrap();
            let subject = format!("corpus.{version}");
            match admit(&bytes, &subject, Some(7)) {
                Admission::Halt(on) => {
                    assert_eq!(
                        on.schema_version, expected,
                        "{path:?}: the version found is the one the envelope declared"
                    );
                    assert_eq!(
                        on.supported,
                        SUPPORTED_SCHEMA_VERSIONS.to_vec(),
                        "{path:?}: the versions this build reads"
                    );
                    assert_eq!(on.subject, subject);
                    assert_eq!(on.stream_seq, Some(7));
                    assert!(
                        on.event_id.is_some(),
                        "{path:?}: the id is read off the envelope so the message can be found"
                    );
                }
                Admission::AckMalformed(err) => panic!(
                    "{path:?}: a v{expected} event took the poison path ({err}); acking it \
                     is the silent loss the boundary exists to stop"
                ),
                Admission::Event(event) => panic!(
                    "{path:?}: a v{expected} event was admitted as `{}`; projecting it as \
                     current history is the silent loss the boundary exists to stop",
                    event.payload.event_type()
                ),
            }
            assert!(
                matches!(
                    Event::from_wire(&bytes),
                    Err(EventParseError::UnsupportedSchemaVersion { found, .. }) if found == expected
                ),
                "{path:?}: the typed boundary agrees with the admission"
            );
        }
    }
}
