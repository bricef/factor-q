//! The DeadLetter atom (plan Phase 4, cohort 4.2, verb 7): triggers
//! that exhausted their delivery budget, read from the log the
//! exhaustion was recorded on.
//!
//! Unlike the Invocation or Worker resources this is not a projection
//! fold — there is no dead-letter table anywhere. The atom is a lens
//! over the event log's `fq.agent.*.failed` subjects
//! ([`fq_runtime::dead_letter::DeadLetter::from_event`] is the whole
//! of the predicate), which is why `fq dead-letters list` needed NATS
//! when `fq events query` did not: the projection stores no
//! annotations, and the annotations are where the trigger lives.
//!
//! Its own module rather than more of `operator_surface.rs` (the
//! Event atom's precedent): that file is the daemon's assembly point
//! and is near its size budget (#189).

use fq_edge::wire::WireError;
use fq_runtime::dead_letter::{DeadLetter, DeadLetterState};
use fq_runtime::events::subjects;

/// Get identity for a DeadLetter: its event-log sequence.
///
/// **Not** the trigger sequence the human listing prints and
/// `dead_letter.requeue` selects by — that is a coordinate on the
/// trigger stream, is not unique across agents, and is absent whenever
/// the trigger aged out before the advisory landed. See
/// [`DeadLetterState`] for the full reasoning.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DeadLetterKey {
    pub(crate) seq: u64,
}

/// List/Stream selection for DeadLetters — the typed, schema'd filter,
/// never a query language.
///
/// It carries exactly the narrowing `fq dead-letters list` offers
/// (`--agent`, `--limit`) and nothing invented alongside: a filter is a
/// promise the surface has to keep, so it grows when a caller needs it
/// to (P11), not when a field looks plausible.
#[derive(Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct DeadLetterFilter {
    /// One agent's dead letters. Absent reads every agent's.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// Cap on one List page — the most recent N matching dead letters.
    #[serde(default)]
    pub(crate) limit: Option<u32>,
}

/// Cap on one stream batch.
const DEAD_LETTER_BATCH_CAP: usize = 64;
/// Default List page, matching `fq dead-letters list --limit`'s default.
const DEAD_LETTER_LIST_DEFAULT_LIMIT: u32 = 50;
/// Ceiling on a `next_batch` long poll, whatever the caller asks.
const DEAD_LETTER_MAX_WAIT_CEILING_MS: u64 = 60_000;

/// The consumer subject one read is scoped to.
///
/// The whole selection pushes down to the subject here, so unlike the
/// Event atom there is no per-message predicate beyond "is this a dead
/// letter at all": the log is partitioned by agent, and agent is the
/// only narrowing this filter offers.
fn selection_subject(filter: &DeadLetterFilter, op: &str) -> Result<String, WireError> {
    match filter.agent.as_deref() {
        // An id that is not a subject token cannot name any event, and
        // interpolating it would build a malformed filter — so it is a
        // verdict on the request rather than an empty answer.
        Some(agent) => fq_runtime::AgentId::new(agent)
            .map(|_| subjects::agent_failed(agent))
            .map_err(|e| WireError::InvalidInput {
                op: op.to_string(),
                message: format!("agent `{agent}`: {e}"),
            }),
        None => Ok(subjects::ALL_AGENTS_FAILED.to_string()),
    }
}

/// Register the DeadLetter atom.
///
/// **It carries a Stream, deliberately.** `fq dead-letters list` does
/// not tail today, and the overlay is not free surface to be minted on
/// a whim — but a dead letter is an immutable fact, an atom's nature is
/// structural in this model, and Get+List+Stream is what that nature
/// derives (`fq_ops::Registry::derived_ops`). Declaring the resource
/// without a stream would mean declaring it as something it is not.
/// The domain model names this exact overlay as one worth having:
/// *"Stream(DeadLetter) — tell me the moment something dead-letters"*.
pub(crate) fn register_dead_letter_atom(
    registry: &mut fq_edge::EdgeRegistry,
    bus: fq_runtime::EventBus,
) -> anyhow::Result<()> {
    let decl = fq_ops::Atom::new::<DeadLetterKey, DeadLetterState, DeadLetterFilter>(
        fq_ops::Domain::DeadLetter,
        "A trigger that exhausted its delivery budget, as its terminal event records it.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Event-log-backed, not a stored table: `seq` is the stream sequence of \
         the `failed` event that recorded the exhaustion — the same cursor \
         receipts, `min_seq` gates and the Event and Turn atoms speak. \
         `trigger_stream_seq` is a different number entirely (the original \
         trigger's, on the trigger stream) and may be absent. List answers \
         with the most recent `limit` matching dead letters in sequence \
         order, bounded by the tip observed at entry; Stream long-polls via \
         next_batch from a sequence, and `from_seq = u64::MAX` seeks the tail. \
         Visibility is bounded by event-stream retention (30 days by default); \
         the trigger payload a requeue needs is bounded by the trigger \
         stream's, which is shorter.",
    );

    let get_bus = bus.clone();
    let list_bus = bus.clone();
    registry
        .atom::<DeadLetterKey, DeadLetterState, DeadLetterState, DeadLetterFilter, _, _, _, _, _, _>(
            decl,
            move |key: DeadLetterKey| {
                let bus = get_bus.clone();
                async move { dead_letter_at(&bus, key.seq).await }
            },
            move |filter: DeadLetterFilter| {
                let bus = list_bus.clone();
                async move {
                    let subject = selection_subject(&filter, "dead_letter.list")?;
                    let limit = filter.limit.unwrap_or(DEAD_LETTER_LIST_DEFAULT_LIMIT) as usize;
                    list_dead_letters(&bus, &subject, limit).await
                }
            },
            move |filter: DeadLetterFilter, from_seq, max_wait_ms| {
                let bus = bus.clone();
                async move {
                    let subject = selection_subject(&filter, "dead_letter.stream")?;
                    stream_dead_letters(&bus, &subject, from_seq, max_wait_ms).await
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}

fn internal(e: fq_runtime::bus::BusError) -> WireError {
    WireError::Internal {
        message: e.to_string(),
    }
}

/// Get one dead letter by log sequence — the read an `AtomRef {
/// domain: DeadLetter, seq }` resolves.
///
/// A sequence that holds something else — an ordinary failure, another
/// agent's turn, a heartbeat — is a miss, not an error: the atom is the
/// exhaustion, not the message that happens to sit at that offset.
async fn dead_letter_at(
    bus: &fq_runtime::EventBus,
    seq: u64,
) -> Result<DeadLetterState, WireError> {
    use futures::StreamExt;
    let not_found = || WireError::NotFound {
        op: "dead_letter.get".into(),
        message: format!("no dead letter at sequence {seq}"),
    };
    let subject = subjects::ALL_AGENTS_FAILED;
    // The consumer below is filtered, so a sequence past the last
    // matching message would block until the timeout rather than
    // answer. Ask the server where the subject ends first: a miss then
    // costs a round trip instead of five seconds.
    let tip = bus
        .last_event_seq_matching(subject)
        .await
        .map_err(internal)?;
    if seq == 0 || seq > tip {
        return Err(not_found());
    }
    let mut events = bus.events_from(subject, seq).await.map_err(internal)?;
    let (got_seq, event) = tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
        .await
        .map_err(|_| not_found())?
        .ok_or_else(not_found)?
        .map_err(internal)?;
    // A filtered consumer hands back the next matching message when
    // the requested sequence is not one; that is a different atom.
    if got_seq != seq {
        return Err(not_found());
    }
    DeadLetter::from_event(&event)
        .map(|dead_letter| DeadLetterState {
            seq: got_seq,
            dead_letter,
        })
        .ok_or_else(not_found)
}

/// The most recent `limit` dead letters on `subject`, in **sequence
/// order**, bounded by the tip observed at entry.
///
/// Sequence order rather than the newest-first the CLI renders: List
/// and Stream compose into one idiom — List says what exists as of a
/// watermark, Stream continues from there — and that composition needs
/// the page to end where the stream begins. Presentation order is the
/// verb's business, and `fq dead-letters list` reverses for display
/// exactly as it always has.
async fn list_dead_letters(
    bus: &fq_runtime::EventBus,
    subject: &str,
    limit: usize,
) -> Result<Vec<DeadLetterState>, WireError> {
    use futures::StreamExt;
    // The bound is the last sequence this *subject* carries, not the
    // stream's: a scan that waited for the stream tip would wait
    // forever whenever the tip is a message the filter excludes — the
    // hang the Turn and Event atoms both had to fix.
    let tip = bus
        .last_event_seq_matching(subject)
        .await
        .map_err(internal)?;
    if tip == 0 || limit == 0 {
        return Ok(Vec::new());
    }
    let mut events = bus.events_from(subject, 1).await.map_err(internal)?;
    let mut window: std::collections::VecDeque<DeadLetterState> = std::collections::VecDeque::new();
    while let Some(next) = events.next().await {
        let (seq, event) = next.map_err(internal)?;
        if let Some(dead_letter) = DeadLetter::from_event(&event) {
            if window.len() == limit {
                window.pop_front();
            }
            window.push_back(DeadLetterState { seq, dead_letter });
        }
        if seq >= tip {
            break;
        }
    }
    Ok(window.into())
}

/// One long-poll batch of dead letters at or after `from_seq`;
/// `u64::MAX` seeks the tail. The cursor advances past ordinary
/// failures too, so an idle poll still makes progress.
async fn stream_dead_letters(
    bus: &fq_runtime::EventBus,
    subject: &str,
    from_seq: u64,
    max_wait_ms: u64,
) -> Result<fq_edge::wire::StreamBatch, WireError> {
    use futures::StreamExt;
    let from_seq = if from_seq == u64::MAX {
        bus.last_event_seq().await.map_err(internal)? + 1
    } else {
        from_seq
    };
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(max_wait_ms.min(DEAD_LETTER_MAX_WAIT_CEILING_MS));
    let mut events = bus.events_from(subject, from_seq).await.map_err(internal)?;
    let mut items = Vec::new();
    let mut next_from_seq = from_seq;
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or(std::time::Duration::ZERO);
        // Once something is in hand, only drain what's immediately
        // ready; before that, wait out the long poll.
        let wait = if items.is_empty() {
            remaining
        } else {
            std::time::Duration::from_millis(10)
        };
        let next = match tokio::time::timeout(wait, events.next()).await {
            Ok(Some(next)) => next.map_err(internal)?,
            Ok(None) | Err(_) => break,
        };
        let (seq, event) = next;
        next_from_seq = seq + 1;
        if let Some(dead_letter) = DeadLetter::from_event(&event) {
            let item = serde_json::to_value(DeadLetterState { seq, dead_letter }).map_err(|e| {
                WireError::Internal {
                    message: e.to_string(),
                }
            })?;
            items.push(fq_edge::wire::StreamItem { seq, item });
            if items.len() >= DEAD_LETTER_BATCH_CAP {
                break;
            }
        }
        if items.is_empty() && remaining.is_zero() {
            break;
        }
    }
    Ok(fq_edge::wire::StreamBatch {
        items,
        next_from_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_filter_scopes_the_consumer_to_that_agent_subject() {
        let filter = DeadLetterFilter {
            agent: Some("researcher".into()),
            ..DeadLetterFilter::default()
        };
        assert_eq!(
            selection_subject(&filter, "dead_letter.list").unwrap(),
            "fq.agent.researcher.failed"
        );
        // No agent narrows to every agent's failed subject — one
        // wildcard covers them, unlike the Event atom's three roots.
        assert_eq!(
            selection_subject(&DeadLetterFilter::default(), "dead_letter.list").unwrap(),
            "fq.agent.*.failed"
        );
    }

    #[test]
    fn an_agent_id_that_is_not_a_subject_token_is_a_verdict_on_the_request() {
        let filter = DeadLetterFilter {
            agent: Some("not a token".into()),
            ..DeadLetterFilter::default()
        };
        let err = selection_subject(&filter, "dead_letter.list").expect_err("must refuse");
        assert!(
            matches!(&err, WireError::InvalidInput { op, message }
                if op == "dead_letter.list" && message.contains("not a token")),
            "expected an InvalidInput naming the agent; got {err:?}"
        );
    }
}
