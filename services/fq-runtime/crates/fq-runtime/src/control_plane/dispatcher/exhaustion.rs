//! What the dead-letter watch asks before it calls a trigger exhausted
//! (issue 967).
//!
//! The decision lands in the next commit; until then every exhausted
//! trigger is dead-lettered, as before.

use std::sync::Arc;

use crate::control_plane::ControlPlaneStore;
use crate::worker::Worker;

use super::TriggerDispatcher;
use super::own_claim::Handling;

/// What the dead-letter watch consults about a trigger whose deliveries
/// ran out: the dispatcher's claim store, the worker that answers for its
/// WAL, and the copies of triggers the dispatcher is handling.
#[derive(Clone)]
pub struct ExhaustionCheck {
    store: Arc<ControlPlaneStore>,
    worker: Arc<dyn Worker>,
    handling: Handling,
    stream_epoch: i64,
}

/// The watch's verdict on one exhausted trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
// Constructed by the decision, which lands in the next commit.
#[allow(dead_code)]
pub(crate) enum Exhausted {
    /// Nothing of the trigger's run is on record: dead-letter it.
    NothingOnRecord,
    /// The trigger's run has a start on record: not dead-lettered.
    Started {
        /// The invocation the claim names.
        invocation_id: String,
        /// Where the start is on record, for the log line.
        on_record: String,
        /// Whether this check marked the claim, which was `claimed`.
        marked: bool,
    },
    /// Not decided yet, for the reason given: decide again later.
    Undecided(String),
}

impl ExhaustionCheck {
    /// The verdict on the trigger at `stream_seq` of the stream this
    /// dispatcher consumes.
    pub(crate) async fn verdict(&self, stream_seq: u64) -> Exhausted {
        let _ = (
            &self.store,
            &self.worker,
            &self.handling,
            self.stream_epoch,
            stream_seq,
        );
        Exhausted::NothingOnRecord
    }
}

impl TriggerDispatcher {
    /// What the dead-letter watch consults before it calls one of this
    /// dispatcher's triggers exhausted, or `None` when the dispatcher
    /// holds no claim store, and so no records to consult.
    pub fn exhaustion_check(&self) -> Option<ExhaustionCheck> {
        let (store, _) = self.claim_store.as_ref()?;
        Some(ExhaustionCheck {
            store: Arc::clone(store),
            worker: Arc::clone(&self.worker),
            handling: self.handling.clone(),
            stream_epoch: self.bus.trigger_stream_epoch(),
        })
    }
}
