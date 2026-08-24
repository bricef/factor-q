//! The `fq invocation` verbs (list, show, drop, resume, transcript): the
//! operator's triage surface over one invocation's life.
//!
//! Split out of `lib.rs` (#189) on the `workers.rs` precedent. Every verb here
//! rides the authenticated edge — the daemon answers from its views — so this
//! module is rendering plus the one round trip each verb needs. `resume` was
//! the last exception, reaching the broker directly; it is a declared command
//! now, and the client no longer knows a NATS url exists.

use anyhow::Context;
use fq_runtime::agent::AgentId;

use crate::cli::{GlobalArgs, TranscriptFormat};
use crate::edge_call::{edge_client_for, edge_invoke, edge_transcript_snapshot, next_turn_batch};
use fq_ops::surface::{InvocationListFilter, InvocationViewKey};

// ============================================================
// fq invocation subcommand
// ============================================================

/// One human-readable line for an invocation list row. Pure;
/// covered by unit tests.
fn format_invocation_list_row_human(item: &fq_ops::views::InvocationSummaryView) -> String {
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
    // Validate locally for a fast, friendly error before dialling.
    status
        .map(fq_ops::surface::validate_invocation_status_filter)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    // The flip (plan Phase 3b): this read speaks the authenticated
    // edge — the daemon serves it from its views. Rendering is
    // untouched; the goldens prove it byte-identical.
    let output = edge_invoke(
        global,
        fq_ops::OpId::List(fq_ops::Domain::Invocation),
        serde_json::to_value(InvocationListFilter {
            status: status.map(str::to_string),
            include_archived,
            limit,
        })?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let items: Vec<fq_ops::views::InvocationSummaryView> = serde_json::from_value(output)?;

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
    let output = edge_invoke(
        global,
        fq_ops::OpId::Get(fq_ops::Domain::Invocation),
        serde_json::to_value(InvocationViewKey {
            invocation_id: id.to_string(),
        })?,
    )
    .await?;
    let detail: fq_ops::views::InvocationDetailView = match output {
        Ok(value) => serde_json::from_value(value)?,
        Err(fq_edge::wire::WireError::NotFound { .. }) => {
            eprintln!("no invocation found with id={id}");
            std::process::exit(1);
        }
        Err(e) => anyhow::bail!("{e}"),
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

pub(crate) async fn invocation_resume(
    global: &GlobalArgs,
    id: &str,
    reason: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let client = edge_client_for(global).await?;
    // A refusal arrives as an error with the daemon's own message —
    // terminal, live, already resumed, or unknown — so the four stay
    // distinguishable without a flag the caller could ignore.
    let receipt = client
        .invoke(
            fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Resume)),
            serde_json::json!({
                "invocation_id": id,
                "reason": reason,
            }),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let receipt: fq_ops::Receipt = serde_json::from_value(receipt)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&receipt)?);
    } else {
        println!("Resumed invocation {id}.");
        println!("Follow with `fq invocation show {id}` or `fq invocation transcript {id}`.");
    }
    Ok(())
}

#[derive(serde::Serialize, Debug)]
struct InvocationDropResult {
    invocation_id: String,
    agent_id: String,
    event_id: String,
    reason: Option<String>,
}

/// The projection's name for the event a drop publishes — the row the
/// rendered `event_id` is read back from.
const OPERATOR_RECOVERED_EVENT_TYPE: &str = "invocation_operator_recovered";

/// Drop an invocation over the authenticated edge (plan Phase 4, verb
/// 18). One client, two questions: the `invocation.drop` command, then
/// the Invocation view read at the receipt's watermark.
///
/// The second question is not a nicety. A receipt references the atoms
/// a command appended — never state (D3/P4) — so the agent and the
/// event's identity, which this line has always printed, now come from
/// a gated read rather than from a locally minted event. Composing the
/// two also makes the closing "follow with `fq invocation show`"
/// honest: the horizon releases the read only once the archive row and
/// the owner flip are visible too.
///
/// Liveness lives entirely daemon-side now (see `arm_drop_halt`): the
/// runner is asked and the halt armed inside the one handler that
/// writes the event, so `--live` travels as an input, not as a
/// separate round trip the client could skip.
pub(crate) async fn invocation_drop(
    global: &GlobalArgs,
    id: &str,
    reason: Option<&str>,
    live: bool,
    json: bool,
) -> anyhow::Result<()> {
    let client = edge_client_for(global).await?;
    let receipt = client
        .invoke(
            fq_ops::OpId::Verb(fq_ops::VerbId::Invocation(fq_ops::Invocation::Drop)),
            serde_json::json!({
                "invocation_id": id,
                "reason": reason,
                "live": live,
            }),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let receipt: fq_ops::Receipt = serde_json::from_value(receipt)?;
    let event_seq = receipt
        .watermark(fq_ops::Domain::Event)
        .context("the drop receipt named no event — cannot confirm the drop")?;

    let detail = client
        .invoke_gated(
        fq_ops::OpId::Get(fq_ops::Domain::Invocation),
        serde_json::to_value(InvocationViewKey {
            invocation_id: id.to_string(),
        })?,
        Some(event_seq),
    )
    .await?
    // The write already landed, so this is never "the drop failed" —
    // say so, and name the sequence the operator can read it back at.
    .map_err(|e| {
        anyhow::anyhow!(
            "dropped invocation {id} at event sequence {event_seq}, but reading it back failed: {e}"
        )
    })?;
    let detail: fq_ops::views::InvocationDetailView = serde_json::from_value(detail)?;
    let result = InvocationDropResult {
        invocation_id: id.to_string(),
        // An invocation whose only events are operator-issued has no
        // agent of its own; the drop was published as `operator`, and
        // that is what it is reported as.
        agent_id: detail
            .agent_id
            .unwrap_or_else(|| AgentId::operator().into_inner()),
        event_id: detail
            .recent_events
            .iter()
            .find(|e| e.event_type == OPERATOR_RECOVERED_EVENT_TYPE)
            .map(|e| e.event_id.clone())
            .with_context(|| {
                format!(
                    "dropped invocation {id} at event sequence {event_seq}, but the drop \
                     event is not among its recent events"
                )
            })?,
        reason: reason.map(str::to_string),
    };

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
/// Every read here rides the authenticated edge (plan Phase 4, verb
/// 20): the snapshot is `turn.list` behind the opening prompt from
/// `invocation.get`, and `--follow` continues on `turn.stream` from a
/// cursor pinned before the snapshot. The client no longer opens the
/// worker WAL for itself — one daemon, one transport, one answer.
pub(crate) async fn invocation_transcript(
    global: &GlobalArgs,
    id: &str,
    follow: bool,
    json: bool,
    format: Option<TranscriptFormat>,
    full: bool,
) -> anyhow::Result<()> {
    use fq_ops::transcript::{DEFAULT_TRUNCATE_BYTES, dedup_key, render_pretty, snapshot_keys};

    let as_json = json || matches!(format, Some(TranscriptFormat::Json));
    if json && matches!(format, Some(TranscriptFormat::Pretty)) {
        anyhow::bail!("--json conflicts with --format pretty");
    }
    if follow && as_json {
        anyhow::bail!("--follow is not supported with --json (json emits a snapshot array)");
    }
    let truncate_bytes = if full {
        None
    } else {
        Some(DEFAULT_TRUNCATE_BYTES)
    };

    // One client for the whole verb: the snapshot's two reads and the
    // follow cursor are one conversation with one daemon incarnation.
    let client = edge_client_for(global).await?;

    // For --follow, seek the turn stream's tail BEFORE reading the
    // snapshot, so a turn that completes in the gap between the read
    // and the seek is not lost: anything published in that window is
    // caught by both the snapshot and the stream, then deduped at the
    // seam (Phase 3d: the tail rides `turn.stream` over the
    // authenticated edge, not a raw NATS subscription — real tool
    // names and parameters included).
    let follow_cursor = if follow {
        let seek = next_turn_batch(&client, id, u64::MAX, 0)
            .await?
            .map_err(|e| anyhow::anyhow!("cannot follow invocation {id}: {e}"))?;
        Some(seek.next_from_seq)
    } else {
        None
    };

    // An empty snapshot is a hard error only for the one-shot view; under
    // --follow it is valid (tailing an invocation that has not dispatched
    // anything yet), so fall through to the live loop.
    let entries = match edge_transcript_snapshot(&client, id).await? {
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

    if as_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    print!("{}", render_pretty(&entries, truncate_bytes));

    // Snapshot-only mode: done. Otherwise long-poll the turn stream
    // from the cursor pinned above (before the snapshot).
    let Some(mut cursor) = follow_cursor else {
        return Ok(());
    };

    println!();
    println!("── following turn.stream (invocation {id}); Ctrl-C to exit ──");

    let mut seen = snapshot_keys(&entries);
    loop {
        let batch = next_turn_batch(&client, id, cursor, 30_000)
            .await?
            .map_err(|e| anyhow::anyhow!("turn.stream: {e}"))?;
        cursor = batch.next_from_seq;
        for item in batch.items {
            let turn: fq_ops::turn::TurnState = serde_json::from_value(item.item)?;
            let entry = turn.transcript_entry();
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
    // The tail loop runs until Ctrl-C or a transport error (`?`).
}

#[cfg(test)]
mod tests;
