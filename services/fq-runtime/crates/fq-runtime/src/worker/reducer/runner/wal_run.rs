//! What this runner's WAL holds of one run, for the `Worker` port the
//! trigger dispatcher asks (issue 967).

use super::{ReducerRunner, map_store_err};
use crate::worker::reducer::Reducer;
use crate::worker::{ExecutorError, WalRun};

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Where this runner's WAL holds `invocation_id`'s run, read by the
    /// row's primary key and mapped as the startup scan maps it
    /// ([`WalRun::of_row`]). `None` when the WAL holds no row for it.
    pub(crate) async fn wal_run_of(
        &self,
        invocation_id: &str,
    ) -> Result<Option<WalRun>, ExecutorError> {
        self.config
            .store
            .durable_start_of(invocation_id)
            .await
            .map(|terminal| terminal.map(WalRun::of_row))
            .map_err(map_store_err)
    }
}
