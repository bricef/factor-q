//! What a drop's event says of the run's archive (issue 1070): `none` only
//! when no row of the run's will ever come on this daemon and no archive is
//! on record.

use fq_runtime::control_plane::store::ControlPlaneStore;
use fq_runtime::events::DropArchive;
use fq_runtime::worker::{RowAtHalt, WorkerStore};

/// Whether an archive of the run may follow its drop, decided once the
/// drop's hold and arm are in place, so nothing this daemon does can start
/// the run's row after it:
///
/// - **A halt that found the run's first write under way, or its row
///   there:** pending. The middle state is never `none`: the row is being
///   written.
/// - **A halt that found no row and none to come, or a run this daemon is
///   not driving:** the worker's row is read, then the archive record, and
///   `none` only when neither is there. Nothing else starts a row for such
///   a run here. A resume begins from a row, and a run queued for a permit
///   or deferred has one. A fresh run is live from before anything names
///   it, and one stopped before its first write is never started again
///   under its id. The worker's row is read first: a finished row leaves
///   the worker only once its archive is on record, so a row moving from
///   one to the other is found in one of them.
/// - **A store that cannot be read:** pending, logged at warn. The drop
///   goes ahead, and the answer that may be wrong is the one that waits
///   for an archive.
pub(super) async fn after_the_drop(
    worker: &WorkerStore,
    control_plane: &ControlPlaneStore,
    row: Option<RowAtHalt>,
    invocation_id: &str,
) -> DropArchive {
    if matches!(
        row,
        Some(RowAtHalt::FirstWriteUnderWay | RowAtHalt::Written)
    ) {
        return DropArchive::Pending;
    }
    let on_the_worker = match worker.get_invocation_state(invocation_id).await {
        Ok(row) => row.is_some(),
        Err(err) => return unreadable(invocation_id, "worker", &err),
    };
    if on_the_worker {
        return DropArchive::Pending;
    }
    match control_plane.get_archive(invocation_id).await {
        Ok(Some(_)) => DropArchive::Pending,
        Ok(None) => DropArchive::WillNotCome,
        Err(err) => unreadable(invocation_id, "control-plane", &err),
    }
}

fn unreadable(invocation_id: &str, store: &str, err: &dyn std::fmt::Display) -> DropArchive {
    tracing::warn!(
        invocation_id,
        store,
        error = %err,
        "could not read whether the dropped run has a row or an archive; its drop says an \
         archive is pending"
    );
    DropArchive::Pending
}

#[cfg(test)]
mod tests;
