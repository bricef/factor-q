//! The terminal outcome persisted on `invocation_state` in v12 (#823).
//!
//! The run loop marks a row terminal *before* it publishes the event the
//! run ends in — `completed`, or a `failed` the reducer itself decides
//! (#946) — the SQL-first, then-NATS order of `data-architecture.md`
//! §5.5. That event is the only one that carries the outcome: task
//! status, result summary, cost and totals, or the failure's kind,
//! message, phase and partial totals. Before v12 a publish lost after that
//! write left a terminal row the archive sweeper could only answer with a
//! bare `invocation.archived`, and the outcome was gone from every read
//! model. So the terminal event rides the terminal write itself, and the
//! sweeper republishes it until it is stamped published.
//!
//! The column holds the **whole event, envelope included**, as the JSON
//! [`EventBus::publish`](crate::bus::EventBus::publish) puts on the wire —
//! not the payload with its `event_id` beside it. A republish is then the
//! lost event exactly. It has the same `event_id`: delivery is
//! at-least-once, and when a copy did land, JetStream's `Nats-Msg-Id` dedup
//! drops the republish inside the stream's 120 s `duplicate_window`, while
//! past it a consumer idempotent on `event_id` (the projection's upsert)
//! still sees one event. It has the same `timestamp`, so the projection
//! does not re-date the completion to when the sweeper healed it, and the
//! same `parent_event_id`, so the chain still holds. A payload-plus-id column
//! would have to rebuild an envelope that differs from the lost one in
//! everything but its id. One JSON column rather than six typed ones, too,
//! so `failed` shares it: [`EventPayload`] is the enum either terminal
//! event already is.
//!
//! The stored event is read back through [`Event::from_wire`], as every
//! consumer of the log reads it, and not with plain serde. An event stored
//! by an older build and still unpublished across an envelope bump may
//! well parse against this build's types, but its `schema_version` is one
//! no consumer reads, and republished it would halt every durable consumer
//! on the stream. Nor is anything published that could not be this row's
//! terminal event: an event of another invocation, or one that is not
//! `completed` or `failed` (an unknown `event_type` reads as
//! [`EventPayload::Unknown`], which is neither). Each of those is a
//! [`HoldReason`], and the sweeper holds the row rather than publish it.

use uuid::Uuid;

use crate::events::{Event, EventParseError, EventPayload};
use crate::worker::store::{InvocationStateRow, WorkerStore, WorkerStoreError};

/// Why a row's stored terminal event is held rather than republished.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HoldReason {
    /// Not an event this build reads: malformed, or an envelope version
    /// outside `SUPPORTED_SCHEMA_VERSIONS` (its message names both).
    #[error("stored terminal event is unreadable: {0}")]
    Unreadable(#[from] EventParseError),
    /// A readable event that is not a terminal one.
    #[error("stored event is `{0}`, not `completed` or `failed`")]
    NotTerminal(&'static str),
    /// A terminal event, but another invocation's.
    #[error("stored event belongs to invocation {0}, not this row's")]
    OtherInvocation(Uuid),
}

pub(super) const MIGRATION_V12_SQL: &str = r#"
ALTER TABLE invocation_state ADD COLUMN terminal_outcome TEXT;
ALTER TABLE invocation_state ADD COLUMN terminal_published_at INTEGER;
"#;

/// Serialise a terminal event for [`InvocationStateRow::terminal_outcome`].
pub(crate) fn encode(event: &Event) -> Result<String, WorkerStoreError> {
    serde_json::to_string(event).map_err(|err| WorkerStoreError::Malformed(err.to_string()))
}

impl InvocationStateRow {
    /// The terminal event this row still owes the bus: its stored outcome,
    /// unless `terminal_published_at` says it already went out. `None` for
    /// a row that stored none — an operator drop, a mid-step `failed`
    /// (published before the row was marked), a reducer-decided `failed`
    /// written by a build before #946, or a row written before v12 —
    /// whose sweep is `invocation.archived` alone.
    /// `Err` when the stored event must not be published (see the module
    /// doc).
    pub(crate) fn unpublished_terminal_event(&self) -> Result<Option<Event>, HoldReason> {
        let (Some(json), None) = (&self.terminal_outcome, self.terminal_published_at) else {
            return Ok(None);
        };
        let event = Event::from_wire(json.as_bytes())?;
        if !matches!(
            event.payload,
            EventPayload::Completed(_) | EventPayload::Failed(_)
        ) {
            return Err(HoldReason::NotTerminal(event.payload.event_type()));
        }
        if event.envelope.invocation_id.to_string() != self.invocation_id {
            return Err(HoldReason::OtherInvocation(event.envelope.invocation_id));
        }
        Ok(Some(event))
    }
}

impl WorkerStore {
    /// Stamp the row's stored terminal event as published, so the archive
    /// sweeper stops republishing it. Called by whichever publish succeeds
    /// first — the run loop's, or the sweeper's after a lost one. The first
    /// stamp stands (a second is a no-op returning `0`), and a row with no
    /// stored outcome is never stamped, so a stamp always names an event.
    pub async fn mark_terminal_published(
        &self,
        invocation_id: &str,
        published_at: i64,
    ) -> Result<u64, WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE invocation_state
            SET terminal_published_at = ?
            WHERE invocation_id = ?
              AND terminal_outcome IS NOT NULL
              AND terminal_published_at IS NULL
            "#,
        )
        .bind(published_at)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}
