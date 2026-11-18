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
        print!("{}", dead_letter_lines(d));
    }
    println!("\n-> `fq dead-letters requeue <agent> [--trigger-seq N]` to re-run one");
    Ok(())
}

/// One dead letter's lines in the listing.
///
/// A dead letter the watch wrote without checking whether the trigger's
/// run started (issue 967) gets two lines of its own: that it was not
/// checked, and what a requeue of it does. It may be false, and requeuing
/// a trigger whose run started runs the work twice, so
/// `fq dead-letters requeue` checks the run's records first (issue 1059):
/// it refuses a start on record, requeues with nothing on record, and
/// when it still cannot tell, refuses unless given
/// `--checked-not-started`. The second line says so, and names the run to
/// check by hand for that last case: its refusal gives the check itself.
///
/// A dead letter that names no run (its claim, or its mark, could not be
/// read) points at the agent's `triggered` events, which name each run and
/// carry its payload: `fq invocation list` reads only ownership rows, so
/// it misses a run still going in this daemon.
///
/// A dead letter the watch wrote for an advisory from an earlier
/// incarnation of the trigger stream (issue 1054) gets a line saying why it
/// names no trigger: listed under the system sentinel with no payload, it
/// would read like one whose trigger aged out, and its `seq=` like a
/// position on the stream as it is now.
fn dead_letter_lines(d: &fq_ops::dead_letter::DeadLetter) -> String {
    let seq = d
        .trigger_stream_seq
        .map(|s| s.to_string())
        .unwrap_or_else(|| "-".to_string());
    let mut out = format!(
        "  {}  {}  seq={} via {}\n      {}\n      payload: {}\n",
        d.timestamp.format("%Y-%m-%d %H:%M:%S"),
        d.agent_id,
        seq,
        d.source,
        d.error_message,
        truncate_json(&d.trigger_payload, 120),
    );
    if let Some(why) = &d.unattributed {
        out.push_str(&format!(
            "      names no trigger: {why}; `fq dead-letters requeue` refuses it\n"
        ));
    }
    if let Some(unchecked) = &d.start_unchecked {
        out.push_str(&format!(
            "      start not checked: {}; its run may have started\n",
            unchecked.reason
        ));
        out.push_str(&match &unchecked.invocation_id {
            Some(id) => format!(
                "      -> `fq dead-letters requeue` checks the run's records first and refuses one \
                 that started; if it cannot tell, it refuses unless given --checked-not-started, \
                 which is for once `fq invocation show {id}` shows the run did not start\n"
            ),
            None => format!(
                "      -> no run is named (the trigger's claim, or this mark, could not be read): \
                 `fq dead-letters requeue` checks the run's records first and refuses one that \
                 started; if it cannot tell, it refuses unless given --checked-not-started, which \
                 is for once you have found the run among {agent}'s `triggered` events \
                 (`fq events query --agent {agent} --type triggered`) and `fq invocation show \
                 <id>` shows it did not start\n",
                agent = d.agent_id
            ),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead(
        start_unchecked: Option<fq_ops::dead_letter::StartUnchecked>,
    ) -> fq_ops::dead_letter::DeadLetter {
        fq_ops::dead_letter::DeadLetter {
            event_id: "019a0000-0000-7000-8000-000000000009".to_string(),
            timestamp: chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
            agent_id: "researcher".to_string(),
            trigger_subject: "fq.trigger.researcher".to_string(),
            trigger_stream_seq: Some(12),
            source: "advisory".to_string(),
            trigger_payload: serde_json::json!({"n": 2}),
            error_message: "trigger exhausted after 5 deliveries (limit 5)".to_string(),
            start_unchecked,
            unattributed: None,
        }
    }

    /// One the watch wrote for an advisory from an earlier incarnation of
    /// the trigger stream says why it names no trigger, and that a requeue
    /// is refused.
    #[test]
    fn an_unattributed_dead_letter_says_why_it_names_no_trigger() {
        let unattributed = fq_ops::dead_letter::DeadLetter {
            agent_id: "system".to_string(),
            trigger_subject: String::new(),
            trigger_payload: serde_json::Value::Null,
            unattributed: Some(
                "its advisory came from an earlier incarnation of the trigger stream".to_string(),
            ),
            ..dead(None)
        };
        assert_eq!(
            dead_letter_lines(&unattributed),
            "  2026-01-01 00:00:00  system  seq=12 via advisory\n      trigger exhausted after 5 \
             deliveries (limit 5)\n      payload: null\n      names no trigger: its advisory \
             came from an earlier incarnation of the trigger stream; `fq dead-letters requeue` \
             refuses it\n"
        );
    }

    /// One that names no trigger and whose start was not checked either
    /// (the trigger's message and its run's records both unreadable past
    /// the limit) cannot be requeued, so its check is not offered as one to
    /// make "before requeueing": it says how to tell whether the run started,
    /// and, with no run named, that nothing on it can tell.
    #[test]
    fn a_nameless_unchecked_dead_letter_is_not_offered_a_requeue_check() {
        let nameless = |invocation_id: Option<&str>| fq_ops::dead_letter::DeadLetter {
            agent_id: "system".to_string(),
            trigger_subject: String::new(),
            trigger_payload: serde_json::Value::Null,
            unattributed: Some("the trigger's message could not be read".to_string()),
            ..dead(Some(fq_ops::dead_letter::StartUnchecked {
                reason: "the worker could not say whether its WAL holds the run".to_string(),
                invocation_id: invocation_id.map(str::to_string),
            }))
        };
        let named = dead_letter_lines(&nameless(Some("019a0000-0000-7000-8000-000000000001")));
        assert!(
            named.ends_with(
                "      start not checked: the worker could not say whether its WAL holds the run; \
                 its run may have started\n      -> it cannot be requeued; to tell whether its run \
                 started, run `fq invocation show 019a0000-0000-7000-8000-000000000001`: it \
                 started if that shows a `Live execution:` block, an `archived:` line, a \
                 `status:` other than `(no coordination row)` or a trigger line saying `durably \
                 started`\n"
            ),
            "got:\n{named}"
        );
        let unnamed = dead_letter_lines(&nameless(None));
        assert!(
            unnamed.ends_with(
                "      -> it cannot be requeued, and it names neither the trigger nor its run (the \
                 claim, or this mark, could not be read), so nothing on it tells whether the run \
                 started\n"
            ),
            "got:\n{unnamed}"
        );
        assert!(!unnamed.contains("before requeueing"), "got:\n{unnamed}");
    }

    /// A dead letter whose start was checked reads as it always has.
    #[test]
    fn a_checked_dead_letter_has_no_warning_line() {
        assert_eq!(
            dead_letter_lines(&dead(None)),
            "  2026-01-01 00:00:00  researcher  seq=12 via advisory\n      trigger exhausted \
             after 5 deliveries (limit 5)\n      payload: {\"n\":2}\n"
        );
    }

    /// One the watch wrote without checking the trigger's start says so,
    /// with the reason, and says what a requeue of it does: the daemon
    /// checks the records first and refuses a run that started, and the
    /// override is for when it cannot tell, once the run it names is
    /// checked; with no invocation named, where to find the run.
    #[test]
    fn an_unchecked_dead_letter_says_so_and_what_a_requeue_does() {
        let named = dead_letter_lines(&dead(Some(fq_ops::dead_letter::StartUnchecked {
            reason: "a copy of the trigger is still handling its claim".to_string(),
            invocation_id: Some("019a0000-0000-7000-8000-000000000001".to_string()),
        })));
        assert!(
            named.ends_with(
                "      start not checked: a copy of the trigger is still handling its claim; its \
                 run may have started\n      -> `fq dead-letters requeue` checks the run's \
                 records first and refuses one that started; if it cannot tell, it refuses \
                 unless given --checked-not-started, which is for once `fq invocation show \
                 019a0000-0000-7000-8000-000000000001` shows the run did not start\n"
            ),
            "got:\n{named}"
        );
        let unnamed = dead_letter_lines(&dead(Some(fq_ops::dead_letter::StartUnchecked {
            reason: "the trigger's claim could not be read: disk I/O error".to_string(),
            invocation_id: None,
        })));
        assert!(
            unnamed.contains(
                "found the run among researcher's `triggered` events (`fq events query --agent \
                 researcher --type triggered`)"
            ),
            "got:\n{unnamed}"
        );
        assert!(
            unnamed.contains("refuses unless given --checked-not-started"),
            "got:\n{unnamed}"
        );
    }
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
///
/// **The verdict is the daemon's** (issue 1059). This client never holds
/// the dead letter, so it cannot check whether the trigger's run started;
/// the daemon does, with the dead letter in hand, and its refusal is
/// printed as it gives it. `checked_not_started` is the operator's word,
/// passed through; the daemon decides what it is worth.
pub(crate) async fn requeue_dead_letter(
    global: &GlobalArgs,
    agent: &str,
    trigger_seq: Option<u64>,
    checked_not_started: bool,
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
            serde_json::json!({
                "agent_id": agent,
                "trigger_seq": trigger_seq,
                "checked_not_started": checked_not_started,
            }),
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
    println!("A running `fqd` daemon will pick this up with a fresh delivery budget.");
    Ok(())
}
