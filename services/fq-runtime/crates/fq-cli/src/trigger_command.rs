//! The Trigger domain, daemon-side (plan Phase 4, verb 6): the **atom**
//! — Get, List and Stream over triggers the runtime has recorded — and
//! the **command** that appends to it, `trigger.publish`.
//!
//! Both halves in one module because the receipt binds them: an
//! `AtomRef.key` must be exactly the key that domain's Get takes, and
//! keeping the two declarations a screen apart is what stops the
//! command's key and the atom's `key_schema` from drifting into two
//! shapes that only a live call would catch. (The Event and DeadLetter
//! atoms each have their own file because neither domain has a command
//! at all.)
//!
//! The client used to connect to the broker and publish
//! `fq.trigger.<agent>` itself. That made every operator a NATS
//! publisher — credentials, subject vocabulary and stream layout all in
//! the thin client — for a fact the daemon already owns. Now the daemon
//! publishes and the client asks it to.
//!
//! # A trigger is kept, so it can be read
//!
//! The atom answers from the projection's `triggers` table, which is a
//! trigger's **permanent** home: the retention sweep only ever deletes
//! from `events`, so a trigger outlives both the trigger stream's 24
//! hours and the event log's 30 days. That is the whole reason the
//! three verbs can promise anything. Reading the atom off the trigger
//! stream would have made a Trigger readable for a day and unreadable
//! for the next twenty-nine; reading it off the event log would have
//! given it thirty days and then `Gone`.
//!
//! The row carries the payload, so **Get needs no second hop** and this
//! atom has no `Unlocatable` and no `Gone` — the two states the Event
//! atom has to name because its index and its payloads live in
//! different stores with different retentions. What remains is one
//! state of "not here", named below.

use fq_edge::wire::WireError;
use fq_runtime::trigger::{Trigger, TriggerView};
use fq_runtime::views::Views;
use std::sync::Arc;

/// Get identity for a Trigger: the `trigger_id` the runtime minted (or
/// honoured) when it took responsibility for the trigger — a UUIDv7 in
/// canonical hyphenated text, and the `triggers` table's primary key.
///
/// This is the shape `trigger.publish`'s receipt hands back, verbatim.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct TriggerKey {
    pub(crate) trigger_id: String,
}

/// List/Stream selection for Triggers — the typed, schema'd filter,
/// never a query language.
///
/// **Two axes, and they are the two the record actually has.** A
/// trigger's permanent row carries its identity, its agent, when it was
/// recorded, its source, its subject and its payload. Of those, agent
/// and time are what an operator narrows by and what the table is
/// indexed on (`idx_triggers_agent_time`, `idx_triggers_time`). Source
/// is a closed three-valued set that nobody has asked to filter by, and
/// payload is opaque JSON the runtime must not interpret — narrowing by
/// it would be the query language this is deliberately not. A filter is
/// a promise the surface has to keep, so it grows when a caller needs
/// it (P11), not when a column looks plausible.
#[derive(Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct TriggerFilter {
    /// One agent's triggers. Absent reads every agent's.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// Only triggers recorded at or after this RFC3339 instant.
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// Cap on one List page — the most recently recorded N matching
    /// triggers, and at most 500 of them (this property's `maximum`).
    /// Absent asks for the default 50.
    ///
    /// **A larger N is refused, never quietly shrunk.** So the count
    /// that comes back is always the one you asked for or the whole
    /// answer: fewer rows than you asked for means there are no more;
    /// exactly as many means there may be. For more than a page,
    /// narrow (`agent`, `since`) or read `trigger.stream`, which is
    /// cursored and selects the same triggers for the same filter.
    ///
    /// Ignored by Stream, which is cursored rather than paged.
    #[serde(default)]
    #[schemars(range(max = TRIGGER_LIST_MAX_LIMIT))]
    pub(crate) limit: Option<u32>,
}

/// Cap on one stream batch.
const TRIGGER_BATCH_CAP: i64 = 64;
/// Default List page, matching the other atoms' listings.
const TRIGGER_LIST_DEFAULT_LIMIT: u32 = 50;
/// The most triggers one List page may carry, whatever a caller asks
/// for — refused rather than quietly applied, and declared on the
/// surface as this filter's `limit` maximum so a consumer reads it off
/// the schema instead of discovering it by failing.
///
/// **The number is the edge's frame, worked backwards, and this atom's
/// rows are small because they are index rows.** One List answer is one
/// frame, and both ends of the edge frame with
/// `LengthDelimitedCodec::new()`, whose default ceiling is 8 MiB. A
/// [`TriggerView`] is two UUIDs, an RFC3339 instant and a source word
/// under four keys — about 180 bytes, every field bounded, none of them
/// free text. 500 rows is therefore ~0.09 MiB, roughly 1% of the frame.
///
/// It could safely be far larger; it is 500 to match `dead_letter.list`
/// rather than to fit. The binding constraint on a trigger listing is
/// not bytes but the reader: a page is something an operator looks at,
/// and past a few hundred rows the answer to "what came in" is a
/// narrower `since`, not a longer page. Bytes are what makes 500 *safe*
/// — the Event atom's 2,000 exists because its rows carry an unbounded
/// `error_message`, and nothing here is unbounded at all. **The payload
/// is not on these rows**, which is the point of the index/state split:
/// with it, one page could be 500 MiB of accepted payloads and the cap
/// would have had to be 8 instead.
pub(crate) const TRIGGER_LIST_MAX_LIMIT: u32 = 500;
/// Ceiling on a `next_batch` long poll, whatever the caller asks.
const TRIGGER_MAX_WAIT_CEILING_MS: u64 = 60_000;
/// How often a long poll re-asks the store. The projection is a local
/// SQLite file rather than a subscription, so a stream waits by asking
/// again; this is the granularity of "the moment a trigger is
/// recorded".
const TRIGGER_POLL_INTERVAL_MS: u64 = 200;

impl TriggerFilter {
    /// The page size this filter asks List for: the caller's own
    /// number, checked against the cap, or the default when they named
    /// none.
    ///
    /// **Over the cap is a refusal, not a shorter page.** List answers
    /// with a bare array — no envelope, no cursor, nowhere to say
    /// "there is more" — so a page the daemon silently shortened is
    /// indistinguishable from a listing that ended, and an operator
    /// reads N rows with no way to tell "that is all of them" from
    /// "that is as many as you may have at once". Refusing keeps
    /// `limit` the caller's own bound, which is the whole reason the
    /// row count is readable at all.
    fn list_limit(&self) -> Result<u32, WireError> {
        let Some(limit) = self.limit else {
            return Ok(TRIGGER_LIST_DEFAULT_LIMIT);
        };
        if limit > TRIGGER_LIST_MAX_LIMIT {
            return Err(WireError::InvalidInput {
                op: "trigger.list".into(),
                message: format!(
                    "limit {limit} is over the {TRIGGER_LIST_MAX_LIMIT}-row cap on one List \
                     page — ask for {TRIGGER_LIST_MAX_LIMIT} or fewer. The cap is not applied \
                     silently because a shortened page and a complete one are the same answer \
                     to look at. For more than a page, narrow with `agent` or `since`, or read \
                     `trigger.stream`, which is cursored and selects the same triggers for the \
                     same filter."
                ),
            });
        }
        Ok(limit)
    }
}

/// A filter validated for one read — the narrowing with every value the
/// request supplied already checked.
///
/// Compiled once per call and shared by List and Stream, so a filter
/// *means* one thing across the atom. Both hand it to the same store,
/// which is what makes "the same triggers for the same filter" a
/// property of the code rather than of two comments agreeing.
#[derive(Debug)]
struct TriggerSelection {
    agent: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
}

impl TriggerSelection {
    fn compile(filter: &TriggerFilter, op: &str) -> Result<Self, WireError> {
        let invalid = |message: String| WireError::InvalidInput {
            op: op.to_string(),
            message,
        };
        // An id that is not a valid agent id cannot name any trigger —
        // no trigger subject was ever built from one — so it is a
        // verdict on the request rather than an empty answer a caller
        // would read as "that agent has had nothing to do".
        if let Some(agent) = filter.agent.as_deref() {
            fq_runtime::AgentId::new(agent)
                .map_err(|e| invalid(format!("agent `{agent}`: {e}")))?;
        }
        // The grammar is `views::since`'s, shared with `fq costs
        // --since` and `event.list --since`: an operator who copies an
        // argument from one verb to another must not discover that they
        // disagree.
        let since = filter
            .since
            .as_deref()
            .map(|s| {
                fq_runtime::views::since::instant(s).map_err(|e| invalid(format!("since {e}")))
            })
            .transpose()?;
        Ok(TriggerSelection {
            agent: filter.agent.clone(),
            since,
        })
    }

    /// `since` as the table stores its timestamps. The column is text
    /// and the comparison lexical, so re-rendering the parsed instant —
    /// rather than passing the caller's spelling through — is what
    /// makes `…07.500Z` and `…07.500+00:00` the same instant to the
    /// query as they are to the reader.
    fn since_as_stored(&self) -> Option<String> {
        self.since.map(|t| t.to_rfc3339())
    }
}

fn internal(e: fq_runtime::views::ViewsError) -> WireError {
    WireError::Internal {
        message: e.to_string(),
    }
}

/// Get one trigger by identity, or say which kind of "not here" it is.
///
/// **The absent case is a state, not a miss.** A trigger that has been
/// published but not yet consumed is a real, durable, queued thing with
/// no record yet — answering `NotFound` would tell an operator the
/// system had lost the trigger it had just been handed the name of. So
/// it comes back as `Unlocatable`: the identity is well-formed, and
/// what cannot be produced is the record, not the trigger.
///
/// It is **one** state and not three, because the daemon genuinely
/// cannot tell its causes apart from here — not yet consumed, recorded
/// before this table existed, or a name that was never real all look
/// identical to a primary-key lookup. Splitting them would mean
/// guessing, so the message names every cause instead.
async fn trigger_by_id(views: &Views, trigger_id: &str) -> Result<Trigger, WireError> {
    // A string that is not a UUID cannot name any trigger: every
    // identity the runtime records is minted or parsed as one. So it is
    // a verdict on the request, and re-rendering the parsed value
    // normalises the spelling the lookup binds.
    let asked = uuid::Uuid::parse_str(trigger_id).map_err(|e| WireError::InvalidInput {
        op: "trigger.get".into(),
        message: format!("trigger_id `{trigger_id}`: {e}"),
    })?;
    views
        .trigger(&asked.to_string())
        .await
        .map_err(internal)?
        .ok_or_else(|| WireError::Unlocatable {
            op: "trigger.get".into(),
            message: format!(
                "trigger `{asked}` has no durable record — the identity is well-formed and this \
                 is not `no such trigger`. A trigger becomes readable when the runtime acts on \
                 it, so one that is queued and not yet dispatched reads exactly like this and \
                 will resolve once a worker picks it up. Two other causes look the same from \
                 here and do not resolve: a trigger recorded before triggers were kept (records \
                 are forward-only, so anything older than that migration can never be found by \
                 id), and an id that names nothing at all."
            ),
        })
}

/// Register the Trigger atom and `trigger.publish` on the daemon's edge.
pub(crate) fn register_trigger_surface(
    registry: &mut fq_edge::EdgeRegistry,
    bus: fq_runtime::EventBus,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    register_trigger_atom(registry, views)?;
    register_trigger_command(registry, bus)
}

fn register_trigger_atom(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
) -> anyhow::Result<()> {
    let decl = fq_ops::Atom::with_index::<TriggerKey, Trigger, TriggerView, TriggerFilter>(
        fq_ops::Domain::Trigger,
        "One request to run an agent, as the runtime recorded it.",
        fq_ops::Stability::Experimental,
    )
    .description(concat!(
        "`trigger_id` is a Trigger's identity — the UUIDv7 the runtime minted \
         when it published the trigger, or the one an inbound `Fq-Trigger-Id` \
         header supplied. It is what `trigger.publish` hands back in its \
         receipt, and handing that key here unchanged is the whole walk. \
         TRIGGERS ARE KEPT INDEFINITELY, PAYLOADS INCLUDED: the record lives \
         in the projection and the retention sweep never reaches it, so a \
         trigger outlives both the trigger stream (24h) and the event log (30 \
         days). All three verbs answer from that one record, so there is no \
         window in which a trigger lists and then cannot be fetched. ",
        "LIST DOES NOT RETURN PAYLOADS. It answers with index rows — \
         identity, agent, when the trigger was recorded, and how the runtime \
         came by it — most recently recorded first, and every row carries the \
         identity that reads the whole trigger back through `trigger.get`. \
         Get and Stream answer with the whole Trigger, payload included. The \
         split is not cosmetic: an accepted payload may be up to 512 KiB and \
         one answer is one 8 MiB frame, so a page of whole triggers is a page \
         that can fail to encode. ",
        "LIST AND STREAM SELECT THE SAME TRIGGERS FOR THE SAME FILTER, from \
         the same store — they differ in shape and in order, not in \
         population. List is newest-first and paged; Stream is \
         sequence-ordered and cursored, long-polling via next_batch, with \
         `from_seq = u64::MAX` seeking the tail of the recorded triggers \
         (not of the event log, which runs ahead of what has been recorded). \
         A trigger whose record arrived without a log position lists and gets \
         but never streams: a cursor is the one thing it has no honest value \
         for. ",
        "A trigger with NO DURABLE RECORD answers `Unlocatable`, never `not \
         found` — a queued trigger nothing has dispatched yet is real and \
         will resolve, and saying `no such trigger` about the name a receipt \
         just issued would be a lie. The same answer covers a record written \
         before triggers were kept (forward-only, so it can never be found by \
         id) and an id that names nothing; the message names all three, \
         because a primary-key lookup cannot tell them apart.",
    ));

    let get_views = views.clone();
    let list_views = views.clone();
    registry
        .atom::<TriggerKey, Trigger, TriggerView, TriggerFilter, _, _, _, _, _, _>(
            decl,
            move |key: TriggerKey| {
                let views = get_views.clone();
                async move { trigger_by_id(&views, &key.trigger_id).await }
            },
            move |filter: TriggerFilter| {
                let views = list_views.clone();
                async move {
                    let selection = TriggerSelection::compile(&filter, "trigger.list")?;
                    let limit = filter.list_limit()?;
                    let since = selection.since_as_stored();
                    views
                        .triggers(
                            selection.agent.as_deref(),
                            since.as_deref(),
                            i64::from(limit),
                        )
                        .await
                        .map_err(internal)
                }
            },
            move |filter: TriggerFilter, from_seq, max_wait_ms| {
                let views = views.clone();
                async move {
                    let selection = TriggerSelection::compile(&filter, "trigger.stream")?;
                    stream_triggers(&views, &selection, from_seq, max_wait_ms).await
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}

/// One long-poll batch of triggers at or after `from_seq`; `u64::MAX`
/// seeks the tail.
///
/// The other atoms long-poll a JetStream consumer, which blocks until a
/// message arrives. This one waits on a local SQLite table, so it waits
/// by asking again on [`TRIGGER_POLL_INTERVAL_MS`] — the same contract
/// (return as soon as there is anything, otherwise at the deadline with
/// a usable cursor) reached with the mechanism the store actually has.
///
/// The cursor is still the event-log sequence — the universal cursor
/// (P5), the same number `min_seq` gates and every other stream speak —
/// because that is what the record carries. It is a position, never an
/// identity: ask for a trigger by `trigger_id`, use this to resume.
async fn stream_triggers(
    views: &Views,
    selection: &TriggerSelection,
    from_seq: u64,
    max_wait_ms: u64,
) -> Result<fq_edge::wire::StreamBatch, WireError> {
    let since = selection.since_as_stored();
    let mut next_from_seq = if from_seq == u64::MAX {
        views.trigger_tip().await.map_err(internal)? + 1
    } else {
        from_seq
    };
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_millis(max_wait_ms.min(TRIGGER_MAX_WAIT_CEILING_MS));
    let mut items = Vec::new();
    loop {
        let page = views
            .triggers_from(
                selection.agent.as_deref(),
                since.as_deref(),
                next_from_seq,
                TRIGGER_BATCH_CAP,
            )
            .await
            .map_err(internal)?;
        for (seq, trigger) in page {
            let item = serde_json::to_value(&trigger).map_err(|e| WireError::Internal {
                message: e.to_string(),
            })?;
            items.push(fq_edge::wire::StreamItem { seq, item });
            next_from_seq = seq + 1;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if !items.is_empty() || remaining.is_zero() {
            break;
        }
        // Clamped to what is left, so a short poll honours the bound it
        // was given rather than the interval: `max_wait_ms: 100` must
        // not cost 200ms because that is how often this happens to ask.
        tokio::time::sleep(
            remaining.min(std::time::Duration::from_millis(TRIGGER_POLL_INTERVAL_MS)),
        )
        .await;
    }
    Ok(fq_edge::wire::StreamBatch {
        items,
        next_from_seq,
    })
}

/// The typed input of `trigger.publish` on the wire.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct PublishCommandInput {
    agent_id: String,
    /// The trigger body, verbatim. Any JSON: agents receive it as the
    /// user message their run opens with. Bounded at
    /// [`fq_runtime::trigger::MAX_TRIGGER_PAYLOAD_BYTES`] — see the
    /// command's description.
    #[serde(default)]
    payload: serde_json::Value,
}

/// `trigger.publish`'s declaration — the value, apart from the
/// handler, so a test can read the contract text the surface publishes
/// without standing up a bus to bind it to.
fn publish_declaration() -> fq_ops::Command {
    fq_ops::Command::new::<PublishCommandInput>(
        fq_ops::Trigger::Publish,
        fq_ops::Authority {
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Trigger,
        },
        "Dispatch a trigger to an agent via the durable trigger stream.",
        fq_ops::Stability::Experimental,
    )
    .description(concat!(
        "At-least-once delivery with a bounded budget: the trigger is durable \
         when this answers, and a delivery that keeps failing is dead-lettered \
         (`fq dead-letters list`) rather than retried forever. The answer means \
         accepted, not run — an agent this daemon does not have is a dead \
         letter, not a refusal here. THE RECEIPT NAMES THE TRIGGER: its \
         `AtomRef` carries `{\"trigger_id\": \"…\"}`, which is exactly the key \
         `trigger.get` takes, so a publish walks to the thing it published. ",
        "The receipt's watermark is the trigger's sequence on the TRIGGER \
         stream, which is a different log from the event stream every other \
         watermark on this surface speaks: it is what `fq dead-letters` \
         reconciles on, and it is NOT a `min_seq` for reading the trigger \
         back. Publishing appends no durable record — the record appears when \
         a worker consumes the trigger — so there is no position a \
         read-your-writes gate could wait for, and `trigger.get` answers \
         `Unlocatable` until then. ",
        "THE PAYLOAD IS BOUNDED AT 512 KiB (524288 bytes of JSON body), and a \
         larger one is REFUSED, never truncated — a truncated payload is a \
         different task, and an agent handed one would do the wrong work \
         while every record said it did the right work. The bound exists \
         because a trigger is kept indefinitely: 512 KiB is roughly sixteen \
         hundred times a real task payload, sixteen times under the edge's 8 \
         MiB frame so `trigger.get` can always answer with the whole thing, \
         and half a stock broker's default max_payload so an accepted trigger \
         is never one the transport then refuses.",
    ))
}

fn register_trigger_command(
    registry: &mut fq_edge::EdgeRegistry,
    bus: fq_runtime::EventBus,
) -> anyhow::Result<()> {
    registry
        .command::<PublishCommandInput, _, _>(
            publish_declaration(),
            move |input: PublishCommandInput| {
                let bus = bus.clone();
                async move {
                    // The daemon validates what it is asked to publish: an
                    // id the subject grammar cannot carry must never reach
                    // the broker, and the client's own check is a courtesy
                    // ahead of this one, not a substitute for it.
                    let agent = fq_runtime::AgentId::new(&input.agent_id).map_err(|e| {
                        WireError::InvalidInput {
                            op: "trigger.publish".into(),
                            message: format!("invalid agent name `{}`: {e}", input.agent_id),
                        }
                    })?;
                    let published =
                        bus.publish_trigger(&agent, &input.payload)
                            .await
                            .map_err(|e| match e {
                                // A verdict on the request, not a fault: the
                                // caller sent something this surface does not
                                // accept, and the message says by how much so
                                // the next attempt is an edit rather than a
                                // guess.
                                fq_runtime::bus::BusError::TriggerPayloadTooLarge {
                                    size,
                                    limit,
                                } => WireError::InvalidInput {
                                    op: "trigger.publish".into(),
                                    message: format!(
                                        "trigger payload is {size} bytes, over the {limit}-byte \
                                     limit on an accepted trigger. It is refused rather than \
                                     truncated: a shortened payload is a different task, and \
                                     the agent would run it as though it were yours. Triggers \
                                     are kept indefinitely, which is why there is a limit at \
                                     all — put the bulk somewhere addressable and send a \
                                     reference to it."
                                    ),
                                },
                                other => WireError::Internal {
                                    message: format!(
                                        "failed to publish trigger for `{agent}`: {other}"
                                    ),
                                },
                            })?;
                    tracing::info!(
                        agent_id = %agent,
                        trigger_id = %published.id,
                        stream_seq = published.stream_seq,
                        "published trigger"
                    );
                    // The trigger is named, and now there is a Get to
                    // resolve the name against — so the reference the
                    // domain model always promised is finally one a caller
                    // can follow. The key is the `TriggerKey` shape and not
                    // a bare string or a position: an `AtomRef.key` is
                    // handed to `<domain>.get` unchanged.
                    //
                    // Named, but with no watermark, and `stream_seq` is
                    // why. It is this publish's ack on the *trigger*
                    // stream, while a trigger becomes gettable only when
                    // the projection folds the event the dispatcher emits
                    // later — a position in the event log, which does not
                    // exist yet and which this command cannot know.
                    //
                    // A receipt's watermark is documented as the number a
                    // caller passes as `min_seq`. Putting the ack there
                    // would hand them a coordinate from a log the reader
                    // never consults, and they would gate on it in good
                    // faith. No watermark makes a caller poll or wait;
                    // the wrong one lets them believe they waited.
                    Ok(fq_ops::Receipt::naming(
                        fq_ops::Domain::Trigger,
                        serde_json::json!(TriggerKey {
                            trigger_id: published.id.to_string()
                        }),
                    ))
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fq_runtime::trigger::MAX_TRIGGER_PAYLOAD_BYTES;

    fn filter_with_limit(limit: Option<u32>) -> TriggerFilter {
        TriggerFilter {
            limit,
            ..TriggerFilter::default()
        }
    }

    /// **A page over the cap is refused, not silently shortened.**
    ///
    /// List answers with a bare array of index rows, so a page the
    /// daemon cut down looks exactly like a listing that ended, and an
    /// operator would read a partial answer as the whole one. The
    /// assertion is two things at once: that the over-cap ask errors,
    /// and that no shortened page comes back in its place.
    #[test]
    fn a_page_over_the_cap_is_refused_rather_than_shortened() {
        for asked in [
            TRIGGER_LIST_MAX_LIMIT + 1,
            TRIGGER_LIST_MAX_LIMIT * 10,
            u32::MAX,
        ] {
            let err = filter_with_limit(Some(asked))
                .list_limit()
                .expect_err("a page over the cap must be refused, not served short");
            assert!(
                matches!(&err, WireError::InvalidInput { op, message }
                    if op == "trigger.list"
                        && message.contains(&asked.to_string())
                        && message.contains(&TRIGGER_LIST_MAX_LIMIT.to_string())),
                "expected an InvalidInput naming {asked} and the cap; got {err:?}"
            );
            // …and it names the ways out, because a cap a caller cannot
            // page past would be a dead end: this atom has no cursor on
            // List, so the answer to "more than a page" is narrowing or
            // the stream, and both of those exist.
            let WireError::InvalidInput { message, .. } = &err else {
                unreachable!("checked above")
            };
            for route in ["agent", "since", "trigger.stream"] {
                assert!(
                    message.contains(route),
                    "the refusal must point at `{route}`; got {message}"
                );
            }
        }
    }

    /// Under the cap, the page size is the caller's own number — which
    /// is what makes a row count readable.
    #[test]
    fn under_the_cap_the_page_is_the_callers_own_number() {
        for asked in [0, 1, TRIGGER_LIST_DEFAULT_LIMIT, TRIGGER_LIST_MAX_LIMIT] {
            assert_eq!(
                filter_with_limit(Some(asked)).list_limit().ok(),
                Some(asked),
                "a page within the cap must be exactly the size asked for"
            );
        }
        // No `limit` is the documented default, not the cap.
        assert_eq!(
            filter_with_limit(None).list_limit().ok(),
            Some(TRIGGER_LIST_DEFAULT_LIMIT)
        );
    }

    /// **The bounds are on the declared surface, not only in the
    /// code.** A consumer has to read them off `operator_surface.json`
    /// rather than discover them by being refused, so both the page cap
    /// and the payload limit are asserted against the constants the
    /// daemon actually enforces — that drift is precisely how a surface
    /// comes to say less than it means.
    #[test]
    fn the_surface_declares_the_bounds_it_enforces() {
        let schema = serde_json::to_value(schemars::schema_for!(TriggerFilter))
            .expect("the filter schema serialises");
        let limit = &schema["properties"]["limit"];
        assert_eq!(
            limit["maximum"].as_u64(),
            Some(u64::from(TRIGGER_LIST_MAX_LIMIT)),
            "the schema's maximum must be the cap the daemon enforces; got {limit}"
        );
        let described = limit["description"].as_str().expect("a described property");
        assert!(
            described.contains(&TRIGGER_LIST_MAX_LIMIT.to_string()),
            "the declared description must name the cap; got {described:?}"
        );
        // The payload limit is a number a publisher has to be able to
        // read before it sends, so the command's contract text carries
        // it in bytes — the unit the refusal is measured in.
        let publish = publish_declaration().description;
        assert!(
            publish.contains(&MAX_TRIGGER_PAYLOAD_BYTES.to_string()),
            "the declared description must name the payload limit in bytes; got {publish:?}"
        );
    }

    /// The two narrowings are the two the record has, and both are
    /// verdicts on the request when they are unusable — an empty
    /// listing would read as "that agent has had nothing to do".
    #[test]
    fn an_unusable_narrowing_is_a_verdict_on_the_request() {
        let refused = |filter: TriggerFilter| {
            TriggerSelection::compile(&filter, "trigger.list").expect_err("must refuse")
        };
        let err = refused(TriggerFilter {
            agent: Some("not a token".into()),
            ..TriggerFilter::default()
        });
        assert!(
            matches!(&err, WireError::InvalidInput { op, message }
                if op == "trigger.list" && message.contains("not a token")),
            "expected an InvalidInput naming the agent; got {err:?}"
        );
        let err = refused(TriggerFilter {
            since: Some("yesterday".into()),
            ..TriggerFilter::default()
        });
        assert!(
            matches!(&err, WireError::InvalidInput { message, .. }
                if message.contains("yesterday") && message.contains("RFC3339")),
            "expected an InvalidInput naming the accepted forms; got {err:?}"
        );
    }

    /// The two stores a `since` reaches must not disagree about which
    /// instant was asked for: the column is text and the comparison
    /// lexical, so the parsed instant is re-rendered the way the table
    /// writes its timestamps rather than passed through as typed. An
    /// operator's bare date is still a lower bound on the whole day —
    /// the spelling QUICKSTART prints, and the one `fq costs --since`
    /// takes.
    #[test]
    fn since_is_normalised_to_the_way_the_table_stores_it() {
        let compiled = |s: &str| {
            TriggerSelection::compile(
                &TriggerFilter {
                    since: Some(s.into()),
                    ..TriggerFilter::default()
                },
                "trigger.list",
            )
            .expect("a valid instant")
            .since_as_stored()
        };
        assert_eq!(
            compiled("2026-04-25").as_deref(),
            Some("2026-04-25T00:00:00+00:00")
        );
        let stored = "2026-01-02T03:04:07.500+00:00";
        assert_eq!(
            compiled("2026-01-02T03:04:07.500Z").as_deref(),
            Some(stored)
        );
        // Same instant, other side of the world: an offset is a
        // spelling, and the query must not be sensitive to it.
        assert_eq!(
            compiled("2026-01-02T08:34:07.500+05:30").as_deref(),
            Some(stored)
        );
        assert_eq!(
            TriggerSelection::compile(&TriggerFilter::default(), "trigger.list")
                .unwrap()
                .since_as_stored(),
            None
        );
    }

    /// **The receipt's key is the key Get takes.** A receipt whose
    /// `AtomRef` regressed to a bare string, a position, or any other
    /// ad-hoc shape would hand a caller a reference it cannot follow —
    /// which is exactly the state this step exists to end. Asserted
    /// against the atom's own declared `key_schema` rather than against
    /// a hand-written expectation, so the two cannot drift.
    #[test]
    fn the_receipt_key_is_the_shape_get_accepts() {
        let key = serde_json::json!(TriggerKey {
            trigger_id: uuid::Uuid::now_v7().to_string()
        });
        let object = key.as_object().expect("a key is an object");
        assert_eq!(
            object.keys().collect::<Vec<_>>(),
            vec!["trigger_id"],
            "the key is exactly `trigger_id` — nothing more, nothing less"
        );
        let schema = serde_json::to_value(schemars::schema_for!(TriggerKey))
            .expect("the key schema serialises");
        assert_eq!(
            schema["required"],
            serde_json::json!(["trigger_id"]),
            "the declared key_schema must require the field the receipt sends"
        );
        // And it round-trips: what the receipt carries deserialises as
        // the key the Get handler is handed.
        let parsed: TriggerKey = serde_json::from_value(key.clone()).expect("Get accepts the key");
        assert_eq!(parsed.trigger_id, key["trigger_id"].as_str().unwrap());
    }
}
