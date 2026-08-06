//! The `fq events` verbs (plan Phase 4, verbs 11–12): the live tail
//! and the history query, both read over the authenticated edge from
//! the daemon's Event atom.
//!
//! Split out of `lib.rs` (#189) on the `workers.rs` precedent: the
//! transplant of `fq events query` onto `event.list` is what pushed
//! that file past its budget, and a subcommand's rendering belongs in
//! its own module anyway. This is the client half of the seam Phase 5
//! splits the binary along — the daemon half of the same atom is
//! `event_atom.rs`.

use super::*;

/// Tail the event stream, formatting each event as a single readable
/// line.
///
/// Rides `event.stream` over the authenticated edge (plan Phase 4,
/// verb 11). It used to hold its own core-NATS subscription, which
/// **drops messages silently** when the consumer falls behind and
/// cannot be resumed; the stream is an ephemeral consumer positioned by
/// sequence, so a slow terminal costs latency rather than events, and
/// the cursor below is a real resume point.
///
/// Selection is the Event atom's typed filter — `--agent`,
/// `--event-type`, the flags `fq events query` takes — and it travels
/// whole rather than being applied here, so a narrowed tail is
/// narrowed at the log rather than at the terminal. The raw NATS
/// subject argument this verb used to take is gone (D8): a subject is
/// a coordinate of the infrastructure the edge maps, not a selection
/// the surface speaks.
pub(crate) async fn tail_events(
    global: &GlobalArgs,
    agent: Option<String>,
    event_type: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let filter = EventFilter {
        agent,
        event_type,
        ..EventFilter::default()
    };
    let config = global.resolve_config()?;

    if !json {
        println!("Connecting to the edge at {}...", config.edge.bind);
    }
    let client = edge_client_for(global).await?;

    // Seek the tail before printing "listening": from here on the
    // cursor is ours, and every batch resumes from where the last one
    // ended — no gap, no silent drop.
    let seek = next_event_batch(&client, &filter, u64::MAX, 0)
        .await?
        .map_err(|e| anyhow::anyhow!("event.stream: {e}"))?;
    let mut cursor = seek.next_from_seq;

    if !json {
        println!("Tailing {}", filter.describe());
        println!("Press Ctrl-C to exit.");
        println!();
    }

    loop {
        let batch = next_event_batch(&client, &filter, cursor, 30_000)
            .await?
            .map_err(|e| anyhow::anyhow!("event.stream: {e}"))?;
        cursor = batch.next_from_seq;
        for item in batch.items {
            let state: fq_runtime::event_tail::EventState = serde_json::from_value(item.item)?;
            if json {
                println!("{}", serde_json::to_string(&state.event)?);
            } else {
                print_event(&state.event);
            }
        }
    }
    // The tail loop runs until Ctrl-C or a transport error (`?`).
}

/// Format one event as a single readable line.
fn print_event(event: &Event) {
    let timestamp = event.envelope.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let invocation = event.envelope.invocation_id.as_simple().to_string();
    let invocation_short: String = invocation.chars().take(8).collect();

    let summary = match &event.payload {
        EventPayload::Triggered(p) => format!("triggered source={:?}", p.trigger_source),
        EventPayload::InvocationSummary(p) => {
            format!("invocation.summary [{:?}] {}", p.kind, p.summary)
        }
        EventPayload::LlmRequest(p) => format!(
            "llm.request model={} messages={}",
            p.model,
            p.messages.len()
        ),
        EventPayload::LlmResponse(p) => {
            // Cost rides on the llm.response envelope (envelope-refactor
            // plan step 3). Render it inline when present so the
            // operator gets the same per-call cost visibility the
            // separate cost event used to provide.
            let cost_suffix = event
                .envelope
                .cost
                .as_ref()
                .map(|c| {
                    format!(
                        " cost=${:.6} cumulative=${:.6}",
                        c.total_cost, c.cumulative_invocation_cost
                    )
                })
                .unwrap_or_default();
            format!(
                "llm.response tokens={}/{} stop={:?}{cost_suffix}",
                p.usage.input_tokens, p.usage.output_tokens, p.stop_reason
            )
        }
        EventPayload::LlmFailure(p) => {
            // Cost only when the provider's usage was recoverable — an
            // empty completion. Absent otherwise, and rendering a `$0`
            // would claim we know the call was free.
            let cost_suffix = event
                .envelope
                .cost
                .as_ref()
                .map(|c| format!(" cost=${:.6}", c.total_cost))
                .unwrap_or_default();
            format!(
                "llm.failure {:?} model={} {}{cost_suffix}",
                p.error_kind, p.model, p.error_message
            )
        }
        EventPayload::ToolCall(p) => format!("tool.call {}", p.tool_name),
        EventPayload::ToolDispatched(p) => format!("tool.dispatched {}", p.tool_name),
        EventPayload::LlmDispatched(p) => format!("llm.dispatched model={}", p.model),
        EventPayload::ToolResult(p) => {
            format!("tool.result {}", if p.is_error { "error" } else { "ok" })
        }
        EventPayload::HostNotice(p) => format!("host.notice kind={} {}", p.kind, p.body),
        EventPayload::Completed(p) => format!(
            "completed duration={}ms cost=${:.6}",
            p.total_duration_ms, p.total_cost
        ),
        EventPayload::Failed(p) => {
            format!("failed {:?} {}", p.error_kind, p.error_message)
        }
        EventPayload::InvocationAmbiguous(p) => format!(
            "invocation.ambiguous entity={} call_id={}",
            p.stuck_entity, p.stuck_call_id
        ),
        EventPayload::InvocationArchived(p) => format!(
            "invocation.archived worker_id={} phase={}",
            p.worker_id, p.final_phase
        ),
        EventPayload::InvocationArchiveAcked(p) => {
            format!("invocation.archive_acked worker_id={}", p.worker_id)
        }
        EventPayload::SystemStartup(p) => format!(
            "system.startup version={} agents={} nats={}",
            p.version, p.agents_loaded, p.nats_url
        ),
        EventPayload::SystemShutdown(p) => {
            format!("system.shutdown reason={} clean={}", p.reason, p.clean)
        }
        EventPayload::SystemRecovery(p) => format!(
            "system.recovery total={} safe_resume={} safe_replay={} ambiguous={}",
            p.total, p.safe_resume, p.safe_replay, p.ambiguous
        ),
        EventPayload::SystemTaskFailed(p) => format!(
            "system.task_failed task={} error={}",
            p.task_name, p.error_message
        ),
        EventPayload::WorkerHeartbeat(p) => format!("worker.heartbeat worker_id={}", p.worker_id),
        EventPayload::WorkerOrphaned(p) => format!(
            "worker.orphaned worker_id={} last_heartbeat_ms={}",
            p.worker_id, p.last_heartbeat_ms
        ),
        EventPayload::McpServerLog(p) => {
            format!("mcp.log server={} level={} {}", p.server, p.level, p.data)
        }
        EventPayload::InvocationOperatorRecovered(p) => format!(
            "invocation.operator_recovered action={} phase={}{}",
            p.action,
            p.final_phase,
            p.reason
                .as_deref()
                .map(|r| format!(" reason={r:?}"))
                .unwrap_or_default()
        ),
        EventPayload::InvocationOperatorResumed(p) => format!(
            "invocation.operator_resumed calls={}{}",
            p.completed_call_ids.join(","),
            p.reason
                .as_deref()
                .map(|r| format!(" reason={r:?}"))
                .unwrap_or_default()
        ),
        // A newer daemon's event type. The envelope still renders; say
        // plainly that the payload is unreadable rather than pretend
        // the line is complete.
        EventPayload::Unknown => {
            "unknown event_type (published by a newer fq; upgrade to read it)".to_string()
        }
    };

    println!(
        "{timestamp} [{invocation_short}] {agent}: {summary}",
        agent = event.envelope.agent_id
    );
}

/// The event history, over the edge (plan Phase 4, verb 12):
/// `event.list`, which answers from the daemon's projection index.
///
/// The rows are index rows and carry no payload — see the Event atom's
/// declared description, which is where that contract is published.
/// Each carries the `seq` `fq events tail`'s cursor and `event.get`
/// speak, so a row here is one call away from the whole event; an
/// operator who wants payloads in bulk tails rather than queries.
///
/// The narrowing travels with the request rather than being applied
/// after the rows have crossed, so a filtered query costs the daemon a
/// filtered query.
pub(crate) async fn query_events(
    global: &GlobalArgs,
    agent: Option<&str>,
    event_type: Option<&str>,
    since: Option<&str>,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let output = edge_invoke(
        global,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::to_value(EventFilter {
            agent: agent.map(str::to_string),
            event_type: event_type.map(str::to_string),
            since: since.map(str::to_string),
            // A negative `--limit` is SQLite's "no limit" through the
            // old local path; keep that reading rather than wrapping it
            // into a tiny page.
            limit: Some(u32::try_from(limit).unwrap_or(u32::MAX)),
        })?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rows: Vec<fq_runtime::views::EventView> = serde_json::from_value(output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No events matched.");
        return Ok(());
    }

    println!(
        "{:<20} {:<40} {:<14} {:<12} invocation",
        "timestamp", "agent", "event", "cost"
    );
    for row in rows {
        let ts = row.timestamp.get(..19).unwrap_or(&row.timestamp);
        let inv_short: String = row.invocation_id.chars().take(8).collect();
        let cost = row
            .total_cost
            .map(|c| format!("${c:.6}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<40} {:<14} {:<12} {}",
            ts, row.agent_id, row.event_type, cost, inv_short
        );
    }
    Ok(())
}
