//! The rows the archive sweeper holds, kept where the daemon's health
//! reports can read them.
//!
//! A hold is this run's verdict on a row, so it lives in memory. That is
//! also what offers a held row again at the next start: a new run begins
//! with no holds, and its sweeper tries every pending row once before
//! holding any. The store keeps nothing of a hold; the row itself is left
//! exactly as it was.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use fq_ops::health::{ArchiveHold, ArchiveHoldReason};

/// Where the operating guide says what to do about a held row.
pub(super) const GUIDE: &str =
    "docs/guide/operating-the-daemon.md, \"When the archive sweeper holds a terminal row\"";

/// The terminal rows the archive sweeper is holding, by invocation id.
///
/// Shared between the sweeper, which writes it, and `control.status` and
/// `control.doctor`, which list it ([`Self::snapshot`]).
#[derive(Debug, Default)]
pub struct ArchiveHolds {
    rows: Mutex<BTreeMap<String, ArchiveHold>>,
}

impl ArchiveHolds {
    /// Every row held now, in invocation-id order (start order, for the
    /// time-ordered ids the runtime mints).
    pub fn snapshot(&self) -> Vec<ArchiveHold> {
        self.lock().values().cloned().collect()
    }

    /// Hold `row` for `reason`. True when that is news: the row was not
    /// held, or was held for something else (a size refusal under another
    /// limit, or of another event). The sweeper logs news at error level
    /// and the rest at debug, so a held row is one loud line per daemon run
    /// rather than one per pass.
    pub(super) fn hold(
        &self,
        invocation_id: &str,
        agent_id: &str,
        reason: ArchiveHoldReason,
    ) -> bool {
        let remedy = remedy(&reason);
        let mut rows = self.lock();
        let news = rows
            .get(invocation_id)
            .is_none_or(|held| held.reason != reason);
        rows.insert(
            invocation_id.to_string(),
            ArchiveHold {
                invocation_id: invocation_id.to_string(),
                agent_id: agent_id.to_string(),
                reason,
                remedy,
            },
        );
        news
    }

    /// The row is no longer held: it went out this pass.
    pub(super) fn release(&self, invocation_id: &str) {
        self.lock().remove(invocation_id);
    }

    /// Forget every row the store no longer lists as awaiting its archive:
    /// acked and deleted, or removed by an operator.
    pub(super) fn retain_pending(&self, pending: &HashSet<&str>) {
        self.lock().retain(|id, _| pending.contains(id.as_str()));
    }

    /// The `max_payload` the row was refused under, when it is held for an
    /// event too large for the broker.
    pub(super) fn too_large_under(&self, invocation_id: &str) -> Option<u64> {
        match self.lock().get(invocation_id)?.reason {
            ArchiveHoldReason::PayloadTooLarge { limit_bytes, .. } => Some(limit_bytes),
            ArchiveHoldReason::StoredEventRefused { .. }
            | ArchiveHoldReason::MalformedRow { .. } => None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ArchiveHold>> {
        self.rows.lock().expect("archive holds lock poisoned")
    }
}

/// What lifts a hold, as the log line and every report say it.
pub(super) fn remedy(reason: &ArchiveHoldReason) -> String {
    match reason {
        ArchiveHoldReason::PayloadTooLarge { size_bytes, .. } => format!(
            "raise the broker's max_payload above {size_bytes} bytes, then restart fqd: it reads \
             the limit when it connects, and offers the row again at start ({GUIDE})"
        ),
        ArchiveHoldReason::StoredEventRefused { .. } => format!(
            "keep a copy of the row's terminal_outcome, then release the row by setting it to \
             NULL; the next pass archives the invocation without its outcome ({GUIDE})"
        ),
        ArchiveHoldReason::MalformedRow { .. } => format!(
            "the runtime never writes such a field, so the row was edited by hand or written by \
             a build with other rules: keep a copy of the row, then correct the field named; \
             the next pass archives it ({GUIDE})"
        ),
    }
}

#[cfg(test)]
mod tests;
