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
    /// Two columns, not the row: what is asked is which runs exist, and a
    /// finished run's state blob can be large.
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
}
