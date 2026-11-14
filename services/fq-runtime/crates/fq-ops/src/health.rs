//! Stream and consumer health, as the operator surface declares them.
//!
//! These are the shapes alone. Producing them means talking to
//! JetStream, which is the daemon's job and stays in `fq-runtime`
//! alongside the probe — a reader that only renders health links this
//! crate and none of that.

use serde::{Deserialize, Serialize};

/// Health of one JetStream stream plus its primary durable consumer.
///
/// Externally tagged (serde's default). The edge does not force the
/// choice: its token preamble is raw length-prefixed bytes and the
/// tarpc session that follows is length-delimited JSON end to end
/// (`fq_edge::server`, `fq_edge::client`), so an internally-tagged
/// enum would encode there too. What pins the representation is that
/// it is *declared* — the schema `describe` publishes for this type
/// spells the tagging out, so changing it is a wire break, not a
/// stylistic edit.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamHealth {
    /// The stream (or its info) could not be fetched; `error` carries
    /// the reason verbatim.
    Unavailable { stream: String, error: String },
    Available {
        stream: String,
        messages: u64,
        bytes: u64,
        first_seq: u64,
        last_seq: u64,
        /// Every durable this daemon expects on the stream, in a fixed
        /// order. It was one — the stream's "primary" consumer — which
        /// meant `control.status` reported two of the runtime's six
        /// durables and a wedged coordination or summary consumer was
        /// invisible to every health surface (review finding B4).
        /// A consumer the daemon does not expect is not listed: the
        /// summariser only exists when `[summary]` names a model, and
        /// reporting it missing otherwise would be a permanent false
        /// red.
        consumers: Vec<ConsumerHealth>,
    },
}

impl StreamHealth {
    /// The stream name, whichever state it is in.
    pub fn stream(&self) -> &str {
        match self {
            StreamHealth::Unavailable { stream, .. } => stream,
            StreamHealth::Available { stream, .. } => stream,
        }
    }

    /// Every consumer on this stream that an operator should act on.
    /// An unavailable stream reports none of its own — the stream line
    /// is the finding there.
    pub fn faulty_consumers(&self) -> impl Iterator<Item = &ConsumerHealth> {
        match self {
            StreamHealth::Unavailable { .. } => [].iter(),
            StreamHealth::Available { consumers, .. } => consumers.as_slice().iter(),
        }
        .filter(|c| c.is_fault())
    }
}

/// Health of one durable consumer on a stream. Externally tagged, for
/// the same encoding reason as [`StreamHealth`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerHealth {
    /// The durable does not exist yet (no daemon has initialised it).
    Missing { name: String },
    /// The durable exists but its info could not be fetched.
    Error { name: String, error: String },
    Active {
        name: String,
        /// Stream sequence the consumer has been delivered up to —
        /// where it sits on the stream, not how far behind it is. The
        /// distance from here to the stream head is not a backlog: a
        /// consumer filtered to a few subjects is never offered the
        /// messages in between, so that distance grows with every
        /// message anybody publishes and never comes back down.
        delivered: u64,
        /// Matching messages delivered and not yet acked: work this
        /// consumer has in hand right now.
        ack_pending: u64,
        /// Matching messages JetStream still has to offer this
        /// consumer — the backlog, as the broker itself counts it,
        /// after the consumer's subject filter has been applied.
        num_pending: u64,
        /// Outstanding redeliveries — messages delivered more than
        /// once and not yet acked. The retry-pressure signal: a
        /// non-zero value means work is being NAK'd or timing out and
        /// walking toward the consumer's delivery bound, past which a
        /// trigger is dead-lettered rather than retried again.
        num_redelivered: u64,
        /// Deliveries this consumer has made past its acked floor,
        /// beyond the first delivery of each still-pending message —
        /// the "delivered count climbing while the watermark is frozen"
        /// that a NAK loop looks like from outside. Zero on a healthy
        /// consumer, however far behind it is.
        redeliveries: u64,
        /// True once `redeliveries` has passed the daemon's
        /// `[bus] stuck_after_redeliveries`: the consumer is retrying
        /// one message rather than making progress, and the fault
        /// behind it has outlasted the whole escalation. The verdict
        /// is computed daemon-side because the threshold is the
        /// daemon's configuration — a reader that judged for itself
        /// would be quoting a number it does not have.
        stuck: bool,
        /// Messages this consumer acked because they were not an event
        /// in any version — bytes that do not parse and never will —
        /// since the consumer started. Poison, not history: skipping
        /// one loses nothing readable. Reported apart from a halt
        /// because "unparseable" and "a version this build cannot
        /// read" are different facts calling for opposite responses.
        malformed_acked: u64,
        /// Redeliveries this consumer acked and dropped because the
        /// invocation they would have started has already started: the
        /// durable claim on that delivery's stream sequence had
        /// recorded its start. Copies of a run that has started. Zero
        /// on a consumer that was never offered a message it had
        /// already begun. Reported because it is the only trace such a
        /// delivery leaves: no invocation is created for it, so
        /// nothing downstream records it. Counted in memory since the
        /// daemon started; a restart resets it.
        duplicate_dropped: u64,
        /// Redeliveries this consumer dropped and left unacked because
        /// they arrived before a start was recorded on their claim, and
        /// the claim is the daemon's own. "Held" in the name is the
        /// claim's state: the daemon holds the durable claim on that
        /// delivery's stream sequence, and it records no start. It is
        /// not an admission hold (a paused model, an agent at its cap,
        /// a wait for a worker permit), though a trigger in one is a
        /// common first copy. Copies of a trigger still in hand. The
        /// first copy may be waiting or still starting (its holds and
        /// its setup renew its ack window, so a copy arrives under it
        /// only when those renewals stop landing for a whole window),
        /// or the daemon may never settle that claim; nothing on the
        /// claim says which, and neither does this count. The daemon
        /// logs each one at warn with its subject, its sequence, its
        /// attempt and the claim's age. Counted in memory since the
        /// daemon started, like `duplicate_dropped`. A report from a
        /// daemon that does not send it reads as zero.
        #[serde(default)]
        held_duplicate_dropped: u64,
        /// Deliveries that met a claim the daemon holds itself, with no
        /// start recorded on it, that no copy of the trigger was handling
        /// any more and whose run had no start on record, in the worker's
        /// WAL or the control plane's: the copy that took the claim ended
        /// before its run's first WAL write and left the delivery
        /// unsettled (its task died, or releasing the claim before a NAK
        /// failed). Each took the claim again under a new invocation id
        /// and ran the trigger, once. Not a duplicate: the work runs. Not
        /// a fault either, though each one follows a failure the daemon
        /// logs; the daemon logs each at warn with its subject, its
        /// sequence, its attempt, the claim's age and the abandoned
        /// invocation id. Counted in memory since the daemon started, like
        /// `duplicate_dropped`. A report from a daemon that does not send
        /// it reads as zero.
        #[serde(default)]
        abandoned_claim_retaken: u64,
    },
    /// The consumer stopped itself on an event whose schema version
    /// this build does not read, and is holding there: the message is
    /// delivered and unacked, nothing after it is consumed, and the
    /// daemon keeps running so this can be reported. Acking it would
    /// have dropped readable history from every projection built from
    /// the stream; halting keeps it, at the cost of a consumer that
    /// makes no progress until a build that reads that version runs.
    Halted {
        name: String,
        /// The event it stopped on: where it sits, what it declared,
        /// and what the reader would have accepted.
        halted_on: UnsupportedEvent,
        /// As on `Active`: the malformed messages acked before the
        /// halt.
        malformed_acked: u64,
    },
}

/// An event a consumer could not read: its envelope declares a schema
/// version outside the set this build parses. Carries where the event
/// sits on the stream and how its envelope identified itself, so an
/// operator can find it, and the versions the reader would have
/// accepted, so the line says which side is behind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct UnsupportedEvent {
    /// The version the envelope declared.
    pub schema_version: u32,
    /// The versions this build reads.
    pub supported: Vec<u32>,
    /// The `event_id` as the envelope spelled it, when it carried one.
    pub event_id: Option<String>,
    /// The subject the message arrived on.
    pub subject: String,
    /// The message's sequence on its stream, when the delivery carried
    /// one.
    pub stream_seq: Option<u64>,
}

impl ConsumerHealth {
    /// The durable's name, whichever state it is in. Every health
    /// surface names the consumer it is talking about — a red line that
    /// does not say which consumer is a red line an operator cannot act
    /// on.
    pub fn name(&self) -> &str {
        match self {
            ConsumerHealth::Missing { name }
            | ConsumerHealth::Error { name, .. }
            | ConsumerHealth::Active { name, .. }
            | ConsumerHealth::Halted { name, .. } => name,
        }
    }

    /// True when this consumer is something to act on: absent from a
    /// daemon that expects it, unreadable, stuck redelivering, or
    /// halted on an event it cannot read. A backlog alone is not a
    /// fault — a consumer catching up is working.
    pub fn is_fault(&self) -> bool {
        match self {
            ConsumerHealth::Missing { .. }
            | ConsumerHealth::Error { .. }
            | ConsumerHealth::Halted { .. } => true,
            ConsumerHealth::Active { stuck, .. } => *stuck,
        }
    }

    /// How this consumer is getting on with its work, for the surfaces
    /// that render a verdict. `None` for a consumer that is not
    /// working at all — missing, unreadable, or halted on an event it
    /// cannot parse — because "how far behind" is not the finding
    /// there and every surface says something else about those.
    ///
    /// The verdict reads the two figures JetStream keeps per consumer,
    /// both already filtered to the subjects the consumer subscribes
    /// to. `num_pending` is the backlog: matching messages the broker
    /// still has to offer. `ack_pending` is work in hand: matching
    /// messages already delivered and not yet acked. Nothing pending
    /// and nothing in flight is caught up, whatever else is on the
    /// stream.
    ///
    /// **The distance to the stream head is not a backlog and is
    /// deliberately not consulted.** A consumer filtered to a few
    /// subjects will never be offered the messages in between, so
    /// `last_seq - delivered` counts heartbeats and coordination
    /// traffic it is not subscribed to; it climbs with every publish
    /// by anyone and never returns to zero. A healthy summariser read
    /// as permanently red that way, and a red glyph an operator learns
    /// to ignore is worse than no glyph at all.
    pub fn progress(&self) -> Option<ConsumerProgress> {
        match self {
            ConsumerHealth::Missing { .. }
            | ConsumerHealth::Error { .. }
            | ConsumerHealth::Halted { .. } => None,
            ConsumerHealth::Active {
                stuck,
                ack_pending,
                num_pending,
                ..
            } => Some(if *stuck {
                ConsumerProgress::Stuck
            } else if *num_pending == 0 && *ack_pending == 0 {
                ConsumerProgress::CaughtUp
            } else if *num_pending < SLIGHTLY_BEHIND_BELOW {
                ConsumerProgress::SlightlyBehind
            } else {
                ConsumerProgress::Lagging
            }),
        }
    }
}

/// The backlog at which a consumer stops reading as a moment behind
/// and starts reading as one an operator should look at.
///
/// Ten, because a durable that is fetching in batches is routinely a
/// handful of messages from empty and calling that "lagging" would put
/// a fault glyph on a healthy runtime. It bounds a *rendering*, not a
/// fault: [`ConsumerHealth::is_fault`] does not consult it, so nothing
/// exits non-zero on the strength of this number.
pub const SLIGHTLY_BEHIND_BELOW: u64 = 10;

/// How an active consumer is getting on, as every health surface
/// phrases it. Derived from a consumer's own figures rather than
/// carried on the wire: it is a reading of the numbers beside it, and a
/// surface that rendered a stored verdict could disagree with the
/// counts printed on the same line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerProgress {
    /// Retrying one message rather than making progress — the fault
    /// verdict, and the only one of the four that is one.
    Stuck,
    /// Nothing pending and nothing in flight: the broker has offered
    /// this consumer everything it matches.
    CaughtUp,
    /// A backlog under [`SLIGHTLY_BEHIND_BELOW`].
    SlightlyBehind,
    /// A backlog at or above [`SLIGHTLY_BEHIND_BELOW`].
    Lagging,
}

/// Health of one shared MCP server, as every operator surface reads
/// it. Externally tagged, for the same encoding reason as
/// [`StreamHealth`].
///
/// Only *shared* servers appear. A grant-bearing server runs
/// per-invocation (ADR-0018), so "unavailable" about one would be a
/// verdict on a single run reported as a standing fact about the
/// daemon.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpServerHealth {
    /// The handshake is in flight. Transient, and visible only if a
    /// report lands during boot or during a retry attempt.
    Starting { name: String },
    /// Connected, with its tools registered.
    Ready {
        name: String,
        /// How many tools it advertised, the host-synthesized resource
        /// tools included.
        tools: u32,
    },
    /// The server did not start, or its discovery was refused. Its
    /// tools are absent and any agent declaring it is refused at
    /// dispatch with this same reason.
    Unavailable {
        name: String,
        /// Why, verbatim — a deadline, a cap, a spawn failure. The
        /// message names the `[mcp]` key behind any bound it hit, so
        /// the line says what to change.
        reason: String,
        /// How many times it has been dialled, the boot attempt
        /// included.
        attempts: u32,
        /// When the next retry is due, epoch milliseconds. `None` when
        /// `[mcp] retry_initial_secs` is 0, which is the one
        /// configuration where unavailable means until restart.
        next_retry_at_ms: Option<i64>,
    },
}

impl McpServerHealth {
    /// The server's declared name, whichever state it is in.
    pub fn name(&self) -> &str {
        match self {
            McpServerHealth::Starting { name }
            | McpServerHealth::Ready { name, .. }
            | McpServerHealth::Unavailable { name, .. } => name,
        }
    }

    /// True when this server is something to act on. `Starting` is not:
    /// it is bounded by the start-up deadline and resolves either way
    /// within it, so reporting it as a fault would make every boot look
    /// unhealthy for a moment.
    pub fn is_fault(&self) -> bool {
        matches!(self, McpServerHealth::Unavailable { .. })
    }
}

/// One model the worker's provider throttle is holding back
/// (<https://github.com/bricef/factor-q/issues/278>), as every operator
/// surface reads it: paused, running under its permit ceiling, or
/// rate-limited in the current success window. A model with no 429
/// history is not listed at all.
///
/// Reported, not judged: a throttled model is the runtime doing its job
/// under a provider's backpressure, so `fq doctor` lists it without
/// changing its verdict.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct ThrottledModel {
    /// The model string requests target — the throttle's key, which
    /// names the `(provider, model)` pair because routing maps a model
    /// to exactly one provider.
    pub model: String,
    /// When the current pause ends, epoch milliseconds on the daemon's
    /// clock. Absent when the model is not paused. A paused model starts
    /// no invocation and is granted no permit until this passes.
    pub paused_until_ms: Option<i64>,
    /// How many calls the model may have in flight right now — halved on
    /// each 429 wave, raised by one per clean success window, never
    /// below 1 and never above `ceiling`.
    pub cap: u32,
    /// The cap's upper bound: `[worker] max_concurrent_invocations`.
    pub ceiling: u32,
    /// Calls holding a permit at the instant of the report.
    pub in_flight: u32,
    /// 429s seen since the success window last completed.
    pub rate_limited_in_window: u32,
    /// Consecutive 429 waves without a success in between — what the
    /// default pause escalates on. Zero once a call succeeds.
    pub waves: u32,
}

impl ThrottledModel {
    /// The permit arithmetic behind a slowdown, phrased once: the
    /// permits the model may hold against its ceiling, the calls
    /// holding one now, and the 429s and waves behind the cap. `fq
    /// status`, `fq doctor` and the dashboard each wrap their own pause
    /// phrase around this line, so the three never drift.
    pub fn permit_summary(&self) -> String {
        format!(
            "{} of {} permits, {} in flight; {} rate-limited this window (wave {})",
            self.cap, self.ceiling, self.in_flight, self.rate_limited_in_window, self.waves
        )
    }
}

/// One agent the dispatcher's per-agent concurrency cap is bounding
/// right now (<https://github.com/bricef/factor-q/issues/718>), as every
/// operator surface reads it: how many of its invocations are running,
/// the cap its definition declares, and how many triggers are being held
/// un-started because the cap is full. An agent with no `max_concurrent`
/// in its definition is not listed at all, and neither is one running
/// under its cap with nothing waiting.
///
/// Reported, not judged — the same tone as [`ThrottledModel`]. An agent
/// at its cap is the runtime honouring the definition, not a fault; but
/// an operator wondering why a labelled issue has not been picked up
/// reads the answer here.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct AgentAtCap {
    /// The agent id, as the definition's `name:` spells it.
    pub agent: String,
    /// Invocations of this agent the daemon is running at this instant
    /// — trigger-started, deferral-resumed and recovery-resumed alike.
    pub in_flight: u32,
    /// `max_concurrent` from the definition that was current when the
    /// last invocation was admitted. A reload changes it for the next
    /// trigger, and this line with it.
    pub cap: u32,
    /// Triggers pulled, un-acked and waiting for a slot. Each is still
    /// its first delivery; none has consumed a redelivery.
    pub held: u32,
}

impl AgentAtCap {
    /// The arithmetic behind a hold, phrased once so `fq doctor` and
    /// any later surface cannot drift: the invocations running against
    /// the cap, and the triggers waiting on them.
    pub fn cap_summary(&self) -> String {
        format!(
            "{}/{} in flight, {} trigger(s) held",
            self.in_flight, self.cap, self.held
        )
    }

    /// Whether this agent is actually full — the line an operator is
    /// looking for when the fleet is quiet.
    pub fn is_at_cap(&self) -> bool {
        self.in_flight >= self.cap
    }
}

/// A finished invocation's row the worker's archive sweeper is holding
/// rather than archiving, as every operator surface reads it.
///
/// The sweeper publishes nothing more for a held row: whatever it owed
/// the bus and could publish before the hold, such as a terminal event
/// whose first publish was lost, has gone out, and what it could not,
/// `invocation.archived` above all, does not. So the control plane sees
/// the invocation as not archived. The row stays in the worker's store,
/// whole, and every other row is archived as usual. The list is the
/// daemon's memory of its own sweeps: a row held by an earlier run is
/// offered again when the daemon starts, and is listed again once this
/// run's sweeper has held it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct ArchiveHold {
    /// The invocation whose row is held, in full.
    pub invocation_id: String,
    /// The agent that ran it.
    pub agent_id: String,
    /// Why the sweeper holds it.
    pub reason: ArchiveHoldReason,
    /// What lifts the hold, in the daemon's words: what to change or do,
    /// and where the operating guide covers it.
    pub remedy: String,
}

/// Why the archive sweeper holds a row. Externally tagged, for the same
/// encoding reason as [`StreamHealth`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveHoldReason {
    /// An event the row has to publish is larger than the broker accepts:
    /// its stored terminal event, or `invocation.archived`, which carries
    /// the run's whole state. The same event is refused on every attempt,
    /// and the daemon reads the broker's `max_payload` once, when it
    /// connects, so the sweeper does not offer the row again in this run;
    /// the next start offers it again.
    PayloadTooLarge {
        /// The refused event's type, spelled as its own `event_type` on
        /// the wire.
        event_type: String,
        /// The event's size on the wire, in bytes: its JSON and the
        /// message-id header sent with it, which is what the broker counts.
        size_bytes: u64,
        /// The broker's `max_payload` it exceeded, in bytes.
        limit_bytes: u64,
    },
    /// The row's stored terminal event must not be republished: this
    /// build cannot read it, or it could not be this row's outcome. The
    /// case to expect is an event an older build stored and had not
    /// published when the daemon was upgraded across an event-schema
    /// change; republished, it would halt every durable consumer on the
    /// stream. No later build reads an older event version, so the hold
    /// lasts until an operator releases the row.
    StoredEventRefused {
        /// Why, verbatim: what the event failed to parse as, or what it
        /// turned out to be.
        detail: String,
    },
    /// The row's ids cannot go on an event: its `agent_id` is not a valid
    /// agent id, or its `invocation_id` is not a UUID. The runtime never
    /// writes either, so the row was edited by hand or written by a build
    /// with other rules. The hold lasts until an operator corrects it.
    MalformedRow {
        /// Which field, its value, and what is wrong with it.
        detail: String,
    },
}

impl ArchiveHoldReason {
    /// The reason as one phrase, for every surface that renders it.
    pub fn summary(&self) -> String {
        match self {
            ArchiveHoldReason::PayloadTooLarge {
                event_type,
                size_bytes,
                limit_bytes,
            } => format!(
                "`{event_type}` is {size_bytes} bytes, over the broker's max_payload of \
                 {limit_bytes}"
            ),
            ArchiveHoldReason::StoredEventRefused { detail }
            | ArchiveHoldReason::MalformedRow { detail } => detail.clone(),
        }
    }
}

/// The worker's archive sweeper as both health reports read it: whether
/// it has looked yet, and the rows it holds.
///
/// The holds are known only once the sweeper has finished a pass over
/// every pending row since the daemon started. Its first pass comes one
/// `[worker] archive_retry_interval_ms` after start, and a pass the bus
/// ends early does not count, so until then an empty list means not yet
/// known, not none. Not yet known is not an issue: it is the absence of a
/// finding, and every start goes through it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, schemars::JsonSchema)]
pub struct ArchiveSweep {
    /// When the sweeper last finished a pass, epoch milliseconds on the
    /// daemon's clock. Absent until its first finished pass of this start;
    /// a report from a daemon that does not send it reads the same way.
    #[serde(default)]
    pub last_pass_at_ms: Option<i64>,
    /// Every row held now, in invocation-id order.
    #[serde(default)]
    pub holds: Vec<ArchiveHold>,
}

impl ArchiveSweep {
    /// Whether the holds are known: the sweeper has finished a pass since
    /// the daemon started.
    pub fn has_swept(&self) -> bool {
        self.last_pass_at_ms.is_some()
    }
}

/// A trigger claim that has not settled and will not settle on its own,
/// as `control.doctor` reports it: a claimed delivery that has neither
/// been released nor recorded as started.
///
/// A claim this daemon holds for an invocation that has not started is
/// never one of these, however old. A trigger can wait behind a paused
/// model or a full agent for hours, and nothing on the claim says
/// whether the daemon holding it is still going to start it, so the
/// report does not guess. Nor is a claim whose delivery can no longer
/// change anything: one whose run started but whose message is gone, and
/// one an exited process left that is older than the report's horizon.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct StrandedTriggerClaim {
    /// The delivery, and the claim on it as the control plane holds it.
    pub delivery: crate::views::TriggerDeliveryView,
    /// The invocation id the claim reserved for its run. Under
    /// `started_unmarked` it names a run that exists; under every other
    /// finding it names no run, only the id one would have had. A claim
    /// written by a build that did not name the id at claim time has
    /// none.
    pub invocation_id: Option<String>,
    /// Whether the claim is this daemon's or a process's that has exited.
    pub holder: ClaimHolder,
    /// Whether the claimed message can still be delivered.
    pub message: ClaimedMessage,
    pub finding: StrandedClaimFinding,
    /// What the stores record of the run the claim names: `in_flight`,
    /// its terminal phase, or the control plane's coordination status for
    /// it. Present under `started_unmarked` and absent under every other
    /// finding.
    #[serde(default)]
    pub invocation_status: Option<String>,
}

/// Whose claim a stranded claim is.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimHolder {
    /// The daemon answering the report.
    ThisDaemon,
    /// Any other process. One daemon dispatches at a time and takes a new
    /// id on every start, so a claim under another id was left by a
    /// process that has exited.
    ExitedProcess,
}

/// Whether the message a claim names can still be delivered: only one
/// that can be is redelivered, and a redelivery is the only thing that
/// settles a claim an exited process left.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimedMessage {
    /// On the stream this daemon consumes, and claimed more recently than
    /// the stream keeps messages, so the broker can still deliver it.
    Deliverable,
    /// Claimed from an earlier incarnation of the stream. That stream is
    /// gone, and the same sequence in the current one is another message.
    EarlierIncarnation,
    /// Claimed longer ago than the stream keeps messages. A message is
    /// published before it is first claimed, so it has aged out.
    AgedOut,
}

/// What `control.doctor` makes of a stranded claim.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StrandedClaimFinding {
    /// Left by a process that has exited, with no start recorded, its
    /// message still deliverable, and this daemon not yet running for the
    /// redelivery window. If the broker redelivers it, this daemon adopts
    /// the claim and starts a run. Not an issue.
    AwaitingRedelivery,
    /// Left by a process that has exited, with no start recorded, and not
    /// going to be redelivered: its message is gone, or it is deliverable
    /// and has not come back although this daemon has been running for
    /// longer than the redelivery window. This delivery never started a
    /// run. Work requeued or published again runs under a new delivery and
    /// a new claim, which this one cannot show, so it stays listed until it
    /// is older than the report's horizon.
    NeverStarted,
    /// The run the claim names has started, the claim still says it has
    /// not, and its message is still deliverable. Held by this daemon,
    /// one of two things happened, and only the daemon log tells which:
    /// the claim store refused the mark and the dispatcher acked the
    /// delivery anyway, keeping its claim; or the run's task panicked in
    /// the step that made its first durable write, and nothing was acked.
    /// Where nothing was acked, or the ack was lost, every redelivery is
    /// dropped until this daemon next starts and its recovery marks the
    /// claim, and a trigger whose deliveries run out first ends as a
    /// `trigger_exhausted` dead letter although its run started. Left by a
    /// process that has exited, a redelivery would be adopted and start
    /// the work again, and a false dead letter may already exist.
    StartedUnmarked,
    /// Left by a process that has exited, by a build that did not name the
    /// invocation at claim time, so the report cannot tell whether it
    /// started a run. Not an issue.
    StartUnknown,
}

impl StrandedClaimFinding {
    /// Whether this finding makes the report one with issues: a delivery
    /// that never started a run, and a run whose claim was never marked. A
    /// claim awaiting its redelivery is the runtime about to repair it, and
    /// one the report cannot judge is not reported as a fault.
    pub fn is_issue(self) -> bool {
        matches!(
            self,
            StrandedClaimFinding::NeverStarted | StrandedClaimFinding::StartedUnmarked
        )
    }
}

#[cfg(test)]
mod tests;
