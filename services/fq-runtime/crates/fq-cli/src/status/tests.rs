use super::*;

use fq_runtime::health::{ConsumerHealth, StreamHealth};

fn config() -> StatusConfig {
    StatusConfig {
        nats_url: "nats://localhost:4222".to_string(),
        agents_dir: PathBuf::from("/srv/agents"),
        cache_dir: PathBuf::from("/var/lib/fq"),
        edge: "127.0.0.1:8787".to_string(),
    }
}

fn stores(initialised: bool) -> StatusStores {
    StatusStores {
        worker_path: PathBuf::from("/var/lib/fq/worker.db"),
        control_plane_path: PathBuf::from("/var/lib/fq/control-plane.db"),
        projection_path: PathBuf::from("/var/lib/fq/projection.db"),
        legacy_events_db: None,
        initialised,
    }
}

fn report() -> StatusReport {
    StatusReport {
        version: "0.1.0+deadbee".to_string(),
        streams: vec![StreamHealth::Available {
            stream: "fq-events".to_string(),
            messages: 12,
            bytes: 2048,
            first_seq: 1,
            last_seq: 12,
            consumer: ConsumerHealth::Active {
                name: "fq-projector".to_string(),
                delivered: 12,
                lag: 0,
                ack_pending: 0,
                num_pending: 0,
                num_redelivered: 0,
            },
        }],
        registry: StatusRegistry {
            agents: 2,
            load_errors: Vec::new(),
        },
        projection_rows: 10,
        recovery: fq_runtime::views::RecoveryView::default(),
    }
}

fn answered() -> StatusDocument {
    StatusDocument {
        config: config(),
        stores: stores(true),
        daemon: Some(report()),
        daemon_error: None,
    }
}

fn unreachable() -> StatusDocument {
    StatusDocument {
        config: config(),
        stores: stores(true),
        daemon: None,
        daemon_error: Some(
            "could not reach the edge at 127.0.0.1:8787: Connection refused (os error 111)"
                .to_string(),
        ),
    }
}

// ------------------------------------------------------------------
// The degraded answer. This is the contract that decides the verb: an
// operator runs `fq status` *because* something is wrong, so what it
// prints when the daemon is the thing that is wrong is not a fallback
// path — it is the main one.
// ------------------------------------------------------------------

/// An unreachable daemon is reported as a finding, with the reason
/// verbatim, and never as a missing report.
#[test]
fn an_unreachable_daemon_is_rendered_as_the_finding() {
    let out = render_status_human(&unreachable());
    assert!(
        out.contains("✗ no daemon answered at 127.0.0.1:8787"),
        "got:\n{out}"
    );
    assert!(out.contains("Connection refused"), "got:\n{out}");
    assert!(out.contains("that is the finding"), "got:\n{out}");
}

/// It says which questions went unanswered. A reader must not have to
/// infer "the stream section is missing, therefore…" — absence with no
/// explanation is indistinguishable from a healthy system that happens
/// to have nothing to say.
#[test]
fn the_degraded_answer_names_what_is_absent() {
    let out = render_status_human(&unreachable());
    for absent in ["stream health", "projection rows", "recovery state"] {
        assert!(
            out.contains(absent),
            "the degraded answer must name `{absent}` as the daemon's to give; got:\n{out}"
        );
    }
    // And it must not print a stale-looking value for any of them.
    assert!(!out.contains("Recovery state\n"), "got:\n{out}");
    assert!(!out.contains("Stream:"), "got:\n{out}");
}

/// What survives with no daemon: the configuration this client
/// resolved, and whether the store files that configuration names
/// exist. Both are local facts, so both are still answered.
#[test]
fn what_is_local_still_answers_with_no_daemon() {
    let out = render_status_human(&unreachable());
    assert!(out.contains("nats://localhost:4222"), "got:\n{out}");
    assert!(out.contains("/srv/agents"), "got:\n{out}");
    assert!(out.contains("/var/lib/fq/worker.db"), "got:\n{out}");
    assert!(out.contains("/var/lib/fq/control-plane.db"), "got:\n{out}");
    assert!(out.contains("/var/lib/fq/projection.db"), "got:\n{out}");
    assert!(
        out.contains("initialised — all three store files exist"),
        "got:\n{out}"
    );
}

/// The store block does not depend on the daemon, so it must not
/// change when one answers. If these two ever diverge, one of the
/// renderings has started reporting something it did not measure.
#[test]
fn the_store_block_is_the_same_either_way() {
    let with = render_status_human(&answered());
    let without = render_status_human(&unreachable());
    let block = |s: &str| {
        s.split("\nStores\n")
            .nth(1)
            .expect("a Stores block")
            .split("\n\n")
            .next()
            .expect("the block")
            .trim_end()
            .to_string()
    };
    assert_eq!(block(&with), block(&without));
}

/// A store directory the daemon has never initialised keeps its
/// original remedy line — the one case where `fq status` tells the
/// operator to start a runtime rather than report on one.
#[test]
fn an_uninitialised_store_says_how_to_create_it() {
    let doc = StatusDocument {
        stores: stores(false),
        ..unreachable()
    };
    let out = render_status_human(&doc);
    assert!(
        out.contains("not initialised (start `fqd` to create)"),
        "got:\n{out}"
    );
}

// ------------------------------------------------------------------
// The answered rendering.
// ------------------------------------------------------------------

#[test]
fn an_answered_status_renders_every_section() {
    let out = render_status_human(&answered());
    for section in [
        "Config",
        "Daemon",
        "Stream: fq-events",
        "Stores",
        "Recovery state",
    ] {
        assert!(out.contains(section), "missing {section}; got:\n{out}");
    }
    assert!(out.contains("✓ answered at 127.0.0.1:8787"), "got:\n{out}");
    assert!(
        out.contains("version:          0.1.0+deadbee"),
        "got:\n{out}"
    );
    assert!(out.contains("projection rows:  10"), "got:\n{out}");
    assert!(out.contains("All clear."), "got:\n{out}");
}

/// A daemon holding a definition it could not parse is the registry
/// fact an operator most needs: the agent they expect to be running is
/// not. It is named, not merely counted.
#[test]
fn a_rejected_definition_is_named_under_the_agent_count() {
    let out = render_registry_human(&StatusRegistry {
        agents: 2,
        load_errors: vec!["failed to parse /srv/agents/notes.md: missing frontmatter".to_string()],
    });
    assert!(out.contains("2 loaded, 1 rejected"), "got:\n{out}");
    assert!(out.contains("/srv/agents/notes.md"), "got:\n{out}");
}

#[test]
fn a_clean_registry_says_only_what_it_loaded() {
    let out = render_registry_human(&StatusRegistry {
        agents: 3,
        load_errors: Vec::new(),
    });
    assert_eq!(out, "  agents:           3 loaded\n");
}

// ------------------------------------------------------------------
// Stream health. Six shapes, and the golden could only ever show one
// of them: the fixture store no daemon had touched had no streams, so
// every `status_*` golden pinned `Unavailable` and nothing else. These
// cover the rest without a broker.
// ------------------------------------------------------------------

fn active(lag: u64) -> StreamHealth {
    StreamHealth::Available {
        stream: "fq-events".to_string(),
        messages: 40,
        bytes: 4096,
        first_seq: 1,
        last_seq: 40,
        consumer: ConsumerHealth::Active {
            name: "fq-projector".to_string(),
            delivered: 40u64.saturating_sub(lag),
            lag,
            ack_pending: 0,
            num_pending: 0,
            num_redelivered: 0,
        },
    }
}

#[test]
fn a_consumers_verdict_follows_its_lag() {
    assert!(render_stream_health_human(&active(0)).contains("✓ caught up"));
    assert!(render_stream_health_human(&active(3)).contains("◐ slightly behind"));
    assert!(render_stream_health_human(&active(99)).contains("✗ lagging"));
}

#[test]
fn an_unavailable_stream_renders_its_reason() {
    let out = render_stream_health_human(&StreamHealth::Unavailable {
        stream: "fq-triggers".to_string(),
        error: "stream not found".to_string(),
    });
    assert!(out.contains("Stream: fq-triggers"), "got:\n{out}");
    assert!(out.contains("✗ stream not found"), "got:\n{out}");
}

#[test]
fn a_missing_consumer_says_nothing_has_initialised_it() {
    let out = render_stream_health_human(&StreamHealth::Available {
        stream: "fq-events".to_string(),
        messages: 0,
        bytes: 0,
        first_seq: 0,
        last_seq: 0,
        consumer: ConsumerHealth::Missing {
            name: "fq-projector".to_string(),
        },
    });
    assert!(out.contains("not present"), "got:\n{out}");
}

/// Retry pressure is the reason these counters are on the report at
/// all, so a non-zero one must reach the operator's screen.
#[test]
fn outstanding_redeliveries_are_rendered_with_their_bound() {
    let out = render_stream_health_human(&StreamHealth::Available {
        stream: "fq-triggers".to_string(),
        messages: 5,
        bytes: 512,
        first_seq: 1,
        last_seq: 5,
        consumer: ConsumerHealth::Active {
            name: "fq-dispatcher".to_string(),
            delivered: 5,
            lag: 0,
            ack_pending: 2,
            num_pending: 1,
            num_redelivered: 3,
        },
    });
    assert!(out.contains("ack pending:    2"), "got:\n{out}");
    assert!(out.contains("num pending:    1"), "got:\n{out}");
    assert!(out.contains("redelivered:    3"), "got:\n{out}");
    assert!(
        out.contains(&format!("bound {}", fq_runtime::bus::TRIGGER_MAX_DELIVER)),
        "got:\n{out}"
    );
}

// ------------------------------------------------------------------
// The recovery block.
// ------------------------------------------------------------------

#[test]
fn render_recovery_guidance_all_clear() {
    let out = render_recovery_guidance(0, 0);
    assert!(out.contains("All clear"), "got: {out:?}");
    // No command hints when nothing's pending.
    assert!(
        !out.contains("fq invocation"),
        "should not hint commands: {out:?}"
    );
    assert!(
        !out.contains("fq workers"),
        "should not hint commands: {out:?}"
    );
}

#[test]
fn render_recovery_guidance_for_ambiguous_only() {
    let out = render_recovery_guidance(3, 0);
    assert!(out.contains("Ambiguous invocations: 3"));
    assert!(out.contains("fq invocation list --status=ambiguous"));
    assert!(out.contains("fq invocation drop"));
    assert!(!out.contains("Stale workers"), "got: {out:?}");
    assert!(!out.contains("All clear"));
}

#[test]
fn render_recovery_guidance_for_stale_only() {
    let out = render_recovery_guidance(0, 2);
    assert!(out.contains("Stale workers: 2"));
    assert!(out.contains("fq workers list --stale-only"));
    // Inspection is offered; removal is not. The retired `fq workers
    // prune` must not come back as advice.
    assert!(!out.contains("prune"), "got: {out:?}");
    assert!(out.contains("retention sweep"));
    assert!(!out.contains("Ambiguous"), "got: {out:?}");
    assert!(!out.contains("All clear"));
}

#[test]
fn render_recovery_guidance_for_both() {
    let out = render_recovery_guidance(1, 1);
    assert!(out.contains("Ambiguous invocations: 1"));
    assert!(out.contains("Stale workers: 1"));
    assert!(out.contains("fq invocation drop"));
    assert!(out.contains("fq workers list --stale-only"));
    assert!(!out.contains("prune"), "got: {out:?}");
}
