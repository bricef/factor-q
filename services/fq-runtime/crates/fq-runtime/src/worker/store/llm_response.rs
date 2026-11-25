//! What a successful model call's row keeps for a resume: the
//! `llm.response` it is published in, until that publish has returned
//! (v14, issue 1168).
//!
//! A model call that succeeds closes its row `completed` with its response
//! and its cost, and only then publishes its `llm.response`: SQL first,
//! then NATS, as every WAL write. That event is the one that carries what
//! the call cost, its usage on the payload and its cost on the envelope.
//! Before v14 a publish lost after the close, to a bus fault or a stop,
//! lost the figure from the stream for good: the next start replayed the
//! row and the run went on, and no cost report ever counted the call.
//!
//! So the statement that closes the row keeps the event on it, whole,
//! envelope included, as the wire JSON, for the reason `terminal_outcome`
//! does: a republish is then the lost event exactly, with the same
//! `event_id`, `timestamp` and `parent_event_id`. Once the publish returns
//! the row lets the event go (`response_event` back to `NULL`), so a row
//! that still keeps one is a call whose `llm.response` may not be on the
//! stream, and a resume publishes it again before the run goes on.
//!
//! **Which rows keep one.** A run awaits each publish before it does
//! anything else, and lets the event go before it returns from the call,
//! so only the run's last call can keep one: a run whose drive ended at
//! that call's publish (the bus refused it, or the daemon stopped), or
//! between the publish and the write that lets the event go. A drain or
//! a halt stops a run at a step boundary, after that write, so a run
//! suspended there keeps none. A run the broker refused the event for its
//! size ends on that refusal (#828) and keeps it; nothing resumes a run
//! that has ended.
//!
//! **Why that counts once.** A row that still keeps its event cannot say
//! whether the publish landed, so the resume publishes it either way. When
//! the first copy did land, JetStream's `Nats-Msg-Id` dedup drops the
//! second inside the stream's 120 s `duplicate_window`, and past it the
//! projection, an upsert on `event_id`, keeps one row, which is what every
//! cost query sums. A copy rebuilt from the row's columns would carry a
//! new `event_id`, and a call whose first copy did land would be charged
//! twice. A reader of the raw stream can meet two copies only when the
//! daemon stopped in the moment between the broker storing the event and
//! the write that lets it go, and the next start came more than 120 s
//! later; keyed on `event_id`, they are one.
//!
//! **Why not keep it for good, as `failure_event` is.** An `llm.response`
//! carries the model's whole answer (`parts`), which the row's `response`
//! column already holds, and `llm_dispatch` rows are never deleted: kept
//! for good, every answer would be stored twice. Let go once published,
//! the column holds at most the run's last answer, while it may be owed.
//! The failed call's `llm.failure` stays: its run ends, and the
//! reproduction of that ending publishes it once, whatever it finds.

use sqlx::Row;

use super::{WorkerStore, WorkerStoreError};
use crate::events::Event;

pub(super) const MIGRATION_V14_SQL: &str =
    "ALTER TABLE llm_dispatch ADD COLUMN response_event TEXT;";

impl WorkerStore {
    /// Close a successful call's row, `dispatched` → `completed`, with its
    /// response, its cost, and the `llm.response` it is published in, in
    /// the one statement every close takes. So every row this build closes
    /// on a response keeps its event, and keeping it adds no write after
    /// which a stop would leave the row `dispatched`.
    pub async fn write_llm_responded(
        &self,
        invocation_id: &str,
        request_id: &str,
        response: &str,
        cost_usd: f64,
        completed_at: i64,
        event: &Event,
    ) -> Result<(), WorkerStoreError> {
        let encoded = serde_json::to_string(event)
            .map_err(|err| WorkerStoreError::Malformed(err.to_string()))?;
        let closed = (response, false, cost_usd, completed_at);
        self.close_llm_dispatch(invocation_id, request_id, closed, None, Some(&encoded))
            .await
    }

    /// The call's `llm.response` is out: the row lets it go. A row that
    /// keeps none is left as it is.
    pub async fn mark_llm_response_published(
        &self,
        invocation_id: &str,
        request_id: &str,
    ) -> Result<(), WorkerStoreError> {
        sqlx::query(
            "UPDATE llm_dispatch SET response_event = NULL \
             WHERE invocation_id = ? AND request_id = ?",
        )
        .bind(invocation_id)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The `llm.response`s a run's rows still keep, as `(request_id,
    /// event)`, the event as it was stored, in the order the rows closed.
    /// Read each with `Event::from_wire`, as every consumer of the log
    /// reads an event.
    pub async fn unpublished_llm_responses(
        &self,
        invocation_id: &str,
    ) -> Result<Vec<(String, String)>, WorkerStoreError> {
        let rows = sqlx::query(
            "SELECT request_id, response_event FROM llm_dispatch \
             WHERE invocation_id = ? AND response_event IS NOT NULL \
             ORDER BY seq, completed_at",
        )
        .bind(invocation_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok((row.try_get("request_id")?, row.try_get("response_event")?)))
            .collect()
    }
}
