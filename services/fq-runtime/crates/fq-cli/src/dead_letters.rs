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
//! `fq dead-letters requeue` stays in `lib.rs` until cohort 4.3 makes
//! it a `dead_letter.requeue` command — the same split `fq agent
//! validate` sits on, and for the same reason: a verb moves out when
//! it is flipped, not before.

use super::*;

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
        // The flag is `usize` and the wire contract `u32`: a limit past
        // four billion asks for everything a page can hold, which is
        // what saturating says.
        limit: Some(u32::try_from(limit).unwrap_or(u32::MAX)),
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
