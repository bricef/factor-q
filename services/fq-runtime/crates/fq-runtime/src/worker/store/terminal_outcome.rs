//! The terminal outcome persisted on `invocation_state` in v12 (#823).
//!
//! The run loop marks a row terminal *before* it publishes `completed` —
//! the SQL-first, then-NATS order of `data-architecture.md` §5.5 — and
//! `completed` is the only event that carries the outcome: task status,
//! result summary, cost and totals. Before v12 a publish lost after that
//! write left a terminal row the archive sweeper could only answer with a
//! bare `invocation.archived`, and the outcome was gone from every read
//! model. So the terminal event rides the terminal write itself, and the
//! sweeper republishes it until it is stamped published.
//!
//! The column holds the **whole event, envelope included**, as the JSON
//! [`EventBus::publish`](crate::bus::EventBus::publish) puts on the wire —
//! not the payload with its `event_id` beside it. A republish is then the
//! lost event exactly: the same `event_id`, so JetStream's `Nats-Msg-Id`
//! dedup and the projection's `event_id` upsert each see one event, never
//! two; the same `timestamp`, so the projection does not re-date the
//! completion to when the sweeper healed it; and the same
//! `parent_event_id`, so the chain still holds. A payload-plus-id column
//! would have to rebuild an envelope that differs from the lost one in
//! everything but its id. One JSON column rather than six typed ones, too,
//! so `failed` can share it: [`EventPayload`](crate::events::EventPayload)
//! is the enum either terminal event already is.

use crate::events::Event;
use crate::worker::store::{InvocationStateRow, WorkerStore, WorkerStoreError};

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
    /// a row that stored none — an operator drop, a `failed` terminal, or a
    /// row written before v12 — whose sweep is `invocation.archived` alone.
    pub(crate) fn unpublished_terminal_event(&self) -> Result<Option<Event>, WorkerStoreError> {
        match (&self.terminal_outcome, self.terminal_published_at) {
            (Some(json), None) => serde_json::from_str(json).map(Some).map_err(|err| {
                WorkerStoreError::Malformed(format!("invalid terminal outcome: {err}"))
            }),
            _ => Ok(None),
        }
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
