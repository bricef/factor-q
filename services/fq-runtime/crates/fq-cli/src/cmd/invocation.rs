use crate::*;

// ============================================================
// fq invocation subcommand
// ============================================================

/// Parse a `--status` filter into an `OwnerStatus`. Returns
/// `Err` on unknown values so the CLI exits with a clear
/// message rather than silently matching no rows.
pub(crate) fn parse_invocation_status_filter(
    s: &str,
) -> anyhow::Result<fq_runtime::control_plane::store::OwnerStatus> {
    use fq_runtime::control_plane::store::OwnerStatus;
    match s {
        "in_flight" => Ok(OwnerStatus::InFlight),
        "ambiguous" => Ok(OwnerStatus::Ambiguous),
        "completed" => Ok(OwnerStatus::Completed),
        "failed" => Ok(OwnerStatus::Failed),
        other => Err(anyhow::anyhow!(
            "unknown status filter `{other}` — try in_flight | ambiguous | completed | failed"
        )),
    }
}

/// One human-readable line for an invocation list row. Pure;
/// covered by unit tests.
pub(crate) fn format_invocation_list_row_human(
    item: &fq_runtime::views::InvocationSummaryView,
) -> String {
    let inv_short: String = item.invocation_id.chars().take(8).collect();
    let agent = item.agent_id.as_deref().unwrap_or("?");
    let agent_trim: String = agent.chars().take(22).collect();
    let worker_trim: String = item.worker_id.chars().take(22).collect();
    let archived_flag = if item.archived { "yes" } else { "no" };
    // The one-line summary (#216) rides last: it is the only
    // variable-width column, truncated char-wise so a long line
    // cannot wrap the table.
    let summary = match item.summary.as_deref() {
        Some(line) if line.chars().count() > 60 => {
            let mut t: String = line.chars().take(59).collect();
            t.push('…');
            t
        }
        Some(line) => line.to_string(),
        None => "—".to_string(),
    };
    format!(
        "{:<11} {:<10} {:<24} {:<24} {:<5} {}",
        inv_short, item.status, agent_trim, worker_trim, archived_flag, summary
    )
}

pub(crate) async fn invocation_list(
    global: &GlobalArgs,
    status: Option<&str>,
    include_archived: bool,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let status_filter = status.map(parse_invocation_status_filter).transpose()?;
    let views = open_views(global).await?;
    let items = views
        .invocation_index(status_filter, include_archived, limit)
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        let what = status
            .map(|s| format!("with status={s} "))
            .unwrap_or_default();
        println!("0 invocations {what}— nothing to list.");
    } else {
        println!(
            "{:<11} {:<10} {:<24} {:<24} {:<5} summary",
            "invocation", "status", "agent", "worker", "arch"
        );
        for item in &items {
            println!("{}", format_invocation_list_row_human(item));
        }
    }
    Ok(())
}

pub(crate) async fn invocation_show(
    global: &GlobalArgs,
    id: &str,
    json: bool,
) -> anyhow::Result<()> {
    let views = open_views(global).await?;
    let Some(detail) = views.invocation(id).await? else {
        eprintln!("no invocation found with id={id}");
        std::process::exit(1);
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        println!("Invocation: {}", detail.invocation_id);
        if let Some(a) = &detail.agent_id {
            println!("  agent:    {a}");
        }
        if let Some(s) = &detail.summary {
            println!("  summary:  {s}");
        }
        if let Some(o) = &detail.owner {
            println!("  status:   {}", o.status);
            println!("  worker:   {}", o.worker_id);
        } else {
            println!("  status:   (no coordination row)");
        }
        if let Some(a) = &detail.archive {
            println!(
                "  archived: phase={} terminal_at_ms={} archived_at_ms={}",
                a.final_phase, a.terminal_at_ms, a.archived_at_ms
            );
        }
        // The "what is it doing right now" block, from the worker WAL —
        // present only while the invocation is in flight.
        if let Some(live) = &detail.live {
            println!("\nLive execution:");
            println!("  phase:      {}", live.phase);
            println!("  step:       {}", live.step_index);
            println!("  updated_at: {} ms", live.updated_at_ms);
            for t in live.tools.iter().filter(|t| t.status != "completed") {
                println!("  tool:       {} [{}]", t.tool_name, t.status);
            }
            for l in live.llms.iter().filter(|l| l.status != "completed") {
                println!("  llm:        {} [{}]", l.model, l.status);
            }
        }
        if !detail.recent_events.is_empty() {
            println!("\nRecent events:");
            for e in &detail.recent_events {
                let ts = e.timestamp.get(..19).unwrap_or(&e.timestamp);
                println!("  {ts}  {}", e.event_type);
            }
        }
    }
    Ok(())
}

#[derive(serde::Serialize, Debug)]
pub(crate) struct InvocationDropResult {
    invocation_id: String,
    agent_id: String,
    event_id: String,
    reason: Option<String>,
}

/// Look up the agent for an invocation, build the
/// `invocation.operator_recovered` event with `action="drop"`,
/// publish it, and return the result struct. Extracted from the
/// CLI handler so tests can drive the publish path without
/// constructing `GlobalArgs` / config files.
pub(crate) async fn publish_invocation_drop(
    bus: &EventBus,
    proj_store: &ProjectionStore,
    control_store: &ControlPlaneStore,
    invocation_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<InvocationDropResult> {
    let res = fq_runtime::control_plane::operator::drop_invocation(
        bus,
        proj_store,
        control_store,
        invocation_id,
        reason,
    )
    .await?;
    Ok(InvocationDropResult {
        invocation_id: res.invocation_id,
        agent_id: res.agent_id,
        event_id: res.event_id,
        reason: res.reason,
    })
}

pub(crate) async fn invocation_drop(
    global: &GlobalArgs,
    id: &str,
    reason: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    // The drop writes to the control-plane store, so this path runs
    // the legacy split if one is pending.
    let db_paths = ensure_split_dbs(&config).await?;
    // allow-direct-store-open: operator write path (`fq invocation drop`), not a read.
    let proj_store = ProjectionStore::open_read_only(&db_paths.projection).await?;
    // allow-direct-store-open: operator write path (`fq invocation drop`), not a read.
    let control_store = ControlPlaneStore::open(&db_paths.control_plane).await?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    let result = publish_invocation_drop(&bus, &proj_store, &control_store, id, reason).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "Dropped invocation {id} (agent={}, event_id={}).",
            result.agent_id, result.event_id
        );
        if let Some(r) = &result.reason {
            println!("Reason: {r}");
        }
        println!("Follow with `fq invocation show {id}` to confirm the archive row.");
    }
    Ok(())
}

/// Render the full payload-bearing transcript for one invocation.
///
/// Snapshot mode (default): open the worker WAL read-only against
/// `worker.db`, collect the ordered `llm_dispatch` + `tool_dispatch`
/// rows for the invocation, and render them with payloads. Read-only
/// and NATS-free. `--follow` additionally subscribes to the invocation's
/// agent subject and appends new turns live until Ctrl-C.
pub(crate) async fn invocation_transcript(
    global: &GlobalArgs,
    id: &str,
    follow: bool,
    format: TranscriptFormat,
    full: bool,
) -> anyhow::Result<()> {
    use fq_runtime::transcript::{
        DEFAULT_TRUNCATE_BYTES, assistant_entry, dedup_key, render_pretty, snapshot_keys,
        tool_result_entry,
    };

    let as_json = matches!(format, TranscriptFormat::Json);
    if follow && as_json {
        anyhow::bail!("--follow is not supported with --format json (json emits a snapshot array)");
    }
    let truncate_bytes = if full {
        None
    } else {
        Some(DEFAULT_TRUNCATE_BYTES)
    };

    let views = open_views(global).await?;

    // For --follow, subscribe to the invocation's agent subject BEFORE
    // reading the WAL snapshot, so a turn that completes in the gap
    // between the read and the subscription is not lost: anything
    // published in that window is caught by both the snapshot and the
    // live stream, then deduped at the seam. Snapshot-only mode needs no
    // NATS. The returned stream owns its connection, so `bus` may drop.
    let follow_stream = if follow {
        let config = global.resolve_config()?;
        let agent_id = views.agent_id_for_invocation(id).await?.ok_or_else(|| {
            let hint = if id.len() != 36 {
                " (not a full invocation id — see `fq invocation list --json`)"
            } else {
                ""
            };
            anyhow::anyhow!(
                "cannot follow invocation {id}: no agent recorded for it in the projection{hint}"
            )
        })?;
        let bus = EventBus::connect(&config.nats.url)
            .await
            .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
        let subject = format!("fq.agent.{agent_id}.>");
        let stream = bus
            .subscribe(subject.clone())
            .await
            .with_context(|| format!("failed to subscribe to {subject}"))?;
        Some((subject, stream))
    } else {
        None
    };

    // An empty snapshot is a hard error only for the one-shot view; under
    // --follow it is valid (tailing an invocation that has not dispatched
    // anything yet), so fall through to the live loop.
    let mut entries = match views.transcript(id).await? {
        Some(entries) => entries,
        None if follow => Vec::new(),
        None => {
            eprintln!(
                "no transcript found for invocation id={id} (no LLM or tool dispatches recorded)"
            );
            // A full invocation id is 36 chars; `fq invocation list` shows an
            // abbreviated one, so a copied id often won't match. Point at the
            // machine-readable form that carries the full id.
            if id.len() != 36 {
                eprintln!(
                    "note: `{id}` is not a full invocation id — `fq invocation list` abbreviates it; \
                     use `fq invocation list --json` to get the full id."
                );
            }
            std::process::exit(1);
        }
    };

    // `Views::transcript` also exposes terminal outcomes. This CLI command's
    // established snapshot contract predates that entry, so retain its
    // byte-identical LLM/tool-only output during the reader migration.
    entries.retain(|entry| {
        !matches!(
            entry,
            fq_runtime::transcript::TranscriptEntry::Outcome { .. }
        )
    });

    if as_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    print!("{}", render_pretty(&entries, truncate_bytes));

    // Snapshot-only mode: done. Otherwise take the live stream that was
    // subscribed above (before the snapshot) and tail it.
    let Some((subject, mut stream)) = follow_stream else {
        return Ok(());
    };

    println!();
    println!("── following {subject} (invocation {id}); Ctrl-C to exit ──");

    let mut seen = snapshot_keys(&entries);
    while let Some(result) = stream.next().await {
        let event = match result {
            Ok(e) => e,
            Err(err) => {
                eprintln!("deserialise error: {err}");
                continue;
            }
        };
        if event.envelope.invocation_id.to_string() != id {
            continue;
        }
        let ts_ms = event.envelope.timestamp.timestamp_millis();
        let entry = match &event.payload {
            EventPayload::LlmResponse(p) => {
                let cost = event.envelope.cost.as_ref().map(|c| c.total_cost);
                Some(assistant_entry(ts_ms, p_model(&event), cost, p))
            }
            EventPayload::ToolResult(p) => {
                // The live event carries the result; tool name/params
                // rode the earlier tool.call. Best-effort: label by the
                // correlation id when we can't recover the name.
                Some(tool_result_entry(
                    ts_ms,
                    format!("(tool_call {})", p.tool_call_id),
                    serde_json::Value::Null,
                    p,
                ))
            }
            _ => None,
        };
        if let Some(entry) = entry {
            if let Some(key) = dedup_key(&entry)
                && !seen.insert(key)
            {
                continue;
            }
            print!(
                "{}",
                render_pretty(std::slice::from_ref(&entry), truncate_bytes)
            );
        }
    }

    Ok(())
}

/// The model string for a live event, if the payload carries one.
pub(crate) fn p_model(event: &Event) -> String {
    match &event.payload {
        EventPayload::LlmResponse(_) => event
            .envelope
            .cost
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_else(|| "?".to_string()),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod invocation_tests {
    use super::*;

    #[test]
    fn parse_invocation_status_filter_accepts_known_values() {
        use fq_runtime::control_plane::store::OwnerStatus;
        assert!(matches!(
            parse_invocation_status_filter("in_flight").unwrap(),
            OwnerStatus::InFlight
        ));
        assert!(matches!(
            parse_invocation_status_filter("ambiguous").unwrap(),
            OwnerStatus::Ambiguous
        ));
        assert!(matches!(
            parse_invocation_status_filter("completed").unwrap(),
            OwnerStatus::Completed
        ));
        assert!(matches!(
            parse_invocation_status_filter("failed").unwrap(),
            OwnerStatus::Failed
        ));
    }

    #[test]
    fn parse_invocation_status_filter_rejects_unknown() {
        let err = parse_invocation_status_filter("garbage").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("garbage"));
        assert!(msg.contains("in_flight"));
    }

    #[test]
    fn format_invocation_list_row_human_renders_short_id_and_truncated_fields() {
        let item = fq_runtime::views::InvocationSummaryView {
            invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
            agent_id: Some("a".repeat(40)),
            worker_id: "worker-42".to_string(),
            status: "ambiguous".to_string(),
            assigned_at_ms: 1_700_000_000_000,
            started_at_ms: 1_700_000_000_000,
            archived: false,
            summary: None,
        };
        let line = format_invocation_list_row_human(&item);
        assert!(line.starts_with("019e3b32"), "expected 8-char id prefix");
        assert!(line.contains("ambiguous"));
        assert!(line.contains("worker-42"));
        assert!(line.contains("no"));
        // Agent string was truncated to 22 chars.
        assert!(line.contains(&"a".repeat(22)));
        assert!(!line.contains(&"a".repeat(23)));
    }

    /// #216: the summary line rides last, truncated char-safe; absent
    /// renders an em-dash.
    #[test]
    fn format_invocation_list_row_human_renders_summary_last() {
        let mut item = fq_runtime::views::InvocationSummaryView {
            invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
            agent_id: Some("m0-issue-fix".to_string()),
            worker_id: "w".to_string(),
            status: "in_flight".to_string(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: false,
            summary: Some("Fixing #7: editing widget.rs".to_string()),
        };
        let line = format_invocation_list_row_human(&item);
        assert!(
            line.ends_with("Fixing #7: editing widget.rs"),
            "got: {line}"
        );

        item.summary = Some("x".repeat(200));
        let line = format_invocation_list_row_human(&item);
        assert!(line.ends_with('…'), "truncated: {line}");
        assert!(line.chars().count() < 150, "bounded: {line}");

        item.summary = None;
        let line = format_invocation_list_row_human(&item);
        assert!(line.ends_with('—'), "fallback dash: {line}");
    }

    #[test]
    fn format_invocation_list_row_human_marks_archived() {
        let item = fq_runtime::views::InvocationSummaryView {
            invocation_id: "inv".to_string(),
            agent_id: Some("a".to_string()),
            worker_id: String::new(),
            status: "completed".to_string(),
            assigned_at_ms: 0,
            started_at_ms: 0,
            archived: true,
            summary: None,
        };
        let line = format_invocation_list_row_human(&item);
        // The archived flag sits before the (now trailing) summary
        // column (#216).
        assert!(
            line.contains(" yes "),
            "archived flag should be 'yes', got: {line:?}"
        );
    }

    #[tokio::test]
    async fn publish_invocation_drop_emits_operator_recovered_for_agent() {
        // NATS-gated end-to-end of the publish path: seed a
        // ProjectionStore with one event so the agent lookup
        // works, then call publish_invocation_drop and capture
        // the event on the agent-scoped operator_recovered
        // subject.
        let server = fq_test_support::NatsServer::start();
        let url = server.url().to_string();

        use fq_runtime::events::{EventPayload as EP, TriggerSource, TriggeredPayload};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let db_paths = fq_runtime::RuntimeDbPaths::under(dir.path());
        let proj_store = ProjectionStore::open(&db_paths.projection).await.unwrap();

        let agent_id = AgentId::new(format!("op-drop-cli-{}", Uuid::now_v7().simple())).unwrap();
        let invocation_id = Uuid::now_v7();

        // Seed one event so agent_id_for_invocation has something
        // to find. Pick triggered — the most representative
        // first event for an invocation.
        let seed = Event::new(
            agent_id.clone(),
            invocation_id,
            EP::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: serde_json::Value::Null,
                config_snapshot: fq_runtime::Agent::builder()
                    .id(agent_id.as_str())
                    .model("claude-haiku")
                    .system_prompt("test")
                    .build()
                    .unwrap()
                    .to_snapshot(),
            }),
        );
        proj_store.insert_event(&seed).await.unwrap();

        let control_store = ControlPlaneStore::open(&db_paths.control_plane)
            .await
            .unwrap();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let mut sub = bus
            .subscribe(format!(
                "fq.agent.{}.invocation.operator_recovered",
                agent_id.as_str()
            ))
            .await
            .expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let result = publish_invocation_drop(
            &bus,
            &proj_store,
            &control_store,
            &invocation_id.to_string(),
            Some("test reason"),
        )
        .await
        .expect("publish_invocation_drop");
        assert_eq!(result.agent_id, agent_id.as_str());
        assert_eq!(result.reason.as_deref(), Some("test reason"));

        let captured = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next())
            .await
            .expect("event timeout")
            .expect("stream closed")
            .expect("deserialise");
        assert_eq!(captured.envelope.invocation_id, invocation_id);
        match &captured.payload {
            EventPayload::InvocationOperatorRecovered(p) => {
                assert_eq!(p.action, "drop");
                assert_eq!(p.final_phase, "failed");
                assert_eq!(p.reason.as_deref(), Some("test reason"));
            }
            other => panic!("expected InvocationOperatorRecovered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_invocation_drop_removes_agentless_owner() {
        let server = fq_test_support::NatsServer::start();
        let url = server.url().to_string();
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let db_paths = fq_runtime::RuntimeDbPaths::under(dir.path());
        let proj_store = ProjectionStore::open(&db_paths.projection).await.unwrap();
        let control_store = ControlPlaneStore::open(&db_paths.control_plane)
            .await
            .unwrap();
        let fake_inv = Uuid::now_v7().to_string();
        control_store
            .register_worker("orphan-worker", "test", 1)
            .await
            .unwrap();
        control_store
            .assign_invocation(&fake_inv, "orphan-worker", 1)
            .await
            .unwrap();
        let bus = EventBus::connect(&url).await.expect("connect NATS");

        let result = publish_invocation_drop(&bus, &proj_store, &control_store, &fake_inv, None)
            .await
            .expect("agent-less owner should drop");
        assert_eq!(result.agent_id, "operator");
        assert!(
            control_store
                .get_invocation_owner(&fake_inv)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn publish_invocation_drop_errors_when_nothing_known() {
        // No projection event *and* no coordination owner row: a truly
        // unknown id must still error rather than emit a phantom
        // operator-recovered event for something that never existed.
        let server = fq_test_support::NatsServer::start();
        let url = server.url().to_string();
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let db_paths = fq_runtime::RuntimeDbPaths::under(dir.path());
        let proj_store = ProjectionStore::open(&db_paths.projection).await.unwrap();
        let control_store = ControlPlaneStore::open(&db_paths.control_plane)
            .await
            .unwrap();
        let bus = EventBus::connect(&url).await.expect("connect NATS");

        let fake_inv = Uuid::now_v7().to_string();
        let err = publish_invocation_drop(&bus, &proj_store, &control_store, &fake_inv, None)
            .await
            .expect_err("unknown invocation should error");
        assert!(format!("{err}").contains("not found"), "got: {err}");
    }

    /// The `--json` list shape is an operator contract: the swap from the
    /// CLI-local struct to `views::InvocationSummaryView` (#105 layer 1)
    /// must not move these fields.
    #[test]
    fn invocation_summary_view_serialises_to_stable_json_shape() {
        let item = fq_runtime::views::InvocationSummaryView {
            invocation_id: "inv-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            worker_id: "worker-1".to_string(),
            status: "in_flight".to_string(),
            assigned_at_ms: 42,
            started_at_ms: 41,
            archived: false,
            summary: None,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["invocation_id"], "inv-1");
        assert_eq!(v["agent_id"], "agent-1");
        assert_eq!(v["worker_id"], "worker-1");
        assert_eq!(v["status"], "in_flight");
        assert_eq!(v["assigned_at_ms"], 42);
        assert_eq!(v["started_at_ms"], 41);
        assert_eq!(v["archived"], false);
    }
}
