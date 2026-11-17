//! The runs this WAL holds, as startup recovery reports them to the control
//! plane (#959, #967).

use sqlx::Row;

use super::{WorkerStore, WorkerStoreError};

impl WorkerStore {
    /// Every invocation this WAL holds a row for, with whether that row is
    /// terminal: the runs in flight, and the finished runs whose archive the
    /// control plane has not yet acknowledged. A row is written by an
    /// invocation's first WAL write and deleted once its archive is
    /// acknowledged, so this is every run of this worker that has durably
    /// started and that the WAL has not yet let go.
    ///
    /// Two columns, not the row, so no state blob is copied out. The read
    /// still walks each blob's pages: `terminal_at` sits after
    /// `state_blob` in the row, and SQLite follows a large blob's overflow
    /// chain to reach it. The review of PR 1030 measured fifty rows of
    /// 2 MB blobs at 12.8 ms this way, against 66.7 ms for the whole rows.
    /// It runs once, at startup, over the runs in flight and the finished
    /// ones awaiting their archive's acknowledgement, which is normally a
    /// handful.
    pub async fn list_durable_starts(&self) -> Result<Vec<(String, bool)>, WorkerStoreError> {
        let rows = sqlx::query(
            "SELECT invocation_id, terminal_at IS NOT NULL AS terminal FROM invocation_state",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| (row.get("invocation_id"), row.get("terminal")))
            .collect())
    }

    /// Whether this WAL holds a row for `invocation_id`, and if it does,
    /// whether that row is terminal: one run's line of
    /// [`Self::list_durable_starts`], read the same way, by the row's
    /// primary key.
    ///
    /// The trigger dispatcher asks it, through the `Worker` port, about a
    /// claim its own process holds that no copy of the trigger is handling
    /// any more (issue 967): a row here is a start on record, and the
    /// trigger must not be started again.
    pub async fn durable_start_of(
        &self,
        invocation_id: &str,
    ) -> Result<Option<bool>, WorkerStoreError> {
        let row = sqlx::query(
            "SELECT terminal_at IS NOT NULL AS terminal FROM invocation_state \
             WHERE invocation_id = ?",
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| row.get("terminal")))
    }
}
