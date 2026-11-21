//! What one operator's drop found, as `fq invocation drop` reports it
//! (issue 1070).
//!
//! The record keeps the first termination request only, so its reading
//! ([`InvocationSummaryView::termination_reading`]) is the first request's.
//! A later drop is reported by its own time against the run's ending, and
//! names the earlier request that the record keeps.

use serde::{Deserialize, Serialize};

use super::{
    InvocationArchiveState, InvocationEnder, InvocationSummaryView, TerminationRequestView,
};

/// What one drop found, read from its own time against the run's ending.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DropCase {
    /// This drop ended the run.
    Dropped,
    /// An earlier drop had already ended the run.
    EndedByAnEarlierDrop,
    /// The run had already ended when this drop was requested.
    AlreadyEnded,
    /// This drop was requested while the run ran, and the run reached its
    /// own ending first.
    ReachedItsOwnEndingFirst,
}

impl DropCase {
    /// The phrase the drop's report reads it as.
    pub fn phrase(self) -> &'static str {
        match self {
            DropCase::Dropped => "dropped",
            DropCase::EndedByAnEarlierDrop => "already ended by an earlier drop",
            DropCase::AlreadyEnded => "already ended when the termination was requested",
            DropCase::ReachedItsOwnEndingFirst => {
                "requested while running; it reached its own ending first"
            }
        }
    }
}

impl InvocationSummaryView {
    /// What the drop requested at `requested_at_ms`, its event's own time,
    /// found, as the row read after its fold says: this drop ended the run
    /// (the request on record is this one), an earlier drop had, or the
    /// run reached its own ending before or after this request. `None`
    /// when the row does not say: no ender on record (a row from before
    /// the record), or the run has not ended.
    pub fn drop_case(&self, requested_at_ms: i64) -> Option<DropCase> {
        match (self.ended_by?, self.ended_at_ms) {
            (InvocationEnder::Drop, _) if self.earlier_request(requested_at_ms).is_none() => {
                Some(DropCase::Dropped)
            }
            (InvocationEnder::Drop, _) => Some(DropCase::EndedByAnEarlierDrop),
            (InvocationEnder::Run, Some(ended_at)) => Some(if ended_at <= requested_at_ms {
                DropCase::AlreadyEnded
            } else {
                DropCase::ReachedItsOwnEndingFirst
            }),
            (InvocationEnder::Run, None) => None,
        }
    }

    /// The request on record when it is not the one made at
    /// `requested_at_ms`: an earlier one, which the record keeps, since it
    /// keeps the first. Told apart by the request's time, the drop
    /// event's own.
    pub fn earlier_request(&self, requested_at_ms: i64) -> Option<&TerminationRequestView> {
        self.termination
            .as_ref()
            .filter(|request| request.requested_at_ms != requested_at_ms)
    }

    /// Whether the row's ending can still be replaced: a drop's until the
    /// run's archive lands, and any ending whose archive will not come
    /// (`none`). The run's own ending, or an archive saying `completed`,
    /// replaces it if the run reached it first.
    pub fn ending_is_provisional(&self) -> bool {
        (self.ended_by == Some(InvocationEnder::Drop)
            && self.archive_state != InvocationArchiveState::Archived)
            || self.archive_state == InvocationArchiveState::WillNotCome
    }
}

#[cfg(test)]
mod tests;
