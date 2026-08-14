use super::*;

/// Write `files` into a fresh directory and load a registry from them,
/// the way the daemon does at boot.
fn registry_of(files: &[(&str, &str)]) -> (tempfile::TempDir, fq_runtime::AgentRegistry) {
    let dir = tempfile::tempdir().expect("agents dir");
    let mut registry = fq_runtime::AgentRegistry::new();
    for (name, body) in files {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("write definition");
        registry.load_file(&path);
    }
    (dir, registry)
}

const RESEARCHER: &str = "---\nname: researcher\nmodel: claude-haiku-4-5\n---\n\nYou research.\n";

/// The census counts what the daemon would run, and carries every
/// rejection verbatim — the operator's most useful registry fact is
/// the definition that is *not* loaded.
#[test]
fn the_registry_census_counts_the_loaded_and_names_the_rejected() {
    let (dir, registry) = registry_of(&[
        ("researcher.md", RESEARCHER),
        ("notes.md", "# Scratch notes\n\nNot an agent definition.\n"),
    ]);
    let census = StatusRegistry::of(&registry);

    assert_eq!(census.agents, 1, "one definition parsed: {census:?}");
    assert_eq!(census.load_errors.len(), 1, "got: {census:?}");
    assert!(
        census.load_errors[0].contains(&dir.path().join("notes.md").display().to_string()),
        "a rejection must name its file so the operator can go fix it; got: {census:?}"
    );
}

/// An empty registry censuses to zero and an empty list, never to
/// absent fields: "this daemon loaded no agents" is a finding, and a
/// reader must be able to tell it from "not checked".
#[test]
fn an_empty_registry_is_a_zero_not_a_gap() {
    let census = StatusRegistry::of(&fq_runtime::AgentRegistry::new());
    assert_eq!(census, StatusRegistry::default());
    assert_eq!(census.agents, 0);
    assert!(census.load_errors.is_empty());
}

/// The report is a value that survives the wire — the declared output
/// type is what the client deserialises, so a field that serialises
/// but does not come back is a break the edge would only find in
/// production.
#[test]
fn the_report_roundtrips_through_its_declared_shape() {
    let report = StatusReport {
        version: "0.1.0+deadbee".to_string(),
        streams: vec![
            fq_runtime::health::StreamHealth::Unavailable {
                stream: "fq-events".to_string(),
                error: "stream not found".to_string(),
            },
            fq_runtime::health::StreamHealth::Available {
                stream: "fq-triggers".to_string(),
                messages: 3,
                bytes: 512,
                first_seq: 1,
                last_seq: 3,
                consumer: fq_runtime::health::ConsumerHealth::Active {
                    name: "fq-dispatcher".to_string(),
                    delivered: 3,
                    lag: 0,
                    ack_pending: 0,
                    num_pending: 0,
                    num_redelivered: 0,
                },
            },
        ],
        registry: StatusRegistry {
            agents: 2,
            load_errors: vec!["failed to parse notes.md".to_string()],
        },
        projection_rows: 10,
        recovery: fq_runtime::views::RecoveryView {
            ambiguous: 1,
            stale_workers: 2,
            stale_worker_ids: vec!["worker-alpha".to_string(), "worker-beta".to_string()],
        },
    };

    let json = serde_json::to_value(&report).expect("serialise");
    let back: StatusReport = serde_json::from_value(json).expect("the declared output shape");
    assert_eq!(back, report);
}
