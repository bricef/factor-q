//! The ownership row's storage: the archive's state the row keeps beside
//! its status, and the one write every move of the row goes through, a
//! compare-and-set on the state the move was decided from. What the moves
//! are is `control_plane::ownership`'s.

use super::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus};
use crate::control_plane::ownership::RowState;

/// The rung that adds `archive_state` beside `status` (issue 1024). Every
/// row a control plane holds when it runs records a run that has not
/// ended (`not_due`) or one whose archive record was written: before the
/// column, only the archive hand-off and an operator's drop wrote a
/// finished status, and each wrote the archive record first.
///
/// Its number is where `MIGRATIONS` lists it, so a change that adds a
/// rung before it renumbers it by that one line; nothing else names it.
///
/// The `CHECK` covers the column's values, not which pairs of status and
/// archive state a row may hold (`RowState::is_valid`). A check across
/// both columns would need the table rebuilt, since SQLite adds no table
/// constraint to an existing table; the pairs are kept by the writers
/// instead. `control_plane::ownership::apply` is the one production
/// writer and moves only between valid pairs, and the seeding helpers
/// tests use (`assign_invocation`, `upsert_invocation_ownership`) write
/// valid pairs too.
pub(super) const ARCHIVE_STATE_SQL: &str = r#"
ALTER TABLE coordination_invocation_owner
    ADD COLUMN archive_state TEXT NOT NULL DEFAULT 'not_due'
    CHECK (archive_state IN ('not_due', 'pending', 'archived'));
UPDATE coordination_invocation_owner SET archive_state = 'archived'
    WHERE status IN ('completed', 'failed');
"#;

/// Whether the control plane holds a run's archive record: the state an
/// ownership row keeps beside its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// column existed: what the rung that adds it (`ARCHIVE_STATE_SQL`)
    /// gives such a row.
    pub fn before_the_column(status: OwnerStatus) -> Self {
        match status {
            OwnerStatus::InFlight | OwnerStatus::Ambiguous => ArchiveState::NotDue,
            OwnerStatus::Completed | OwnerStatus::Failed => ArchiveState::Archived,
        }
    }
}

impl ControlPlaneStore {
    /// Move `invocation_id`'s ownership row to `to`, only from `expected`:
    /// insert it when `expected` is no row, and update it when the row
    /// still holds exactly `expected`. `worker_id` and `assigned_at`
    /// replace the row's when given, and a row this inserts needs both.
    ///
    /// One statement either way, so the condition and the write cannot be
    /// separated. True when the row moved; false when it was not in
    /// `expected` and nothing was written, which tells the caller to read
    /// the row again and decide again (`control_plane::ownership::apply`).
    pub async fn compare_and_set_ownership(
        &self,
        invocation_id: &str,
        expected: Option<RowState>,
        to: RowState,
        worker_id: Option<&str>,
        assigned_at: Option<i64>,
    ) -> Result<bool, ControlPlaneStoreError> {
        // `apply` moves only between pairs a row may hold; a write of any
        // other is a caller's mistake (the column's CHECK covers values,
        // not pairs).
        debug_assert!(to.is_valid(), "an ownership row cannot hold {}", to.name());
        let written = match expected {
            None => {
                let (Some(worker_id), Some(assigned_at)) = (worker_id, assigned_at) else {
                    return Err(ControlPlaneStoreError::Malformed(format!(
                        "the ownership row of {invocation_id} cannot be inserted without its \
                         worker and time"
                    )));
                };
                sqlx::query(
                    "INSERT INTO coordination_invocation_owner \
                         (invocation_id, worker_id, assigned_at, status, archive_state) \
                     VALUES (?, ?, ?, ?, ?) \
                     ON CONFLICT(invocation_id) DO NOTHING",
                )
                .bind(invocation_id)
                .bind(worker_id)
                .bind(assigned_at)
                .bind(to.status.as_str())
                .bind(to.archive.as_str())
                .execute(&self.pool)
                .await?
            }
            Some(expected) => {
                sqlx::query(
                    "UPDATE coordination_invocation_owner \
                     SET status = ?, archive_state = ?, \
                         worker_id = COALESCE(?, worker_id), \
                         assigned_at = COALESCE(?, assigned_at) \
                     WHERE invocation_id = ? AND status = ? AND archive_state = ?",
                )
                .bind(to.status.as_str())
                .bind(to.archive.as_str())
                .bind(worker_id)
                .bind(assigned_at)
                .bind(invocation_id)
                .bind(expected.status.as_str())
                .bind(expected.archive.as_str())
                .execute(&self.pool)
                .await?
            }
        };
        Ok(written.rows_affected() == 1)
    }

    /// Write `status`, any text, over `invocation_id`'s ownership row, as a
    /// hand edit or a newer build would: how a test holds a row this build
    /// cannot read. True when a row was there to write over.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn overwrite_owner_status_for_test(
        &self,
        invocation_id: &str,
        status: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        let written = sqlx::query(
            "UPDATE coordination_invocation_owner SET status = ? WHERE invocation_id = ?",
        )
        .bind(status)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(written.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests;
