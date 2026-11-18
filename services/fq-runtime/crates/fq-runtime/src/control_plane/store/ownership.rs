//! Moves of an invocation's ownership row that must not land on a row in
//! the wrong state, written as compare-and-set on the status they expect.

use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus};

/// What [`ControlPlaneStore::mark_invocation_ambiguous`] did to the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbiguousMark {
    /// The row was absent or `in_flight`, and now reads `ambiguous`.
    Marked,
    /// The row already read `ambiguous`, and is left exactly as it was:
    /// the same report, delivered again.
    AlreadyAmbiguous,
    /// The row records a finished run, `completed` or `failed` (an
    /// operator's drop is `failed`), and is left exactly as it was.
    Refused(OwnerStatus),
}

impl ControlPlaneStore {
    /// Mark an invocation awaiting triage, unless its row says it has
    /// finished.
    ///
    /// One conditional write decides: it inserts the row as `ambiguous`
    /// when there is none, and moves it there only from `in_flight`, taking
    /// `worker_id` and `assigned_at` as it does. Any other row is left as
    /// it was. A finished run is never put back to triage: an
    /// `invocation.ambiguous` can reach the control plane after the run's
    /// ending, and the ending stands. When the write changes nothing, the
    /// row's status is read in the same transaction, which the write has
    /// already made a write transaction, so no other writer can move the
    /// row between the two and the answer names the status that refused
    /// the move.
    pub async fn mark_invocation_ambiguous(
        &self,
        invocation_id: &str,
        worker_id: &str,
        assigned_at: i64,
    ) -> Result<AmbiguousMark, ControlPlaneStoreError> {
        let mut tx = self.pool.begin().await?;
        let written = sqlx::query(
            "INSERT INTO coordination_invocation_owner \
                 (invocation_id, worker_id, assigned_at, status) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(invocation_id) DO UPDATE SET \
                 worker_id = excluded.worker_id, \
                 assigned_at = excluded.assigned_at, \
                 status = excluded.status \
             WHERE coordination_invocation_owner.status = ?",
        )
        .bind(invocation_id)
        .bind(worker_id)
        .bind(assigned_at)
        .bind(OwnerStatus::Ambiguous.as_str())
        .bind(OwnerStatus::InFlight.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let mark = if written == 1 {
            AmbiguousMark::Marked
        } else {
            let row = sqlx::query(
                "SELECT status FROM coordination_invocation_owner WHERE invocation_id = ?",
            )
            .bind(invocation_id)
            .fetch_one(&mut *tx)
            .await?;
            let status: String = row.get("status");
            match OwnerStatus::parse(&status) {
                Some(OwnerStatus::Ambiguous) => AmbiguousMark::AlreadyAmbiguous,
                Some(finished) => AmbiguousMark::Refused(finished),
                None => {
                    return Err(ControlPlaneStoreError::Malformed(format!(
                        "unknown owner status `{status}`"
                    )));
                }
            }
        };
        tx.commit().await?;
        Ok(mark)
    }
}
