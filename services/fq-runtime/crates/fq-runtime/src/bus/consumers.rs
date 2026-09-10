//! Every durable JetStream consumer factory on the bus, in one place.
//!
//! They were spread through `bus.rs` alongside the connection, the
//! stream definitions and the publish path, and each one re-stated its
//! own `get_or_create` + drift-repair dance. Collecting them is what
//! makes the redelivery policy statable once: one private seam stamps
//! `ack_wait` and `max_deliver` onto every config that passes through
//! it, and repairs an existing durable whose stamped fields
//! have drifted, so "explicit `ack_wait` on every durable" is a
//! property of the seam rather than a thing five call sites remember.
//!
//! **Two delivery bounds, and the split is the point (review finding
//! B4, #549).** A durable with a dead-letter path — the trigger
//! consumer — keeps a finite `max_deliver`: a poison trigger must
//! eventually stop being retried, and the dispatcher emits a terminal
//! failure on the last delivery so the exhaustion is a record rather
//! than a silence. Every durable *without* one — the projector, the
//! coordination, heartbeat and summary consumers, the advisory watch —
//! carries [`UNLIMITED_MAX_DELIVER`], because the alternative is worse
//! than a slow retry: a persistent transient fault such as a full disk
//! would exhaust the bound and JetStream would drop the message, which
//! is an event silently skipped by the projection (the loss class of
//! <https://github.com/bricef/factor-q/issues/409>). Unlimited
//! redelivery is only tolerable *because* the NAK delay escalates: see
//! [`super::retry`].

use std::time::Duration;

use async_nats::jetstream::consumer::{self, FromConsumer};
use tracing::debug;

use super::{
    ADVISORY_STREAM_NAME, BusError, EventBus, STREAM_NAME, TRIGGER_MAX_DELIVER,
    TRIGGER_RETRY_BACKOFF, TRIGGER_STREAM_NAME,
};
use crate::events::subjects::ALL_TRIGGERS;

/// `max_deliver` for a durable with no dead-letter path: JetStream's
/// "no limit". Spelled as a named constant because `-1` at a call site
/// reads like a mistake, and because the decision it encodes — never
/// drop a control-plane event to a redelivery bound — is the whole of
/// finding B4's answer.
pub const UNLIMITED_MAX_DELIVER: i64 = -1;

impl EventBus {
    /// Get-or-create one durable on `stream_name`, with this bus's
    /// redelivery policy stamped on it.
    ///
    /// `get_or_create_consumer` keeps an existing durable's
    /// configuration, which is right for its position and wrong for its
    /// policy: a consumer created before this daemon's `[bus]` settings
    /// would keep the old `ack_wait` forever, silently. So the fields
    /// this seam owns — `ack_wait`, `max_deliver`, `backoff`, and the
    /// caller's `max_ack_pending` and filters — are compared against
    /// what came back and repaired in place when they differ. The
    /// repair edits the *existing* config rather than sending the
    /// desired one whole, so a durable's acked floor and delivery
    /// policy are never disturbed by a policy change.
    ///
    /// `ack_wait` overrides the bus-wide window for one durable. The
    /// bus default is sized for a handler that writes to SQLite and
    /// returns; a consumer whose handler can legitimately take longer
    /// — the summariser calls an LLM inline — has to say so, or its
    /// message is redelivered while the first attempt is still running
    /// and the work is done, and paid for, twice.
    async fn durable(
        &self,
        stream_name: &str,
        name: &str,
        ack_wait: Option<Duration>,
        mut desired: consumer::pull::Config,
    ) -> Result<consumer::PullConsumer, BusError> {
        desired.durable_name = Some(name.to_string());
        desired.ack_policy = consumer::AckPolicy::Explicit;
        desired.ack_wait = ack_wait.unwrap_or(self.redelivery.ack_wait);

        let stream = self
            .jetstream
            .get_stream(stream_name)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;
        let consumer = stream
            .get_or_create_consumer(name, desired.clone())
            .await
            .map_err(|err| BusError::Stream(err.to_string()))?;

        let existing = consumer.cached_info().config.clone();
        if !drifted(&existing, &desired) {
            return Ok(consumer);
        }
        debug!(
            consumer = name,
            stream = stream_name,
            "durable consumer config drifted from this daemon's policy; repairing in place"
        );
        let mut repaired = consumer::pull::Config::try_from_consumer_config(existing)
            .map_err(|err| BusError::Stream(err.to_string()))?;
        repaired.ack_wait = desired.ack_wait;
        repaired.max_deliver = desired.max_deliver;
        repaired.backoff = desired.backoff.clone();
        repaired.max_ack_pending = desired.max_ack_pending;
        repaired.filter_subject = desired.filter_subject.clone();
        repaired.filter_subjects = desired.filter_subjects.clone();
        stream
            .update_consumer(repaired)
            .await
            .map_err(|err| BusError::Stream(err.to_string()))
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// factor-q event stream.
    ///
    /// Durable consumers remember their position across restarts, so
    /// the projection consumer can be stopped and restarted without
    /// losing events or redelivering old ones. The returned
    /// [`consumer::PullConsumer`] can be used with `.messages()` to
    /// iterate over delivered messages.
    pub async fn durable_consumer(
        &self,
        name: &str,
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        self.durable_consumer_from(name, consumer::DeliverPolicy::All, ack_wait)
            .await
    }

    /// [`EventBus::durable_consumer`], starting where `deliver_policy`
    /// says when the durable is first created. The policy is a
    /// creation-time fact: an existing durable keeps its position, and
    /// the get-or-create seam never rewrites a delivery policy, so a
    /// caller that needs a durable moved deletes it first (the
    /// projection reset does).
    pub async fn durable_consumer_from(
        &self,
        name: &str,
        deliver_policy: consumer::DeliverPolicy,
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            ?deliver_policy,
            "getting/creating durable JetStream consumer"
        );
        self.durable(
            STREAM_NAME,
            name,
            ack_wait,
            consumer::pull::Config {
                deliver_policy,
                ..event_stream_config()
            },
        )
        .await
    }

    /// [`EventBus::durable_consumer`], with **resolved-contiguous
    /// delivery**: `max_ack_pending = 1`, so the server never delivers
    /// a message until the previous one is resolved (acked — success
    /// or permanent skip). After a NAK the next delivery is the retry
    /// of the same message, which is what makes an advance-on-success
    /// watermark contiguous by construction: sequence S is never
    /// exposed while an earlier sequence is still pending redelivery.
    /// Costs throughput — one outstanding message per round-trip —
    /// which the projection's fold accepts as the price of
    /// read-your-writes.
    ///
    /// `get_or_create` keeps an existing durable's settings, so a
    /// pre-existing consumer is repaired in place when its
    /// `max_ack_pending` or scope differs: a mark-bearing consumer
    /// vouches for every sequence, so a durable that was created
    /// filtered keeps its acked floor (no replay) but must widen to
    /// the whole stream. The shared get-or-create seam does the repair.
    pub async fn durable_consumer_strict(
        &self,
        name: &str,
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        self.durable_consumer_strict_from(name, consumer::DeliverPolicy::All, ack_wait)
            .await
    }

    /// [`EventBus::durable_consumer_strict`], starting where
    /// `deliver_policy` says when the durable is first created — the
    /// projector's shape after a rebuild, which replays from the floor
    /// the file records rather than from the beginning. As with
    /// [`EventBus::durable_consumer_from`], the policy only ever
    /// applies to a durable that does not exist yet.
    pub async fn durable_consumer_strict_from(
        &self,
        name: &str,
        deliver_policy: consumer::DeliverPolicy,
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            ?deliver_policy,
            "getting/creating strict-order durable JetStream consumer"
        );
        self.durable(
            STREAM_NAME,
            name,
            ack_wait,
            consumer::pull::Config {
                max_ack_pending: 1,
                deliver_policy,
                ..event_stream_config()
            },
        )
        .await
    }

    /// Durable JetStream consumer scoped to a subject filter.
    ///
    /// Used by the coordination consumer (step 7) which only
    /// cares about a small subset of events
    /// (`fq.agent.*.invocation.*`); subscribing to the whole
    /// event stream would force every coordination consumer
    /// instance to handle messages it doesn't act on.
    pub async fn durable_consumer_with_filter(
        &self,
        name: &str,
        filter_subject: &str,
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            "getting/creating filtered durable JetStream consumer"
        );
        self.durable(
            STREAM_NAME,
            name,
            ack_wait,
            consumer::pull::Config {
                filter_subject: filter_subject.to_string(),
                ..event_stream_config()
            },
        )
        .await
    }

    /// Durable JetStream consumer scoped to *several* subject
    /// filters. Same shape as
    /// [`Self::durable_consumer_with_filter`], for consumers that
    /// react to a handful of unrelated event types (the summary
    /// consumer, #216: triggered + llm_response + completed +
    /// failed) — a single-wildcard filter would force it to churn
    /// through the tool-event firehose it never acts on.
    pub async fn durable_consumer_with_filters(
        &self,
        name: &str,
        filter_subjects: &[&str],
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filters = ?filter_subjects,
            "getting/creating multi-filter durable JetStream consumer"
        );
        self.durable(
            STREAM_NAME,
            name,
            ack_wait,
            consumer::pull::Config {
                filter_subjects: filter_subjects.iter().map(|s| s.to_string()).collect(),
                ..event_stream_config()
            },
        )
        .await
    }

    /// Like [`Self::durable_consumer_with_filters`] but starting
    /// from new messages only. Test-oriented, mirroring
    /// [`Self::durable_consumer_with_filter_from_new`].
    pub async fn durable_consumer_with_filters_from_new(
        &self,
        name: &str,
        filter_subjects: &[String],
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filters = ?filter_subjects,
            "getting/creating multi-filter durable JetStream consumer (deliver_policy=new)"
        );
        self.durable(
            STREAM_NAME,
            name,
            ack_wait,
            consumer::pull::Config {
                filter_subjects: filter_subjects.to_vec(),
                deliver_policy: consumer::DeliverPolicy::New,
                ..event_stream_config()
            },
        )
        .await
    }

    /// Like [`Self::durable_consumer_with_filter`] but the
    /// consumer starts from new messages only (skips the
    /// stream's historical messages on first creation).
    ///
    /// Test-oriented: the acceptance harness needs fresh
    /// consumers per test, but the stream is shared across
    /// runs and contains thousands of historical messages.
    /// Starting from `New` avoids the catch-up wait while
    /// keeping production's recovery-from-history semantics
    /// untouched.
    ///
    /// Note: `get_or_create_consumer` returns the existing
    /// consumer's config if `name` already exists, so this
    /// only affects the first creation. Pair with a unique
    /// per-test consumer name to actually get the new
    /// behaviour.
    pub async fn durable_consumer_with_filter_from_new(
        &self,
        name: &str,
        filter_subject: &str,
        ack_wait: Option<Duration>,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            "getting/creating filtered durable JetStream consumer (deliver_policy=new)"
        );
        self.durable(
            STREAM_NAME,
            name,
            ack_wait,
            consumer::pull::Config {
                filter_subject: filter_subject.to_string(),
                deliver_policy: consumer::DeliverPolicy::New,
                ..event_stream_config()
            },
        )
        .await
    }

    /// Durable consumer over the advisory capture stream (#169).
    ///
    /// Unlimited redelivery, like the event-stream durables and unlike
    /// the trigger consumer it watches. It used to share the trigger's
    /// bound, on the reasoning that a poison advisory must not redeliver
    /// forever — but a poison advisory never reaches the retry path:
    /// [`crate::control_plane::advisory_watch`] consumes an unparseable
    /// or foreign advisory outright. What NAKs here is transient
    /// bus/stream trouble, and an advisory dropped to a delivery bound
    /// is an exhausted trigger with no dead letter — the one record of
    /// that exhaustion, lost.
    pub async fn advisory_consumer(&self, name: &str) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            "getting/creating durable advisory consumer"
        );
        self.durable(ADVISORY_STREAM_NAME, name, None, event_stream_config())
            .await
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// trigger stream, filtered to all trigger subjects.
    pub async fn trigger_consumer(
        &self,
        name: &str,
        max_ack_pending: i64,
    ) -> Result<consumer::PullConsumer, BusError> {
        self.trigger_consumer_with_filter(name, ALL_TRIGGERS, max_ack_pending)
            .await
    }

    /// Create (or open) a durable JetStream pull consumer on the
    /// trigger stream with an explicit filter subject.
    ///
    /// Work-queue streams require every consumer to be "filtered".
    /// Production callers use [`Self::trigger_consumer`] which
    /// passes the broad `fq.trigger.>` pattern. Tests use
    /// narrower filters (e.g. a specific agent's trigger subject)
    /// so that parallel test consumers do not compete for each
    /// other's messages on the same work-queue stream. NATS
    /// delivers each published trigger to exactly one consumer
    /// whose filter matches; with disjoint per-test filters, tests
    /// do not cross-talk.
    ///
    /// The one durable with a **finite** `max_deliver`, because it is
    /// the one with a dead-letter path: the dispatcher emits a terminal
    /// failure on the final delivery, so an exhausted trigger becomes a
    /// record rather than a silence.
    pub async fn trigger_consumer_with_filter(
        &self,
        name: &str,
        filter_subject: &str,
        max_ack_pending: i64,
    ) -> Result<consumer::PullConsumer, BusError> {
        debug!(
            consumer = name,
            filter = filter_subject,
            max_ack_pending,
            "getting/creating durable trigger consumer"
        );
        self.durable(
            TRIGGER_STREAM_NAME,
            name,
            None,
            consumer::pull::Config {
                filter_subject: filter_subject.to_string(),
                // Explicit ack window, sized by the caller from its
                // concurrency bound (see NATS_DEFAULT_MAX_ACK_PENDING
                // for the floor rationale).
                max_ack_pending,
                // Never retry a poison trigger indefinitely. The dispatcher
                // emits a terminal failure on the last delivery before
                // acknowledging it.
                max_deliver: TRIGGER_MAX_DELIVER,
                // Paces ack-wait redelivery (a crashed dispatcher never
                // reaches the explicit NAK delay in the handle path).
                backoff: TRIGGER_RETRY_BACKOFF.to_vec(),
                ..Default::default()
            },
        )
        .await
    }
}

/// The shape every durable without a dead-letter path starts from:
/// unlimited redelivery, and nothing else assumed. `ack_wait`,
/// `durable_name` and the ack policy are stamped by
/// [`EventBus::durable`].
fn event_stream_config() -> consumer::pull::Config {
    consumer::pull::Config {
        max_deliver: UNLIMITED_MAX_DELIVER,
        ..Default::default()
    }
}

/// Whether an existing durable's configuration differs from this
/// daemon's in any field [`EventBus::durable`] owns.
///
/// Two fields are compared conditionally, and both conditions are the
/// server's rules rather than ours:
///
/// * `max_ack_pending` only when the caller asked for one — a zero in
///   the Rust config means "the server's default", which comes back
///   from the server as its actual number, and comparing those two
///   would report drift on every start and rewrite the consumer for
///   nothing.
/// * `ack_wait` only when there is no `backoff` schedule. JetStream
///   *replaces* a consumer's `ack_wait` with the first `backoff` entry,
///   so a consumer with a schedule always reads back an `ack_wait` it
///   was not given, and comparing them would rewrite it forever. The
///   trigger consumer is the only durable with a schedule, and the
///   consequence — its effective first-delivery ack window is
///   `TRIGGER_RETRY_BACKOFF[0]`, not `[bus] ack_wait_ms` — belongs to
///   exactly-once dispatch (<https://github.com/bricef/factor-q/issues/327>),
///   not here.
fn drifted(existing: &consumer::Config, desired: &consumer::pull::Config) -> bool {
    (desired.backoff.is_empty() && existing.ack_wait != desired.ack_wait)
        || existing.max_deliver != desired.max_deliver
        || existing.backoff != desired.backoff
        || (desired.max_ack_pending != 0 && existing.max_ack_pending != desired.max_ack_pending)
        || existing.filter_subject != desired.filter_subject
        || existing.filter_subjects != desired.filter_subjects
}

#[cfg(test)]
mod tests;
