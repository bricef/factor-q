use super::*;

use fq_ops::health::{ConsumerHealth, StreamHealth};

fn config() -> StatusConfig {
    StatusConfig {
        edge: "127.0.0.1:8787".to_string(),
    }
}

fn stores(initialised: bool) -> StatusStores {
    StatusStores {
        worker_path: "/var/lib/fq/worker.db".to_string(),
        control_plane_path: "/var/lib/fq/control-plane.db".to_string(),
        projection_path: "/var/lib/fq/projection.db".to_string(),
        legacy_events_db: None,
        initialised,
    }
}

fn report() -> StatusReport {
    StatusReport {
        mcp_servers: Vec::new(),
        version: "0.1.0+deadbee".to_string(),
        drain_deadline_ms: 180_000,
        stuck_after_ms: 4_210_000,
        stores: stores(true),
        streams: vec![StreamHealth::Available {
            stream: "fq-events".to_string(),
            messages: 12,
            bytes: 2048,
            first_seq: 1,
            last_seq: 12,
            consumers: vec![ConsumerHealth::Active {
                name: "fq-projector".to_string(),
                delivered: 12,
                lag: 0,
                ack_pending: 0,
                num_pending: 0,
                num_redelivered: 0,
                redeliveries: 0,
                stuck: false,
                malformed_acked: 0,
            }],
        }],
        registry: StatusRegistry {
            agents: 2,
            load_errors: Vec::new(),
        },
        projection_rows: 10,
        projection_rebuild: None,
        recovery: fq_ops::views::RecoveryView::default(),
    }
}

fn answered() -> StatusDocument {
    StatusDocument {
        config: config(),
        daemon: Some(report()),
        daemon_error: None,
    }
}

fn unreachable() -> StatusDocument {
    StatusDocument {
        config: config(),
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

/// What survives with no daemon is the configuration this client
/// resolved — and only that. The edge address is the one piece of
/// configuration a client owns, so it is answered whatever happened
/// on the wire.
#[test]
fn what_is_local_still_answers_with_no_daemon() {
    let out = render_status_human(&unreachable());
    assert!(
        out.contains("edge:             127.0.0.1:8787"),
        "got:\n{out}"
    );
}

/// The store paths are the *daemon's*, so a client with none
/// answering says so rather than printing where its own machine
/// would have kept them.
///
/// It used to derive them from its own config, which is right only
/// while the client and the daemon share a host and silently wrong
/// the moment they do not — a guess indistinguishable, on screen,
/// from a measurement. Absence with an explanation is the same
/// discipline the stream and recovery sections already follow.
#[test]
fn no_daemon_means_no_guess_at_the_store_paths() {
    let out = render_status_human(&unreachable());
    assert!(!out.contains("\nStores\n"), "got:\n{out}");
    for guessed in ["worker.db", "control-plane.db", "projection.db"] {
        assert!(
            !out.contains(guessed),
            "the client must not guess `{guessed}`; got:\n{out}"
        );
    }
    assert!(
        out.contains("the store paths too: they are the daemon's"),
        "the absence must be explained, not merely left blank; got:\n{out}"
    );
}

/// …and when one does answer, the block is what that daemon reported
/// about its own files.
#[test]
fn the_store_block_is_the_answering_daemons() {
    let out = render_status_human(&answered());
    assert!(out.contains("\nStores\n"), "got:\n{out}");
    for path in [
        "/var/lib/fq/worker.db",
        "/var/lib/fq/control-plane.db",
        "/var/lib/fq/projection.db",
    ] {
        assert!(out.contains(path), "got:\n{out}");
    }
    assert!(
        out.contains("initialised — all three store files exist"),
        "got:\n{out}"
    );
}

/// A report whose stores are not all on disk keeps the original
/// remedy line — the one case where `fq status` tells the operator to
/// start a runtime rather than report on one.
///
/// The finding is the daemon's now, so it arrives on the report
/// rather than from a client `stat` of its own filesystem. A daemon
/// that answered has normally opened all three (so it reports
/// `initialised`), but it re-checks per call rather than caching, and
/// a store file removed under a running daemon is exactly what that
/// re-check is for.
#[test]
fn an_uninitialised_store_says_how_to_create_it() {
    let doc = StatusDocument {
        daemon: Some(StatusReport {
            stores: stores(false),
            ..report()
        }),
        ..answered()
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
        consumers: vec![ConsumerHealth::Active {
            name: "fq-projector".to_string(),
            delivered: 40u64.saturating_sub(lag),
            lag,
            ack_pending: 0,
            num_pending: 0,
            num_redelivered: 0,
            redeliveries: 0,
            stuck: false,
            malformed_acked: 0,
        }],
    }
}

fn rebuild(consumer_reset_pending: bool, in_progress: bool) -> fq_ops::surface::ProjectionRebuild {
    fq_ops::surface::ProjectionRebuild {
        started_at: "2026-09-07T10:00:00+00:00".to_string(),
        reason: "schema version 0 -> 1".to_string(),
        from_version: Some(0),
        schema_version: 1,
        target_seq: Some(4_242),
        floor_seq: Some(4_000),
        carried_below_floor: 3,
        consumer_reset_pending,
        in_progress,
    }
}

/// A projection that was never rebuilt prints no rebuild line at all —
/// the common case is not a finding — and one that was says where the
/// replay stands: resetting, replaying from the floor to a named
/// position with the older rows carried as-is, or done and saying the
/// same in the past tense. A record with no floor (the reset not yet
/// performed) says what it can; a floor past the target says nothing
/// was replayed.
#[test]
fn the_rebuild_line_follows_the_replay() {
    assert!(
        !render_status_human(&answered()).contains("projection rebuild"),
        "no rebuild, no line"
    );

    let with = |r: fq_ops::surface::ProjectionRebuild| {
        render_status_human(&StatusDocument {
            daemon: Some(StatusReport {
                projection_rebuild: Some(r),
                ..report()
            }),
            ..answered()
        })
    };
    let resetting = with(rebuild(true, true));
    assert!(
        resetting.contains("projection rebuild: in progress (the consumer is resetting)"),
        "{resetting}"
    );
    let replaying = with(rebuild(false, true));
    assert!(
        replaying.contains(
            "in progress (replaying from sequence 4000 to 4242; 3 older rows carried as-is)"
        ),
        "{replaying}"
    );
    assert!(
        replaying.contains("schema version 0 -> 1"),
        "the reason is on the line: {replaying}"
    );
    let done = with(rebuild(false, false));
    assert!(
        done.contains(
            "projection rebuild: complete (replayed from sequence 4000; 3 older rows carried \
             as-is) — started 2026-09-07T10:00:00+00:00"
        ),
        "{done}"
    );

    let without_floor = with(fq_ops::surface::ProjectionRebuild {
        floor_seq: None,
        carried_below_floor: 0,
        ..rebuild(false, true)
    });
    assert!(
        without_floor.contains("in progress (replaying to stream sequence 4242)"),
        "{without_floor}"
    );
    let nothing_replayed = with(fq_ops::surface::ProjectionRebuild {
        floor_seq: Some(4_243),
        carried_below_floor: 9,
        ..rebuild(false, false)
    });
    assert!(
        nothing_replayed.contains(
            "complete (nothing to replay: the stream holds no event this build reads; 9 rows \
             carried as-is)"
        ),
        "{nothing_replayed}"
    );
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
        consumers: vec![ConsumerHealth::Missing {
            name: "fq-projector".to_string(),
        }],
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
        consumers: vec![ConsumerHealth::Active {
            name: "fq-dispatcher".to_string(),
            delivered: 5,
            lag: 0,
            ack_pending: 2,
            num_pending: 1,
            num_redelivered: 3,
            redeliveries: 0,
            stuck: false,
            malformed_acked: 0,
        }],
    });
    assert!(out.contains("ack pending:    2"), "got:\n{out}");
    assert!(out.contains("num pending:    1"), "got:\n{out}");
    assert!(out.contains("redelivered:    3"), "got:\n{out}");
    assert!(
        out.contains(&format!(
            "trigger bound {}",
            fq_ops::surface::TRIGGER_MAX_DELIVER
        )),
        "got:\n{out}"
    );
}

/// A consumer halted on an event it cannot read is reported as halted
/// — not as lagging, which is what its JetStream figures would say —
/// with the version found, the versions this build reads, and where
/// the event sits; and the malformed count it acked before halting is
/// a separate line, because poison and unreadable history are two
/// different facts.
#[test]
fn a_halted_consumer_names_the_version_gap_and_locates_the_event() {
    let out = render_stream_health_human(&StreamHealth::Available {
        stream: "fq-events".to_string(),
        messages: 50,
        bytes: 4096,
        first_seq: 1,
        last_seq: 50,
        consumers: vec![ConsumerHealth::Halted {
            name: "fq-projector".to_string(),
            halted_on: fq_ops::health::UnsupportedEvent {
                schema_version: 2,
                supported: vec![3],
                event_id: Some("01990000-0000-7000-8000-000000000002".to_string()),
                subject: "fq.agent.researcher.triggered".to_string(),
                stream_seq: Some(42),
            },
            malformed_acked: 1,
        }],
    });
    assert!(
        out.contains("consumer fq-projector: ✗ halted on schema_version 2 (this build reads [3])"),
        "got:\n{out}"
    );
    assert!(
        out.contains(
            "event 01990000-0000-7000-8000-000000000002 on fq.agent.researcher.triggered, seq 42"
        ),
        "got:\n{out}"
    );
    assert!(out.contains("unacked"), "got:\n{out}");
    assert!(out.contains("malformed acked: 1"), "got:\n{out}");
    assert!(!out.contains("lagging"), "a halt is not a lag: {out}");
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
