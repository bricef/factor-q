//! The `fq events` verbs (plan Phase 4, verbs 11–12): the live tail,
//! the history query and the read of one whole event, all three over
//! the authenticated edge from the daemon's Event atom.
//!
//! Split out of `lib.rs` (#189) on the `workers.rs` precedent: the
//! transplant of `fq events query` onto `event.list` is what pushed
//! that file past its budget, and a subcommand's rendering belongs in
//! its own module anyway. This is the client half of the seam Phase 5
//! splits the binary along — the daemon half of the same atom is
//! `event_atom.rs`.
//!
//! **The listing and the read are one verb pair, not two verbs.**
//! `event.list` is allowed to answer without payloads only because
//! every row it returns names the identity that reads its event back
//! — so a listing an operator cannot walk from is a listing that has
//! quietly lost the payload rather than deferred it. That is why the
//! human table prints the identity in full (see [`query_events`]) and
//! why [`get_event`] exists at all: the walk the atom's declared
//! description promises had no path through this surface, so it was
//! reachable by piping `--json` through `jq` and no other way.

use fq_edge::wire::WireError;
use fq_ops::events::{Event, EventPayload, EventState};

use crate::cli::GlobalArgs;
use crate::edge_call::{edge_client_for, edge_invoke, next_event_batch};
use fq_ops::surface::EventFilter;

use fq_ops::surface::EventKey;

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

    if !json {
        println!(
            "Connecting to the edge at {}...",
            crate::edge_call::daemon_addr(global)?
        );
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
        println!("Tailing {}", describe_filter(&filter));
        println!("Press Ctrl-C to exit.");
        println!();
    }

    loop {
        let batch = next_event_batch(&client, &filter, cursor, 30_000)
            .await?
            .map_err(|e| anyhow::anyhow!("event.stream: {e}"))?;
        cursor = batch.next_from_seq;
        for item in batch.items {
            let state: EventState = serde_json::from_value(item.item)?;
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
    println!(
        "{timestamp} [{invocation_short}] {agent}: {summary}",
        agent = event.envelope.agent_id,
        summary = event_summary(event),
    );
}

/// What this event *is*, in one line — the payload said back in the
/// terms an operator reads it in.
///
/// Extracted from [`print_event`] so `fq events get` can head its
/// detail with the same sentence the tail prints. One renderer, so a
/// payload variant cannot come to mean two things depending on which
/// verb an operator happened to reach for.
fn event_summary(event: &Event) -> String {
    match &event.payload {
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
    }
}

/// The event history, over the edge (plan Phase 4, verb 12):
/// `event.list`, which answers from the daemon's projection index.
///
/// The rows are index rows and carry no payload — see the Event atom's
/// declared description, which is where that contract is published.
/// Each carries the `event_id` `event.get` takes, so a row here is one
/// call away from the whole event for as long as the log still holds
/// its payload; an operator who wants payloads in bulk tails rather
/// than queries.
///
/// **The table prints that identity in full**, which is the whole
/// reason `--json` is not the only way out of this verb: the walk to
/// `fq events get` is what buys List the right to answer without
/// payloads, and an identity an operator cannot copy off the screen is
/// not a walk. Full, and never a prefix — `event.get` resolves an
/// exact `event_id` and has no prefix search, so a shortened id would
/// make the walk *look* reachable and fail, which is worse than the
/// honest absence it replaced.
///
/// It costs the `invocation` column, which was already the wrong
/// thing to spend the width on: it was truncated to eight characters,
/// and eight characters is not an invocation id — `fq invocation show`
/// would refuse it, so that column could not be walked either. The
/// full invocation id is still one `fq events get` away, and `--json`
/// carries it unchanged.
///
/// The narrowing travels with the request rather than being applied
/// after the rows have crossed, so a filtered query costs the daemon a
/// filtered query.
pub(crate) async fn query_events(
    global: &GlobalArgs,
    agent: Option<String>,
    event_type: Option<String>,
    since: Option<String>,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let output = edge_invoke(
        global,
        fq_ops::OpId::List(fq_ops::Domain::Event),
        serde_json::to_value(EventFilter {
            agent,
            event_type,
            since,
            // `--limit` travels as the caller wrote it, and the daemon
            // is the one authority on how big a page may be — so the
            // only thing decided here is the number that has no page
            // to travel as at all.
            //
            // It used to be `u32::MAX`, meaning "no limit", because a
            // negative LIMIT is unbounded to SQLite and this query read
            // `projection.db` itself. It no longer does: the read runs
            // in the daemon, where an unbounded page is the whole table
            // in memory and then a frame too big to send. So "no limit"
            // is not a thing to preserve — it is a promise nothing
            // keeps — and saying so here beats sending `4294967295`
            // for the daemon to reject with a number the operator
            // never typed.
            limit: Some(u32::try_from(limit).map_err(|_| {
                anyhow::anyhow!(
                    "--limit {limit} is not a page size: one page is at most {cap} rows. \
                     If you meant \"no limit\", there is no longer such a thing — a \
                     negative --limit was unbounded while this query read the projection \
                     file directly, and it now runs in the daemon, which will not \
                     materialise an unbounded answer. For more than a page, narrow with \
                     --agent/--type/--since, or use `fq events tail`, which is cursored \
                     and selects the same events for the same filter.",
                    cap = fq_ops::surface::EVENT_LIST_MAX_LIMIT
                )
            })?),
        })?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rows: Vec<fq_ops::views::EventView> = serde_json::from_value(output)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No events matched.");
        return Ok(());
    }

    println!(
        "{:<20} {:<40} {:<14} {:<12} event-id",
        "timestamp", "agent", "event", "cost"
    );
    for row in rows {
        let ts = row.timestamp.get(..19).unwrap_or(&row.timestamp);
        let cost = row
            .total_cost
            .map(|c| format!("${c:.6}"))
            .unwrap_or_else(|| "-".to_string());
        // Last column, and unpadded: nothing follows it, so the full
        // 36-character identity costs the table no alignment. The
        // width is the price of the walk and the trade was made
        // deliberately — see this function's docs.
        println!(
            "{:<20} {:<40} {:<14} {:<12} {}",
            ts, row.agent_id, row.event_type, cost, row.event_id
        );
    }
    Ok(())
}

/// One whole event, over the edge: `event.get`, taking the identity
/// `fq events query` prints (plan Phase 4, cohort 4.2).
///
/// This is the second half of the walk, and neither half is worth
/// anything alone. `event.list` answers with index rows and no
/// payloads on the strength of every row naming the identity that
/// reads its event back; until this verb existed, that promise was
/// declared on the surface, exercised over the edge, and unreachable
/// from a terminal.
///
/// **The three unavailable states are rendered apart.** `event.get`
/// resolves an identity in two hops — the index for a log position,
/// then the log — and there are three ways that ends without an
/// event. They are three different facts about the system, and
/// [`WireError`] carries them as three variants precisely so a
/// consumer does not have to read English to tell them apart:
///
/// * `NotFound` — no row. Nothing here has ever seen that event.
/// * `Unlocatable` — a row, and no position. The event is known; where
///   its payload sits is not, and no retry will change that.
/// * `Gone` — a position the log has passed, or a log recreated under
///   an index that outlived it. Routine rather than a fault:
///   cost-bearing rows are kept indefinitely and the log keeps thirty
///   days, so every retained row reaches this state eventually.
///
/// Collapsing them into one "not found" would undo the reason the
/// distinction exists. So each renders as its own verdict line, and
/// the daemon's own message rides along because it carries facts this
/// side does not have: the log position, and the identity of the event
/// found sitting at it.
pub(crate) async fn get_event(
    global: &GlobalArgs,
    event_id: &str,
    json: bool,
) -> anyhow::Result<()> {
    let output = edge_invoke(
        global,
        fq_ops::OpId::Get(fq_ops::Domain::Event),
        serde_json::to_value(EventKey {
            event_id: event_id.to_string(),
        })?,
    )
    .await?;
    let state: EventState = match output {
        Ok(value) => serde_json::from_value(value)?,
        Err(unavailable) => {
            let (verdict, means) = unavailable_event(&unavailable)?;
            eprintln!("{verdict}");
            eprintln!("{means}");
            std::process::exit(1);
        }
    };

    let event = &state.event;
    if json {
        // The event as published, which is the shape `fq events tail
        // --json` emits line by line — one parser reads either verb.
        // `seq` is deliberately not here: it is where this read landed
        // in the log, a cursor for resuming a stream, and handing it
        // back from a Get invites a consumer to store a transport
        // coordinate as an identity. That habit is exactly what this
        // atom was corrected out of.
        println!("{}", serde_json::to_string_pretty(event)?);
        return Ok(());
    }

    println!("Event: {}", event.envelope.event_id);
    println!(
        "  time:        {}",
        event.envelope.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ")
    );
    println!("  agent:       {}", event.envelope.agent_id);
    println!("  invocation:  {}", event.envelope.invocation_id);
    println!("  type:        {}", event.payload.event_type());
    println!("  summary:     {}", event_summary(event));
    // What the walk was for. The listing showed every line above; the
    // payload is the one thing an index row does not carry.
    println!("\nPayload:");
    println!("{}", serde_json::to_string_pretty(&event.payload)?);
    Ok(())
}

/// How one unavailable event reads to an operator: the verdict, and
/// what it means about this system.
///
/// Split out so the three states are legible side by side — the
/// failure mode this guards against is not a wrong message but three
/// messages quietly becoming one. Anything that is not one of the
/// three is somebody else's error (a denied token, a lagging read)
/// and surfaces unchanged.
fn unavailable_event(err: &WireError) -> anyhow::Result<(String, &'static str)> {
    Ok(match err {
        WireError::NotFound { message, .. } => (
            format!("not found: {message}"),
            "This daemon's index has no row for that identity — nothing here has ever seen \
             that event. `fq events query` lists the identities it will answer for.",
        ),
        WireError::Unlocatable { message, .. } => (
            format!("unlocatable: {message}"),
            "The event is not missing: `fq events query` still lists its row, and --json \
             carries every field the index extracted from it. Only the payload is \
             unreachable, and no retry will produce it — a row projected before the index \
             recorded log positions stays in this state.",
        ),
        WireError::Gone { message, .. } => (
            format!("gone: {message}"),
            "The index outlives the log: cost-bearing rows are kept indefinitely while the \
             log keeps thirty days, so an old event that lists without a readable payload \
             is the ordinary answer here rather than a fault.",
        ),
        other => anyhow::bail!("{other}"),
    })
}

/// What an [`EventFilter`] selects, in words — `fq events tail` says it
/// back in its preamble so an operator can see at a glance that the
/// narrowing they asked for is the one in force. Domain terms, like
/// the filter itself: the verb used to echo the raw NATS subject it
/// had subscribed to, which named a coordinate of the infrastructure
/// rather than anything the operator selected.
///
/// A free function rather than a method because the filter is now a
/// shared declared shape ([`fq_ops::surface`]) and this sentence is
/// the CLI's own — terminal prose has no business travelling with a
/// wire contract.
pub(crate) fn describe_filter(filter: &EventFilter) -> String {
    match (filter.agent.as_deref(), filter.event_type.as_deref()) {
        (None, None) => "all events".to_string(),
        (Some(agent), None) => format!("all events for agent {agent}"),
        (None, Some(event_type)) => format!("all {event_type} events"),
        (Some(agent), Some(event_type)) => format!("{event_type} events for agent {agent}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tail's preamble, in the domain's terms rather than the
    /// transport's. It travelled with the daemon's Event atom while
    /// the filter and the handler shared a crate; the sentence is the
    /// client's, so it is asserted where it is written.
    #[test]
    fn a_filter_describes_itself_in_domain_terms() {
        let described = |agent: Option<&str>, event_type: Option<&str>| {
            let filter = EventFilter {
                agent: agent.map(str::to_string),
                event_type: event_type.map(str::to_string),
                ..EventFilter::default()
            };
            describe_filter(&filter)
        };
        assert_eq!(described(None, None), "all events");
        assert_eq!(
            described(Some("researcher"), None),
            "all events for agent researcher"
        );
        assert_eq!(described(None, Some("tool_call")), "all tool_call events");
        assert_eq!(
            described(Some("researcher"), Some("tool_call")),
            "tool_call events for agent researcher"
        );
    }
}
