//! Generated envelopes pin version admission at the wire boundary (#411).

use proptest::prelude::*;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{EventParseError, SUPPORTED_SCHEMA_VERSIONS};
use crate::agent::AgentId;
use crate::events::{CompletedPayload, Event, EventPayload, SCHEMA_VERSION, TaskStatus};

#[derive(Clone, Debug)]
enum VersionCase {
    Number(u32),
    Missing,
    Boolean(bool),
    StringDigits(u32),
    Negative(i64),
    TooLarge(u64),
}

fn versions() -> impl Strategy<Value = VersionCase> {
    prop_oneof![
        5 => any::<u32>().prop_map(VersionCase::Number),
        2 => Just(VersionCase::Number(0)),
        2 => Just(VersionCase::Number(SCHEMA_VERSION)),
        2 => Just(VersionCase::Number(u32::MAX)),
        2 => Just(VersionCase::Missing),
        2 => any::<bool>().prop_map(VersionCase::Boolean),
        2 => any::<u32>().prop_map(VersionCase::StringDigits),
        2 => (-1_000_000i64..0).prop_map(VersionCase::Negative),
        2 => ((u32::MAX as u64 + 1)..=u64::MAX).prop_map(VersionCase::TooLarge),
    ]
}

#[derive(Clone, Debug)]
enum IdCase {
    Absent,
    String(String),
    Garbage(Value),
}

fn event_ids() -> impl Strategy<Value = IdCase> {
    prop_oneof![
        Just(IdCase::Absent),
        "[a-z0-9-]{0,40}".prop_map(IdCase::String),
        any::<bool>().prop_map(|value| IdCase::Garbage(json!(value))),
        any::<i64>().prop_map(|value| IdCase::Garbage(json!(value))),
    ]
}

fn insert_version(object: &mut Map<String, Value>, version: &VersionCase) {
    let value = match version {
        VersionCase::Number(value) => json!(value),
        VersionCase::Missing => return,
        VersionCase::Boolean(value) => json!(value),
        VersionCase::StringDigits(value) => json!(value.to_string()),
        VersionCase::Negative(value) => json!(value),
        VersionCase::TooLarge(value) => json!(value),
    };
    object.insert("schema_version".into(), value);
}

fn insert_id(object: &mut Map<String, Value>, id: &IdCase) {
    match id {
        IdCase::Absent => {}
        IdCase::String(value) => {
            object.insert("event_id".into(), json!(value));
        }
        IdCase::Garbage(value) => {
            object.insert("event_id".into(), value.clone());
        }
    }
}

fn envelope(version: &VersionCase, id: &IdCase, nested: bool) -> Vec<u8> {
    let mut probe = Map::new();
    insert_version(&mut probe, version);
    insert_id(&mut probe, id);

    let value = if nested {
        json!({"envelope": Value::Object(probe), "payload": "deliberately invalid"})
    } else {
        let mut flat = probe;
        flat.insert("payload".into(), json!("deliberately invalid"));
        Value::Object(flat)
    };
    serde_json::to_vec(&value).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn generated_envelopes_are_classified_by_their_declared_version(
        version in versions(),
        id in event_ids(),
        nested in any::<bool>(),
    ) {
        let bytes = envelope(&version, &id, nested);
        let result = Event::from_wire(&bytes);

        match version {
            VersionCase::Number(found) if !SUPPORTED_SCHEMA_VERSIONS.contains(&found) => {
                match result {
                    Err(EventParseError::UnsupportedSchemaVersion { found: actual, supported, .. }) => {
                        prop_assert_eq!(actual, found);
                        prop_assert_eq!(supported, SUPPORTED_SCHEMA_VERSIONS);
                    }
                    other => prop_assert!(false, "expected unsupported, got {:?}", other),
                }
            }
            _ => {
                match result {
                    Err(EventParseError::Malformed(_)) => {}
                    other => prop_assert!(false, "expected malformed, got {:?}", other),
                }
            }
        }
    }

    #[test]
    fn supported_events_round_trip_unchanged(
        supported_index in 0usize..SUPPORTED_SCHEMA_VERSIONS.len(),
        invocation in any::<u128>(),
        calls in any::<u16>(),
    ) {
        let mut event = Event::new(
            AgentId::new("property-agent").unwrap(),
            Uuid::from_u128(invocation),
            EventPayload::Completed(CompletedPayload {
                task_status: TaskStatus::Success,
                result_summary: Some("generated".into()),
                total_llm_calls: u32::from(calls),
                total_tool_calls: 0,
                total_cost: 0.0,
                total_duration_ms: 0,
            }),
        );
        event.envelope.schema_version = SUPPORTED_SCHEMA_VERSIONS[supported_index];

        let wire = serde_json::to_vec(&event).unwrap();
        let parsed = Event::from_wire(&wire).unwrap();
        prop_assert_eq!(serde_json::to_value(parsed).unwrap(), serde_json::to_value(event).unwrap());
    }
}
