//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390).

use super::*;
use crate::test_support::corpus::corpus_bytes;
use crate::test_support::nats::test_nats;
use futures::StreamExt;
use uuid::Uuid;

/// The committed v2 `llm_request` — history from before the reasoning
/// break (#510 bumped the envelope 2 → 3), whose `messages` carry the
/// flat `{role, content, tool_calls}` shape today's tagged `Message`
/// reads as `missing field kind`. The exact bytes the live instance
/// still holds.
fn legacy_llm_request() -> Vec<u8> {
    corpus_bytes("v2")
        .into_iter()
        .find(|(name, _)| name == "llm_request.json")
        .expect("the v2 llm_request corpus file")
        .1
}

fn triggered(agent: &crate::agent::AgentId, invocation: Uuid) -> Event {
    Event::new(
        agent.clone(),
        invocation,
        crate::events::EventPayload::Triggered(crate::events::TriggeredPayload {
            trigger_id: None,
            trigger_source: crate::events::TriggerSource::Manual,
            trigger_subject: None,
            trigger_payload: serde_json::json!({}),
            config_snapshot: crate::events::ConfigSnapshot {
                name: agent.as_str().to_string(),
                model: "claude-haiku-4-5".into(),
                system_prompt: "probe".into(),
                tools: vec![],
                sandbox: crate::events::SandboxSnapshot::default(),
                budget: None,
                ..Default::default()
            },
        }),
    )
}

/// **#673.** A message in a schema version this build does not read is
/// yielded with no event and its sequence intact — never an error over
/// the whole tail.
///
/// This is the property the transcript page rests on. `turn.list` scans
/// an agent's *entire* subject from sequence 1 and filters by
/// invocation afterwards, so it meets every event that agent ever
/// wrote; when the tail failed on the first message it could not read,
/// one pre-#510 event refused the listing for every invocation of that
/// agent — including ones that ran minutes ago. The sequence has to
/// survive too, because a scan bounded by a tip terminates on seeing
/// it and would otherwise wait for a message it never gets handed.
#[tokio::test]
async fn history_this_build_cannot_read_is_yielded_not_fatal() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = crate::agent::AgentId::new(format!("tail-{}", Uuid::now_v7().simple())).unwrap();
    let subject = format!("fq.agent.{}.llm.request", agent.as_str());

    // History first, current second — the live stream's shape.
    let legacy_seq = bus
        .jetstream()
        .publish(subject, legacy_llm_request().into())
        .await
        .expect("publish v2 history")
        .await
        .expect("v2 history stored")
        .sequence;
    let invocation = Uuid::now_v7();
    let current_seq = bus
        .publish(&triggered(&agent, invocation))
        .await
        .expect("publish a current event");
    assert!(legacy_seq < current_seq, "history is below the current tip");

    let mut tail = bus
        .events_from(&format!("fq.agent.{}.>", agent.as_str()), 1)
        .await
        .expect("open the tail");

    let first = tail
        .next()
        .await
        .expect("a first message")
        .expect("the unreadable one must not fail the tail");
    assert_eq!(first.seq, legacy_seq);
    assert!(
        first.event.is_none(),
        "a v2 message is not an event this build reads"
    );

    let second = tail
        .next()
        .await
        .expect("a second message")
        .expect("the current event reads");
    assert_eq!(second.seq, current_seq);
    assert_eq!(
        second
            .event
            .expect("the current event is readable")
            .envelope
            .invocation_id,
        invocation,
        "the tail carried on past history it could not read"
    );
}

/// Malformed bytes *within* a version this build reads are still an
/// error. The skip above is for well-formed history, not a licence to
/// swallow anything that fails to parse: that would be a bug, and a
/// tail that hides it is the silent loss #409 exists to stop.
#[tokio::test]
async fn malformed_bytes_in_a_supported_version_still_error() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = crate::agent::AgentId::new(format!("tail-{}", Uuid::now_v7().simple())).unwrap();
    let subject = format!("fq.agent.{}.llm.request", agent.as_str());

    // The current version, and a body that is not any payload shape.
    let bytes = serde_json::to_vec(&serde_json::json!({
        "envelope": {"schema_version": crate::events::SCHEMA_VERSION},
        "payload": {"event_type": "llm_request", "payload": {"nope": true}},
    }))
    .unwrap();
    bus.jetstream()
        .publish(subject, bytes.into())
        .await
        .expect("publish malformed bytes")
        .await
        .expect("malformed bytes stored");

    let mut tail = bus
        .events_from(&format!("fq.agent.{}.>", agent.as_str()), 1)
        .await
        .expect("open the tail");
    let err = tail
        .next()
        .await
        .expect("a message")
        .expect_err("malformed bytes are an error, not a skip");
    assert!(
        matches!(err, BusError::Deserialise(_)),
        "and the error names the read side: {err}"
    );
}
