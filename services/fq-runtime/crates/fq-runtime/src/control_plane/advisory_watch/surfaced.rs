//! Whether the dead letter an advisory would write is on the log already
//! (issue 1066).
//!
//! The watch writes a dead letter and then acks the advisory, so a watch
//! that stops between the two meets the advisory again: after the advisory
//! consumer's ack window (30 seconds by default) if only the ack was lost,
//! and when it next starts if the daemon went down, however long that was,
//! up to the advisory stream's day. And the dispatcher can have written the
//! trigger's dead letter itself before the trigger came to the watch, when
//! its publish landed and only its acknowledgement was lost. The checks
//! below keep one exhaustion to one dead letter, whichever form came first;
//! a dead letter that names the trigger may still follow one that names
//! none, since it is the one that can be requeued. Where a check cannot be
//! certain, the watch writes: a second dead letter is the cheaper error, and
//! no answer these checks give loses one.
//!
//! - **What this watch wrote, it remembers.** A copy of an advisory whose
//!   dead letter this watch already wrote since it started is answered from
//!   memory ([`WrittenHere`]), before any read: copies of one advisory can
//!   wait in the watch's buffer together (it handles one advisory at a time,
//!   and one whose ack window ran out while it waited is delivered again),
//!   and each would otherwise be one more look at the log. The reads below
//!   are then for what memory cannot hold: a dead letter written before a
//!   restart, and the dispatcher's.
//! - **One that names its trigger** goes under the trigger's agent, as the
//!   dispatcher's own dead letter does. The check reads the agent's `failed`
//!   events since the trigger's message was published, not the agent's last
//!   failure alone, since another failure of the agent can come in between.
//!   It takes a dead letter at the trigger's sequence for this trigger's only
//!   when that is certain: a trigger sequence names one message only within
//!   one incarnation of the trigger stream, so the dead letter must record
//!   the incarnation of the stream the watch is connected to
//!   ([`DEAD_LETTER_STREAM_EPOCH_KEY`]), and, when the trigger was published
//!   with an id, name that id. One that records no incarnation, written
//!   before dead letters recorded it, is never taken: a second dead letter at
//!   worst. A trigger published without an id is matched by the sequence and
//!   the incarnation alone, since the dispatcher gives it an id for each
//!   delivery and its dead letter carries one the watch cannot know; such a
//!   match is trusted only once the broker says the trigger stream is still
//!   the incarnation this watch holds, for a daemon left running over a
//!   recreated stream holds the old one (issue 1065), and its dispatcher can
//!   write the old message's dead letter at a sequence the new stream reuses.
//!   That is one request to the broker, made only for such a match: none for
//!   a trigger with an id, none when no dead letter is found, and none for a
//!   copy answered from memory. So an earlier stream's dead letter at the
//!   same sequence is not taken for a new trigger's, whenever it was written
//!   and whatever the servers' clocks say, with one exception. A match on the
//!   trigger's id is not asked about, so a watch holding the earlier
//!   incarnation takes the earlier stream's dead letter that names an id for
//!   that id published again at the same sequence of the recreated stream,
//!   and writes none. Requeue is keyed on the id, so the work stays
//!   requeueable from the earlier one. The read starts at the trigger's
//!   publish only to bound its cost: no dead letter for the trigger can be
//!   older than its message.
//! - **One that names no trigger**, or whose trigger's subject names no
//!   legal agent, goes under the system sentinel. Every dead letter the
//!   watch writes carries the broker's id for its advisory
//!   ([`DEAD_LETTER_ADVISORY_ID_KEY`]), and before writing one under the
//!   system sentinel the watch reads every agent's `failed` events since the
//!   advisory was emitted for one with that id: its own nameless one, and a
//!   named one it wrote before the trigger's message went. A dead letter for
//!   an advisory cannot be older than the advisory, so the read starts at the
//!   advisory's timestamp. An advisory with no id, or an empty one, cannot
//!   be told apart from another, so its dead letter is written every time,
//!   and says its check could not be made.
//!
//! Both reads stop at the last event the subject held on entry, skip an
//! event this build cannot read (it is not a dead letter this build wrote),
//! and take only the broker's "no message" as "none there". A read the
//! broker answers with an error, or one that takes longer than its limit
//! ([`SCAN_LIMIT`]), leaves the advisory to be decided again (NAKed), as it
//! is when the dead letter cannot be published. Past the watch's limit, the
//! hour and three looks every held verdict ends at, a check that still
//! cannot be made does not hold the dead letter back: it is written, and it
//! says so ([`crate::dead_letter::DEAD_LETTER_DUPLICATE_CHECK_KEY`]). That
//! may write a second one; it never loses one.
//!
//! **What the reads cost** follows how many `failed` events came since the
//! trigger's publish (that agent's) or since the advisory (every agent's),
//! at most the advisory stream's day of them. Two reads start at the
//! subject's first event instead, and cost that much more without changing
//! their answer: one for an advisory that carries no timestamp the watch can
//! read, and one whose consumer the client has to create again before its
//! first message. A backlog of advisories after a restart costs about the
//! square of its size, since each one's read passes the dead letters written
//! for those before it; nothing is lost by it while the backlog clears within
//! the advisory stream's day, after which an advisory still waiting ages out
//! with no dead letter, as every held one does (issue 1087). Where the
//! servers leading the streams disagree on the time, or the broker's clock
//! steps back between a publish or an advisory and the write, a dead letter
//! can look older than the read's start, and a second is written.
//!
//! What these checks still leave, each a second dead letter, never a lost one
//! to the check (issue 1091):
//!
//! - **The dispatcher publishes its own dead letter after the watch has
//!   looked**: one still on its way to the events stream when the trigger
//!   came to the watch. Closing it needs both emitters to publish the dead
//!   letter for one trigger message under one broker message id, a change to
//!   the dispatcher.
//! - **A nameless one after the dispatcher's own named one.** The
//!   dispatcher's dead letter carries no advisory id, so when the trigger's
//!   message is already gone at the watch's first look (the watch down for
//!   about a day, or the message deleted), a nameless dead letter follows
//!   it.
//! - **A check that cannot be made in time**: past the limit the dead letter
//!   is written, so one a run of the daemon before this one wrote is written
//!   again.
//! - **An advisory with no id**: its nameless dead letter is written every
//!   time the advisory comes back to a watch that does not remember it.
//! - **A daemon left running over a recreated stream** (issue 1065) holds
//!   the earlier stream's incarnation. For a trigger published without an id
//!   whose dead letter is already on the log, stamped with that earlier
//!   incarnation, its watch asks the broker, finds the stream recreated, and
//!   writes a second; a watch started after the recreate takes neither for
//!   the trigger's and writes a third.
//! - **Clocks**, above.
//!
//! And what the check trusts without checking: which message the advisory
//! names. An advisory with no timestamp the watch can read is taken as this
//! incarnation's, and a daemon left running over a recreated stream reads an
//! earlier stream's advisory at its sequence in the new one (issue 1065);
//! in both the check looks for that message's dead letter.
//!
//! The event stream keeps an event for 30 days by default, longer than the
//! advisory stream keeps an advisory, so a dead letter written for an
//! advisory the watch can still be handed is still there to find. With
//! `[events] max_age` set below a day, a dead letter can age out of the
//! events stream first, and a redelivery after that writes it again. The
//! broker's own duplicate window would not do for either check: it keeps two
//! minutes, and a redelivery after a restart comes whenever the daemon does.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_nats::jetstream::message::StreamMessage;
use async_nats::jetstream::stream::{RawMessageError, RawMessageErrorKind};
use futures::StreamExt;
use tracing::info;
use uuid::Uuid;

use super::naming::TriggerMessage;
use super::{AdvisoryWatch, AdvisoryWatchError, CONSUMER_NAME, MaxDeliverAdvisory};
use crate::agent::AgentId;
use crate::bus::{BusError, DEFAULT_TRIGGER_MAX_AGE, STREAM_NAME, TRIGGER_STREAM_NAME};
use crate::dead_letter::{
    DEAD_LETTER_ADVISORY_ID_KEY, DEAD_LETTER_DUPLICATE_CHECK_KEY, DEAD_LETTER_STREAM_EPOCH_KEY,
    DEAD_LETTER_STREAM_SEQ_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
};
use crate::event_tail::RawTail;
use crate::events::{Event, EventPayload, FailureKind, subjects};

/// How long a read of the log for a dead letter already written may take
/// before the watch gives up on it and decides the advisory again later.
pub(super) const SCAN_LIMIT: Duration = Duration::from_secs(10);

/// What the check for a dead letter already written found
/// ([`AdvisoryWatch::written_before`]).
pub(super) enum Check {
    /// This exhaustion's dead letter is on the log: write none.
    Written,
    /// It is not: write it.
    NotWritten,
    /// The check could not be made, and the advisory is past the watch's
    /// limit: write it anyway, saying why the check failed.
    Unmade(String),
}

/// The trigger a dead letter names, as far as the check for one already
/// written needs it.
pub(super) struct NamedTrigger {
    /// The agent whose `failed` events hold its dead letter.
    agent: AgentId,
    /// The id its message carries, when it carries one.
    id: Option<Uuid>,
    /// When its message was published.
    published: async_nats::datetime::DateTime,
}

impl NamedTrigger {
    /// The trigger `message` names, when its subject names a legal agent:
    /// otherwise its dead letter goes under the system sentinel, and is
    /// looked for there.
    pub(super) fn of(message: &TriggerMessage) -> Option<Self> {
        message.agent_id.clone().map(|agent| NamedTrigger {
            agent,
            id: message.trigger_id,
            published: message.published,
        })
    }
}

/// How many advisories the watch remembers writing a dead letter for, at
/// most: memory a flood of exhaustions cannot grow past. One it has
/// forgotten is looked for on the log, as after a restart.
const REMEMBERED: usize = 100_000;

/// The advisories this watch has written a dead letter for since it
/// started, by the broker's id for each, and whether the one it wrote named
/// its trigger. One is forgotten a day after it was written, when its
/// advisory has aged out of the advisory stream and cannot come back, or
/// sooner past [`REMEMBERED`].
#[derive(Default)]
pub(super) struct WrittenHere(Mutex<Remembered>);

#[derive(Default)]
struct Remembered {
    named: HashMap<String, bool>,
    order: VecDeque<(Instant, String)>,
}

impl WrittenHere {
    /// Remember that the watch wrote a dead letter for `advisory`, naming
    /// its trigger when `named`.
    pub(super) fn note(&self, advisory: &MaxDeliverAdvisory, named: bool) {
        let Some(id) = advisory_id(advisory) else {
            return;
        };
        let mut remembered = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        while remembered
            .order
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > DEFAULT_TRIGGER_MAX_AGE)
            || remembered.order.len() >= REMEMBERED
        {
            if let Some((_, old)) = remembered.order.pop_front() {
                remembered.named.remove(&old);
            }
        }
        match remembered.named.get_mut(id) {
            Some(was) => *was |= named,
            None => {
                remembered.named.insert(id.to_string(), named);
                remembered.order.push_back((now, id.to_string()));
            }
        }
    }

    /// Whether the watch wrote a dead letter for `advisory` that stands for
    /// the one it would write now: any, when that one names no trigger; one
    /// that named it, when it does, since a dead letter that names the
    /// trigger may follow one that named none.
    pub(super) fn wrote(&self, advisory: &MaxDeliverAdvisory, named: bool) -> bool {
        let Some(id) = advisory_id(advisory) else {
            return false;
        };
        let remembered = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let wrote = remembered.named.get(id).is_some_and(|was| *was || !named);
        if wrote {
            info!(
                consumer = CONSUMER_NAME,
                stream_seq = advisory.stream_seq,
                advisory_id = id,
                "this watch already wrote a dead letter for this advisory; skipping"
            );
        }
        wrote
    }
}

/// The broker's id for `advisory`, when it carries one: an empty one is
/// none, since it cannot tell one advisory from another.
fn advisory_id(advisory: &MaxDeliverAdvisory) -> Option<&str> {
    advisory.id.as_deref().filter(|id| !id.is_empty())
}

/// `event`, a dead letter the watch writes, annotated with the broker's id
/// for its advisory, when the advisory carries one.
pub(super) fn with_advisory_id(event: Event, advisory: &MaxDeliverAdvisory) -> Event {
    match advisory_id(advisory) {
        Some(id) => event.annotate(
            DEAD_LETTER_ADVISORY_ID_KEY,
            serde_json::Value::String(id.to_string()),
        ),
        None => event,
    }
}

/// `event`, a dead letter the watch writes, saying its check for one
/// already written could not be made, when it could not
/// ([`DEAD_LETTER_DUPLICATE_CHECK_KEY`]).
pub(super) fn with_duplicate_check(event: Event, check: &Option<String>) -> Event {
    match check {
        Some(check) => event.annotate(
            DEAD_LETTER_DUPLICATE_CHECK_KEY,
            serde_json::Value::String(check.clone()),
        ),
        None => event,
    }
}

impl AdvisoryWatch {
    /// Whether the dead letter `advisory` would write is on the log already:
    /// under the trigger's agent since its message was published when the
    /// dead letter names the trigger (`named`), and otherwise under any
    /// agent since the advisory, by the advisory's id. A nameless one whose
    /// advisory has no id, or an empty one, cannot be told apart, and is
    /// written. A check that cannot be made is an error inside the watch's
    /// limit, and once `looked_long_enough` is [`Check::Unmade`].
    pub(super) async fn written_before(
        &self,
        advisory: &MaxDeliverAdvisory,
        named: Option<NamedTrigger>,
        looked_long_enough: bool,
    ) -> Result<Check, AdvisoryWatchError> {
        let nameless_id = advisory_id(advisory);
        if named.is_none() && nameless_id.is_none() {
            return Ok(Check::Unmade(
                "its advisory carries no id, so a dead letter already written for it cannot \
                 be told from another's"
                    .to_string(),
            ));
        }
        let read = async {
            match (&named, nameless_id) {
                (Some(trigger), _) => self.named_written(trigger, advisory.stream_seq).await,
                (None, Some(id)) => self.written_for(id, emitted_since(advisory)).await,
                // Answered above: there is nothing to look for.
                (None, None) => Ok(false),
            }
        };
        let err = match tokio::time::timeout(self.scan_limit, read).await {
            Ok(Ok(true)) => {
                match &named {
                    Some(trigger) => info!(
                        consumer = CONSUMER_NAME,
                        agent_id = %trigger.agent,
                        stream_seq = advisory.stream_seq,
                        "a dead letter naming this trigger is already on the log, the \
                         dispatcher's or this watch's; skipping"
                    ),
                    None => info!(
                        consumer = CONSUMER_NAME,
                        stream_seq = advisory.stream_seq,
                        advisory_id = advisory_id(advisory),
                        "a dead letter was already written for this advisory; skipping"
                    ),
                }
                return Ok(Check::Written);
            }
            Ok(Ok(false)) => return Ok(Check::NotWritten),
            Ok(Err(err)) => err,
            Err(_) => AdvisoryWatchError::Log(format!(
                "reading the log for a dead letter already written took longer than {:?}",
                self.scan_limit
            )),
        };
        if looked_long_enough {
            Ok(Check::Unmade(err.to_string()))
        } else {
            Err(err)
        }
    }

    /// Whether the trigger's agent holds a dead letter that is certainly
    /// this trigger's ([`dead_letter_for`]), written since its message was
    /// published. The agent's last `failed` event answers when it is older
    /// than that or is the dead letter; otherwise the events since the
    /// publish are read, up to that last one.
    async fn named_written(
        &self,
        trigger: &NamedTrigger,
        stream_seq: u64,
    ) -> Result<bool, AdvisoryWatchError> {
        let subject = subjects::agent_failed(trigger.agent.as_str());
        let Some(last) = self.last_on(&subject).await? else {
            return Ok(false);
        };
        // Needed, not only quicker: a read that starts after the subject's
        // last event waits for one that never comes, and would end only at
        // the limit.
        if last.time < trigger.published {
            return Ok(false);
        }
        let epoch = self.bus.trigger_stream_epoch();
        let ours = |event: &Event| dead_letter_for(event, stream_seq, trigger.id, epoch);
        let found = Event::from_wire(&last.payload).is_ok_and(|event| ours(&event)) || {
            let messages = self
                .bus
                .raw_events_since(&subject, Some(trigger.published))
                .await
                .map_err(read_error)?;
            find(messages, last.sequence, ours).await?
        };
        // A match that rests on the sequence and the incarnation alone, with
        // no trigger id to confirm it, is certain only while the stream is
        // still the incarnation this watch holds: a daemon left running over
        // a recreated stream holds the old one, and its dispatcher can write
        // the old message's dead letter at a sequence the new stream reuses.
        Ok(found && (trigger.id.is_some() || self.still_this_incarnation().await?))
    }

    /// Whether the trigger stream is still the incarnation this watch
    /// connected to: its creation time, asked of the broker, is the one
    /// this process holds.
    async fn still_this_incarnation(&self) -> Result<bool, AdvisoryWatchError> {
        let stream = self
            .bus
            .jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .map_err(|err| {
                AdvisoryWatchError::TriggerStream(format!("reading its creation: {err}"))
            })?;
        let created = stream.cached_info().created.unix_timestamp_nanos();
        Ok(created == i128::from(self.bus.trigger_stream_epoch()))
    }

    /// Whether any agent holds a dead letter carrying `advisory_id`, reading
    /// every `failed` event stored since `since` (all of them with none) up
    /// to the last on entry.
    async fn written_for(
        &self,
        advisory_id: &str,
        since: Option<async_nats::datetime::DateTime>,
    ) -> Result<bool, AdvisoryWatchError> {
        let subject = subjects::ALL_AGENTS_FAILED;
        let Some(last) = self.last_on(subject).await? else {
            return Ok(false);
        };
        // Needed, as in the named check: a read that starts after the
        // subject's last event would wait out the limit.
        if since.is_some_and(|since| last.time < since) {
            return Ok(false);
        }
        let messages = self
            .bus
            .raw_events_since(subject, since)
            .await
            .map_err(read_error)?;
        let id = serde_json::Value::String(advisory_id.to_string());
        find(messages, last.sequence, |event| {
            exhausted(event) && event.annotations.0.get(DEAD_LETTER_ADVISORY_ID_KEY) == Some(&id)
        })
        .await
    }

    /// The last message on `subject` in the events stream, or none. The
    /// stream handle is made without asking the broker, so a missing stream
    /// is this read's own answer.
    async fn last_on(&self, subject: &str) -> Result<Option<StreamMessage>, AdvisoryWatchError> {
        let stream = self
            .bus
            .jetstream()
            .get_stream_no_info(STREAM_NAME)
            .await
            .map_err(|err| AdvisoryWatchError::Log(err.to_string()))?;
        last_message(stream.get_last_raw_message_by_subject(subject).await)
    }
}

/// When `advisory` was emitted, as a time the broker can start a read at,
/// when it carries one the watch can read.
fn emitted_since(advisory: &MaxDeliverAdvisory) -> Option<async_nats::datetime::DateTime> {
    advisory
        .emitted_at()
        .and_then(|at| at.timestamp_nanos_opt())
        .and_then(|nanos| async_nats::datetime::from_nanos(nanos.into()).ok())
}

/// The last message on a subject, as the checks take the broker's answer:
/// "no message" is none, and any other error is an error, never none.
pub(super) fn last_message(
    answer: Result<StreamMessage, RawMessageError>,
) -> Result<Option<StreamMessage>, AdvisoryWatchError> {
    match answer {
        Ok(last) => Ok(Some(last)),
        Err(err) if err.kind() == RawMessageErrorKind::NoMessageFound => Ok(None),
        Err(err) => Err(AdvisoryWatchError::Log(err.to_string())),
    }
}

/// Whether `messages` hold an event `matches` takes, reading up to the
/// sequence `tip`, which the tail must reach. An event this build cannot
/// read is skipped.
async fn find(
    mut messages: RawTail,
    tip: u64,
    matches: impl Fn(&Event) -> bool,
) -> Result<bool, AdvisoryWatchError> {
    while let Some(next) = messages.next().await {
        let (seq, payload) = next.map_err(read_error)?;
        if Event::from_wire(&payload).is_ok_and(|event| matches(&event)) {
            return Ok(true);
        }
        if seq >= tip {
            return Ok(false);
        }
    }
    Err(AdvisoryWatchError::Log(
        "the log ended before the last event seen on entry".to_string(),
    ))
}

/// A read of the log that failed, as the checks report one: "event log
/// read error", whatever part of the read failed.
fn read_error(err: BusError) -> AdvisoryWatchError {
    AdvisoryWatchError::Log(err.to_string())
}

/// Whether `event` is the dead letter for the trigger at `stream_seq` of
/// the trigger stream created at `epoch`, named `id` when the trigger
/// carries one: a dead letter at that sequence that records that
/// incarnation and does not name another trigger. One that records no
/// incarnation is never taken. Anything less is not taken, and the watch
/// writes.
fn dead_letter_for(event: &Event, stream_seq: u64, id: Option<Uuid>, epoch: i64) -> bool {
    let annotation = |key| event.annotations.0.get(key);
    exhausted(event)
        && annotation(DEAD_LETTER_STREAM_SEQ_KEY) == Some(&serde_json::json!(stream_seq))
        && annotation(DEAD_LETTER_STREAM_EPOCH_KEY)
            == Some(&serde_json::Value::String(epoch.to_string()))
        && id.is_none_or(|id| {
            annotation(DEAD_LETTER_TRIGGER_ID_KEY)
                == Some(&serde_json::Value::String(id.to_string()))
        })
}

/// Whether `event` is a dead letter: a `failed` event of a trigger that ran
/// out of deliveries.
fn exhausted(event: &Event) -> bool {
    matches!(
        &event.payload,
        EventPayload::Failed(p) if matches!(p.error_kind, FailureKind::TriggerExhausted)
    )
}

#[cfg(test)]
mod tests;
