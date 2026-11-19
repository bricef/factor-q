//! The `fq invocation` verbs (list, show, drop, resume, transcript): the
//! operator's triage surface over one invocation's life.
//!
//! Split out of `lib.rs` (#189) on the `workers.rs` precedent. Every verb here
//! rides the authenticated edge — the daemon answers from its views — so this
//! module is rendering plus the one round trip each verb needs. `resume` was
//! the last exception, reaching the broker directly; it is a declared command
//! now, and the client no longer knows a NATS url exists.

use anyhow::Context;
use fq_ops::agent::AgentId;

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
    let archive = archive_label(item.archive_state);
    let ended_by = ended_by_label(item);
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
        "{:<11} {:<10} {:<24} {:<24} {:<8} {:<8} {}",
        inv_short, item.status, agent_trim, worker_trim, archive, ended_by, summary
    )
}

/// The list's `ended by` column: who ended the run, `run` or `drop`; `-`
/// while it has not ended, and where the row does not say (a run that
/// ended before its row recorded an ender, an archive-only row). Pure;
/// covered by unit tests.
pub(crate) fn ended_by_label(item: &fq_ops::views::InvocationSummaryView) -> &'static str {
    item.ended_by
        .map_or("-", fq_ops::views::InvocationEnder::as_str)
}

/// The `ended by:` line of `fq invocation show`: who ended the run and
/// when, or `not recorded` for a run that ended before its row recorded an
/// ender. `None` while the run has not ended. Pure; covered by unit tests.
fn format_ended_by(owner: &fq_ops::views::InvocationSummaryView) -> Option<String> {
    match (owner.ended_by, owner.ended_at_ms) {
        (Some(ender), Some(at)) => Some(format!("{} at {}", ender.as_str(), utc(at))),
        (Some(ender), None) => Some(ender.as_str().to_string()),
        (None, _) if matches!(owner.status.as_str(), "completed" | "failed") => {
            Some("not recorded".to_string())
        }
        (None, _) => None,
    }
}

/// A request as a human line reads it: its time, who asked and why, each
/// only where the record says, the free text made safe for the terminal.
fn request_said(request: &fq_ops::views::TerminationRequestView) -> String {
    let mut said = format!("requested {}", utc(request.requested_at_ms));
    if let Some(by) = &request.requested_by {
        said.push_str(&format!(
            " by {}",
            fq_ops::text::one_line(by, fq_ops::text::REQUESTER_CHARS)
        ));
    }
    if let Some(reason) = &request.reason {
        said.push_str(&format!(
            ": {}",
            fq_ops::text::one_line(reason, fq_ops::text::REASON_CHARS)
        ));
    }
    said
}

/// The `termination:` line of `fq invocation show`: how the run's ending
/// reads beside the operator's request, then the request itself, its time,
/// who asked and why, each only where the record says. The record keeps
/// the first request, so this is the first one's. `None` when no request
/// is on record. Pure; covered by unit tests.
fn format_termination(owner: &fq_ops::views::InvocationSummaryView) -> Option<String> {
    let said = request_said(owner.termination.as_ref()?);
    Some(match owner.termination_reading() {
        Some(reading) => format!("{} ({said})", reading.phrase()),
        None => said,
    })
}

/// The list's `archive` column: whether the control plane holds the run's
/// archive. A run reads finished from its ending, and this says whether
/// the archive followed. Pure; covered by unit tests.
fn archive_label(state: fq_ops::views::InvocationArchiveState) -> &'static str {
    match state {
        fq_ops::views::InvocationArchiveState::NotDue => "-",
        fq_ops::views::InvocationArchiveState::Pending => "pending",
        fq_ops::views::InvocationArchiveState::Held => "held",
        fq_ops::views::InvocationArchiveState::Archived => "yes",
        fq_ops::views::InvocationArchiveState::WillNotCome => "none",
    }
}

/// The `archive:` line of `fq invocation show`, for a run that has ended
/// and whose archive has not reached the control plane: pending, held by
/// the worker's archive sweeper with the reason and what lifts it, or none
/// to come after a drop. `None` otherwise. Pure; covered by unit tests.
fn format_archive_state(detail: &fq_ops::views::InvocationDetailView) -> Option<String> {
    let owner = detail.owner.as_ref()?;
    match (owner.archive_state, &detail.archive_hold) {
        (fq_ops::views::InvocationArchiveState::Held, Some(hold)) => Some(format!(
            "held by the worker's archive sweeper: {}\n            -> {}",
            hold.reason.summary(),
            hold.remedy
        )),
        (
            fq_ops::views::InvocationArchiveState::Pending
            | fq_ops::views::InvocationArchiveState::Held,
            _,
        ) => Some(
            "pending: the run has ended and its archive has not reached the control plane"
                .to_string(),
        ),
        (fq_ops::views::InvocationArchiveState::WillNotCome, _) => Some(
            "none: the run was dropped with no row on its worker, so no archive will come"
                .to_string(),
        ),
        _ => None,
    }
}

/// The `trigger:` line of `fq invocation show` (#954): which delivery on
/// the trigger stream started the run, and where its claim stands.
///
/// Said when there is none, too, and worded for what an absent claim
/// shows. A delivery retried before its run's first durable write gives its
/// claim back, though the run can already have published events under the
/// claim's id (one dealt with keeps it, consumed), an adoption writes the
/// adopter's id over the old one, and the daemon's retention sweep deletes a claim once it is older than
/// `[state] trigger_claim_retention_days`. So no claim is not proof the
/// run came from somewhere other than the stream.
/// Pure; covered by unit tests.
fn format_trigger_delivery(trigger: Option<&fq_ops::views::TriggerDeliveryView>) -> String {
    let Some(t) = trigger else {
        return "none recorded: not started from the trigger stream, its claim was released or \
                taken over before the run's first durable write, or the claim is older than its \
                retention window"
            .to_string();
    };
    let state = match t.state {
        fq_ops::views::TriggerClaimState::DurablyStarted => "durably started".to_string(),
        fq_ops::views::TriggerClaimState::Claimed => "claimed, start not recorded".to_string(),
        fq_ops::views::TriggerClaimState::Consumed => {
            let why = t.consumed_why.as_deref().unwrap_or("no reason given");
            match t.operator_dropped_at_ms {
                // Beside the reason, never in its place: the run's status
                // and archive come from the drop, not from a start.
                Some(at) => format!(
                    "consumed without a run: {why}; dropped by an operator, recorded {}",
                    utc(at)
                ),
                None => format!("consumed without a run: {why}"),
            }
        }
        fq_ops::views::TriggerClaimState::Unknown => {
            "in a state this build does not know".to_string()
        }
    };
    format!(
        "{} seq {} ({state}; claimed {} by {})",
        t.stream,
        t.stream_seq,
        utc(t.claimed_at_ms),
        t.claimant
    )
}

/// An epoch-ms time as the claim line shows it, or the raw number when it
/// is not a time chrono can show.
fn utc(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{at_ms}ms"))
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
            "{:<11} {:<10} {:<24} {:<24} {:<8} {:<8} summary",
            "invocation", "status", "agent", "worker", "archive", "ended by"
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
            if let Some(ended_by) = format_ended_by(o) {
                println!("  ended by: {ended_by}");
            }
            if let Some(termination) = format_termination(o) {
                println!("  termination: {termination}");
            }
        } else {
            println!("  status:   (no coordination row)");
        }
        if let Some(archive) = format_archive_state(&detail) {
            println!("  archive:  {archive}");
        }
        if let Some(a) = &detail.archive {
            println!(
                "  archived: phase={} terminal_at_ms={} archived_at_ms={}",
                a.final_phase, a.terminal_at_ms, a.archived_at_ms
            );
        }
        println!(
            "  trigger:  {}",
            format_trigger_delivery(detail.trigger.as_ref())
        );
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
        print!("{}", format_resume_accepted(id));
    }
    Ok(())
}

/// What `fq invocation resume` prints once the daemon has accepted.
///
/// A receipt says what was done, not where the run stands, so the lines
/// say what the daemon does with it (#994) rather than claiming it is
/// already running. The agent's cap is the one bound the run is not held
/// at (issue 1009): the sentence is printed every time, and worded so it
/// is true of every agent, because the receipt names only the invocation
/// and knowing the agent's `max_concurrent` would take two more reads.
/// Pure; covered by unit tests.
fn format_resume_accepted(id: &str) -> String {
    format!(
        "Resume accepted for invocation {id}.\n\
         It runs under [worker] max_concurrent_invocations like any other invocation: \
         at once if a worker is free, otherwise when one frees.\n\
         If its agent sets max_concurrent, this run counts against that limit but is not held \
         back by it, so the agent can be over its limit until one of its invocations ends.\n\
         Follow with `fq invocation show {id}` or `fq invocation transcript {id}`.\n"
    )
}

#[derive(serde::Serialize, Debug)]
struct InvocationDropResult {
    invocation_id: String,
    /// The run's agent, `operator` for a run whose only events are
    /// operator-issued. `null` when reading the drop back failed.
    agent_id: Option<String>,
    event_id: String,
    reason: Option<String>,
    /// What this drop found, read from its own event's time against the
    /// run's ending on the owner row at the receipt: `dropped`,
    /// `ended_by_an_earlier_drop`, `already_ended` or
    /// `reached_its_own_ending_first`. `null` when the row does not say.
    case: Option<fq_ops::views::DropCase>,
    /// Whether the ending the row reads can still be replaced: a drop's
    /// until the run's archive lands. If the run reached its own ending
    /// first, that ending stands, and `fq invocation show` reads it.
    provisional: bool,
    /// The request on record when it is not this one: an earlier drop's,
    /// which the record keeps. `null` when this drop's is the one on
    /// record.
    earlier_request: Option<fq_ops::views::TerminationRequestView>,
    /// Why reading the drop back failed, when it did: the drop is
    /// recorded, and only what it found is not known. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    read_back_failed: Option<String>,
    /// Whether no archive of the run is expected (its archive is `none`):
    /// what the human line says of a provisional ending. Not in JSON,
    /// whose `provisional` says the same of every row.
    #[serde(skip)]
    expects_no_archive: bool,
}

impl InvocationDropResult {
    /// A drop read back: its own event's time, `requested_at_ms`, against
    /// the run's ending on `detail`'s owner row.
    fn read_back(
        id: &str,
        event_id: String,
        reason: Option<&str>,
        requested_at_ms: i64,
        detail: &fq_ops::views::InvocationDetailView,
    ) -> Self {
        let owner = detail.owner.as_ref();
        Self {
            invocation_id: id.to_string(),
            // An invocation whose only events are operator-issued has no
            // agent of its own; the drop was published as `operator`, and
            // that is what it is reported as.
            agent_id: Some(
                detail
                    .agent_id
                    .clone()
                    .unwrap_or_else(|| AgentId::operator().into_inner()),
            ),
            event_id,
            reason: reason.map(str::to_string),
            case: owner.and_then(|owner| owner.drop_case(requested_at_ms)),
            provisional: owner
                .is_some_and(fq_ops::views::InvocationSummaryView::ending_is_provisional),
            earlier_request: owner
                .and_then(|owner| owner.earlier_request(requested_at_ms))
                .cloned(),
            read_back_failed: None,
            expects_no_archive: owner.is_some_and(|owner| {
                owner.archive_state == fq_ops::views::InvocationArchiveState::WillNotCome
            }),
        }
    }

    /// A drop recorded whose reading back failed with `error`.
    fn unread(id: &str, event_id: String, reason: Option<&str>, error: String) -> Self {
        Self {
            invocation_id: id.to_string(),
            agent_id: None,
            event_id,
            reason: reason.map(str::to_string),
            case: None,
            provisional: false,
            earlier_request: None,
            read_back_failed: Some(error),
            expects_no_archive: false,
        }
    }
}

/// Read a recorded drop back, gated at its receipt's `event_seq`: its own
/// event, by the identity the receipt names (a run with more events than
/// the detail's recent list holds may not list it), for its time; and the
/// run's detail, for its row.
async fn read_the_drop_back(
    client: &fq_edge::EdgeClient,
    id: &str,
    event_id: &str,
    event_seq: u64,
) -> anyhow::Result<(i64, fq_ops::views::InvocationDetailView)> {
    let event = client
        .invoke_gated(
            fq_ops::OpId::Get(fq_ops::Domain::Event),
            serde_json::to_value(fq_ops::surface::EventKey {
                event_id: event_id.to_string(),
            })?,
            Some(event_seq),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("the drop event: {e}"))?;
    let event: fq_ops::events::EventState = serde_json::from_value(event)?;
    let detail = client
        .invoke_gated(
            fq_ops::OpId::Get(fq_ops::Domain::Invocation),
            serde_json::to_value(InvocationViewKey {
                invocation_id: id.to_string(),
            })?,
            Some(event_seq),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("the invocation: {e}"))?;
    Ok((
        event.event.envelope.timestamp.timestamp_millis(),
        serde_json::from_value(detail)?,
    ))
}

/// Drop an invocation over the authenticated edge (plan Phase 4, verb
/// 18). One client, three questions: the `invocation.drop` command, then,
/// gated at the receipt's watermark, the drop event the receipt names
/// (its own time) and the Invocation view (the run's row).
///
/// The reads are not a nicety. A receipt references the atoms a command
/// appended — never state (D3/P4) — so the agent, and what the drop
/// found, come from gated reads rather than from a locally minted event.
/// The horizon releases them only once the drop's fold, the ownership
/// row's move and the request, is visible too. A read that fails is
/// reported as such: the drop is recorded, and the command succeeds.
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
    let event_id = receipt
        .atoms
        .iter()
        .find(|atom| atom.domain == fq_ops::Domain::Event)
        .and_then(|atom| atom.key.get("event_id"))
        .and_then(serde_json::Value::as_str)
        .context("the drop receipt named no event — cannot confirm the drop")?
        .to_string();
    // The drop is recorded from here on. What it found comes from reading
    // it back; a read that fails is reported as that, never as a failed
    // drop, and the command still succeeds.
    let result = match read_the_drop_back(&client, id, &event_id, event_seq).await {
        Ok((requested_at_ms, detail)) => {
            InvocationDropResult::read_back(id, event_id, reason, requested_at_ms, &detail)
        }
        Err(err) => InvocationDropResult::unread(id, event_id, reason, format!("{err:#}")),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print!("{}", format_drop_recorded(&result));
    }
    Ok(())
}

/// What `fq invocation drop` prints: the drop is recorded, and what this
/// drop found, read from its own time against the run's ending at the
/// receipt; that the ending can still be replaced, while it can; the
/// earlier request the record keeps, when there is one; and the reason
/// given. Pure; covered by unit tests.
fn format_drop_recorded(result: &InvocationDropResult) -> String {
    let id = &result.invocation_id;
    let mut out = match (&result.read_back_failed, &result.agent_id) {
        (Some(error), _) => format!(
            "Drop of invocation {id} recorded (event_id={}), but reading it back failed, so \
             what it found is not known: {}.\n",
            result.event_id,
            fq_ops::text::one_line(error, fq_ops::text::REASON_CHARS)
        ),
        (None, agent) => {
            let found = result
                .case
                .map(|case| format!(": {}", case.phrase()))
                .unwrap_or_default();
            format!(
                "Drop of invocation {id} recorded (agent={}, event_id={}){found}.\n",
                agent.as_deref().unwrap_or("?"),
                result.event_id
            )
        }
    };
    if result.provisional {
        out.push_str(match (result.case, result.expects_no_archive) {
            // The line above names no ending: say only what can come.
            (None, _) => {
                "If the run's own ending arrives, it is recorded in place of what the row holds \
                 now, and `fq invocation show` reads the final record.\n"
            }
            (Some(_), true) => {
                "No archive of the run is expected; if its own ending arrives after all, that \
                 ending stands, and `fq invocation show` reads the final record.\n"
            }
            (Some(_), false) => {
                "That ending is provisional until the run's archive lands: if the run reached \
                 its own ending first, its own ending stands, and `fq invocation show` reads \
                 the final record.\n"
            }
        });
    }
    if let Some(earlier) = &result.earlier_request {
        out.push_str(&format!(
            "An earlier request is on record ({}); the record keeps the first.\n",
            request_said(earlier)
        ));
    }
    if let Some(reason) = &result.reason {
        out.push_str(&format!(
            "Reason given: {}\n",
            fq_ops::text::one_line(reason, fq_ops::text::REASON_CHARS)
        ));
    }
    out.push_str(&format!(
        "Follow with `fq invocation show {id}` to read the record.\n"
    ));
    out
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
    reasoning: bool,
) -> anyhow::Result<()> {
    use fq_ops::transcript::{
        DEFAULT_TRUNCATE_BYTES, RenderOptions, dedup_key, render_pretty, snapshot_keys,
    };

    let as_json = json || matches!(format, Some(TranscriptFormat::Json));
    if json && matches!(format, Some(TranscriptFormat::Pretty)) {
        anyhow::bail!("--json conflicts with --format pretty");
    }
    if follow && as_json {
        anyhow::bail!("--follow is not supported with --json (json emits a snapshot array)");
    }
    let render = RenderOptions {
        truncate_bytes: if full {
            None
        } else {
            Some(DEFAULT_TRUNCATE_BYTES)
        },
        reasoning,
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

    print!("{}", render_pretty(&entries, render));

    // Snapshot-only mode: done. Otherwise long-poll the turn stream
    // from the cursor pinned above (before the snapshot).
    let Some(mut cursor) = follow_cursor else {
        return Ok(());
    };

    println!();
    println!("── following turn.stream (invocation {id}); Ctrl-C to exit ──");

    let mut seen = snapshot_keys(&entries);
    'follow: loop {
        let batch = next_turn_batch(&client, id, cursor, 30_000)
            .await?
            .map_err(|e| anyhow::anyhow!("turn.stream: {e}"))?;
        cursor = batch.next_from_seq;
        for item in batch.items {
            let turn: fq_ops::turn::TurnState = serde_json::from_value(item.item)?;
            let entry = turn.transcript_entry();
            // The outcome is the invocation's last turn, so this is
            // where the tail ends — the CLI twin of the dashboard
            // client's `s.done`. Read *before* the dedup check, not
            // after: the outcome keys on a constant, so on a run that
            // was already finished when `--follow` started, the stream
            // redelivers an outcome the snapshot printed, dedup skips
            // the print, and a `break` behind that check would never
            // fire. Without it the loop keeps issuing 30 s polls from a
            // cursor past the end of a log that will never grow.
            let terminal = matches!(entry, fq_ops::transcript::TranscriptEntry::Outcome { .. });
            if let Some(key) = dedup_key(&entry)
                && !seen.insert(key)
            {
                if terminal {
                    break 'follow;
                }
                continue;
            }
            print!("{}", render_pretty(std::slice::from_ref(&entry), render));
            if terminal {
                break 'follow;
            }
        }
    }
    Ok(())
    // Otherwise the tail loop runs until Ctrl-C or a transport error (`?`).
}

#[cfg(test)]
mod tests;
