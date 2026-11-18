//! Moves of an invocation's ownership row that must not land on a row in
//! the wrong state, written as compare-and-set on the status they expect,
//! and the archive's state the row keeps beside its status.

use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus};

/// The rung that adds `archive_state` beside `status` (issue 1024). Every
/// row a control plane holds when it runs records a run that has not
/// ended (`not_due`) or one whose archive record was written: before the
/// column, only the archive hand-off and an operator's drop wrote a
/// finished status, and each wrote the archive record first.
///
/// Its number is where `MIGRATIONS` lists it, so a change that adds a
/// rung before it renumbers it by that one line.
pub(super) const ARCHIVE_STATE_V3_SQL: &str = r#"
ALTER TABLE coordination_invocation_owner
    ADD COLUMN archive_state TEXT NOT NULL DEFAULT 'not_due'
    CHECK (archive_state IN ('not_due', 'pending', 'archived'));
UPDATE coordination_invocation_owner SET archive_state = 'archived'
    WHERE status IN ('completed', 'failed');
"#;

/// Whether the control plane holds a run's archive record: the state an
/// ownership row keeps beside its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveState {
    /// The run has not ended, so no archive is due.
    NotDue,
    /// The run has ended, and its archive record is not written yet: the
    /// worker's `invocation.archived` is on its way, refused, held by the
    /// archive sweeper, or its worker is gone.
    Pending,
    /// The archive record was written, from the worker's archive or by an
    /// operator's drop. A record retention later sweeps leaves the row
    /// `archived`.
    Archived,
}

impl ArchiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveState::NotDue => "not_due",
            ArchiveState::Pending => "pending",
            ArchiveState::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "not_due" => Some(ArchiveState::NotDue),
            "pending" => Some(ArchiveState::Pending),
            "archived" => Some(ArchiveState::Archived),
            _ => None,
        }
    }

    /// The archive's state of a row with `status` written before the
    /// column existed: what the schema's third rung gives such a row.
    pub fn recorded_before_v3(status: OwnerStatus) -> Self {
        match status {
            OwnerStatus::InFlight | OwnerStatus::Ambiguous => ArchiveState::NotDue,
            OwnerStatus::Completed | OwnerStatus::Failed => ArchiveState::Archived,
        }
    }
}

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
    /// when there is none, and moves it there only from `in_flight`. Any
    /// other row is left as it was. A finished run is never put back to
    /// triage: an `invocation.ambiguous` can reach the control plane after
    /// the run's ending, and the ending stands. When the write changes
    /// nothing, the row's status is read in the same transaction, so the
    /// answer names the status that refused the move.
    ///
    /// The move from `in_flight` also writes `worker_id` and `assigned_at`
    /// over the row's, so a row startup recovery restored under this
    /// daemon's worker id ends up naming the agent id the caller passes
    /// in its place. That is known and not intended: the next change of
    /// issue 1024 keeps the row's worker id and time on such a move.
    pub async fn mark_invocation_ambiguous(
        &self,
        invocation_id: &str,
        worker_id: &str,
        assigned_at: i64,
    ) -> Result<AmbiguousMark, ControlPlaneStoreError> {
        // `BEGIN IMMEDIATE` takes the write lock before the first
        // statement, so no other writer can move the row between the write
        // and the read that explains it, whatever the first statement is.
        // A deferred `BEGIN` would hold only while the write comes first: a
        // read placed ahead of it would make the write a read-to-write
        // upgrade, which SQLite fails at once under contention instead of
        // waiting out the busy timeout (`db/schema.rs` says the same of
        // the schema lock).
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
                Some(finished @ (OwnerStatus::Completed | OwnerStatus::Failed)) => {
                    AmbiguousMark::Refused(finished)
                }
                // The write moves an `in_flight` row, so a row that reads
                // `in_flight` after it changed nothing contradicts the
                // write. An error, not a refusal: the caller's event is
                // retried rather than acked with the report lost.
                Some(OwnerStatus::InFlight) => {
                    return Err(ControlPlaneStoreError::Backend(format!(
                        "the ownership row of {invocation_id} reads in_flight after the write \
                         that moves an in_flight row changed nothing"
                    )));
                }
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

#[cfg(test)]
mod tests;
