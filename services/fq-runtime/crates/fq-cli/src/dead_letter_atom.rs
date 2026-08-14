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
    /// Cap on one List page — the most recent N matching dead
    /// letters, and at most 500 of them (this property's `maximum`).
    /// Absent asks for the default 50.
    ///
    /// **A larger N is refused, never quietly shrunk.** So the count
    /// that comes back is always the one you asked for or the whole
    /// answer, and it reads unambiguously: fewer rows than you asked
    /// for means there are no more; exactly as many means there may
    /// be. For more than a page, narrow with `agent` — the only
    /// narrowing this filter offers — or read the same dead letters
    /// from `dead_letter.stream`, which is cursored.
    ///
    /// Ignored by Stream, which is cursored rather than paged.
    #[serde(default)]
    #[schemars(range(max = DEAD_LETTER_LIST_MAX_LIMIT))]
    pub(crate) limit: Option<u32>,
}

impl DeadLetterFilter {
    /// The page size this filter asks List for: the caller's own
    /// number, checked against the cap, or the default when they named
    /// none.
    ///
    /// **Over the cap is a refusal, not a shorter page.** Clamping is
    /// the tempting fix and the wrong one. List answers with a bare
    /// array of dead letters — no envelope, no cursor, nowhere to say
    /// "there is more" — so a page the daemon silently shortened is
    /// indistinguishable from a listing that ended, and an operator
    /// reads N rows with no way to tell "that is all of them" from
    /// "that is as many as you may have at once". Refusing keeps
    /// `limit` the caller's own bound, which is the whole reason the
    /// row count is readable at all.
    ///
    /// And the cap is not a dead end, which is the other half of the
    /// choice: more than a page is served by narrowing with `agent`,
    /// or by `dead_letter.stream`, which is cursored and resumes from
    /// the sequence List ends at — the composition this atom's List
    /// order exists to make possible (see [`list_dead_letters`]). A
    /// silent clamp would have been a dead end *and* a lie.
    fn list_limit(&self) -> Result<u32, WireError> {
        let Some(limit) = self.limit else {
            return Ok(DEAD_LETTER_LIST_DEFAULT_LIMIT);
        };
        if limit > DEAD_LETTER_LIST_MAX_LIMIT {
            return Err(WireError::InvalidInput {
                op: "dead_letter.list".into(),
                message: format!(
                    "limit {limit} is over the {DEAD_LETTER_LIST_MAX_LIMIT}-row cap on one \
                     List page — ask for {DEAD_LETTER_LIST_MAX_LIMIT} or fewer. The cap is \
                     not applied silently because a shortened page and a complete one are \
                     the same answer to look at. For more than a page, narrow with `agent` \
                     — the only narrowing this filter offers — or read \
                     `dead_letter.stream`, which is cursored and resumes from the sequence \
                     this listing ends at."
                ),
            });
        }
        Ok(limit)
    }
}

/// Cap on one stream batch.
const DEAD_LETTER_BATCH_CAP: usize = 64;
/// Default List page, matching `fq dead-letters list --limit`'s default.
const DEAD_LETTER_LIST_DEFAULT_LIMIT: u32 = 50;
/// The most dead letters one List page may carry, whatever a caller
/// asks for — refused rather than quietly applied (see
/// [`DeadLetterFilter::list_limit`]), and declared on the surface as
/// this filter's `limit` maximum so a consumer reads it off the schema
/// instead of discovering it by failing.
///
/// **The number is the edge's frame, worked backwards.** One List
/// answer is one frame, and both ends of the edge frame with
/// `LengthDelimitedCodec::new()`, whose default ceiling is 8 MiB
/// (8,388,608 bytes). A row's fixed part — one UUID, an RFC3339
/// timestamp, ten keys — is 235 bytes; the golden listing measures 319
/// and 323; and a production row measures 698: a github-watcher
/// task payload (`task`/`refs`/`constraints`/`done_criteria`/`github`,
/// 328 bytes of it) under the inline emitter's message. So 500 rows
/// leaves 8,388,608 / 500 = 16,777 bytes for each of them — twenty-four
/// times the production row, or ~16 KB of trigger payload on *every*
/// row — and a full page of production rows is 0.33 MiB, 4% of the
/// frame.
///
/// **It is smaller than the Event atom's 2,000 because the row is
/// bigger and the slack has two claimants, not one.** A dead letter
/// carries the trigger that died with it: `trigger_payload` is opaque
/// JSON the producer chose, which the wire contract says outright
/// (`docs/design/committed/trigger-wire-contract.md`: "an **opaque JSON
/// value**... Any valid JSON value is accepted"), and nothing in the
/// runtime truncates it — `fq dead-letters list` truncates for
/// *display* only, and `--json` prints it whole. `error_message` is
/// unbounded too: the inline emitter interpolates the `ExecutorError`
/// that lost the last delivery, which can be a provider's error body.
/// At 698 bytes the row is 2.4x an `EventView`'s 294, so matching that
/// atom's fourteen-fold headroom would land near 850; 500 is the round
/// number below it, and the difference is the second unbounded field's
/// margin.
///
/// The only ceiling either field already has is the broker's: a
/// publish above the server's advertised `max_payload` is refused
/// (`EventBus::publish`), which is 16 MB on the dogfood broker
/// (`ops/dogfood/infra/nats.conf`) — *above* the 8 MiB frame, so one
/// pathological row can outgrow a response all by itself. This cap
/// does not fix that, and does not pretend to; it bounds the ordinary
/// page.
///
/// What it replaces had no bound at all. Note that an oversized
/// `limit` never allocated a page that size up front:
/// [`list_dead_letters`] scans the subject forward and keeps a sliding
/// window of at most `limit`, so the memory was however many dead
/// letters the log actually held. The scan is the whole subject either
/// way. What an unbounded `limit` bought was a response that grew with
/// retained history until the codec refused it — around 12,000
/// production-sized rows — after the scan had already been paid for.
pub(crate) const DEAD_LETTER_LIST_MAX_LIMIT: u32 = 500;
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
         Visibility is bounded by event-stream retention (30 days by default). \
         A row carries the trigger's payload, which is what `dead_letter.requeue` \
         re-publishes — it reads this record and never the trigger stream, so a \
         dead letter is requeueable for as long as it is listable.",
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
                    let limit = filter.list_limit()? as usize;
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

/// Get one dead letter by log sequence.
///
/// Not the read an `AtomRef` resolves: a receipt names atoms by
/// identity, and this domain has none, so no command hands a caller a
/// DeadLetter reference.
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
///
/// `limit` has already been through [`DeadLetterFilter::list_limit`],
/// so it is at most [`DEAD_LETTER_LIST_MAX_LIMIT`] and the window
/// below — and with it the answer's frame — is bounded by a number the
/// caller was allowed to name. The scan is not: it walks the subject
/// from sequence 1 to the tip whatever the page size, evicting from
/// the front so only the last `limit` matches are held. That cost is
/// proportional to retained history rather than to `limit`, and the
/// cap neither adds to it nor takes it away.
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

    fn filter_with_limit(limit: Option<u32>) -> DeadLetterFilter {
        DeadLetterFilter {
            limit,
            ..DeadLetterFilter::default()
        }
    }

    /// **A page over the cap is refused, not silently shortened.**
    ///
    /// The shortening is the failure this exists to prevent: List
    /// answers with a bare array of dead letters, so a page the daemon
    /// cut down looks exactly like a listing that ended, and an
    /// operator would read a partial answer as the whole one — on the
    /// one listing whose whole purpose is "did anything fall on the
    /// floor". The assertion is therefore two things at once: that the
    /// over-cap ask errors, and that no shortened page comes back in
    /// its place.
    #[test]
    fn a_page_over_the_cap_is_refused_rather_than_shortened() {
        for asked in [
            DEAD_LETTER_LIST_MAX_LIMIT + 1,
            DEAD_LETTER_LIST_MAX_LIMIT * 10,
            // What a saturating `--limit` used to arrive as: every
            // dead letter the log still retains.
            u32::MAX,
        ] {
            let err = filter_with_limit(Some(asked))
                .list_limit()
                .expect_err("a page over the cap must be refused, not served short");
            // The refusal names the cap, so the caller's next request
            // is one edit away rather than a guess — and names the op,
            // because Stream ignores `limit` entirely.
            assert!(
                matches!(&err, WireError::InvalidInput { op, message }
                    if op == "dead_letter.list"
                        && message.contains(&asked.to_string())
                        && message.contains(&DEAD_LETTER_LIST_MAX_LIMIT.to_string())),
                "expected an InvalidInput naming {asked} and the cap; got {err:?}"
            );
            // …and it names the way out, because a cap a caller cannot
            // page past would be a dead end: this atom has no cursor on
            // List, so the answer to "more than a page" is the `agent`
            // narrowing or the stream — both of which exist, which is
            // the point of naming them rather than a route that does
            // not.
            let WireError::InvalidInput { message, .. } = &err else {
                unreachable!("checked above")
            };
            for route in ["agent", "dead_letter.stream"] {
                assert!(
                    message.contains(route),
                    "the refusal must point at `{route}`; got {message}"
                );
            }
        }
    }

    /// Under the cap, the page size is the caller's own number — which
    /// is what makes a row count readable.
    ///
    /// The daemon never substitutes a number of its own, so a listing
    /// shorter than the `limit` asked for is the complete answer, and
    /// one exactly as long as the `limit` may have more behind it. A
    /// clamp would have broken that inference for a bound the caller
    /// never chose, which is the whole objection to clamping.
    #[test]
    fn under_the_cap_the_page_is_the_callers_own_number() {
        for asked in [
            0,
            1,
            DEAD_LETTER_LIST_DEFAULT_LIMIT,
            DEAD_LETTER_LIST_MAX_LIMIT,
        ] {
            assert_eq!(
                filter_with_limit(Some(asked)).list_limit().ok(),
                Some(asked),
                "a page within the cap must be exactly the size asked for"
            );
        }
        // No `limit` is the documented default, not the cap: asking
        // for nothing in particular must not become asking for the
        // largest page the daemon will serve.
        assert_eq!(
            filter_with_limit(None).list_limit().ok(),
            Some(DEAD_LETTER_LIST_DEFAULT_LIMIT)
        );
    }

    /// **The cap is on the declared surface, not only in the code.**
    ///
    /// A consumer has to be able to read the bound off
    /// `operator_surface.json` rather than discover it by being
    /// refused — so it ships twice on the filter's `limit` property:
    /// as the schema's machine-readable `maximum`, and in the prose a
    /// human reads. Both are asserted against the constant the daemon
    /// actually enforces, so the declaration cannot drift away from
    /// the behaviour: that drift is precisely how a surface comes to
    /// say less than it means.
    #[test]
    fn the_surface_declares_the_cap_it_enforces() {
        let schema = serde_json::to_value(schemars::schema_for!(DeadLetterFilter))
            .expect("the filter schema serialises");
        let limit = &schema["properties"]["limit"];
        assert_eq!(
            limit["maximum"].as_u64(),
            Some(u64::from(DEAD_LETTER_LIST_MAX_LIMIT)),
            "the schema's maximum must be the cap the daemon enforces; got {limit}"
        );
        let described = limit["description"].as_str().expect("a described property");
        assert!(
            described.contains(&DEAD_LETTER_LIST_MAX_LIMIT.to_string()),
            "the declared description must name the cap; got {described:?}"
        );
        // The operator's own copy of the contract. The cap is not
        // re-declared as a clap range: a client-side range check would
        // be a second copy of the number in the place least able to
        // notice the daemon disagreeing — an older `fq` would refuse
        // pages a newer daemon serves, and quote its own stale cap
        // doing it. So `--limit` travels and the daemon rules on it,
        // and the number lives in the help text, pinned here rather
        // than left to drift.
        let help = <crate::cli::Cli as clap::CommandFactory>::command()
            .find_subcommand("dead-letters")
            .and_then(|dead| dead.clone().find_subcommand("list").cloned())
            .expect("`fq dead-letters list` exists")
            .get_arguments()
            .find(|arg| arg.get_id() == "limit")
            .and_then(|arg| arg.get_help().map(ToString::to_string))
            .expect("`--limit` is documented");
        assert!(
            help.contains(&DEAD_LETTER_LIST_MAX_LIMIT.to_string()),
            "`fq dead-letters list --limit`'s help must name the cap; got {help:?}"
        );
    }
}
