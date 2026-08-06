//! The `fq dead-letters list` verb (plan Phase 4, verb 7): triggers
//! that exhausted their delivery budget, read over the authenticated
//! edge.
//!
//! It used to open its own NATS connection and run an ephemeral scan
//! of the event stream's `failed` subjects — the one read that could
//! not go through the projection, because the projection stores no
//! annotations and the annotations are where the trigger lives. The
//! substrate is unchanged; the scan is the daemon's, so `--agent`
//! narrows at the log rather than after the fact, and a client needs
//! no broker credentials to ask.
//!
//! `fq dead-letters requeue` sits alongside it but is not flipped: it
//! still publishes its own trigger over a direct broker connection,
//! and becomes a `dead_letter.requeue` command in cohort 4.3 — the
//! same split `fq agent validate` sits on, and for the same reason: a
//! verb rides the edge once it is flipped, not before.

use anyhow::Context;
use fq_runtime::EventBus;

use crate::cli::GlobalArgs;
use crate::dead_letter_atom::DeadLetterFilter;
use crate::edge_call::edge_invoke;
use crate::truncate_json;

/// The rendered listing, newest first.
///
/// The atom answers in sequence order, which is what makes List and
/// Stream compose; "newest first" is this listing's presentation, and
/// it is what the verb has always printed.
pub(crate) async fn list_dead_letters(
    global: &GlobalArgs,
    agent: Option<&str>,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let filter = DeadLetterFilter {
        agent: agent.map(str::to_string),
        // `--limit` travels as the caller wrote it, and the daemon is
        // the one authority on how big a page may be — so the only
        // thing decided here is the number that has no page to travel
        // as at all. The flag is `usize`, the wire contract `u32`, so
        // that is exactly the values above `u32::MAX`: nothing
        // negative can be typed, and everything else converts.
        //
        // It used to saturate to `u32::MAX`, on the reading that a
        // limit past four billion "asks for everything a page can
        // hold". Nothing holds everything — one List answer is one
        // frame, and the edge frames at 8 MiB — and `u32::MAX` is
        // itself far over the cap, so saturating swapped the
        // operator's number for one they never typed and then had
        // *that* refused, quoting the substitute back at them.
        limit: Some(u32::try_from(limit).map_err(|_| {
            anyhow::anyhow!(
                "--limit {limit} is not a page size: one page is at most {cap} dead \
                 letters. There is no spelling of \"all of them\" here — this used to \
                 substitute the largest number the wire can carry and let the daemon \
                 refuse that instead, reporting a limit you never typed. For more than a \
                 page, narrow with --agent — the only narrowing this listing offers; the \
                 cursored read of the same dead letters is `dead_letter.stream` on the \
                 operator surface, which no `fq` verb consumes yet.",
                cap = crate::dead_letter_atom::DEAD_LETTER_LIST_MAX_LIMIT
            )
        })?),
    };
    let output = edge_invoke(
        global,
        fq_ops::OpId::List(fq_ops::Domain::DeadLetter),
        serde_json::to_value(&filter)?,
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let states: Vec<fq_runtime::dead_letter::DeadLetterState> = serde_json::from_value(output)?;
    let dead: Vec<&fq_runtime::dead_letter::DeadLetter> =
        states.iter().rev().map(|s| &s.dead_letter).collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&dead)?);
        return Ok(());
    }
    if dead.is_empty() {
        println!("No dead-lettered triggers.");
        return Ok(());
    }
    println!("{} dead-lettered trigger(s), newest first:\n", dead.len());
    for d in &dead {
        let seq = d
            .trigger_stream_seq
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {}  {}  seq={} via {}",
            d.timestamp.format("%Y-%m-%d %H:%M:%S"),
            d.agent_id,
            seq,
            d.source,
        );
        println!("      {}", d.error_message);
        println!("      payload: {}", truncate_json(&d.trigger_payload, 120));
    }
    println!("\n-> `fq dead-letters requeue <agent> [--trigger-seq N]` to re-run one");
    Ok(())
}

/// `fq dead-letters requeue`: re-publish a dead-lettered trigger as a
/// fresh trigger. Not idempotent — see the command help.
pub(crate) async fn requeue_dead_letter(
    global: &GlobalArgs,
    agent: &str,
    trigger_seq: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    fq_runtime::AgentId::new(agent).with_context(|| format!("invalid agent name '{agent}'"))?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
    let result =
        fq_runtime::control_plane::operator::requeue_dead_letter(&bus, agent, trigger_seq).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "Requeued dead-lettered trigger for '{}' as trigger seq {} (from event {}).",
        result.agent_id, result.new_trigger_seq, result.source_event_id
    );
    println!("  payload: {}", truncate_json(&result.trigger_payload, 120));
    println!("A running `fq run` daemon will pick this up with a fresh delivery budget.");
    Ok(())
}
