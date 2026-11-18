//! The write a run makes to its own WAL row once the row exists (issue
//! 1070).
//!
//! `upsert_invocation_state` inserts a row that is missing and updates one
//! still in flight, and silently does neither to a row that is terminal.
//! That is right for a run's first write. After it, a row that is terminal
//! or missing is one someone else finished: an operator's drop, folded
//! into this store, marked it terminal, and the archive hand-off may have
//! deleted it since. The run must notice rather than write over it or
//! bring it back, so its later writes go through
//! [`WorkerStore::update_invocation_state`], which says whether it landed.

use super::{InvocationStateRow, STATE_BLOB_WARN_THRESHOLD_BYTES, WorkerStore, WorkerStoreError};

impl WorkerStore {
    /// Write `row` over the invocation's WAL row, only while that row is
    /// in flight, and never create it. True when the write landed; false
    /// when the row is terminal or missing, and nothing was written. The
    /// columns are those `upsert_invocation_state` writes, and the ones it
    /// leaves to their owners it leaves too.
    pub async fn update_invocation_state(
        &self,
        row: &InvocationStateRow,
    ) -> Result<bool, WorkerStoreError> {
        if row.state_blob.len() > STATE_BLOB_WARN_THRESHOLD_BYTES {
            tracing::warn!(
                invocation_id = %row.invocation_id,
                agent_id = %row.agent_id,
                blob_size_bytes = row.state_blob.len(),
                threshold_bytes = STATE_BLOB_WARN_THRESHOLD_BYTES,
                "state_blob exceeds soft threshold; consider moving to filesystem-backed storage"
            );
        }
        let written = sqlx::query(
            r#"
            UPDATE invocation_state SET
                phase = ?,
                state_blob = ?,
                step_index = ?,
                updated_at = ?,
                terminal_at = ?,
                workspace_ref = ?,
                trigger_source = ?,
                trigger_subject = ?,
                trigger_payload = ?,
                terminal_outcome = ?
            WHERE invocation_id = ? AND terminal_at IS NULL
            "#,
        )
        .bind(&row.phase)
        .bind(&row.state_blob)
        .bind(row.step_index as i64)
        .bind(row.updated_at)
        .bind(row.terminal_at)
        .bind(&row.workspace_ref)
        .bind(&row.trigger_source)
        .bind(&row.trigger_subject)
        .bind(&row.trigger_payload)
        .bind(&row.terminal_outcome)
        .bind(&row.invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(written.rows_affected() == 1)
    }
}
