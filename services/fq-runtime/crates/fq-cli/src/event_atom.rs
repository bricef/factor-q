//! The Event atom (plan Phase 4, cohort 4.2): the substrate itself on
//! the operator surface — Get by log sequence, List over the recent
//! window, and the sequence-resumable Stream that replaces `fq events
//! tail`'s silent-drop core-NATS subscription.
//!
//! **Two stores, one atom.** Get and Stream answer from the event log,
//! because the payload only lives there. List answers from the
//! projection's index, because the question `fq events query` asks —
//! "what happened recently, narrowed and capped" — is what an index is
//! for, and answering it from the log makes the verb an operator
//! reaches for exactly when the log is largest into a scan of that
//! log. The index carries extracted fields, not payloads, so the rows
//! carry `seq`: any row walks to its whole event through `event.get`.
//! The general rule is recorded in
//! `docs/design/committed/operator-surface-domain-model.md`.
//!
//! Its own module rather than more of `operator_surface.rs`: that file
//! is the daemon's assembly point and is near its size budget (#189).

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::event_tail::EventState;
use fq_runtime::events::Event;
use fq_runtime::views::{EventView, Views};

/// Get identity for an Event: its event-log sequence.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct EventKey {
    pub(crate) seq: u64,
}

/// List/Stream selection for Events — the typed, schema'd filter.
///
/// Never a query language, and deliberately never a bus subject: a
/// subject pattern is a coordinate of the infrastructure the edge maps
/// (D8), so the selection travels in domain terms and the daemon
/// decides which subjects answer it. It carries exactly the narrowing
/// `fq events query` offers, which is the same narrowing a tail wants.
#[derive(Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct EventFilter {
    /// One agent's events. Absent reads the whole log.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// One event type, as the payload names itself (`llm_response`,
    /// `tool_call`, `system_startup`, …). An unrecognised value
    /// matches nothing rather than failing: event types are values,
    /// and a newer daemon has types this binary has never heard of.
    #[serde(default)]
    pub(crate) event_type: Option<String>,
    /// Only events at or after this RFC3339 instant.
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// Cap on one List page — the most recent N matching rows.
    /// Ignored by Stream, which is cursored rather than paged.
    #[serde(default)]
    pub(crate) limit: Option<u32>,
}

impl EventFilter {
    /// What this filter selects, in words — `fq events tail` says it
    /// back in its preamble so an operator can see at a glance that
    /// the narrowing they asked for is the one in force. Domain terms,
    /// like the filter itself: the verb used to echo the raw NATS
    /// subject it had subscribed to, which named a coordinate of the
    /// infrastructure rather than anything the operator selected.
    pub(crate) fn describe(&self) -> String {
        match (self.agent.as_deref(), self.event_type.as_deref()) {
            (None, None) => "all events".to_string(),
            (Some(agent), None) => format!("all events for agent {agent}"),
            (None, Some(event_type)) => format!("all {event_type} events"),
            (Some(agent), Some(event_type)) => format!("{event_type} events for agent {agent}"),
        }
    }
}

/// Cap on one stream batch.
const EVENT_BATCH_CAP: usize = 64;
/// Default List page, matching `fq events query --limit`'s default.
const EVENT_LIST_DEFAULT_LIMIT: u32 = 50;
/// Ceiling on a `next_batch` long poll, whatever the caller asks.
const EVENT_MAX_WAIT_CEILING_MS: u64 = 60_000;

/// A filter validated for one read — the narrowing, in domain terms,
/// with every value the request supplied already checked.
///
/// Compiled once per call, and deliberately store-agnostic: the same
/// value narrows the log (Stream) and the projection (List), so a
/// filter *means* one thing across the atom and the two verbs differ
/// only in where they go looking for it. Rendering the narrowing as
/// bus coordinates or SQL bindings happens at the edges of this type,
/// never inside a handler.
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
        // An id that is not a subject token cannot name any event, and
        // interpolating it would build a malformed consumer filter —
        // so it is a verdict on the request, on both verbs, whether or
        // not the store that answers would have minded.
        if let Some(agent) = filter.agent.as_deref() {
            fq_runtime::AgentId::new(agent)
                .map_err(|e| invalid(format!("agent `{agent}`: {e}")))?;
        }
        let since = filter
            .since
            .as_deref()
            .map(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .map_err(|e| invalid(format!("since `{s}` is not an RFC3339 instant: {e}")))
            })
            .transpose()?;
        Ok(EventSelection {
            agent: filter.agent.clone(),
            event_type: filter.event_type.clone(),
            since,
        })
    }

    /// The consumer subject the log read is scoped to. Empty means
    /// "every subject this stream captures" — the event log is
    /// `fq.agent.>` + `fq.system.>` + `fq.worker.>`, which no single
    /// pattern names. Only the agent narrowing pushes down; the log is
    /// partitioned that way and the rest is not, so the rest is
    /// [`Self::matches`].
    fn subject(&self) -> String {
        match self.agent.as_deref() {
            Some(agent) => format!("fq.agent.{agent}.>"),
            None => String::new(),
        }
    }

    /// `since` as the projection stores its timestamps. The column is
    /// text and the comparison lexical, so re-rendering the parsed
    /// instant — rather than passing the caller's spelling through —
    /// is what makes `…07.500Z` and `…07.500+00:00` the same instant
    /// to the query as they are to the reader.
    fn since_as_stored(&self) -> Option<String> {
        self.since.map(|t| t.to_rfc3339())
    }

    fn matches(&self, event: &Event) -> bool {
        self.event_type
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
    .description(
        "`seq` is the event-log stream sequence — the same cursor receipts, \
         `min_seq` gates and `turn` sequences speak. Get and Stream answer \
         from the log with the whole event, payload included. LIST DOES NOT \
         RETURN PAYLOADS: it answers from the projection's index, one row of \
         extracted fields (timestamp, agent, invocation, event type, model, \
         cost, error, duration) per event, most recent `limit` first. Every \
         row carries the `seq` that reads the whole event back through \
         `event.get`, so a listing is always a step away from the fact; \
         `seq` is null only for a row projected before the index recorded \
         log positions. Read payloads in bulk by streaming, not by listing. \
         List is a projection read and honours `min_seq`; Stream long-polls \
         via next_batch from a sequence, and `from_seq = u64::MAX` seeks the \
         tail without consuming anything. Resuming at a cursor loses \
         nothing: the log is durable and the consumer is positional.",
    );

    let get_bus = bus.clone();
    registry
        .atom::<EventKey, EventState, EventView, EventFilter, _, _, _, _, _, _>(
            decl,
            move |key: EventKey| {
                let bus = get_bus.clone();
                async move { event_at(&bus, key.seq).await }
            },
            move |filter: EventFilter| {
                let views = views.clone();
                async move {
                    let selection = EventSelection::compile(&filter, "event.list")?;
                    let limit = filter.limit.unwrap_or(EVENT_LIST_DEFAULT_LIMIT);
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

/// Get one event by log sequence — the read `AtomRef { domain: Event,
/// seq }` in a receipt resolves.
async fn event_at(bus: &fq_runtime::EventBus, seq: u64) -> Result<EventState, WireError> {
    use futures::StreamExt;
    let not_found = || WireError::NotFound {
        op: "event.get".into(),
        message: format!("no event at sequence {seq}"),
    };
    let mut events = bus.events_from("", seq).await.map_err(internal)?;
    let (got_seq, event) = tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
        .await
        .map_err(|_| not_found())?
        .ok_or_else(not_found)?
        .map_err(internal)?;
    // A sequence the stream skipped (retention, a subject this stream
    // never captured) hands back the next one along; that is a
    // different atom, so it is a miss.
    if got_seq != seq {
        return Err(not_found());
    }
    Ok(EventState {
        seq: got_seq,
        event,
    })
}

/// The most recent `limit` matching index rows, newest first.
///
/// Answered from the projection, which is the store that already holds
/// this question's answer: it is timestamp-ordered and indexed on
/// exactly the columns this filter narrows by, so a capped recent
/// window costs a `LIMIT` rather than a walk of the log. The rows are
/// the index's — extracted fields, no payload — and each carries the
/// `seq` that reads its event back whole. The alternative, scanning
/// the log the way `turn.list` does, put the cost of an operator's
/// most-reached-for read in direct proportion to how much history the
/// system had accumulated, which is the wrong way round.
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
    let mut events = bus
        .events_from(&selection.subject(), from_seq)
        .await
        .map_err(internal)?;
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

    #[test]
    fn a_filter_describes_itself_in_domain_terms() {
        let described = |agent: Option<&str>, event_type: Option<&str>| {
            EventFilter {
                agent: agent.map(str::to_string),
                event_type: event_type.map(str::to_string),
                ..EventFilter::default()
            }
            .describe()
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

    #[test]
    fn an_unparseable_since_is_a_verdict_on_the_request() {
        let filter = EventFilter {
            since: Some("yesterday".into()),
            ..EventFilter::default()
        };
        let err = EventSelection::compile(&filter, "event.list").expect_err("must refuse");
        assert!(
            matches!(&err, WireError::InvalidInput { op, message }
                if op == "event.list" && message.contains("RFC3339")),
            "expected an InvalidInput naming the format; got {err:?}"
        );
    }

    #[test]
    fn an_agent_filter_scopes_the_consumer_to_that_agent_subject() {
        let filter = EventFilter {
            agent: Some("researcher".into()),
            ..EventFilter::default()
        };
        let selection = EventSelection::compile(&filter, "event.stream").unwrap();
        assert_eq!(selection.subject(), "fq.agent.researcher.>");
        // No agent means no consumer filter at all: the log spans
        // `fq.agent.>`, `fq.system.>` and `fq.worker.>`, and no single
        // pattern covers those three.
        let all = EventSelection::compile(&EventFilter::default(), "event.stream").unwrap();
        assert_eq!(all.subject(), "");
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
