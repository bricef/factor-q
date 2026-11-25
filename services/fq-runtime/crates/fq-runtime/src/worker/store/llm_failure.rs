//! What a failed model call's row keeps for a resume: the stamp of a
//! deferral decided on it (v10, #278). A resume reads an errored row by
//! it: a stamped row is the 429 a deferral was decided on, and is
//! skipped; any other is a failure whose ending was lost.
//!
//! Moved here unchanged from `store.rs`, which sits at its size budget,
//! so the next thing a failed call's row keeps (issue 1108) has room.

use super::{DispatchStatus, WorkerStore, WorkerStoreError};

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
}
