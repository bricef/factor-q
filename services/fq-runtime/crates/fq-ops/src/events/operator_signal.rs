//! The operator-signal payload: one shape for "a person should look at
//! this", whoever inside the daemon is saying it.
//!
//! Out-of-band operator signals used to leave the system as text —
//! `ops/dogfood/notify.sh` shelling a Pushover message for a deploy, a
//! disk alarm, a rollback — so nothing that happened was afterwards a
//! *fact* the log held. This payload is that fact: a component says what
//! it saw, how loudly it is saying it, and what a reader should look at
//! next, on the same event log as everything else (ADR-0026).
//!
//! **Two severities, and the difference is a human's hours.** A
//! [notification](SignalSeverity::Notification) is handled during normal
//! hours and looked at by the operator; a successful deploy and a
//! refused pricing change are notifications. An
//! [alert](SignalSeverity::Alert) reaches the operator out of hours,
//! escalates, and names something the system cannot recover from on its
//! own. The distinction is the contract the dashboard's pane implements
//! (<https://github.com/bricef/factor-q/issues/736>) and the operator
//! guide records; it is a property of the situation, not of how bad the
//! producer feels about it.
//!
//! **An alert is open until a later signal resolves it.** A signal may
//! name the one it closes — [`resolves`](OperatorSignalPayload::resolves),
//! an event id — and a recovery normally does so as a notification,
//! because the end of an alarm is not itself alarming. Nothing else
//! closes one: without this, "open alerts" is a number that can only
//! grow, and a number that can only grow is one nobody reads.
//!
//! **One namespace, one home** — the [`subjects`](super::subjects)
//! discipline, applied to kinds. A kind is a dotted name whose first
//! segment is the component that owns it, and every kind this build
//! knows is spelled once, in [`kinds`]. A producer that spells its own
//! kind inline is a kind that can be spelled two ways, and the pane
//! filters on it.
//!
//! ## Adding a kind
//!
//! 1. Add a `pub const` to [`kinds`] and list it in
//!    [`kinds::REGISTERED`] — the test below checks every entry parses
//!    and that none is a duplicate.
//! 2. Document it in the kind registry in
//!    `docs/design/committed/event-schema.md`: its severity, and the
//!    fields its `detail` carries.
//! 3. Emit it with [`SignalKind::registered`], which takes only a
//!    registry entry.
//!
//! No new event type, no schema version bump: the payload's shape does
//! not change when the vocabulary does, which is the whole reason `kind`
//! is data rather than a variant.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::agent::AgentId;

/// How loudly a signal is saying it — and, concretely, whether it is
/// allowed to wake somebody.
///
/// Two variants, and no `Info`: a signal nobody is expected to look at
/// is a log line, and the log already has those. The value of the pane
/// is that everything in it was worth a person's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSeverity {
    /// Handled during normal hours, and looked at by the operator. It
    /// may also travel to a Slack channel or similar. A deploy that
    /// succeeded is a notification; so is a refused pricing change,
    /// because the daemon carries on at the prior price.
    Notification,
    /// Reaches the operator out of hours, escalates, and requires human
    /// intervention: the system cannot recover from this on its own. A
    /// service that has stopped accepting requests with no graceful
    /// recovery path is an alert.
    Alert,
}

impl SignalSeverity {
    /// The wire spelling, for a renderer or an index column that wants
    /// the same string serde writes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::Alert => "alert",
        }
    }

    /// Whether this severity is allowed to wake somebody.
    pub fn is_alert(self) -> bool {
        matches!(self, Self::Alert)
    }
}

impl std::fmt::Display for SignalSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a signal *is*, as a dotted name whose first segment is the
/// component that owns it: `pricing.change_refused`, `deploy.succeeded`.
///
/// **The source is the kind's first segment, not a second field.** A
/// payload carrying both `source: "pricing"` and `kind:
/// "pricing.change_refused"` spells one fact twice, and two spellings of
/// one fact drift — the failure the [`subjects`](super::subjects) module
/// was built to stop, one layer up. So the source is read off the kind
/// ([`SignalKind::source`]) and cannot disagree with it.
///
/// Validated on the way in and on the way off the wire: at least two
/// segments, each a non-empty `[a-z0-9_]` word. That is stricter than a
/// NATS subject token needs (a test below asserts the implication), and
/// deliberately so — `Pricing.ChangeRefused` and `pricing.change_refused`
/// would otherwise be two kinds naming one thing in a pane that groups
/// by name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SignalKind(String);

impl SignalKind {
    /// Parse and validate a kind.
    pub fn new(s: impl Into<String>) -> Result<Self, SignalKindError> {
        let s = s.into();
        if s.is_empty() {
            return Err(SignalKindError::Empty);
        }
        let segments: Vec<&str> = s.split('.').collect();
        if segments.len() < 2 {
            return Err(SignalKindError::MissingSource(s));
        }
        for segment in &segments {
            let legible = !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !legible {
                return Err(SignalKindError::InvalidSegment(s));
            }
        }
        Ok(Self(s))
    }

    /// A kind from [`kinds::REGISTERED`], for a producer.
    ///
    /// Infallible by construction rather than by hope: the argument must
    /// be a registry entry, and a test parses every entry, so neither
    /// assertion below can fire for a caller that followed the
    /// module header's three steps. A producer minting an ad-hoc kind is
    /// the thing this refuses — the pane's filter list is the registry.
    pub fn registered(kind: &'static str) -> Self {
        assert!(
            kinds::REGISTERED.contains(&kind),
            "`{kind}` is not in the operator-signal kind registry; add it to \
             `kinds::REGISTERED` and to the registry table in \
             docs/design/committed/event-schema.md"
        );
        Self::new(kind).expect("a registered kind is a valid kind")
    }

    /// The component that owns this kind — the first segment.
    pub fn source(&self) -> &str {
        self.0
            .split('.')
            .next()
            .expect("a kind has a first segment")
    }

    /// What this kind says about its source — everything after the
    /// first segment, dots included.
    pub fn name(&self) -> &str {
        self.0
            .split_once('.')
            .expect("a kind has at least two segments")
            .1
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SignalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SignalKind {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for SignalKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SignalKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Why a string is not a [`SignalKind`].
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SignalKindError {
    #[error("an operator-signal kind must not be empty")]
    Empty,
    #[error("`{0}` names no source: a kind is `<source>.<name>`, e.g. `pricing.change_refused`")]
    MissingSource(String),
    #[error("`{0}` has a segment that is not a lowercase `[a-z0-9_]` word")]
    InvalidSegment(String),
}

/// Every operator-signal kind this build knows, spelled once.
///
/// The registry is code *and* documentation: the constants here are what
/// a producer emits, and the table in
/// `docs/design/committed/event-schema.md` says, per kind, what its
/// `detail` carries. See the module header for how to add one.
pub mod kinds {
    /// A live pricing table proposed a change that acceptance refused —
    /// a drift beyond the bound, or a price of zero
    /// (<https://github.com/bricef/factor-q/issues/735>). A
    /// notification: the daemon carries on at the prior price. One per
    /// model per run, and a restart between a refresh and its ack
    /// re-runs the refresh, so the same refusal can appear twice under
    /// different envelopes.
    pub const PRICING_CHANGE_REFUSED: &str = "pricing.change_refused";

    /// The pricing table has not refreshed within its staleness window
    /// (<https://github.com/bricef/factor-q/issues/735>). An alert: no
    /// refresh recovers a source that has stopped answering, and prices
    /// silently older than the models they price is the failure ADR-0004's
    /// guarantee exists to prevent.
    pub const PRICING_STALE: &str = "pricing.stale";

    /// The pricing document could not be fetched, so the daemon is
    /// serving the last table it accepted
    /// (<https://github.com/bricef/factor-q/issues/735>). A
    /// notification: last-known-good is a working state, and the alert
    /// for one that stays that way too long is [`PRICING_STALE`].
    pub const PRICING_FETCH_FAILED: &str = "pricing.fetch_failed";

    /// A scheduled maintenance task ran and failed
    /// (<https://github.com/bricef/factor-q/issues/344>). A
    /// notification: the schedule is the retry, so the daemon carries on
    /// and the next fire tries again — but unattended work that stopped
    /// working, with nobody watching the log, is exactly what the pane
    /// exists for. Raised once per run and never on a redelivery within
    /// one daemon's lifetime — the ledger that suppresses it is in
    /// process, so a restart between a run and its ack re-runs the task
    /// and repeats this. A *refused* task (a name this build does not
    /// know) raises nothing, because that is a `fq-cron.toml` error its
    /// own owner sees.
    pub const MAINTENANCE_RUN_FAILED: &str = "maintenance.run_failed";

    /// A new build reached the dogfood instance and came up
    /// (<https://github.com/bricef/factor-q/pull/707>). A notification.
    /// Reserved: the deploy message reaches Pushover today and becomes
    /// this event's second producer.
    pub const DEPLOY_SUCCEEDED: &str = "deploy.succeeded";

    /// The registry as values — what
    /// [`SignalKind::registered`](super::SignalKind::registered) will
    /// accept, and the set a test checks.
    pub const REGISTERED: &[&str] = &[
        PRICING_CHANGE_REFUSED,
        PRICING_FETCH_FAILED,
        PRICING_STALE,
        MAINTENANCE_RUN_FAILED,
        DEPLOY_SUCCEEDED,
    ];
}

/// What a signal is *about*, when it is about something addressable.
///
/// Separate from the envelope on purpose. An operator signal is
/// daemon-scoped — it rides `fq.system.operator_signal` and its envelope
/// names the runtime, not an agent — because the component that raised
/// it is a part of the daemon rather than a step of somebody's
/// invocation. Putting a concerned agent in `envelope.agent_id` would
/// attribute a daemon's observation to whichever agent happened to be
/// running, which is the fiction `mcp_server_log` refuses for the same
/// reason. So the references are payload data: present when the signal
/// concerns a particular invocation or names a page to open, absent
/// otherwise, and never a claim about where the event came from.
///
/// **The identities are spelled as the envelope spells them.** A reader
/// joining `references.agent_id` to `envelope.agent_id` is reading one
/// key, not two that happen to mean the same thing; `agent` and
/// `invocation` would have been shorter and would have made the join a
/// thing a reader has to know rather than see.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalReferences {
    /// The agent the signal concerns, if it concerns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    /// The invocation the signal concerns, if it concerns one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<Uuid>,
    /// Somewhere to look: a pull request, a CI run, a dashboard page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl SignalReferences {
    /// Whether there is nothing to point at — the whole struct is
    /// omitted from the wire when so.
    pub fn is_empty(&self) -> bool {
        self.agent_id.is_none() && self.invocation_id.is_none() && self.url.is_none()
    }
}

/// A component of the daemon saying an operator should look at
/// something.
///
/// Construct one through [`notification`](Self::notification) or
/// [`alert`](Self::alert) and add what applies:
///
/// ```
/// use fq_ops::events::operator_signal::kinds;
/// use fq_ops::events::{OperatorSignalPayload, SignalKind};
/// use serde_json::json;
///
/// let signal = OperatorSignalPayload::notification(
///     SignalKind::registered(kinds::PRICING_CHANGE_REFUSED),
///     "moonshotai/kimi-k3 input price moved 6.2x; kept the prior price",
/// )
/// .with_detail(json!({
///     "model": "moonshotai/kimi-k3",
///     "field": "input_cost_per_token",
///     "rule": "drift_bound",
/// }));
///
/// assert_eq!(signal.kind.source(), "pricing");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorSignalPayload {
    /// Notification or alert — whether this is allowed to wake somebody.
    pub severity: SignalSeverity,
    /// What this is, and (in its first segment) who is saying it.
    pub kind: SignalKind,
    /// One line, for the pane's list. The whole story is `detail`; this
    /// is what an operator reads while deciding whether to open it.
    pub summary: String,
    /// The structured particulars, shaped by the kind and documented
    /// with it in the registry. Producer-defined and never parsed by the
    /// runtime: the pane renders it, a person reads it.
    ///
    /// Absent on the wire when there are none, rather than an empty
    /// object — "this kind carries no particulars" and "this producer
    /// sent none" read the same either way, and the shorter form is the
    /// honest one.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub detail: Value,
    /// The [`event_id`] of the signal this one resolves, when it
    /// resolves one.
    ///
    /// **An alert is open until a later signal resolves it.** Without
    /// this, "how many alerts are open" can only ever grow: a pane
    /// counting every alert on record shows a week of a broken upstream
    /// as twenty-eight things to act on, none of which can ever close,
    /// and a count that only grows is a count nobody reads. So a
    /// recovery says which signal it is the recovery *of*, by id, and
    /// the count is a fold over the log rather than a row count.
    ///
    /// A notification may carry it too, and normally does: the recovery
    /// of an alert is not itself alarming, so it is a notification
    /// naming the alert it closes. A resolving signal names the same
    /// [`kind`](Self::kind) as the signal it resolves — the topic has
    /// not changed, only its state — so the registry's severity is the
    /// severity a kind is *raised* at.
    ///
    /// Not part of [`references`](Self::references), which is where a
    /// person looks *next*: this is a relation between two events on
    /// one log, the payload's counterpart to
    /// [`parent_event_id`](crate::events::Envelope::parent_event_id),
    /// and it is what the pane's index reads rather than what it renders
    /// as a link.
    ///
    /// [`event_id`]: crate::events::Envelope::event_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolves: Option<Uuid>,
    /// What to look at next, when there is something.
    #[serde(default, skip_serializing_if = "SignalReferences::is_empty")]
    pub references: SignalReferences,
}

impl OperatorSignalPayload {
    /// A signal for normal hours.
    pub fn notification(kind: SignalKind, summary: impl Into<String>) -> Self {
        Self::new(SignalSeverity::Notification, kind, summary)
    }

    /// A signal that may wake somebody. Reserve it for what a person
    /// must intervene in: the pane's alert stripe is worth nothing if
    /// everything wears one.
    pub fn alert(kind: SignalKind, summary: impl Into<String>) -> Self {
        Self::new(SignalSeverity::Alert, kind, summary)
    }

    fn new(severity: SignalSeverity, kind: SignalKind, summary: impl Into<String>) -> Self {
        Self {
            severity,
            kind,
            summary: summary.into(),
            detail: Value::Null,
            resolves: None,
            references: SignalReferences::default(),
        }
    }

    /// Attach the structured particulars this kind's registry entry
    /// documents.
    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }

    /// Say which invocation the signal concerns.
    pub fn about_invocation(mut self, agent: AgentId, invocation: Uuid) -> Self {
        self.references.agent_id = Some(agent);
        self.references.invocation_id = Some(invocation);
        self
    }

    /// Say which earlier signal this one resolves, by its
    /// `envelope.event_id` — the recovery of an alert, or the answer to
    /// a notification that asked for something.
    ///
    /// The producer holds that id because it published the signal it is
    /// now closing; nothing here checks that the id names a signal, for
    /// the same reason nothing parses `detail` — a claim about the log
    /// is answered by reading the log, and a reader that cannot find the
    /// resolved signal has learned something true.
    pub fn resolving(mut self, signal: Uuid) -> Self {
        self.resolves = Some(signal);
        self
    }

    /// Say where to look — a pull request, a CI run, a dashboard page.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.references.url = Some(url.into());
        self
    }

    /// The component that raised this signal — the kind's first segment.
    pub fn source(&self) -> &str {
        self.kind.source()
    }
}

/// A signal and the `event_id` it will be published under.
///
/// **An alert is open until a later signal's `resolves` names its
/// `event_id`**, so a producer that raises an alert it intends to close
/// later has to know that id *before* the alert is on the log. Producers
/// here return signals rather than publishing them — the maintenance
/// consumer publishes them behind the outcome that gates them, which is
/// what makes "once per run, never on a replay" one rule in one place —
/// so the id cannot come back from the publisher without a callback
/// pointing the wrong way. Minting it at the producer and stamping it on
/// the envelope is the seam that keeps the publishing rule where it is.
///
/// A producer with nothing to resolve never needs this type: `.into()`
/// from a payload mints an id it can ignore.
#[derive(Debug, Clone)]
pub struct PendingSignal {
    /// The `envelope.event_id` this signal will carry. A v7 uuid, minted
    /// at the moment the signal was decided rather than at publish, so
    /// it still sorts by when it happened.
    pub event_id: Uuid,
    /// What to say.
    pub payload: OperatorSignalPayload,
}

impl PendingSignal {
    /// Mint an id for `payload`, so a later signal can name it.
    pub fn new(payload: OperatorSignalPayload) -> Self {
        Self {
            event_id: Uuid::now_v7(),
            payload,
        }
    }

    /// The kind this signal carries — what a caller filters on without
    /// reaching through the payload.
    pub fn kind(&self) -> &SignalKind {
        &self.payload.kind
    }

    /// The event that puts this signal on the log, under the id it was
    /// minted with.
    pub fn into_event(self, runtime_id: Uuid) -> crate::events::Event {
        let mut event = crate::events::Event::system(
            runtime_id,
            crate::events::EventPayload::OperatorSignal(self.payload),
        );
        event.envelope.event_id = self.event_id;
        event
    }
}

impl From<OperatorSignalPayload> for PendingSignal {
    fn from(payload: OperatorSignalPayload) -> Self {
        Self::new(payload)
    }
}

#[cfg(test)]
mod tests;
