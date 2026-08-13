//! Unit tests for [`super`]. Extracted from the parent module so the
//! file that ships is the file you read (#390); `super::*` keeps the
//! same access it had inline.

use super::*;
use crate::control_plane::store::{InvocationArchiveRow, WorkerRow, WorkerStatus};
use crate::worker::store::{DispatchStatus, InvocationStateRow, ToolDispatchRow};

// ---- Pure From-conversion tests (no DB) ----

#[test]
fn worker_row_maps_to_view() {
    let row = WorkerRow {
        worker_id: "w1".into(),
        host: "localhost".into(),
        registered_at: 100,
        last_heartbeat: 200,
        status: WorkerStatus::Stale,
    };
    let view = WorkerView::from(row);
    assert_eq!(view.worker_id, "w1");
    assert_eq!(view.status, "stale");
    assert_eq!(view.last_heartbeat_ms, 200);
}

#[test]
fn tool_dispatch_row_maps_to_view() {
    let row = ToolDispatchRow {
        invocation_id: "i1".into(),
        tool_call_id: "tc1".into(),
        tool_name: "file_read".into(),
        status: DispatchStatus::Completed,
        parameters: "{}".into(),
        result: Some("ok".into()),
        is_error: Some(false),
        intent_at: 1,
        dispatched_at: Some(2),
        completed_at: Some(3),
        seq: None,
    };
    let view = ToolDispatchView::from(row);
    assert_eq!(view.tool_name, "file_read");
    assert_eq!(view.status, "completed");
    assert_eq!(view.is_error, Some(false));
    assert_eq!(view.completed_at_ms, Some(3));
}

#[test]
fn cost_report_totals_across_agents() {
    // Build a report the way `costs()` does, from two agents — one of
    // them the summariser, whose spend is all framework (#466).
    let mut report = CostReport::default();
    for (agent, cost, framework, ins, outs) in [
        ("a", 1.5_f64, 0.0_f64, 10_i64, 20_i64),
        (AgentId::SUMMARY_STR, 2.0, 2.0, 5, 7),
    ] {
        report.total_cost += cost;
        report.total_input_tokens += ins;
        report.total_output_tokens += outs;
        report.framework_cost += framework;
        report.agents.push(CostView {
            agent_id: agent.into(),
            event_count: 1,
            total_cost: cost,
            total_input_tokens: ins,
            total_output_tokens: outs,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            invocation_count: 1,
            framework_cost: framework,
        });
    }
    assert_eq!(report.agents.len(), 2);
    assert!((report.total_cost - 3.5).abs() < f64::EPSILON);
    assert_eq!(report.total_input_tokens, 15);
    assert_eq!(report.total_output_tokens, 27);
    // The report carries the unallocated remainder itself, so the
    // renderer never has to re-derive it from the rows.
    assert!((report.framework_cost - 2.0).abs() < f64::EPSILON);
}

/// The RFC3339 projection timestamp becomes epoch ms on the view;
/// an unparseable value degrades to 0, never a panic.
#[test]
fn invocation_cost_view_parses_rfc3339_start() {
    let summary = |ts: &str| InvocationCostSummary {
        invocation_id: "inv-1".into(),
        first_timestamp: ts.into(),
        event_count: 1,
        total_cost: 0.1,
        total_input_tokens: 10,
        total_output_tokens: 5,
        total_cache_read_tokens: 0,
        total_cache_write_tokens: 0,
    };
    let v = InvocationCostView::from(summary("1970-01-01T00:00:01+00:00"));
    assert_eq!(v.started_at_ms, 1_000);
    let bad = InvocationCostView::from(summary("not-a-time"));
    assert_eq!(bad.started_at_ms, 0);
}

/// The command gist: argv arrays join, strings pass through,
/// absent/odd shapes are None, and the cap truncates on a char
/// boundary with an ellipsis.
#[test]
fn open_tool_command_reads_both_shapes_and_caps() {
    assert_eq!(
        open_tool_command(r#"{"command":["cargo","test","--lib"],"cwd":"/w"}"#),
        Some("cargo test --lib".to_string())
    );
    assert_eq!(
        open_tool_command(r#"{"command":"[\"ls\", \"-la\"]","cwd":"/w"}"#),
        Some("[\"ls\", \"-la\"]".to_string())
    );
    assert_eq!(open_tool_command(r#"{"path":"/tmp/x"}"#), None);
    assert_eq!(open_tool_command(r#"{"command":42}"#), None);
    assert_eq!(open_tool_command("not json"), None);
    assert_eq!(open_tool_command(r#"{"command":[]}"#), None);
    let long = format!(r#"{{"command":["{}"]}}"#, "x".repeat(300));
    let capped = open_tool_command(&long).unwrap();
    assert_eq!(capped.chars().count(), OPEN_TOOL_COMMAND_CAP + 1);
    assert!(capped.ends_with('…'));
}

// ---- DB wiring smoke test (empty, freshly-created stores) ----

/// Create the three stores' schemas in one temp DB file, then open a
/// read-only `Views` over it and assert the query methods wire up and
/// return empty / not-found on an empty database.
#[tokio::test]
async fn open_and_query_empty_db() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());

    // Open each store in write mode once to create its tables in the
    // shared file, then drop the write handles.
    {
        let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        let _ws = WorkerStore::open(&paths.worker).await.unwrap();
        let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
    }

    let views = Views::open(&paths).await.unwrap();

    assert_eq!(views.event_count().await.unwrap(), 0);
    assert!(views.workers().await.unwrap().is_empty());
    assert!(views.events(None, None, None, 50).await.unwrap().is_empty());
    assert!(
        views
            .costs(None, None, false)
            .await
            .unwrap()
            .agents
            .is_empty()
    );
    assert!(
        views
            .agent_costs("no-such-agent", None, 10)
            .await
            .unwrap()
            .is_none()
    );
    assert!(views.failures().await.unwrap().is_empty());
    assert!(views.invocations(None, 50).await.unwrap().is_empty());
    assert!(
        views
            .invocation_index(None, true, 50)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(views.recovery(1_000, 30_000).await.unwrap().ambiguous, 0);
    assert_eq!(
        views
            .executions(1_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap()
            .in_flight,
        0
    );
    assert!(
        views
            .active_invocations(1_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        views
            .invocation(
                "no-such-id",
                1_000,
                30_000,
                DEFAULT_LONG_DISPATCH_THRESHOLD_MS
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(views.worker("no-such-worker").await.unwrap().is_none());
}

/// Archive-only tombstones are written by operator recovery with a reserved
/// agent id; they have no attributable agent on the invocation list.
#[tokio::test]
async fn invocation_index_hides_sentinel_archive_agents() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());
    {
        let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        for (id, agent_id) in [("tombstone", AgentId::OPERATOR_STR), ("real", "agent-a")] {
            cp.insert_archive(&InvocationArchiveRow {
                invocation_id: id.into(),
                agent_id: agent_id.into(),
                final_phase: "failed".into(),
                final_state_blob: vec![],
                started_at: 10,
                terminal_at: 20,
                archived_at: if id == "tombstone" { 30 } else { 40 },
            })
            .await
            .unwrap();
        }
        let _ws = WorkerStore::open(&paths.worker).await.unwrap();
        let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
    }

    let index = Views::open(&paths)
        .await
        .unwrap()
        .invocation_index(None, true, 50)
        .await
        .unwrap();
    assert_eq!(
        index
            .iter()
            .find(|row| row.invocation_id == "tombstone")
            .unwrap()
            .agent_id,
        None
    );
    assert_eq!(
        index
            .iter()
            .find(|row| row.invocation_id == "real")
            .unwrap()
            .agent_id
            .as_deref(),
        Some("agent-a")
    );
}

/// #216: the one-line summary joins onto both invocation surfaces
/// (the active list and the invocation index) from the
/// projection's `invocation_summary` table.
#[tokio::test]
async fn summary_line_joins_onto_invocation_surfaces() {
    use crate::agent::AgentId;
    use crate::events::{Event, EventPayload, InvocationSummaryPayload, SummaryKind};

    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());
    let inv = uuid::Uuid::now_v7();
    {
        let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        let ws = WorkerStore::open(&paths.worker).await.unwrap();
        let proj = ProjectionStore::open(&paths.projection).await.unwrap();
        cp.register_worker("w1", "host", 100).await.unwrap();
        cp.assign_invocation(&inv.to_string(), "w1", 100)
            .await
            .unwrap();
        let row = InvocationStateRow {
            invocation_id: inv.to_string(),
            agent_id: "agent-a".into(),
            schema_version: 1,
            phase: "reducing".into(),
            state_blob: vec![],
            step_index: 1,
            started_at: 100,
            updated_at: 150,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
        };
        ws.upsert_invocation_state(&row).await.unwrap();
        proj.insert_event(
            &Event::new(
                AgentId::summary(),
                inv,
                EventPayload::InvocationSummary(InvocationSummaryPayload {
                    kind: SummaryKind::Progress,
                    summary: "Fixing #7: editing widget.rs".to_string(),
                }),
            ),
            None,
        )
        .await
        .unwrap();
    }

    let views = Views::open(&paths).await.unwrap();

    let active = views
        .active_invocations(200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(
        active[0].summary.as_deref(),
        Some("Fixing #7: editing widget.rs")
    );

    let index = views.invocation_index(None, true, 50).await.unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(
        index[0].summary.as_deref(),
        Some("Fixing #7: editing widget.rs")
    );
}

/// Seed a worker and an in-flight invocation, then read them back through
/// `Views` — exercises the cross-store composition end to end.
#[tokio::test]
async fn reads_back_seeded_worker_and_invocation() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());

    {
        let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        cp.register_worker("w1", "localhost", 100).await.unwrap();

        let ws = WorkerStore::open(&paths.worker).await.unwrap();
        let row = InvocationStateRow {
            invocation_id: "inv-1".into(),
            agent_id: "agent-a".into(),
            schema_version: 1,
            phase: "reducing".into(),
            state_blob: vec![],
            step_index: 3,
            started_at: 100,
            updated_at: 150,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: Some("manual".into()),
            trigger_subject: None,
            trigger_payload: None,
        };
        ws.upsert_invocation_state(&row).await.unwrap();
        ws.write_tool_intent("inv-1", "call-1", "exec", "{}", 160)
            .await
            .unwrap();
        ws.write_tool_dispatched("inv-1", "call-1", 170)
            .await
            .unwrap();
        let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
    }

    let views = Views::open(&paths).await.unwrap();

    let workers = views.workers().await.unwrap();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].worker_id, "w1");
    assert_eq!(workers[0].status, "alive");
    assert_eq!(workers[0].in_flight_count, 0);

    // Worker detail resolves (no ownership rows seeded → owned empty).
    let detail = views.worker("w1").await.unwrap().expect("w1 exists");
    assert_eq!(detail.worker.worker_id, "w1");
    assert!(detail.owned.is_empty());

    // In-flight execution shows up in the executions view...
    let execs = views
        .executions(200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(execs.in_flight, 1);
    assert_eq!(execs.stuck, 0);

    // An open exec dispatch remains working even though the invocation
    // WAL has not advanced for longer than the ordinary stuck threshold.
    let execs_working = views
        .executions(60_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(execs_working.working, 1);
    assert_eq!(execs_working.stuck, 0);

    // ...but is flagged stuck once the dispatch itself is too old.
    let execs_late = views
        .executions(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(execs_late.working, 0);
    assert_eq!(execs_late.stuck, 1);
    assert_eq!(execs_late.stuck_ids, vec!["inv-1".to_string()]);

    // The detail view composes the live WAL state.
    let detail = views
        .invocation("inv-1", 200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap()
        .unwrap();
    let live = detail.live.expect("in-flight invocation has live state");
    assert_eq!(live.phase, "reducing");
    assert_eq!(live.step_index, 3);
    assert_eq!(live.tools.len(), 1);
    assert_eq!(live.tools[0].tool_name, "exec");

    // The active list carries the same WAL row, row-form.
    let active = views
        .active_invocations(200, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].invocation_id, "inv-1");
    assert_eq!(active[0].agent_id, "agent-a");
    assert_eq!(active[0].phase, "reducing");
    assert_eq!(active[0].step_index, 3);
    assert_eq!(active[0].open_tools.len(), 1);
    assert_eq!(active[0].open_tools[0].tool_name, "exec");
    assert_eq!(active[0].open_tools[0].command, None);
    // Fresh open dispatch → the same verdict health counts.
    assert_eq!(active[0].liveness, Liveness::Working);

    // Same row viewed much later: the dispatch has gone stale and
    // the WAL never advanced — the active table says stuck exactly
    // when the health tile does.
    let active_late = views
        .active_invocations(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(active_late[0].liveness, Liveness::Stuck);
}

#[tokio::test]
async fn terminal_coordination_status_hides_orphaned_live_execution() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());
    {
        let cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        cp.upsert_invocation_ownership("inv-dropped", "w1", 100, OwnerStatus::Failed)
            .await
            .unwrap();
        let ws = WorkerStore::open(&paths.worker).await.unwrap();
        ws.upsert_invocation_state(&InvocationStateRow {
            invocation_id: "inv-dropped".into(),
            agent_id: "agent-a".into(),
            schema_version: 1,
            phase: "dispatching_tools".into(),
            state_blob: vec![],
            step_index: 25,
            started_at: 100,
            updated_at: 150,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
        })
        .await
        .unwrap();
        let _projection = ProjectionStore::open(&paths.projection).await.unwrap();
    }

    let views = Views::open(&paths).await.unwrap();
    let executions = views
        .executions(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(executions.in_flight, 0);
    assert_eq!(executions.stuck, 0);
    assert!(
        views
            .active_invocations(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS,)
            .await
            .unwrap()
            .is_empty()
    );
    let detail = views
        .invocation(
            "inv-dropped",
            1_000_000,
            30_000,
            DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(detail.live.is_none());
}

/// An open LLM dispatch counts as working the same way a tool dispatch
/// does (#130) — a long reducer-side model call is not a stuck invocation.
#[tokio::test]
async fn open_llm_dispatch_counts_as_working() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());

    {
        let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        let ws = WorkerStore::open(&paths.worker).await.unwrap();
        let row = InvocationStateRow {
            invocation_id: "inv-llm".into(),
            agent_id: "agent-a".into(),
            schema_version: 1,
            phase: "reducing".into(),
            state_blob: vec![],
            step_index: 1,
            started_at: 100,
            updated_at: 150,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
        };
        ws.upsert_invocation_state(&row).await.unwrap();
        ws.write_llm_intent("inv-llm", "req-1", "claude-opus-4-8", "{}", 160)
            .await
            .unwrap();
        ws.write_llm_dispatched("inv-llm", "req-1", 170)
            .await
            .unwrap();
        let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
    }

    let views = Views::open(&paths).await.unwrap();

    // WAL is stale past the stuck threshold, but the LLM dispatch is
    // fresh — working, not stuck.
    let execs = views
        .executions(60_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(execs.working, 1);
    assert_eq!(execs.working_ids, vec!["inv-llm".to_string()]);
    assert_eq!(execs.stuck, 0);

    // Once the dispatch itself exceeds the long-dispatch threshold the
    // invocation falls through to the stuck check.
    let execs_late = views
        .executions(1_000_000, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(execs_late.working, 0);
    assert_eq!(execs_late.stuck, 1);
}

/// A terminal invocation's transcript closes with an Outcome entry —
/// the explicit "no more turns expected" signal; a live one carries
/// no Outcome (#105 SSE slice).
#[tokio::test]
async fn transcript_outcome_reflects_terminality() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());
    {
        let ws = WorkerStore::open(&paths.worker).await.unwrap();
        for (inv, terminal_at) in [("inv-done", Some(150_i64)), ("inv-live", None)] {
            ws.write_llm_intent(inv, "req-1", "m", "{}", 100)
                .await
                .unwrap();
            ws.write_llm_dispatched(inv, "req-1", 101).await.unwrap();
            ws.write_llm_completed(inv, "req-1", r#"{"content":"done"}"#, false, 0.01, 102)
                .await
                .unwrap();
            let row = InvocationStateRow {
                invocation_id: inv.into(),
                agent_id: "agent-a".into(),
                schema_version: 1,
                phase: if terminal_at.is_some() {
                    "completed".into()
                } else {
                    "reducing".into()
                },
                state_blob: vec![],
                step_index: 4,
                started_at: 100,
                updated_at: 140,
                terminal_at,
                workspace_ref: None,
                archive_status: None,
                archive_published_at: None,
                trigger_source: None,
                trigger_subject: None,
                trigger_payload: None,
            };
            ws.upsert_invocation_state(&row).await.unwrap();
        }
        let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
    }

    let views = Views::open(&paths).await.unwrap();

    let done = views.transcript("inv-done").await.unwrap().expect("some");
    match done.last().expect("entries") {
        crate::transcript::TranscriptEntry::Outcome {
            phase,
            timestamp_ms,
        } => {
            assert_eq!(phase, "completed");
            assert_eq!(*timestamp_ms, 150);
        }
        other => panic!("expected Outcome last, got {other:?}"),
    }

    let live = views.transcript("inv-live").await.unwrap().expect("some");
    assert!(
        !live
            .iter()
            .any(|e| matches!(e, crate::transcript::TranscriptEntry::Outcome { .. })),
        "live invocation must not carry an Outcome"
    );
}

/// An in-flight row whose `updated_at` is in the future (worker clock
/// ahead) is not "stuck" — `is_stale`'s saturating age handles skew.
/// This guard moved here from `fq doctor`'s tests when the stuck
/// determination moved into `executions()` (#105 layer 1).
#[tokio::test]
async fn executions_ignore_clock_skew() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimeDbPaths::under(dir.path());

    const NOW: i64 = 1_000_000;
    {
        let ws = WorkerStore::open(&paths.worker).await.unwrap();
        let row = InvocationStateRow {
            invocation_id: "inv-future".into(),
            agent_id: "agent-a".into(),
            schema_version: 1,
            phase: "reducing".into(),
            state_blob: vec![],
            step_index: 1,
            started_at: NOW,
            updated_at: NOW + 60_000,
            terminal_at: None,
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
        };
        ws.upsert_invocation_state(&row).await.unwrap();
        let _cp = ControlPlaneStore::open(&paths.control_plane).await.unwrap();
        let _proj = ProjectionStore::open(&paths.projection).await.unwrap();
    }

    let views = Views::open(&paths).await.unwrap();
    let execs = views
        .executions(NOW, 30_000, DEFAULT_LONG_DISPATCH_THRESHOLD_MS)
        .await
        .unwrap();
    assert_eq!(execs.in_flight, 1);
    assert_eq!(execs.stuck, 0, "future updated_at must not read as stuck");
}
