//! What a failed model call's row keeps for a resume: the stamp of a
//! deferral decided on it (v10, #278), or the `llm.failure` it ended in
//! (v13, issue 1108). A resume reads an errored row by these two: a
//! stamped row is the 429 a deferral was decided on, and is skipped; any
//! other is a failure whose ending was lost, and its `llm.failure` is
//! published again before the run's `failed`.
//!
//! **The `llm.failure`.** A model call that fails closes its row
//! `completed` with `is_error` and the call's cost, and only then
//! publishes the call's `llm.dispatched` and `llm.failure`: SQL first,
//! then NATS, as every WAL write. The `llm.failure` is the one event that
//! carries what the call cost, its usage on the payload and its cost on
//! the envelope. Before v13 a stop between the row's close and those
//! publishes lost the figure from the stream for good: the next start
//! found the row closed and reproduced only the run's `failed`, so no
//! cost report ever counted the call. Now the row keeps the event, written
//! by the statement that closes the row, and a resume publishes it again.
//!
//! The column holds the **whole event, envelope included**, as the wire
//! JSON, for the reason `terminal_outcome` does: a republish is then the
//! lost event exactly, with the same `event_id`, `timestamp` and
//! `parent_event_id`. That is what makes it count once. A resume cannot
//! tell an event that was lost from one that reached the stream before
//! the daemon stopped, so it publishes it either way. When the first copy
//! did land, JetStream's `Nats-Msg-Id` dedup drops the second inside the
//! stream's 120 s `duplicate_window`, and past it the projection, an
//! upsert on `event_id`, keeps one row, which is what every cost query
//! sums. A copy rebuilt from the row's columns would carry a new
//! `event_id`, and a run whose first copy did land would be charged
//! twice.
//!
//! `llm.dispatched` is not kept. It carries no usage and no cost, nothing
//! the `llm.failure` does not repeat (`call_id`, `model`), and the
//! projection gives it no cost column.

use sqlx::Row;

use super::{DispatchStatus, WorkerStore, WorkerStoreError};
use crate::events::Event;

pub(super) const MIGRATION_V13_SQL: &str =
    "ALTER TABLE llm_dispatch ADD COLUMN failure_event TEXT;";

impl WorkerStore {
    /// Stamp the errored row a deferral was decided on (#278). Only a
    /// `completed` row closed `is_error` qualifies — the 429 the retry
    /// layer gave up on is recorded there before the deferral is
    /// decided — so a stamp that finds no such row is a WAL transition
    /// error, not a silent no-op.
    pub async fn mark_llm_deferred(
        &self,
        invocation_id: &str,
        request_id: &str,
        deferred_at: i64,
    ) -> Result<(), WorkerStoreError> {
        let res = sqlx::query(
            r#"
            UPDATE llm_dispatch
            SET deferred_at = ?
            WHERE invocation_id = ? AND request_id = ? AND status = ? AND is_error = 1
            "#,
        )
        .bind(deferred_at)
        .bind(invocation_id)
        .bind(request_id)
        .bind(DispatchStatus::Completed.as_str())
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(WorkerStoreError::WalTransitionFailed {
                entity: "llm_dispatch",
                invocation_id: invocation_id.to_string(),
                call_id: request_id.to_string(),
                reason: "no completed error row to mark deferred".to_string(),
            });
        }
        Ok(())
    }

    /// Close a failed call's row, `dispatched` → `completed` with
    /// `is_error`, its cost, and the `llm.failure` it ends in, in the one
    /// statement `write_llm_completed` closes a row with. So every row this
    /// build closes as failed keeps its event, and keeping it adds no write
    /// after which a stop would leave the row `dispatched`.
    pub async fn write_llm_failed(
        &self,
        invocation_id: &str,
        request_id: &str,
        error_message: &str,
        cost_usd: f64,
        completed_at: i64,
        failure: &Event,
    ) -> Result<(), WorkerStoreError> {
        let encoded = serde_json::to_string(failure)
            .map_err(|err| WorkerStoreError::Malformed(err.to_string()))?;
        let closed = (error_message, true, cost_usd, completed_at);
        self.close_llm_dispatch(invocation_id, request_id, closed, Some(&encoded))
            .await
    }

    /// The `llm.failure` a call's row keeps, as it was stored: `None` for a
    /// row that keeps none (one that did not fail, or was closed by a build
    /// before v13). Read it with `Event::from_wire`, as every consumer of
    /// the log reads an event.
    pub async fn llm_failure_event(
        &self,
        invocation_id: &str,
        request_id: &str,
    ) -> Result<Option<String>, WorkerStoreError> {
        let row = sqlx::query(
            "SELECT failure_event FROM llm_dispatch WHERE invocation_id = ? AND request_id = ?",
        )
        .bind(invocation_id)
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| row.get::<Option<String>, _>("failure_event")))
    }
}
