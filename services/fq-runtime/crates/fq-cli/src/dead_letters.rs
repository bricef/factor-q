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
//! `fq dead-letters requeue` sits alongside it and is now flipped too
//! (plan Phase 4, verb 8): it used to open its own broker connection
//! and publish a fresh trigger from the client, which is also why it
//! could not be idempotent — a client that publishes has nowhere to
//! record that it did. The daemon records the requeue and the record
//! is the guarantee.

use anyhow::Context;

use crate::cli::GlobalArgs;
use crate::edge_call::{edge_client_for, edge_invoke};
use crate::truncate_json;
use fq_ops::surface::DeadLetterFilter;
use fq_ops::surface::TriggerKey;

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
                cap = fq_ops::surface::DEAD_LETTER_LIST_MAX_LIMIT
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
    let states: Vec<fq_ops::dead_letter::DeadLetterState> = serde_json::from_value(output)?;
    let dead: Vec<&fq_ops::dead_letter::DeadLetter> =
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

/// What `fq dead-letters requeue --json` prints: the requeued trigger,
/// as the surface answers for it.
///
/// Composed from the receipt and the read it walks to, not from
/// anything this process minted — the client publishes nothing now, so
/// there is nothing here it could know first-hand.
#[derive(serde::Serialize)]
struct RequeueResult {
    agent_id: String,
    /// The requeued trigger's identity — `trigger.get` takes this.
    trigger_id: String,
    /// The trigger it was requeued from.
    requeued_from: Option<String>,
    trigger_payload: serde_json::Value,
}

/// `fq dead-letters requeue`: re-run a dead-lettered trigger over the
/// authenticated edge (plan Phase 4, verb 8). One client, two
/// questions: the `dead_letter.requeue` command, then the Trigger the
/// receipt names.
///
/// The second question is not a nicety, and it is `invocation drop`'s
/// arrangement for `invocation drop`'s reason: a receipt references the
/// atoms a command appended and never state (D3/P4), so the payload
/// this line has always printed now comes from a read rather than from
/// a result struct the client half-built.
///
/// **The read needs no watermark**, which is the difference from drop.
/// A requeue writes the trigger's permanent record before it publishes
/// — that write is what makes the requeue idempotent — so by the time
/// the receipt is in hand the record is there. There is no fold to wait
/// for and nothing to gate on.
pub(crate) async fn requeue_dead_letter(
    global: &GlobalArgs,
    agent: &str,
    trigger_seq: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    // Answered offline, in the same breath as `fq agent validate`,
    // rather than costing a round trip. The daemon checks it again —
    // this is a courtesy, never a substitute.
    fq_ops::agent::AgentId::new(agent).with_context(|| format!("invalid agent name '{agent}'"))?;

    let client = edge_client_for(global).await?;
    let receipt = client
        .invoke(
            fq_ops::OpId::Verb(fq_ops::VerbId::DeadLetter(fq_ops::DeadLetter::Requeue)),
            serde_json::json!({ "agent_id": agent, "trigger_seq": trigger_seq }),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let receipt: fq_ops::Receipt = serde_json::from_value(receipt)?;
    let key = receipt
        .atoms
        .iter()
        .find(|atom| atom.domain == fq_ops::Domain::Trigger)
        .map(|atom| atom.key.clone())
        .context("the requeue receipt named no trigger — cannot confirm the requeue")?;
    let trigger_id: TriggerKey = serde_json::from_value(key.clone())?;

    let trigger = client
        .invoke(fq_ops::OpId::Get(fq_ops::Domain::Trigger), key)
        .await?
        // The requeue already landed, so this is never "the requeue
        // failed" — say so, and name what it made.
        .map_err(|e| {
            anyhow::anyhow!(
                "requeued as trigger {}, but reading it back failed: {e}",
                trigger_id.trigger_id
            )
        })?;
    let trigger: fq_ops::trigger::Trigger = serde_json::from_value(trigger)?;
    let result = RequeueResult {
        agent_id: agent.to_string(),
        trigger_id: trigger.id.to_string(),
        requeued_from: trigger.requeued_from.map(|id| id.to_string()),
        trigger_payload: trigger.payload,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "Requeued dead-lettered trigger for '{}' as trigger {}.",
        result.agent_id, result.trigger_id
    );
    if let Some(from) = &result.requeued_from {
        println!("  requeued from trigger {from}");
    }
    println!("  payload: {}", truncate_json(&result.trigger_payload, 120));
    println!("Requeueing the same dead letter again is refused, and names this trigger.");
    println!("A running `fq run` daemon will pick this up with a fresh delivery budget.");
    Ok(())
}
