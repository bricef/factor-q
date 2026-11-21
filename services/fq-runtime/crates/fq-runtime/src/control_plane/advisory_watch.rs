//! MAX_DELIVERIES advisory watch (#169): the durable backstop for
//! trigger exhaustion the dispatcher never observes, or could not record.
//!
//! The dispatcher's inline dead-letter path (#49) ACKs the final
//! delivery once its dead letter is published, and an ACK suppresses the
//! server's MAX_DELIVERIES advisory — probed empirically against a live
//! broker (2026-07-14; the NAK re-probed on the pinned broker for issue
//! 1062):
//!
//! - NAK with no delay on the final delivery → advisory fires at once
//!   while a pull is waiting on the consumer, else at its next pull;
//!   with a delay, even zero, the trigger consumer's backoff puts it
//!   eleven and a half minutes out, so a dead letter the dispatcher could
//!   not publish is handed here with a bare NAK (issue 1062)
//! - ACK on the final delivery → **no advisory**
//! - never acked (crashed dispatcher; ack_wait lapse) → advisory fires
//! - bound lowered below an existing message's delivery count (the
//!   consumer-upgrade scenario) → advisory fires on the next
//!   redelivery evaluation
//!
//! So exactly the exhaustion the inline path misses produces an
//! advisory — and it can fire while no dispatcher is alive to hear it
//! (the final delivery's ack window is the retry ladder's last entry,
//! [`crate::bus::TRIGGER_RETRY_BACKOFF`], so up to twelve minutes after
//! a crash), which is why advisories are captured server-side into a
//! durable stream ([`crate::bus::ADVISORY_STREAM_NAME`]) rather than
//! subscribed live.
//!
//! **It decides by the records** (issue 967). The broker reports a
//! trigger exhausted whenever its last delivery's answer never reached
//! it, and that happens to a run that started too: the last delivery's
//! task died after its first WAL write, or its ack was lost. So before
//! it writes a dead letter the watch asks the dispatcher what is on
//! record for the trigger ([`ExhaustionCheck`]). With nothing on record
//! it dead-letters the trigger. With a start on record it writes none,
//! since requeuing it would run the work twice, and counts it. When it
//! cannot tell, because a copy of the trigger is still handling its claim,
//! the records cannot be read, or the claim changed between two reads of
//! it with no start found, it holds its verdict, NAKing the advisory to
//! decide again. A start it has found is never held. The hold ends once
//! the advisory is [`HELD_VERDICT_LIMIT`] old, counted from when the
//! broker emitted it, and the watch has looked at it
//! [`HELD_VERDICT_MIN_LOOKS`] times: a last look then counts any start it
//! finds, and otherwise the watch writes the dead letter, marked as one
//! whose start could not be checked and naming the run the claim names.
//! An advisory from an earlier incarnation of the trigger stream is
//! dead-lettered without looking at the records, which key on this
//! incarnation, or at the message this stream holds at its sequence, which
//! is another trigger (issue 1054): the dead letter names no trigger, as
//! for one whose message aged out of the stream, and carries why
//! ([`crate::dead_letter::DEAD_LETTER_UNATTRIBUTED_KEY`]). The incarnation is
//! decided first, from the advisory's timestamp against the creation of
//! the stream this daemon connected to, and one with no timestamp the
//! watch can read is taken as this incarnation's.
//!
//! **A trigger whose message cannot be read is held too** (issue 1066).
//! A message the stream no longer holds names nothing, ever, and the dead
//! letter is written at once without a name. A read that fails for any
//! other reason may succeed later, so the verdict is held under the same
//! limit, and past it the dead letter names no trigger and says the
//! message could not be read (`naming`).
//!
//! Delivery semantics: at-least-once end to end, like every producer
//! on this bus (the archive sweeper republishes too). A watch crash
//! between emit and ack can duplicate one event; the shared
//! `trigger_stream_seq` and `trigger_stream_epoch` annotations make
//! duplicates reconcilable, and a
//! check before the write suppresses the duplicate, whichever form came
//! first (`surfaced`, issue 1066): what this watch remembers writing since
//! it started; for a dead letter that names its trigger, a read of the
//! agent's `failed` events since the trigger's message was published, for
//! one at its sequence that records this stream's incarnation, the
//! dispatcher's or the watch's own; for one filed under the system
//! sentinel, a read of every agent's since the advisory, for one carrying
//! the advisory's id, which every dead letter the watch writes carries.
//! Where a check cannot be certain or cannot be made, the dead letter is
//! written: a second one, never none. What that leaves open is in
//! `surfaced`.

use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::bus::{
    ADVISORY_STREAM_NAME, BusError, ConsumerRedeliveryPolicy, EventBus, RedeliveryLog,
    TRIGGER_MAX_DELIVER, TRIGGER_STREAM_NAME,
};
use crate::control_plane::dispatcher::{Exhausted, ExhaustionCheck, TriggerDispatcher};
use crate::dead_letter::{
    DEAD_LETTER_PAYLOAD_KEY, DEAD_LETTER_SOURCE_KEY, DEAD_LETTER_START_UNCHECKED_KEY,
    DEAD_LETTER_STREAM_SEQ_KEY, DEAD_LETTER_SUBJECT_KEY, DEAD_LETTER_TRIGGER_ID_KEY,
    StartUnchecked, with_stream_epoch,
};
use crate::events::{
    Event, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
};

mod consumed;
mod naming;
mod surfaced;

use consumed::on_a_consumed_claim;
use naming::Named;
use surfaced::{Check, NamedTrigger, with_advisory_id, with_duplicate_check};

/// Name of the durable JetStream consumer the advisory watch creates.
pub const CONSUMER_NAME: &str = "fq-advisory-watch";

/// How long the watch holds its verdict on an exhausted trigger before it
/// writes the dead letter anyway, marked as one whose start could not be
/// checked ([`StartUnchecked`]), measured from when the broker emitted the
/// advisory. Issue 967: a held verdict must end on purpose.
///
/// One hour, as a trade. Held longer, a trigger that may never run is kept
/// from the operator longer; given up sooner, more of these dead letters
/// are false. Nothing makes a held verdict resolve within the hour: the
/// retry ladder bounds the trigger's redeliveries, not how long a copy
/// still handling its claim may take to start, and such a copy can start
/// its run after the limit. That is why the dead letter is marked. The
/// bound above the hour is the advisory stream's own: it keeps an advisory
/// for 24 hours ([`crate::bus::DEFAULT_TRIGGER_MAX_AGE`]), and a held
/// advisory that ages out is gone with no dead letter and no record, which
/// an hour ends far inside.
///
/// The trigger's own message is kept for the same 24 hours from its
/// publish, which came before the advisory, so it ages out first. A dead
/// letter written at the limit carries the trigger's subject, payload and
/// id only if the deliveries ran out less than about 23 hours after the
/// publish (holds and a daemon that was down can make it later). Otherwise
/// the message is gone and the dead letter is written without them,
/// attributed to the system, a form `fq dead-letters requeue` refuses.
///
/// The same limit ends a verdict held because the trigger's message cannot
/// be read for a reason other than its absence (issue 1066): past it, the
/// dead letter is written in that form, saying the message could not be
/// read.
///
/// Measured from the advisory's own timestamp, so a restart does not reset
/// it, against this daemon's clock: a daemon clock ahead of the broker's by
/// some amount ends the hold that much early, one behind ends it that much
/// late, and an hour absorbs any ordinary difference. An advisory with no
/// timestamp the watch can read is held only for its first
/// [`HELD_VERDICT_MIN_LOOKS`] looks: its age cannot be told, so it could
/// never be said to have run out, and it is dead-lettered unchecked after
/// them rather than held until it ages out of its stream.
pub const HELD_VERDICT_LIMIT: Duration = Duration::from_secs(60 * 60);

/// How many times the watch looks at an exhausted trigger before it will
/// write a dead letter it could not check, whatever the advisory's age:
/// the advisory's delivery count to the watch. An advisory first seen past
/// [`HELD_VERDICT_LIMIT`], because the daemon was down for longer, would
/// otherwise get one look, and a reason that clears in a moment (a claim
/// that changed between two reads, a store busy for a second) would become
/// an unchecked dead letter. Three looks take about three seconds under the
/// bus's escalating NAK delay (one second, then two).
pub const HELD_VERDICT_MIN_LOOKS: u64 = 3;

/// The fields this watch consumes from the server's
/// `io.nats.jetstream.advisory.v1.max_deliver` payload; unknown
/// fields are ignored.
#[derive(Debug, Deserialize)]
struct MaxDeliverAdvisory {
    stream: String,
    consumer: String,
    stream_seq: u64,
    deliveries: i64,
    /// When the server emitted it, RFC 3339.
    #[serde(default)]
    timestamp: Option<String>,
    /// The server's id for this advisory, the same on every delivery of
    /// it: written on every dead letter the watch writes, it is how a
    /// redelivered advisory about to be filed under the system sentinel
    /// finds the dead letter the watch already wrote for it, in either form
    /// (`surfaced`).
    #[serde(default)]
    id: Option<String>,
}

impl MaxDeliverAdvisory {
    /// When the advisory was emitted, if that was before the incarnation of
    /// the trigger stream created at `stream_epoch` (nanoseconds): an
    /// advisory from an earlier incarnation. `None` for one emitted for that
    /// incarnation or a later one, and for one without a timestamp the
    /// watch can read, which is taken as this incarnation's: the server
    /// always stamps it.
    fn emitted_before(&self, stream_epoch: i64) -> Option<chrono::DateTime<chrono::Utc>> {
        self.emitted_at().filter(|at| {
            at.timestamp_nanos_opt()
                .is_some_and(|nanos| nanos < stream_epoch)
        })
    }

    /// When the server emitted it, if the watch can read its timestamp.
    fn emitted_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.timestamp
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&chrono::Utc))
    }

    /// Whether the advisory was emitted less than `limit` before `now`.
    /// One with no timestamp the watch can read is not: its age cannot be
    /// told. One stamped after `now` (a broker clock ahead of this
    /// daemon's) is.
    fn emitted_within(&self, limit: Duration, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.emitted_at()
            .is_some_and(|at| (now - at).to_std().unwrap_or_default() < limit)
    }

    /// Whether the watch has looked at this advisory long enough to write a
    /// dead letter it could not check: on its
    /// [`HELD_VERDICT_MIN_LOOKS`]th delivery or later, and emitted `limit`
    /// or more before `now`, or at a time the watch cannot read.
    fn looked_long_enough(
        &self,
        delivered: u64,
        limit: Duration,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        delivered >= HELD_VERDICT_MIN_LOOKS && !self.emitted_within(limit, now)
    }
}

/// Errors that prevent the watch from starting or progressing.
#[derive(Debug, thiserror::Error)]
pub enum AdvisoryWatchError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("advisory stream error: {0}")]
    Stream(String),

    /// The event log could not be read to tell whether a dead letter was
    /// written already.
    #[error("event log read error: {0}")]
    Log(String),

    /// The trigger stream could not be asked which incarnation it is, to
    /// tell whether a dead letter matched without a trigger id is this
    /// trigger's.
    #[error("trigger stream read error: {0}")]
    TriggerStream(String),
}

/// What handling one advisory did — returned for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisoryOutcome {
    /// A dead-letter event was emitted.
    Emitted,
    /// This exhaustion's dead letter is on the log already: the
    /// dispatcher's, or one the watch wrote for this advisory before
    /// (`surfaced`).
    AlreadySurfaced,
    /// Unparseable or foreign advisory; consumed without effect.
    Skipped,
    /// The trigger's run has a start on record: no dead letter, and the
    /// claim is marked if it was this watch's to mark (issue 967).
    RunStarted,
    /// The trigger was consumed without its run starting: dealt with, so
    /// no dead letter (issue 1055).
    Consumed,
    /// The trigger's claim says it was consumed, and past the watch's
    /// limit the records that could contradict it still could not be read:
    /// no dead letter, since none is due either way (issue 1055).
    ConsumedUnchecked,
    /// The verdict is held, for the reason given: the advisory is NAKed
    /// and decided again when it comes back.
    Held(Hold),
    /// A dead-letter event was emitted without the trigger's start
    /// checked, and the event says so ([`StartUnchecked`]): its verdict
    /// could not be taken within [`HELD_VERDICT_LIMIT`] of the advisory
    /// (or, for an advisory whose age cannot be told, within
    /// [`HELD_VERDICT_MIN_LOOKS`] looks), and the last look found no start.
    Unchecked,
}

/// A verdict the watch holds, with what its line names: the trigger's
/// sequence and subject, why, and the invocation the claim names. The
/// subject is empty when the watch could not read the trigger's message,
/// gone or unreadable; the invocation is none when the records name none,
/// as on a hold on the unreadable message itself, which the watch takes
/// only once the records have nothing to hold on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub why: String,
    pub stream_seq: u64,
    pub subject: String,
    pub invocation_id: Option<String>,
}

/// Why the watch NAKs an advisory, for its line.
enum Nak<'a> {
    /// It holds the verdict.
    Held(&'a Hold),
    /// Handling the advisory failed.
    Failed(&'a str),
}

/// The advisory watch. Owns the bus; spawn as a tokio task via
/// [`Self::run`], alongside the dispatcher whose blind spots it
/// covers.
pub struct AdvisoryWatch {
    bus: EventBus,
    /// What the dispatcher has on record for a trigger, consulted before
    /// one is called exhausted. `None` only for a dispatcher that holds no
    /// claim store, a test's, and then every exhausted trigger is
    /// dead-lettered without looking.
    records: Option<ExhaustionCheck>,
    /// How long a held verdict is held ([`HELD_VERDICT_LIMIT`]).
    held_verdict_limit: Duration,
    /// How long the read of the log for a dead letter already written may
    /// take ([`surfaced::SCAN_LIMIT`]).
    scan_limit: Duration,
    /// The advisories this watch has written a dead letter for since it
    /// started ([`surfaced::WrittenHere`]).
    written_here: surfaced::WrittenHere,
}

impl AdvisoryWatch {
    /// The watch over `dispatcher`'s triggers, consulting its records
    /// ([`TriggerDispatcher::exhaustion_check`]) before it calls one
    /// exhausted (issue 967). It takes the dispatcher, not the records, so
    /// a daemon cannot build a watch that dead-letters a trigger whose run
    /// started without looking: there is no constructor without them
    /// outside the tests.
    pub fn new(bus: EventBus, dispatcher: &TriggerDispatcher) -> Self {
        let records = dispatcher.exhaustion_check();
        if records.is_none() {
            warn!(
                consumer = CONSUMER_NAME,
                "the dispatcher holds no claim store, so the dead-letter watch consults no \
                 records: every exhausted trigger is dead-lettered without a look at whether \
                 its run started"
            );
        }
        Self {
            bus,
            records,
            held_verdict_limit: HELD_VERDICT_LIMIT,
            scan_limit: surfaced::SCAN_LIMIT,
            written_here: surfaced::WrittenHere::default(),
        }
    }

    /// A watch with no dispatcher to consult, for the tests of what it
    /// does with an advisory before any records come into it.
    #[cfg(test)]
    pub(crate) fn without_records(bus: EventBus) -> Self {
        Self {
            bus,
            records: None,
            held_verdict_limit: HELD_VERDICT_LIMIT,
            scan_limit: surfaced::SCAN_LIMIT,
            written_here: surfaced::WrittenHere::default(),
        }
    }

    /// Hold verdicts for `limit` instead of [`HELD_VERDICT_LIMIT`], so a
    /// test can run an advisory past the limit without waiting an hour.
    #[cfg(test)]
    pub(crate) fn with_held_verdict_limit(mut self, limit: Duration) -> Self {
        self.held_verdict_limit = limit;
        self
    }

    /// Give the read for a dead letter already written `limit` instead of
    /// [`surfaced::SCAN_LIMIT`], so a test can run one out of time.
    #[cfg(test)]
    pub(crate) fn with_scan_limit(mut self, limit: Duration) -> Self {
        self.scan_limit = limit;
        self
    }

    /// Run the watch loop until `shutdown` fires.
    pub async fn run(self, mut shutdown: oneshot::Receiver<()>) -> Result<(), AdvisoryWatchError> {
        info!(
            consumer = CONSUMER_NAME,
            stream = ADVISORY_STREAM_NAME,
            "advisory watch starting"
        );
        let policy = self.bus.redelivery_policy();
        let mut redelivery_log = RedeliveryLog::new(policy);
        let consumer = self.bus.advisory_consumer(CONSUMER_NAME).await?;
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| AdvisoryWatchError::Stream(err.to_string()))?;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!(consumer = CONSUMER_NAME, "advisory watch received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => self.answer(&msg, policy, &mut redelivery_log).await,
                        Some(Err(err)) => {
                            warn!(consumer = CONSUMER_NAME, error = %err, "error reading advisory message");
                        }
                        None => {
                            warn!(consumer = CONSUMER_NAME, "advisory stream ended unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!(consumer = CONSUMER_NAME, "advisory watch stopped");
        Ok(())
    }

    /// Decide one delivered advisory and answer the broker: NAK a held
    /// verdict or a failure to decide again, ack anything else.
    async fn answer(
        &self,
        msg: &async_nats::jetstream::Message,
        policy: ConsumerRedeliveryPolicy,
        redelivery_log: &mut RedeliveryLog,
    ) {
        // A delivery count the broker does not give counts as enough looks:
        // read as the first, it would keep a verdict held on every delivery
        // until the advisory aged out of its stream, with no dead letter.
        let looks = delivered(msg).unwrap_or(HELD_VERDICT_MIN_LOOKS);
        match self.handle_advisory(&msg.payload, looks).await {
            Ok(AdvisoryOutcome::Held(hold)) => {
                nak_for_retry(msg, policy, redelivery_log, Nak::Held(&hold)).await;
            }
            Ok(outcome) => {
                debug!(consumer = CONSUMER_NAME, ?outcome, "advisory handled");
                if let Err(err) = msg.ack().await {
                    error!(consumer = CONSUMER_NAME, error = %err, "failed to ack advisory");
                }
            }
            Err(err) => {
                let err = err.to_string();
                nak_for_retry(msg, policy, redelivery_log, Nak::Failed(&err)).await;
            }
        }
    }

    /// Handle one captured advisory payload, on its `delivered`th delivery
    /// to the watch. `pub(crate)` so the broker-backed tests drive it
    /// directly.
    pub(crate) async fn handle_advisory(
        &self,
        payload: &[u8],
        delivered: u64,
    ) -> Result<AdvisoryOutcome, AdvisoryWatchError> {
        let advisory: MaxDeliverAdvisory = match serde_json::from_slice(payload) {
            Ok(a) => a,
            Err(err) => {
                warn!(consumer = CONSUMER_NAME, error = %err, "unparseable advisory; consuming");
                return Ok(AdvisoryOutcome::Skipped);
            }
        };
        // The capture subject is scoped to the trigger stream; be
        // defensive about a widened filter anyway.
        if advisory.stream != TRIGGER_STREAM_NAME {
            debug!(consumer = CONSUMER_NAME, stream = %advisory.stream,
                "advisory for a foreign stream; skipping");
            return Ok(AdvisoryOutcome::Skipped);
        }

        // The incarnation of the trigger stream the advisory was emitted
        // for, decided before anything is read from the stream: a sequence
        // from before the stream was last created is not this stream's.
        let earlier = advisory.emitted_before(self.bus.trigger_stream_epoch());
        let this_incarnation = earlier.is_none();
        // Looked at long enough, a verdict that still cannot be taken, on
        // the records or on a trigger whose message cannot be read, is
        // written as a dead letter, after a last look for a start on record.
        let looked_long_enough =
            advisory.looked_long_enough(delivered, self.held_verdict_limit, chrono::Utc::now());

        // Issue 967: a trigger whose run has a start on record is not
        // dead-lettered, and a verdict that cannot be taken yet is held.
        // The claims key on the stream's incarnation, which the advisory
        // does not carry: one emitted before the stream was last created
        // names a message of the old stream, and today's claim at that
        // sequence says nothing about it, so the claims are not consulted.
        // Past the limit the verdict is a last look, and one that still
        // cannot be taken marks the dead letter unchecked. The records key
        // on the incarnation and the advisory's sequence, so they are asked
        // first (issue 1066): before the trigger's message, which is needed
        // only to write a dead letter and is read before that only, best
        // effort, for the subject on the lines below; and before the check
        // for a dead letter already written, so their verdict decides.
        let mut unchecked = None;
        if let Some(records) = self.records.as_ref().filter(|_| this_incarnation) {
            let verdict = if looked_long_enough {
                records.last_look(advisory.stream_seq).await
            } else {
                records.verdict(advisory.stream_seq).await
            };
            match verdict {
                Exhausted::NoClaim | Exhausted::NothingOnRecord => {}
                Exhausted::Started {
                    invocation_id,
                    on_record,
                    mark,
                } => {
                    self.bus
                        .consumer_ledger()
                        .note_started_not_dead_lettered(CONSUMER_NAME);
                    let subject = self.subject_at(advisory.stream_seq).await;
                    warn!(consumer = CONSUMER_NAME, %subject,
                        stream_seq = advisory.stream_seq, deliveries = advisory.deliveries,
                        %invocation_id, run = %on_record, claim = %mark,
                        "not dead-lettering a trigger whose deliveries ran out: its run has a start \
                         on record, so a `trigger_exhausted` dead letter would be false and \
                         requeuing it would run the work twice; one written for it before the \
                         start was on record may still be on the log; `claim` says what became of its \
                         claim; if no task is driving the run, which is so when its last \
                         delivery's task died after its first WAL write, it stays in flight until \
                         this daemon next starts and its recovery resumes it, and shows meanwhile \
                         among `fq doctor`'s stuck executions");
                    return Ok(AdvisoryOutcome::RunStarted);
                }
                Exhausted::Consumed(consumed) => {
                    let subject = self.subject_at(advisory.stream_seq).await;
                    let (long, ledger) = (looked_long_enough, self.bus.consumer_ledger());
                    return Ok(on_a_consumed_claim(
                        &advisory, subject, consumed, long, ledger,
                    ));
                }
                Exhausted::Undecided { why, invocation_id } => {
                    if !looked_long_enough {
                        return Ok(AdvisoryOutcome::Held(Hold {
                            why,
                            stream_seq: advisory.stream_seq,
                            subject: self.subject_at(advisory.stream_seq).await,
                            invocation_id,
                        }));
                    }
                    unchecked = Some(StartUnchecked {
                        reason: why,
                        invocation_id,
                    });
                }
            }
        }

        let (message, unattributed) = match self
            .named_trigger(&advisory, earlier, looked_long_enough)
            .await
        {
            Ok(Named::Trigger(message)) => (Some(message), None),
            Ok(Named::Nothing(why)) => (None, Some(why)),
            Err(hold) => return Ok(AdvisoryOutcome::Held(hold)),
        };
        let named = message.as_ref().and_then(NamedTrigger::of);
        let (agent_id, trigger_subject, trigger_payload, trigger_id) = match message {
            Some(m) => (m.agent_id, m.subject, m.payload, m.trigger_id),
            None => (None, String::new(), serde_json::Value::Null, None),
        };
        // A copy of an advisory this watch has written a dead letter for since
        // it started is answered without reading the log (issue 1066).
        if self.written_here.wrote(&advisory, agent_id.is_some()) {
            return Ok(AdvisoryOutcome::AlreadySurfaced);
        }

        // One exhaustion, one dead letter, whichever form came first (issue
        // 1066); past the limit, a check that cannot be made does not hold the
        // dead letter back, and the dead letter says so.
        let duplicate_check = match self
            .written_before(&advisory, named, looked_long_enough)
            .await?
        {
            Check::Written => return Ok(AdvisoryOutcome::AlreadySurfaced),
            Check::NotWritten => None,
            Check::Unmade(why) => Some(format!("could not be made: {why}")),
        };

        let payload = EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::TriggerExhausted,
            error_message: format!(
                "trigger exhausted after {} deliveries (limit {TRIGGER_MAX_DELIVER}) — \
                 surfaced from the MAX_DELIVERIES advisory (consumer {})",
                advisory.deliveries, advisory.consumer
            ),
            phase: FailurePhase::Setup,
            partial_totals: InvocationTotals::default(),
            detail: None,
        });
        let event = match &agent_id {
            Some(agent) => Event::new(agent.clone(), Uuid::now_v7(), payload),
            // No trigger named (it aged out of the stream, could not be read
            // by the limit, or the advisory is an earlier incarnation's): the
            // agent is unknowable, but the exhaustion still counts.
            None => Event::system(Uuid::now_v7(), payload),
        }
        .annotate(
            DEAD_LETTER_SUBJECT_KEY,
            serde_json::Value::String(trigger_subject.clone()),
        )
        .annotate(DEAD_LETTER_PAYLOAD_KEY, trigger_payload)
        .annotate(
            DEAD_LETTER_STREAM_SEQ_KEY,
            serde_json::json!(advisory.stream_seq),
        )
        .annotate(
            DEAD_LETTER_SOURCE_KEY,
            serde_json::Value::String("advisory".to_string()),
        );
        let event = match trigger_id {
            Some(id) => event.annotate(
                DEAD_LETTER_TRIGGER_ID_KEY,
                serde_json::Value::String(id.to_string()),
            ),
            None => event,
        };
        let event = match &unattributed {
            Some(why) => why.annotate(event),
            None => event,
        };
        // The incarnation the trigger was on, when the advisory is this one's.
        let epoch = this_incarnation.then_some(self.bus.trigger_stream_epoch());
        let event = with_stream_epoch(event, epoch);
        let event = with_duplicate_check(with_advisory_id(event, &advisory), &duplicate_check);
        let why = unattributed.as_ref().map(naming::Unattributed::reason);
        let event = match &unchecked {
            Some(unchecked) => event.annotate(
                DEAD_LETTER_START_UNCHECKED_KEY,
                serde_json::to_value(unchecked).unwrap_or_default(),
            ),
            None => event,
        };
        self.bus.publish(&event).await?;
        self.written_here.note(&advisory, agent_id.is_some());
        if let Some(unchecked) = unchecked {
            let invocation_id = unchecked.invocation_id.as_deref().unwrap_or("none");
            if advisory.emitted_at().is_some() {
                error!(consumer = CONSUMER_NAME, agent_id = %event.envelope.agent_id,
                    stream_seq = advisory.stream_seq, subject = %trigger_subject,
                    reason = %unchecked.reason, %invocation_id,
                    unattributed = why, duplicate_check,
                    limit_secs = self.held_verdict_limit.as_secs(),
                    "dead-lettered an exhausted trigger without checking whether its run \
                     started: its verdict could not be taken within the watch's limit and its \
                     last look found no start, so the dead letter is marked as unchecked and may \
                     be false; look at the invocation before running its work again");
            } else {
                error!(consumer = CONSUMER_NAME, agent_id = %event.envelope.agent_id,
                    stream_seq = advisory.stream_seq, subject = %trigger_subject,
                    reason = %unchecked.reason, %invocation_id, looks = delivered,
                    unattributed = why, duplicate_check,
                    "dead-lettered an exhausted trigger without checking whether its run \
                     started: its advisory carries no timestamp the watch can read, so its age \
                     cannot be told and it was held only for the watch's first looks, and the \
                     last look found no start; the dead letter is marked as unchecked and may be \
                     false; look at the invocation before running its work again");
            }
            return Ok(AdvisoryOutcome::Unchecked);
        }
        // A dead letter that names no trigger says why on this line too, so
        // a log read at error alone does not take an earlier stream's
        // sequence for this one's; the field is absent when the dead letter
        // names its trigger.
        error!(
            consumer = CONSUMER_NAME,
            agent_id = %event.envelope.agent_id,
            stream_seq = advisory.stream_seq,
            deliveries = advisory.deliveries,
            unattributed = why,
            duplicate_check,
            "trigger exhausted with nothing of its run on record, or none consulted; emitted a \
             dead-letter event from its MAX_DELIVERIES advisory"
        );
        Ok(AdvisoryOutcome::Emitted)
    }
}

/// NAK an advisory to come back after the bus's escalating delay. The
/// advisory consumer redelivers without limit (an advisory dropped to a
/// delivery bound is an exhausted trigger with no record of it), so the
/// delay is what keeps a persistent fault, or a held verdict, from becoming
/// a hot loop (#549). The line goes under the loop's [`RedeliveryLog`]:
/// the first, then each step of the escalation, then one per interval,
/// shared by every advisory the loop NAKs. A held verdict also gets a line
/// of its own on the advisory's first delivery, so every held trigger is
/// named in the log at least once, by sequence, and by subject when its
/// message can be read, however many are held together.
async fn nak_for_retry(
    msg: &async_nats::jetstream::Message,
    policy: ConsumerRedeliveryPolicy,
    redelivery_log: &mut RedeliveryLog,
    why: Nak<'_>,
) {
    // The delay escalates from the first step when the count is not given.
    let delivered = delivered(msg).unwrap_or(1);
    let delay = policy.nak_delay(delivered);
    let admitted = redelivery_log.admit(delivered, std::time::Instant::now());
    match why {
        Nak::Held(hold) if admitted || delivered == 1 => {
            warn!(consumer = CONSUMER_NAME, stream_seq = hold.stream_seq,
                subject = %hold.subject, reason = %hold.why,
                invocation_id = %hold.invocation_id.as_deref().unwrap_or("none"), delivered,
                retry_in_ms = delay.as_millis() as u64,
                "holding the dead-letter verdict on an exhausted trigger; nak to decide again");
        }
        Nak::Failed(err) if admitted => {
            warn!(consumer = CONSUMER_NAME, error = %err, delivered,
                retry_in_ms = delay.as_millis() as u64, "advisory handling failed; nak for retry");
        }
        Nak::Held(_) | Nak::Failed(_) => {}
    }
    if let Err(nak_err) = msg
        .ack_with(async_nats::jetstream::AckKind::Nak(Some(delay)))
        .await
    {
        error!(consumer = CONSUMER_NAME, error = %nak_err, "failed to nak advisory");
    }
}

/// How many times the broker has delivered `msg` to the watch, this
/// delivery included, if it says.
fn delivered(msg: &async_nats::jetstream::Message) -> Option<u64> {
    msg.info()
        .ok()
        .and_then(|info| u64::try_from(info.delivered).ok())
}

#[cfg(test)]
mod once_tests;

#[cfg(test)]
mod named_once_tests;

#[cfg(test)]
mod read_tests;

#[cfg(test)]
pub(crate) mod test_kit;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::bus::STREAM_NAME;
    use crate::events::subjects;
    use serde_json::json;
    use std::time::Duration;

    /// Poll the advisory capture stream for the MAX_DELIVERIES
    /// advisory of one specific consumer (names are unique per test,
    /// so the subject is precise).
    async fn wait_for_captured_advisory(bus: &EventBus, consumer_name: &str) -> Vec<u8> {
        let subject = format!(
            "$JS.EVENT.ADVISORY.CONSUMER.MAX_DELIVERIES.{TRIGGER_STREAM_NAME}.{consumer_name}"
        );
        let stream = bus
            .jetstream()
            .get_stream(ADVISORY_STREAM_NAME)
            .await
            .expect("advisory stream");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(raw) = stream.get_last_raw_message_by_subject(&subject).await {
                return raw.payload.to_vec();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "advisory for {consumer_name} never captured"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// #169 end-to-end against a live broker: a trigger exhausted with
    /// no live dispatcher (never acked — the crash shape) produces a
    /// captured advisory; handling it emits the dead-letter event with
    /// the same kind and annotations as the inline path; handling the
    /// same advisory again (redelivery / inline overlap) is suppressed
    /// by the stream-sequence dedup.
    #[tokio::test]
    async fn exhausted_unacked_trigger_is_surfaced_from_the_advisory() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        use futures::StreamExt;
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let suffix = Uuid::now_v7().simple().to_string();
        let agent_id_str = format!("advisory-e2e-{suffix}");
        let consumer_name = format!("advisory-e2e-{suffix}");
        let trigger_subject = crate::events::subjects::trigger(&agent_id_str);

        let published = bus
            .publish_trigger(
                &AgentId::new(&agent_id_str).unwrap(),
                &json!({"input": "poison"}),
            )
            .await
            .expect("publish trigger");

        // A scratch consumer shaped like the probe's crash case: tight
        // bound, short ack_wait, no backoff — exhaustion in ~2s. The
        // advisory fires for any consumer of the trigger stream.
        let stream = bus
            .jetstream()
            .get_stream(TRIGGER_STREAM_NAME)
            .await
            .expect("trigger stream");
        let consumer = stream
            .create_consumer(async_nats::jetstream::consumer::pull::Config {
                durable_name: Some(consumer_name.clone()),
                ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                filter_subject: trigger_subject.clone(),
                max_deliver: 2,
                ack_wait: Duration::from_secs(1),
                ..Default::default()
            })
            .await
            .expect("scratch consumer");
        let mut messages = consumer.messages().await.expect("messages");
        for i in 0..2 {
            let _unacked = tokio::time::timeout(Duration::from_secs(5), messages.next())
                .await
                .unwrap_or_else(|_| panic!("delivery {i}"))
                .unwrap()
                .unwrap();
            // never acked — the dispatcher is "dead"
        }

        let advisory = wait_for_captured_advisory(&bus, &consumer_name).await;
        let watch = AdvisoryWatch::without_records(bus.clone());
        assert_eq!(
            watch.handle_advisory(&advisory, 1).await.unwrap(),
            AdvisoryOutcome::Emitted
        );

        // The event is on the bus, kind + annotations intact.
        let events_stream = bus
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .expect("event stream");
        let raw = events_stream
            .get_last_raw_message_by_subject(&subjects::agent_failed(&agent_id_str))
            .await
            .expect("dead-letter event");
        let event: Event = serde_json::from_slice(&raw.payload).expect("event parses");
        match &event.payload {
            EventPayload::Failed(p) => {
                assert!(matches!(p.error_kind, FailureKind::TriggerExhausted));
                assert!(
                    p.error_message.contains("MAX_DELIVERIES advisory"),
                    "got: {}",
                    p.error_message
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_SUBJECT_KEY),
            Some(&json!(trigger_subject))
        );
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_PAYLOAD_KEY),
            Some(&json!({"input": "poison"}))
        );
        assert!(
            event
                .annotations
                .0
                .get(DEAD_LETTER_STREAM_SEQ_KEY)
                .is_some_and(|seq| seq.as_u64().is_some())
        );
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_SOURCE_KEY),
            Some(&json!("advisory"))
        );
        // Read off the still-present trigger's header, not invented
        // here: this watch never dispatched the trigger, so the only
        // name it can record is the one already on the message.
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY),
            Some(&json!(published.id.to_string())),
            "the dead letter names the trigger the publisher named"
        );

        // Redelivered / overlapping advisory: suppressed by the seq
        // dedup against the event just emitted, read off the log by a watch
        // that remembers nothing it wrote, as after a restart.
        assert_eq!(
            AdvisoryWatch::without_records(bus.clone())
                .handle_advisory(&advisory, 1)
                .await
                .unwrap(),
            AdvisoryOutcome::AlreadySurfaced
        );
    }

    /// A trigger that aged out of the stream before its advisory was
    /// processed still counts — attributed to the system sentinel.
    #[tokio::test]
    async fn aged_out_trigger_still_surfaces_under_the_system_sentinel() {
        let server = crate::test_support::nats::test_nats();
        let url = server.url().to_string();
        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let watch = AdvisoryWatch::without_records(bus.clone());
        // No message exists at this sequence.
        let advisory = format!(
            r#"{{"type":"io.nats.jetstream.advisory.v1.max_deliver","stream":"{TRIGGER_STREAM_NAME}","consumer":"gone","stream_seq":18446744073709551,"deliveries":5}}"#
        );
        assert_eq!(
            watch.handle_advisory(advisory.as_bytes(), 1).await.unwrap(),
            AdvisoryOutcome::Emitted
        );
        let events_stream = bus
            .jetstream()
            .get_stream(STREAM_NAME)
            .await
            .expect("event stream");
        let raw = events_stream
            .get_last_raw_message_by_subject(&subjects::agent_failed("system"))
            .await
            .expect("sentinel dead-letter event");
        let event: Event = serde_json::from_slice(&raw.payload).expect("event parses");
        assert!(matches!(
            &event.payload,
            EventPayload::Failed(p) if matches!(p.error_kind, FailureKind::TriggerExhausted)
        ));
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_STREAM_SEQ_KEY),
            Some(&json!(18446744073709551u64))
        );
        assert_eq!(
            event.annotations.0.get(DEAD_LETTER_TRIGGER_ID_KEY),
            None,
            "with the trigger gone there is no name to record, and this \
             watch does not invent one"
        );
        assert_eq!(
            event.annotations.0.get("duplicate_check"),
            Some(&json!(
                "could not be made: its advisory carries no id, so a dead letter already \
                 written for it cannot be told from another's"
            )),
            "with no id to look for, the check cannot be made, and the dead letter says so"
        );
        let why = crate::dead_letter::from_event(&event)
            .and_then(|dead| dead.unattributed)
            .expect("it says why it names no trigger");
        assert!(
            why.contains("for an advisory that carries no timestamp the watch can read"),
            "{why}"
        );
    }

    /// The exact payload shape the server emits (captured from a live
    /// broker probe, 2026-07-14) parses into the fields we consume.
    #[test]
    fn parses_the_servers_advisory_payload() {
        let payload = r#"{"type":"io.nats.jetstream.advisory.v1.max_deliver","id":"g1ZC7klaOZ1E53SLlZN5hk","timestamp":"2026-07-14T16:18:43.605745113Z","stream":"fq-triggers","consumer":"fq-dispatcher","stream_seq":42,"deliveries":5}"#;
        let advisory: MaxDeliverAdvisory = serde_json::from_slice(payload.as_bytes()).unwrap();
        assert_eq!(advisory.stream, "fq-triggers");
        assert_eq!(advisory.consumer, "fq-dispatcher");
        assert_eq!(advisory.stream_seq, 42);
        assert_eq!(advisory.deliveries, 5);
        assert_eq!(
            advisory.timestamp.as_deref(),
            Some("2026-07-14T16:18:43.605745113Z")
        );
        assert_eq!(advisory.id.as_deref(), Some("g1ZC7klaOZ1E53SLlZN5hk"));
    }

    /// The incarnation an advisory belongs to is read off its timestamp:
    /// one emitted before the stream's creation is an earlier
    /// incarnation's, and gives when it was emitted; one at or after it is
    /// this one's, and so is one with no readable timestamp, or one too far
    /// from 1970 to count in nanoseconds.
    #[test]
    fn an_advisory_belongs_to_the_incarnation_its_timestamp_falls_in() {
        let at = |timestamp: Option<&str>| MaxDeliverAdvisory {
            stream: TRIGGER_STREAM_NAME.to_string(),
            consumer: "fq-dispatcher".to_string(),
            stream_seq: 1,
            deliveries: 5,
            timestamp: timestamp.map(str::to_string),
            id: None,
        };
        let time = |at: &str| {
            chrono::DateTime::parse_from_rfc3339(at)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let created = time("2026-07-14T16:00:00Z").timestamp_nanos_opt().unwrap();
        assert_eq!(
            at(Some("2026-07-14T16:18:43Z")).emitted_before(created),
            None
        );
        assert_eq!(
            at(Some("2026-07-14T16:00:00Z")).emitted_before(created),
            None
        );
        assert_eq!(
            at(Some("2026-07-14T15:59:59Z")).emitted_before(created),
            Some(time("2026-07-14T15:59:59Z"))
        );
        assert_eq!(at(None).emitted_before(created), None);
        assert_eq!(at(Some("not a time")).emitted_before(created), None);
        assert_eq!(
            at(Some("0001-01-01T00:00:00Z")).emitted_before(created),
            None
        );
    }

    /// The reason a dead letter written past the limit with the trigger's
    /// message unreadable gives says what the last read answered, and that
    /// the message may still be on the stream only when the answer leaves
    /// that open: not when it was that the trigger stream is missing.
    #[test]
    fn an_unreadable_messages_dead_letter_says_what_the_read_answered() {
        let open = naming::unreadable_at_the_limit("request timed out", false);
        assert_eq!(
            open,
            "the trigger's message could not be read off the trigger stream by the watch's \
             limit (the last read answered: request timed out), so the trigger is not named, \
             though its message may still be on the stream"
        );
        let missing = naming::unreadable_at_the_limit("stream not found", true);
        assert_eq!(
            missing,
            "the trigger's message could not be read by the watch's limit: the trigger stream \
             was missing (the last read answered: stream not found), so the trigger is not named"
        );
    }

    /// The reason a dead letter whose message the broker says is gone
    /// names no trigger gives what the broker answered and when the watch
    /// last read the stream, then what the watch makes of it, with the
    /// advisory's time when it can read one.
    #[test]
    fn a_gone_messages_dead_letter_says_what_the_broker_answered() {
        let time = |at: &str| {
            chrono::DateTime::parse_from_rfc3339(at)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        assert_eq!(
            naming::gone_reason(
                time("2026-11-18T09:30:00.125Z"),
                Some(time("2026-11-18T09:29:58Z"))
            ),
            "the broker answered that the trigger stream held no message at this sequence when \
             the watch last read it, at 2026-11-18T09:30:00.125Z, for an advisory emitted at \
             2026-11-18T09:29:58Z: the stream keeps a message 24h after its publish, so it most \
             likely aged out, or was deleted sooner; the broker answers the same for a message \
             it cannot load"
        );
        assert_eq!(
            naming::gone_reason(time("2026-11-18T09:30:00Z"), None),
            "the broker answered that the trigger stream held no message at this sequence when \
             the watch last read it, at 2026-11-18T09:30:00Z, for an advisory that carries no \
             timestamp the watch can read: the stream keeps a message 24h after its publish, so \
             it most likely aged out, or was deleted sooner; the broker answers the same for a \
             message it cannot load"
        );
    }

    /// The reason an earlier incarnation's dead letter names no trigger
    /// gives both times, so an operator can tell when the stream was
    /// recreated, and says the sequence is not this stream's.
    #[test]
    fn an_earlier_incarnations_dead_letter_says_when_the_stream_was_created() {
        let time = |at: &str| {
            chrono::DateTime::parse_from_rfc3339(at)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        let created = time("2026-11-17T08:00:00Z").timestamp_nanos_opt().unwrap();
        assert_eq!(
            naming::unattributed_reason(time("2026-11-16T10:00:00.250Z"), created),
            "its advisory came from an earlier incarnation of the trigger stream (emitted \
             2026-11-16T10:00:00.250Z, before the stream was created at 2026-11-17T08:00:00Z), \
             so its sequence is that stream's, whose messages went with it"
        );
    }

    /// The age that ends a hold is read off the same timestamp, against the
    /// daemon's clock. An advisory stamped after `now` (a broker clock
    /// ahead of the daemon's) is within the limit; one stamped a moment
    /// less than the limit ago is; one stamped exactly the limit ago, or
    /// earlier, is not; and one whose stamp is missing or unreadable is
    /// not, since its age cannot be told. The watch writes an unchecked
    /// dead letter only once it is past the limit and has looked at the
    /// advisory [`HELD_VERDICT_MIN_LOOKS`] times.
    #[test]
    fn an_advisory_is_within_the_limit_only_while_its_readable_stamp_is_younger() {
        let at = |timestamp: Option<&str>| MaxDeliverAdvisory {
            stream: TRIGGER_STREAM_NAME.to_string(),
            consumer: "fq-dispatcher".to_string(),
            stream_seq: 1,
            deliveries: 5,
            timestamp: timestamp.map(str::to_string),
            id: None,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-17T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let hour = Duration::from_secs(60 * 60);
        assert!(at(Some("2026-11-17T12:05:00Z")).emitted_within(hour, now));
        assert!(at(Some("2026-11-17T11:00:00.001Z")).emitted_within(hour, now));
        assert!(!at(Some("2026-11-17T11:00:00Z")).emitted_within(hour, now));
        assert!(!at(Some("2026-11-17T09:00:00Z")).emitted_within(hour, now));
        assert!(!at(Some("not a time")).emitted_within(hour, now));
        assert!(!at(None).emitted_within(hour, now));

        let old = at(Some("2026-11-17T09:00:00Z"));
        assert!(!old.looked_long_enough(HELD_VERDICT_MIN_LOOKS - 1, hour, now));
        assert!(old.looked_long_enough(HELD_VERDICT_MIN_LOOKS, hour, now));
        assert!(!at(None).looked_long_enough(HELD_VERDICT_MIN_LOOKS - 1, hour, now));
        assert!(at(None).looked_long_enough(HELD_VERDICT_MIN_LOOKS, hour, now));
        assert!(!at(Some("2026-11-17T12:05:00Z")).looked_long_enough(100, hour, now));
    }
}
