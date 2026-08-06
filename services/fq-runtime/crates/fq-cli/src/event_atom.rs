//! The Event atom (plan Phase 4, cohort 4.2): the substrate itself on
//! the operator surface — Get by log sequence, List over the recent
//! window, and the sequence-resumable Stream that replaces `fq events
//! tail`'s silent-drop core-NATS subscription.
//!
//! Its own module rather than more of `operator_surface.rs`: that file
//! is the daemon's assembly point and is near its size budget (#189).

use fq_edge::wire::WireError;
use fq_runtime::event_tail::EventState;
use fq_runtime::events::Event;

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
    /// Cap on one List page — the most recent N matching events.
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

/// A filter compiled for one read: the subject the log consumer is
/// scoped to, plus the predicates applied per event.
///
/// The agent narrowing becomes a consumer filter because the log is
/// already partitioned that way; the rest cannot be, so they are
/// applied here. Compiling once per call keeps the per-event work to
/// comparisons.
#[derive(Debug)]
struct EventSelection {
    /// Empty means "every subject this stream captures" — the event
    /// log is `fq.agent.>` + `fq.system.>` + `fq.worker.>`, which no
    /// single pattern names.
    subject: String,
    event_type: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
}

impl EventSelection {
    fn compile(filter: &EventFilter, op: &str) -> Result<Self, WireError> {
        let invalid = |message: String| WireError::InvalidInput {
            op: op.to_string(),
            message,
        };
        let subject = match filter.agent.as_deref() {
            Some(agent) => {
                // An id that is not a subject token cannot name any
                // event, and interpolating it would build a malformed
                // filter — so it is a verdict on the request.
                fq_runtime::AgentId::new(agent)
                    .map_err(|e| invalid(format!("agent `{agent}`: {e}")))?;
                format!("fq.agent.{agent}.>")
            }
            None => String::new(),
        };
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
            subject,
            event_type: filter.event_type.clone(),
            since,
        })
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

/// Register the Event atom: the log read directly, with no projection
/// in the path. Get and List follow the Turn atom's shape; Stream is
/// the one this cohort exists for — an ephemeral consumer started at a
/// sequence, so a caller that reconnects from its cursor is handed
/// everything after it instead of whatever core NATS still had in
/// flight.
pub(crate) fn register_event_atom(
    registry: &mut fq_edge::EdgeRegistry,
    bus: fq_runtime::EventBus,
) -> anyhow::Result<()> {
    let decl = fq_ops::Atom::new::<EventKey, EventState, EventFilter>(
        fq_ops::Domain::Event,
        "One recorded event: the substrate every other resource folds from.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Event-log-backed: `seq` is the stream sequence — the same cursor \
         receipts, `min_seq` gates and `turn` sequences speak. List answers \
         with the most recent `limit` matching events in sequence order, \
         bounded by the tip observed at entry; Stream long-polls via \
         next_batch from a sequence, and `from_seq = u64::MAX` seeks the tail \
         without consuming anything. Resuming at a cursor loses nothing: the \
         log is durable and the consumer is positional.",
    );

    let get_bus = bus.clone();
    let list_bus = bus.clone();
    registry
        .atom::<EventKey, EventState, EventFilter, _, _, _, _, _, _>(
            decl,
            move |key: EventKey| {
                let bus = get_bus.clone();
                async move { event_at(&bus, key.seq).await }
            },
            move |filter: EventFilter| {
                let bus = list_bus.clone();
                async move {
                    let selection = EventSelection::compile(&filter, "event.list")?;
                    let limit = filter.limit.unwrap_or(EVENT_LIST_DEFAULT_LIMIT) as usize;
                    list_events(&bus, &selection, limit).await
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

/// The most recent `limit` matching events, in sequence order,
/// bounded by the tip observed at entry.
///
/// The scan is the whole log (the Turn atom's List has the same shape)
/// because the event log is the only place the payload lives: the
/// projection's index carries columns, not facts. A caller that wants
/// a cheap recent-window read narrows with `agent`, which the consumer
/// pushes down to the subject.
async fn list_events(
    bus: &fq_runtime::EventBus,
    selection: &EventSelection,
    limit: usize,
) -> Result<Vec<EventState>, WireError> {
    use futures::StreamExt;
    // The bound is the last sequence this filter matches, not the
    // stream's: a scan that waited for the stream tip would wait
    // forever whenever the tip is a message the filter excludes.
    let tip = bus
        .last_event_seq_matching(&selection.subject)
        .await
        .map_err(internal)?;
    if tip == 0 || limit == 0 {
        return Ok(Vec::new());
    }
    let mut events = bus
        .events_from(&selection.subject, 1)
        .await
        .map_err(internal)?;
    let mut window: std::collections::VecDeque<EventState> = std::collections::VecDeque::new();
    while let Some(next) = events.next().await {
        let (seq, event) = next.map_err(internal)?;
        if selection.matches(&event) {
            if window.len() == limit {
                window.pop_front();
            }
            window.push_back(EventState { seq, event });
        }
        if seq >= tip {
            break;
        }
    }
    Ok(window.into())
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
        .events_from(&selection.subject, from_seq)
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
        assert_eq!(selection.subject, "fq.agent.researcher.>");
        // No agent means no consumer filter at all: the log spans
        // `fq.agent.>`, `fq.system.>` and `fq.worker.>`, and no single
        // pattern covers those three.
        let all = EventSelection::compile(&EventFilter::default(), "event.stream").unwrap();
        assert_eq!(all.subject, "");
    }
}
