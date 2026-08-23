//! The Event atom (plan Phase 4, cohort 4.2): the substrate itself on
//! the operator surface — Get by the event's own identity, List over
//! the recent window, and the sequence-resumable Stream that replaces
//! `fq events tail`'s silent-drop core-NATS subscription.
//!
//! **Two stores, one atom.** Get and Stream answer from the event log,
//! because the payload only lives there. List answers from the
//! projection's index, because the question `fq events query` asks —
//! "what happened recently, narrowed and capped" — is what an index is
//! for, and answering it from the log makes the verb an operator
//! reaches for exactly when the log is largest into a scan of that
//! log. The index carries extracted fields, not payloads, so the rows
//! carry `event_id`: any row walks to its whole event through
//! `event.get`. The general rule is recorded in
//! `docs/design/committed/operator-surface-domain-model.md`.
//!
//! **The identity is the domain's, not the transport's.** Get was
//! keyed on the JetStream stream sequence, which is a *position*
//! doing an *identity's* job — and the two come apart in ordinary
//! operation: recreate `fq-events` and sequences restart at 1 while
//! `projection.db` survives, so a stored sequence silently addresses
//! a different event; and cost-bearing rows are exempt from the
//! retention sweep and kept indefinitely, while the log keeps 30
//! days, so their sequence resolves to nothing — or to a neighbour.
//! The rule the atom now follows: **cursors may be transport
//! coordinates; identities may not.** `min_seq` and `from_seq` are
//! cursors and are unchanged.
//!
//! **Two stores, one population.** Two stores is a fact about where
//! the answers come from; it must not become a fact about *what the
//! answers are*. It had: the projection never indexed transients while
//! the log served them, and `agent` narrowed List by the envelope's
//! `agent_id` and Stream by the consumer subject `fq.agent.<id>.>` —
//! so the same filter selected different events, and the declared
//! description claimed one row per event while saying nothing about
//! either gap. Both are closed at the source rather than documented:
//! the transient set lives in `fq_runtime::events::transient` and
//! every reader derives from it, and `agent` is the envelope's field
//! on both verbs. What remains different is the *window* — the
//! projection's retention against the log's 30 days, with cost-bearing
//! rows outliving both — which the atom's declared description states
//! rather than glosses.
//!
//! Its own module rather than more of `operator_surface.rs`: that file
//! is the daemon's assembly point and is near its size budget (#189).

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_ops::surface::EventKey;
use fq_runtime::event_tail::EventState;
use fq_runtime::events::Event;
use fq_runtime::surface::EventFilter;
use fq_runtime::views::{EventLocation, EventView, Views};

/// The page size a filter asks List for: the caller's own number,
/// checked against the cap, or the default when they named none.
///
/// **Over the cap is a refusal, not a shorter page.** Clamping is
/// the tempting fix and the wrong one. List answers with a bare
/// array of index rows — no envelope, no cursor, nowhere to say
/// "there is more" — so a page the daemon silently shortened is
/// indistinguishable from a listing that ended, and an operator
/// reads N rows with no way to tell "that is all of them" from
/// "that is as many as you may have at once". Refusing keeps
/// `limit` the caller's own bound, which is the whole reason the
/// row count is readable at all.
///
/// And the cap is not a dead end, which is the other half of the
/// choice: more than a page is served by narrowing, or by
/// `event.stream`, which is cursored, resumable, and selects the
/// same events for the same filter — the route the atom's
/// description already sends bulk reads down. A silent clamp would
/// have been a dead end *and* a lie.
///
/// The daemon's half of the contract, so it stays here while the
/// filter itself is shared: it speaks `WireError`, which the runtime
/// core does not know and should not learn.
fn list_limit(filter: &EventFilter) -> Result<u32, WireError> {
    let Some(limit) = filter.limit else {
        return Ok(EVENT_LIST_DEFAULT_LIMIT);
    };
    if limit > EVENT_LIST_MAX_LIMIT {
        return Err(WireError::InvalidInput {
            op: "event.list".into(),
            message: format!(
                "limit {limit} is over the {EVENT_LIST_MAX_LIMIT}-row cap on one List \
                 page — ask for {EVENT_LIST_MAX_LIMIT} or fewer. The cap is not applied \
                 silently because a shortened page and a complete one are the same \
                 answer to look at. For more than a page, narrow with `agent`, \
                 `event_type` or `since`, or read `event.stream`, which is cursored and \
                 selects the same events for the same filter."
            ),
        });
    }
    Ok(limit)
}

/// Cap on one stream batch.
const EVENT_BATCH_CAP: usize = 64;
/// Default List page, matching `fq events query --limit`'s default.
const EVENT_LIST_DEFAULT_LIMIT: u32 = 50;
/// The List page cap, re-exported at the name this module's handlers
/// and tests already use. Its reasoning lives with the definition,
/// which is on the declared filter it bounds.
pub(crate) use fq_runtime::surface::EVENT_LIST_MAX_LIMIT;
/// Ceiling on a `next_batch` long poll, whatever the caller asks.
const EVENT_MAX_WAIT_CEILING_MS: u64 = 60_000;

/// A filter validated for one read — the narrowing, in domain terms,
/// with every value the request supplied already checked.
///
/// Compiled once per call, and deliberately store-agnostic: the same
/// value narrows the log (Stream) and the projection (List), so a
/// filter *means* one thing across the atom and the two verbs differ
/// only in where they go looking for it. This type holds the
/// narrowing in the domain's own terms; what a store makes of it —
/// SQL bindings, or the predicate in [`Self::matches`] — happens at
/// its edges. Never bus coordinates: a subject is where the transport
/// put a message, and narrowing by one made `agent` mean something
/// different here than it meant on List.
#[derive(Debug)]
struct EventSelection {
    agent: Option<String>,
    event_type: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
}

impl EventSelection {
    fn compile(filter: &EventFilter, op: &str) -> Result<Self, WireError> {
        let invalid = |message: String| WireError::InvalidInput {
            op: op.to_string(),
            message,
        };
        // An id that is not a valid agent id cannot name any event —
        // no envelope was ever stamped with one — so it is a verdict
        // on the request, on both verbs, rather than an empty answer a
        // caller would read as "that agent has been quiet".
        if let Some(agent) = filter.agent.as_deref() {
            fq_runtime::AgentId::new(agent)
                .map_err(|e| invalid(format!("agent `{agent}`: {e}")))?;
        }
        // The grammar is `views::since`'s, not this atom's: `fq costs
        // --since` narrows the same projection by the same column, and
        // an operator who copies an argument from one verb to the other
        // must not discover that they disagree.
        let since = filter
            .since
            .as_deref()
            .map(|s| {
                fq_runtime::views::since::instant(s).map_err(|e| invalid(format!("since {e}")))
            })
            .transpose()?;
        Ok(EventSelection {
            agent: filter.agent.clone(),
            event_type: filter.event_type.clone(),
            since,
        })
    }

    /// `since` as the projection stores its timestamps. The column is
    /// text and the comparison lexical, so re-rendering the parsed
    /// instant — rather than passing the caller's spelling through —
    /// is what makes `…07.500Z` and `…07.500+00:00` the same instant
    /// to the query as they are to the reader.
    fn since_as_stored(&self) -> Option<String> {
        self.since.map(|t| t.to_rfc3339())
    }

    /// Whether one event is in this selection — the whole narrowing,
    /// applied to the event itself.
    ///
    /// The projection answers the same question in SQL over the
    /// columns it extracted from these same fields, which is what
    /// makes List and Stream one selection rather than two that
    /// resemble each other.
    ///
    /// **`agent` is the envelope's `agent_id`.** It used to be the
    /// consumer subject `fq.agent.<id>.>`, and the two disagree for
    /// every event that is not agent-partitioned: an
    /// `invocation_archive_acked` is published on `fq.worker.<worker>`
    /// under its *agent's* envelope, and `system_startup` on
    /// `fq.system.*` under agent `system` — so rows that listed under
    /// `--agent system` could never be tailed with the same filter. A
    /// filter means one thing, and that thing is the domain's: a
    /// subject is where the transport put the message.
    fn matches(&self, event: &Event) -> bool {
        // Transients are not on this surface at all — the projection
        // does not index them, so serving them here is the population
        // gap that made the same filter mean two things.
        !event.payload.is_transient()
            && self
                .agent
                .as_deref()
                .is_none_or(|want| event.envelope.agent_id.as_str() == want)
            && self
                .event_type
                .as_deref()
                .is_none_or(|want| event.payload.event_type() == want)
            && self
                .since
                .is_none_or(|since| event.envelope.timestamp >= since)
    }
}

/// Register the Event atom. Get and Stream read the log directly, with
/// no projection in the path — the payload lives nowhere else. List
/// reads the projection's index, and declares that it does: the atom
/// carries a distinct `index_schema`, and the contract text below is
/// the surface saying so about itself rather than a reader having to
/// find this comment.
pub(crate) fn register_event_atom(
    registry: &mut fq_edge::EdgeRegistry,
    bus: fq_runtime::EventBus,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    let decl = fq_ops::Atom::with_index::<EventKey, EventState, EventView, EventFilter>(
        fq_ops::Domain::Event,
        "One recorded event: the substrate every other resource folds from.",
        fq_ops::Stability::Experimental,
    )
    .description(concat!(
        "`event_id` is an Event's identity — the UUIDv7 the event stamps on \
         itself when it is constructed, and the projection index's primary \
         key. It is NOT the log sequence: a sequence is a transport \
         coordinate, and recreating the event stream restarts it at 1 while \
         the index survives. Get resolves an identity in two O(1) hops (the \
         index for the log position, then the log) and VERIFIES that the \
         event it read carries the identity asked for, so a rewound or \
         recreated log fails loudly instead of answering with a neighbour. \
         Get and Stream answer from the log with the whole event, payload \
         included. LIST DOES NOT RETURN PAYLOADS: it answers from the \
         projection's index, one row of extracted fields (identity, \
         timestamp, agent, invocation, event type, model, cost, error, \
         duration) per event, most recent `limit` first. Every row carries \
         the identity that reads the whole event back through `event.get` \
         WHEN THE PAYLOAD IS STILL RETAINED, so a listing is a step away \
         from the fact — with the two unavailable cases named rather than \
         collapsed into `not found`: `Unlocatable` (the row is indexed but \
         its log position was never recorded, so we know the event and not \
         where its payload is) and `Gone` (the position is known and the \
         log has aged past it, or was replaced). Read payloads in bulk by \
         streaming, not by listing. ",
        "LIST AND STREAM SELECT THE SAME EVENTS FOR THE SAME FILTER. \
         `agent` means the envelope's `agent_id` on both — the domain's \
         answer to whose event this is, never the bus subject the message \
         was routed on, which disagrees for every event that is not \
         agent-partitioned (an `invocation_archive_acked` rides \
         `fq.worker.*` under its agent's envelope; `system_startup` rides \
         `fq.system.*` under agent `system`). And NEITHER VERB SERVES A \
         TRANSIENT EVENT TYPE (",
        fq_runtime::transient_event_types!(),
        "): a heartbeat is operational signal rather than part of this \
         interface, and the fact it carries is on `worker.list` as \
         `last_heartbeat_ms` — tap the transport directly to watch the raw \
         ones. WHAT THE TWO STILL DIFFER ON IS THE WINDOW, not which \
         events are in it. List sees the projection, which sweeps non-cost \
         rows older than `state.retention_days` (30 by default) and keeps \
         COST-BEARING ROWS INDEFINITELY; Stream sees the log, which keeps \
         30 days of everything and exempts nothing. So old spend lists \
         after it can no longer be streamed or Got — `Gone` is the normal \
         answer there, not a fault — and the two are two windows over one \
         substrate rather than one set. ",
        "List is a projection read and honours `min_seq`; Stream \
         long-polls via next_batch from a sequence, and `from_seq = \
         u64::MAX` seeks the tail without consuming anything. Resuming at \
         a cursor loses nothing: the log is durable and the consumer is \
         positional — a cursor may be a transport coordinate, an identity \
         may not.",
    ));

    let get_bus = bus.clone();
    let get_views = views.clone();
    registry
        .atom::<EventKey, EventState, EventView, EventFilter, _, _, _, _, _, _>(
            decl,
            move |key: EventKey| {
                let bus = get_bus.clone();
                let views = get_views.clone();
                async move { event_by_id(&bus, &views, &key.event_id).await }
            },
            move |filter: EventFilter| {
                let views = views.clone();
                async move {
                    let selection = EventSelection::compile(&filter, "event.list")?;
                    let limit = list_limit(&filter)?;
                    list_events(&views, &selection, limit).await
                }
            },
            move |filter: EventFilter, from_seq, max_wait_ms| {
                let bus = bus.clone();
                async move {
                    let selection = EventSelection::compile(&filter, "event.stream")?;
                    stream_events(&bus, &selection, from_seq, max_wait_ms).await
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

/// Get one event by its identity — two O(1) hops and a self-check.
///
/// The identity resolves in the projection (a primary-key lookup for
/// the log position that row records), and the position reads the
/// payload out of the log. Neither hop scans, which is what made the
/// sequence tempting as a key in the first place: an id lookup *in
/// the log* would have walked it.
///
/// **The event that comes back is checked against the one asked
/// for.** The position is a transport coordinate held across a
/// boundary the transport makes no promise about — recreate the
/// stream and sequence 42 is a different event, or none — so an
/// unverified second hop answers confidently with a neighbour. This
/// check is the whole reason the identity moved: without it, keying
/// on `event_id` would only have relocated the lie.
async fn event_by_id(
    bus: &fq_runtime::EventBus,
    views: &Views,
    event_id: &str,
) -> Result<EventState, WireError> {
    // A string that is not a UUID cannot name any event: every
    // `event_id` in the index was written from one. So it is a
    // verdict on the request, as an unparseable `since` is — and
    // re-rendering the parsed value normalises the spelling the
    // lookup binds.
    let asked = uuid::Uuid::parse_str(event_id).map_err(|e| WireError::InvalidInput {
        op: "event.get".into(),
        message: format!("event_id `{event_id}`: {e}"),
    })?;
    let location = views
        .event_location(&asked.to_string())
        .await
        .map_err(|e| WireError::Internal {
            message: e.to_string(),
        })?;
    let seq = match location {
        EventLocation::Unindexed => {
            return Err(WireError::NotFound {
                op: "event.get".into(),
                message: format!("no event `{asked}`"),
            });
        }
        // Indexed, but we do not know where its payload is. Not a
        // miss: the event is a fact this daemon has seen.
        EventLocation::Unlocated => {
            return Err(WireError::Unlocatable {
                op: "event.get".into(),
                message: format!(
                    "event `{asked}` is indexed but its log position was never recorded — \
                     the event is known, where its payload sits is not"
                ),
            });
        }
        EventLocation::At(seq) => seq,
    };
    let gone = |detail: String| WireError::Gone {
        op: "event.get".into(),
        message: format!("event `{asked}` is indexed at log position {seq} but {detail}"),
    };
    let state = event_at(bus, seq)
        .await?
        .ok_or_else(|| gone("the log no longer holds that position".into()))?;
    if state.event.envelope.event_id != asked {
        return Err(gone(format!(
            "that position holds event `{}` — the log was rewound or recreated under \
             an index that outlived it, so the position no longer means what it did",
            state.event.envelope.event_id
        )));
    }
    Ok(state)
}

/// The event at one log position, or `None` when the log does not
/// hold that position.
async fn event_at(bus: &fq_runtime::EventBus, seq: u64) -> Result<Option<EventState>, WireError> {
    use futures::StreamExt;
    // Ask the server where the log ends before opening a consumer: a
    // position past the tip is now an ordinary outcome (a recreated
    // stream restarts at 1 under an index that remembers the old
    // numbers), and discovering it by waiting out a five-second read
    // would make the expected case the slow one.
    if seq == 0 || seq > bus.last_event_seq().await.map_err(internal)? {
        return Ok(None);
    }
    let mut events = bus.events_from("", seq).await.map_err(internal)?;
    let Ok(Some(next)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), events.next()).await
    else {
        return Ok(None);
    };
    let (got_seq, event) = next.map_err(internal)?;
    // A position the stream skipped (retention, a subject this stream
    // never captured) hands back the next one along — so the position
    // asked for holds nothing, whatever the read returned.
    if got_seq != seq {
        return Ok(None);
    }
    Ok(Some(EventState {
        seq: got_seq,
        event,
    }))
}

/// The most recent `limit` matching index rows, newest first.
///
/// Answered from the projection, which is the store that already holds
/// this question's answer: it is timestamp-ordered and indexed on
/// exactly the columns this filter narrows by, so a capped recent
/// window costs a `LIMIT` rather than a walk of the log. The rows are
/// the index's — extracted fields, no payload — and each carries the
/// `event_id` that reads its event back whole. The alternative, scanning
/// the log the way `turn.list` does, put the cost of an operator's
/// most-reached-for read in direct proportion to how much history the
/// system had accumulated, which is the wrong way round.
///
/// `limit` has already been through [`list_limit`], so it is at most
/// `EVENT_LIST_MAX_LIMIT` and the `Vec` this allocates is
/// bounded before the query runs rather than after the rows are in
/// hand.
async fn list_events(
    views: &Views,
    selection: &EventSelection,
    limit: u32,
) -> Result<Vec<EventView>, WireError> {
    let since = selection.since_as_stored();
    views
        .events(
            selection.agent.as_deref(),
            selection.event_type.as_deref(),
            since.as_deref(),
            i64::from(limit),
        )
        .await
        .map_err(|e| WireError::Internal {
            message: e.to_string(),
        })
}

/// One long-poll batch of events at or after `from_seq`; `u64::MAX`
/// seeks the tail. The cursor advances past non-matching events too,
/// so an idle poll still makes progress.
async fn stream_events(
    bus: &fq_runtime::EventBus,
    selection: &EventSelection,
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
        + std::time::Duration::from_millis(max_wait_ms.min(EVENT_MAX_WAIT_CEILING_MS));
    // The whole log, narrowed in [`EventSelection::matches`] rather
    // than by a consumer subject. A subject pattern is only ever a
    // *pre*-filter, and no sound one exists here: the narrowing a
    // caller asks for is `agent`, and an agent's events are not all on
    // `fq.agent.<id>.>` (an archive ack rides `fq.worker.*` under the
    // agent's envelope, and agent `system` has no `fq.agent.*` subject
    // at all). Pushing it down cost the daemon a scan it now does
    // itself, and bought a filter that meant something different here
    // than it did on List.
    let mut events = bus.events_from("", from_seq).await.map_err(internal)?;
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
        if selection.matches(&event) {
            let item = serde_json::to_value(EventState { seq, event }).map_err(|e| {
                WireError::Internal {
                    message: e.to_string(),
                }
            })?;
            items.push(fq_edge::wire::StreamItem { seq, item });
            if items.len() >= EVENT_BATCH_CAP {
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

    fn filter_with_limit(limit: Option<u32>) -> EventFilter {
        EventFilter {
            limit,
            ..EventFilter::default()
        }
    }

    /// **A page over the cap is refused, not silently shortened.**
    ///
    /// The shortening is the failure this exists to prevent: List
    /// answers with a bare array of index rows, so a page the daemon
    /// cut down looks exactly like a listing that ended, and an
    /// operator would read a partial answer as the whole one. The
    /// assertion is therefore two things at once — that the over-cap
    /// ask errors, and that no shortened page comes back in its place.
    #[test]
    fn a_page_over_the_cap_is_refused_rather_than_shortened() {
        for asked in [
            EVENT_LIST_MAX_LIMIT + 1,
            EVENT_LIST_MAX_LIMIT * 10,
            // What `--limit -1` used to arrive as: the whole table.
            u32::MAX,
        ] {
            let err = list_limit(&filter_with_limit(Some(asked)))
                .expect_err("a page over the cap must be refused, not served short");
            // The refusal names the cap, so the caller's next request
            // is one edit away rather than a guess — and names the op,
            // because Stream ignores `limit` entirely.
            assert!(
                matches!(&err, WireError::InvalidInput { op, message }
                    if op == "event.list"
                        && message.contains(&asked.to_string())
                        && message.contains(&EVENT_LIST_MAX_LIMIT.to_string())),
                "expected an InvalidInput naming {asked} and the cap; got {err:?}"
            );
            // …and it names the way out, because a cap a caller cannot
            // page past would be a dead end: this atom has no cursor on
            // List, so the answer to "more than a page" is narrowing or
            // the stream.
            let WireError::InvalidInput { message, .. } = &err else {
                unreachable!("checked above")
            };
            assert!(
                message.contains("event.stream"),
                "the refusal must point at the cursored read; got {message}"
            );
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
        for asked in [0, 1, EVENT_LIST_DEFAULT_LIMIT, EVENT_LIST_MAX_LIMIT] {
            assert_eq!(
                list_limit(&filter_with_limit(Some(asked))).ok(),
                Some(asked),
                "a page within the cap must be exactly the size asked for"
            );
        }
        // No `limit` is the documented default, not the cap: asking
        // for nothing in particular must not become asking for the
        // largest page the daemon will serve.
        assert_eq!(
            list_limit(&filter_with_limit(None)).ok(),
            Some(EVENT_LIST_DEFAULT_LIMIT)
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
        let schema = serde_json::to_value(schemars::schema_for!(EventFilter))
            .expect("the filter schema serialises");
        let limit = &schema["properties"]["limit"];
        assert_eq!(
            limit["maximum"].as_u64(),
            Some(u64::from(EVENT_LIST_MAX_LIMIT)),
            "the schema's maximum must be the cap the daemon enforces; got {limit}"
        );
        let described = limit["description"].as_str().expect("a described property");
        assert!(
            described.contains(&EVENT_LIST_MAX_LIMIT.to_string()),
            "the declared description must name the cap; got {described:?}"
        );
        // The operator's own copy of the contract is the client's, and
        // is pinned against this same constant in `fq-cli`'s
        // `cli::tests`. `fq events query --limit` takes an i64 and
        // cannot be range-checked by clap against a u32 cap, so the
        // number lives in its help text and is asserted where that
        // text is declared.
    }

    /// Compile just the `since` narrowing, which is what both stores
    /// disagree about when this goes wrong.
    fn compiled_since(spelling: &str) -> EventSelection {
        EventSelection::compile(
            &EventFilter {
                since: Some(spelling.into()),
                ..EventFilter::default()
            },
            "event.list",
        )
        .unwrap_or_else(|e| panic!("`{spelling}` must name an instant; got {e:?}"))
    }

    /// The grammar an operator already types. Before `event.list`
    /// crossed the edge, `since` was handed to a lexical `timestamp >=
    /// ?` against a column of RFC3339 text, so a *prefix* of a stored
    /// timestamp was a working lower bound — `--since 2026-04-25` (the
    /// spelling QUICKSTART prints, one page away from `fq costs
    /// --since 2026-04-25`) and `--since 2026-04-25T10:00:00` both
    /// selected what an operator meant by them. Parsing the argument
    /// must not narrow that: a bare date still names the day's first
    /// moment, or a query for "the 25th onwards" silently drops that
    /// morning, and an offset-less time is still read as UTC.
    #[test]
    fn an_operators_date_is_still_a_lower_bound_on_the_whole_day() {
        assert_eq!(
            compiled_since("2026-04-25").since_as_stored().as_deref(),
            Some("2026-04-25T00:00:00+00:00")
        );
        assert_eq!(
            compiled_since("2026-04-25T10:00:00")
                .since_as_stored()
                .as_deref(),
            Some("2026-04-25T10:00:00+00:00")
        );
        // The log and the projection must be narrowed to one instant,
        // not to a parsed instant and a re-parsed string: `matches`
        // compares against this, `since_as_stored` renders it.
        assert_eq!(
            compiled_since("2026-04-25").since,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-04-25T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            )
        );
    }

    #[test]
    fn an_unparseable_since_is_a_verdict_on_the_request() {
        let filter = EventFilter {
            since: Some("yesterday".into()),
            ..EventFilter::default()
        };
        let err = EventSelection::compile(&filter, "event.list").expect_err("must refuse");
        // The refusal quotes what was written and names what would
        // have worked — an operator is one edit away either way.
        assert!(
            matches!(&err, WireError::InvalidInput { op, message }
                if op == "event.list"
                    && message.contains("yesterday")
                    && message.contains("RFC3339")
                    && message.contains("2026-04-25")),
            "expected an InvalidInput naming the accepted forms; got {err:?}"
        );
    }

    /// An event stamped for `agent`, published on whatever subject
    /// its payload routes to.
    fn event_for(agent: &str, payload: fq_runtime::events::EventPayload) -> Event {
        Event::new(
            fq_runtime::AgentId::new(agent).unwrap(),
            uuid::Uuid::now_v7(),
            payload,
        )
    }

    fn heartbeat_payload() -> fq_runtime::events::EventPayload {
        fq_runtime::events::EventPayload::WorkerHeartbeat(
            fq_runtime::events::WorkerHeartbeatPayload {
                worker_id: fq_runtime::worker::WorkerId::new("w-1".to_string()).unwrap(),
            },
        )
    }

    fn archive_acked_payload() -> fq_runtime::events::EventPayload {
        fq_runtime::events::EventPayload::InvocationArchiveAcked(
            fq_runtime::events::InvocationArchiveAckedPayload {
                worker_id: fq_runtime::worker::WorkerId::new("w-1".to_string()).unwrap(),
            },
        )
    }

    fn selection_for(filter: EventFilter) -> EventSelection {
        EventSelection::compile(&filter, "event.stream").expect("a valid filter")
    }

    /// **The narrowing is the envelope's, not the transport's.**
    ///
    /// `agent` used to compile to the consumer subject
    /// `fq.agent.<id>.>`, which is a different question from the one
    /// `event.list` answers against the index's `agent_id` column —
    /// and the two disagree for every event that is not
    /// agent-partitioned. This is that case, sharpened: an archive ack
    /// is stamped with its agent's envelope and published on
    /// `fq.worker.<worker>.…`, so the subject-scoped stream could
    /// never deliver a row the list had just handed back.
    #[test]
    fn an_agent_filter_selects_by_the_envelope_not_the_subject() {
        let mine = selection_for(EventFilter {
            agent: Some("researcher".into()),
            ..EventFilter::default()
        });
        let acked = event_for("researcher", archive_acked_payload());
        assert!(
            !acked.subject().starts_with("fq.agent."),
            "this fixture is only interesting because the subject is not the agent's: {}",
            acked.subject()
        );
        assert!(
            mine.matches(&acked),
            "an agent's event is its agent's wherever the transport routed it"
        );
        assert!(!mine.matches(&event_for("fixer", archive_acked_payload())));

        // The case that could not be tailed at all: `system` has no
        // `fq.agent.*` subject, so the old narrowing selected nothing.
        let system = selection_for(EventFilter {
            agent: Some("system".into()),
            ..EventFilter::default()
        });
        let startup = Event::system(
            uuid::Uuid::now_v7(),
            fq_runtime::events::EventPayload::SystemStartup(
                fq_runtime::events::SystemStartupPayload {
                    runtime_id: uuid::Uuid::now_v7(),
                    version: "0".into(),
                    nats_url: String::new(),
                    agents_loaded: 0,
                    pricing_entries: 0,
                },
            ),
        );
        assert!(startup.subject().starts_with("fq.system."));
        assert!(system.matches(&startup));
        assert!(!mine.matches(&startup));

        // No agent selects every agent, which is what it always meant.
        let all = selection_for(EventFilter::default());
        assert!(all.matches(&acked) && all.matches(&startup));
    }

    /// A transient is not on this surface, so the stream does not
    /// serve it — under any filter, including one that names it. The
    /// projection never indexed it either, and that agreement is the
    /// point: `event.list` and `event.stream` answer the same
    /// population because they consult the same predicate, not
    /// because two comments agree.
    #[test]
    fn a_transient_matches_no_filter_at_all() {
        let heartbeat = Event::system(uuid::Uuid::now_v7(), heartbeat_payload());
        for filter in [
            EventFilter::default(),
            EventFilter {
                agent: Some("system".into()),
                ..EventFilter::default()
            },
            EventFilter {
                event_type: Some(heartbeat.payload.event_type().to_string()),
                ..EventFilter::default()
            },
        ] {
            // The filter as it travels, which is what the daemon
            // rules on. `fq events tail`'s prose rendering of the
            // same value is the client's and is asserted there.
            let asked = serde_json::to_string(&filter).expect("a filter serialises");
            assert!(
                !selection_for(filter).matches(&heartbeat),
                "a heartbeat must not be streamed, and `{asked}` did"
            );
        }
        // …while the durable event on the same worker subject still is.
        assert!(
            selection_for(EventFilter::default())
                .matches(&event_for("researcher", archive_acked_payload()))
        );
    }

    /// The two stores must not disagree about which instant a caller
    /// asked for. The log compares parsed instants; the projection
    /// compares stored text — so `since` is re-rendered the way the
    /// projection writes its timestamps, and a `Z` spelling and a
    /// `+00:00` spelling become the same query rather than two.
    #[test]
    fn since_is_normalised_to_the_way_the_projection_stores_it() {
        let compiled = |s: &str| {
            EventSelection::compile(
                &EventFilter {
                    since: Some(s.into()),
                    ..EventFilter::default()
                },
                "event.list",
            )
            .expect("a valid RFC3339 instant")
            .since_as_stored()
        };
        let stored = chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:07.500Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            .to_rfc3339();
        assert_eq!(
            compiled("2026-01-02T03:04:07.500Z").as_deref(),
            Some(&*stored)
        );
        assert_eq!(
            compiled("2026-01-02T03:04:07.500+00:00").as_deref(),
            Some(&*stored)
        );
        // Same instant, other side of the world: the offset is a
        // spelling, and the query must not be sensitive to it.
        assert_eq!(
            compiled("2026-01-02T08:34:07.500+05:30").as_deref(),
            Some(&*stored)
        );
        assert_eq!(
            EventSelection::compile(&EventFilter::default(), "event.list")
                .unwrap()
                .since_as_stored(),
            None
        );
    }
}
