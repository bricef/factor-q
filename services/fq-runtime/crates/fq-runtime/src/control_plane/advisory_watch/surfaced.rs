//! Whether the dead letter an advisory would write under the system
//! sentinel is on the log already (issue 1066).
//!
//! The watch writes a dead letter and then acks the advisory, so a watch
//! that stops between the two meets the advisory again: after the advisory
//! consumer's ack window (30 seconds by default) if only the ack was lost,
//! and when it next starts if the daemon went down, however long that was,
//! up to the advisory stream's day. A dead letter that names its trigger's
//! agent goes under that agent, and the watch's last-event check there
//! keeps it from being written twice. One filed under the system sentinel,
//! because it names no trigger or its trigger's subject names no legal
//! agent, has no agent to look under, so it carries the broker's id for the
//! advisory ([`DEAD_LETTER_ADVISORY_ID_KEY`]), and before writing one the
//! watch reads the system sentinel's dead letters for that id.
//!
//! - **What this watch wrote, it remembers.** A copy of an advisory whose
//!   dead letter this watch already wrote since it started is answered from
//!   memory ([`WrittenHere`]), before any read: copies of one advisory can
//!   wait in the watch's buffer together (it handles one advisory at a time,
//!   and one whose ack window ran out while it waited is delivered again),
//!   and each would otherwise be one more look at the log. The read below is
//!   then for what memory cannot hold: a dead letter written before a
//!   restart.
//! - **Every one since the advisory, not only the last.** After a restart
//!   the watch can decide other advisories before a redelivered one. A dead
//!   letter for an advisory cannot be older than the advisory, and both
//!   times are the broker's, so the read starts at the advisory's timestamp
//!   and stops at the last event the subject held on entry: its cost follows
//!   how many `failed` events the system sentinel got since the advisory was
//!   emitted, and the advisory stream keeps an advisory for a day. Two reads
//!   start at the subject's first event instead, and cost that much more
//!   without changing their answer: one for an advisory that carries no
//!   timestamp the watch can read, and one whose consumer the client has to
//!   create again before its first message. A backlog of advisories after a
//!   restart costs about the square of its size, since each one's read
//!   passes the dead letters written for those before it; nothing is lost by
//!   it (issue 1087). Where the two streams are led by servers whose clocks
//!   differ, or the broker's clock steps back between the advisory and the
//!   write, a dead letter can look older than its advisory, and a second is
//!   written rather than none.
//! - **An event this build cannot read is skipped**: it is not a dead letter
//!   this build wrote.
//! - **What the read cannot answer is not "not written".** A read the
//!   broker answers with an error, or one that takes longer than its limit
//!   ([`SCAN_LIMIT`]), leaves the advisory to be decided again (NAKed), as
//!   it is when the dead letter cannot be published. Only "no message" is
//!   "none there". Past the watch's limit, the hour and three looks every
//!   held verdict ends at, a check that still cannot be made does not hold
//!   the dead letter back: it is written, and its line says the check could
//!   not be made. That may write a second one for a dead letter written
//!   before the watch last started; it never loses one.
//! - **An advisory with no id, or an empty one, cannot be told apart** from
//!   another, so its dead letter is written every time.
//!
//! A dead letter that names no trigger can still follow one that named it
//! for the same exhaustion: written, its advisory's ack lost, and the
//! trigger's message gone by the time the advisory comes back. The named one
//! carries no advisory id, and this check reads only the system sentinel's.
//!
//! The event stream keeps an event for 30 days by default, longer than the
//! advisory stream keeps an advisory, so a dead letter written for an
//! advisory the watch can still be handed is still there to find. With
//! `[events] max_age` set below a day, a dead letter can age out of the
//! events stream first, and a redelivery after that writes it again.
//!
//! The broker's own duplicate window would not do: it keeps two minutes,
//! and a redelivery after a restart comes whenever the daemon does.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_nats::jetstream::message::StreamMessage;
use async_nats::jetstream::stream::{RawMessageError, RawMessageErrorKind};
use futures::StreamExt;
use tracing::info;

use super::{AdvisoryWatch, AdvisoryWatchError, CONSUMER_NAME, MaxDeliverAdvisory};
use crate::agent::AgentId;
use crate::bus::{BusError, DEFAULT_TRIGGER_MAX_AGE, STREAM_NAME};
use crate::dead_letter::DEAD_LETTER_ADVISORY_ID_KEY;
use crate::event_tail::RawTail;
use crate::events::{Event, EventPayload, FailureKind, subjects};

/// How long the read of the system sentinel's dead letters may take before
/// the watch gives up on it and decides the advisory again later.
pub(super) const SCAN_LIMIT: Duration = Duration::from_secs(10);

/// What the check for a dead letter already written under the system
/// sentinel found ([`AdvisoryWatch::written_before`]).
pub(super) enum Check {
    /// This advisory's dead letter is on the log: write none.
    Written,
    /// It is not: write it.
    NotWritten,
    /// The check could not be made, and the advisory is past the watch's
    /// limit: write it anyway, saying why the check failed.
    Unmade(String),
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

/// `event`, a dead letter filed under the system sentinel, annotated with
/// the broker's id for its advisory, when the advisory carries one.
pub(super) fn with_advisory_id(event: Event, advisory: &MaxDeliverAdvisory) -> Event {
    match advisory_id(advisory) {
        Some(id) => event.annotate(
            DEAD_LETTER_ADVISORY_ID_KEY,
            serde_json::Value::String(id.to_string()),
        ),
        None => event,
    }
}

impl AdvisoryWatch {
    /// Whether the dead letter `advisory` would file under the system
    /// sentinel is on the log already, by the advisory's id. An advisory
    /// with no id, or an empty one, cannot be told apart, and is written. A
    /// check that cannot be made is an error inside the watch's limit, and
    /// once `looked_long_enough` is [`Check::Unmade`].
    pub(super) async fn written_before(
        &self,
        advisory: &MaxDeliverAdvisory,
        looked_long_enough: bool,
    ) -> Result<Check, AdvisoryWatchError> {
        let Some(id) = advisory_id(advisory) else {
            return Ok(Check::NotWritten);
        };
        let since = advisory
            .emitted_at()
            .and_then(|at| at.timestamp_nanos_opt())
            .and_then(|nanos| async_nats::datetime::from_nanos(nanos.into()).ok());
        let unmade = |err: AdvisoryWatchError| {
            if looked_long_enough {
                Ok(Check::Unmade(err.to_string()))
            } else {
                Err(err)
            }
        };
        match tokio::time::timeout(self.scan_limit, self.written_for(id, since)).await {
            Ok(Ok(true)) => {
                info!(
                    consumer = CONSUMER_NAME,
                    stream_seq = advisory.stream_seq,
                    advisory_id = id,
                    "a dead letter filed under the system sentinel was already written for this \
                     advisory; skipping"
                );
                Ok(Check::Written)
            }
            Ok(Ok(false)) => Ok(Check::NotWritten),
            Ok(Err(err)) => unmade(err),
            Err(_) => unmade(AdvisoryWatchError::Log(format!(
                "reading the system's dead letters took longer than {:?}",
                self.scan_limit
            ))),
        }
    }

    /// Whether a dead letter the system sentinel holds carries
    /// `advisory_id`, reading every one stored since `since` (all of them
    /// with none) up to the last the subject held on entry.
    async fn written_for(
        &self,
        advisory_id: &str,
        since: Option<async_nats::datetime::DateTime>,
    ) -> Result<bool, AdvisoryWatchError> {
        let subject = subjects::agent_failed(AgentId::SYSTEM_STR);
        // Made without asking the broker, so a missing stream is the tip
        // read's own answer.
        let stream = self
            .bus
            .jetstream()
            .get_stream_no_info(STREAM_NAME)
            .await
            .map_err(|err| AdvisoryWatchError::Log(err.to_string()))?;
        let Some(last) = last_message(stream.get_last_raw_message_by_subject(&subject).await)?
        else {
            return Ok(false);
        };
        if since.is_some_and(|since| last.time < since) {
            return Ok(false);
        }
        let messages = self
            .bus
            .raw_events_since(&subject, since)
            .await
            .map_err(read_error)?;
        let id = serde_json::Value::String(advisory_id.to_string());
        find(messages, last.sequence, |event| {
            exhausted(event) && event.annotations.0.get(DEAD_LETTER_ADVISORY_ID_KEY) == Some(&id)
        })
        .await
    }
}

/// The last message on a subject, as the check takes the broker's answer:
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
        "the system's dead letters ended before the last seen on entry".to_string(),
    ))
}

/// A read of the log that failed, as the check reports one: "event log read
/// error", whatever part of the read failed.
fn read_error(err: BusError) -> AdvisoryWatchError {
    AdvisoryWatchError::Log(err.to_string())
}

/// Whether `event` is a dead letter: a `failed` event of a trigger that ran
/// out of deliveries.
fn exhausted(event: &Event) -> bool {
    matches!(
        &event.payload,
        EventPayload::Failed(p) if matches!(p.error_kind, FailureKind::TriggerExhausted)
    )
}
