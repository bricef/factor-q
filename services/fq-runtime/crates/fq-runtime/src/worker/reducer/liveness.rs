//! The runner's live registry: which invocations this process is
//! driving *right now*, whose work each one is, and which of them an
//! operator has asked to halt. Split from `runner.rs` (the `rounds`
//! precedent) because it is one concept, not three fields — and because
//! it is the concept two operator preconditions rest on.
//!
//! It is the system's only **zero-lag** authority on liveness. Every
//! durable answer to "is this invocation running" is behind it by
//! construction: the projection and the coordination owner rows are
//! folded by asynchronous durable consumers, and nothing on the
//! dispatch path writes an owner row at all. The process doing the work
//! knows first, and it knows without asking anyone.
//!
//! That is why the registry keys on the invocation and *values* the
//! agent rather than being a bare set. Liveness and identity are the
//! same fact here — this runner cannot be driving an invocation without
//! knowing whose it is — so an operator command that has established
//! "this is live" can resolve it without falling back to a store that
//! may not have heard yet. `invocation.drop` depends on exactly that:
//! having armed a halt on the strength of this registry, it must never
//! then report the invocation unknown (#107).
//!
//! Worker-local by construction; cross-worker liveness is the
//! #107/#374 coordination story.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::AgentId;

/// The invocations this runner is driving, and the halts armed against
/// them. Entries live exactly as long as the drive does — see
/// [`ActiveInvocation`].
#[derive(Default)]
pub(crate) struct LiveRegistry {
    /// Invocation → the agent it is being driven for, registered for
    /// the duration of every run/resume entry.
    active: Mutex<HashMap<Uuid, AgentId>>,
    /// Invocation-scoped halt requests, consumed at the next step
    /// boundary. Unlike daemon drain, these terminate only the named
    /// invocation after the operator-drop event reconciles its durable
    /// row.
    halt_requested: Mutex<HashSet<Uuid>>,
    /// Invocations whose WAL row this drive has written, or was resumed
    /// from. A run in this set whose row is gone was finished and handed
    /// off by someone else, not one that never started (issue 1070): the
    /// drive's own knowledge of its own write, the same fact its
    /// `DurableStart` holds, kept where the failure paths can read it.
    wrote_row: Mutex<HashSet<Uuid>>,
    /// Drives whose first WAL write is under way, each with the words of
    /// the drops whose halts were armed during it, if any were. Begun under
    /// the `active` lock a halt is armed under
    /// ([`Self::begin_first_write`]), so a halt can say whether the run's
    /// row is to come ([`RowAtHalt`]).
    first_writes: Mutex<HashMap<Uuid, Option<DropWords>>>,
}

/// The words of the drops whose halts found a run's first write under way:
/// one sender for each such drop ([`DropWord`]), and the receiver the run
/// reads once its write has landed.
struct DropWords {
    word: mpsc::Sender<String>,
    heard: mpsc::Receiver<String>,
}

/// Where a run's own WAL row stood when an operator's halt was armed on
/// it (issue 1070). Exact at the moment of the arm: every change to it is
/// made under the `active` lock the arm holds, or ordered against the
/// arm's reads (`LiveRegistry::arm_halt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAtHalt {
    /// None, and none will come from this drive: its first write has not
    /// begun, and takes the halt and is not made, or it failed.
    NeverWritten,
    /// The run's first write is under way. If it lands, the run takes the
    /// halt at once and stops before its first model call, and marks its
    /// row as the drop's fold does once the drop that armed the halt says
    /// it is published ([`DropWord`]), since that fold may have come before
    /// the row existed. If it fails, the run stops as halted before its
    /// first write, with no row.
    FirstWriteUnderWay,
    /// The row is there, for the drop's mark to reach: the run's first
    /// write landed, or the drive was resumed from a row. A resume reads
    /// its row after it is entered, so for an instant this is also the
    /// answer for a resumed run whose row someone else has since finished
    /// or handed off; such a run ends without a step, and its archive is
    /// on record or on its way from the sweeper.
    Written,
}

/// A halt armed on a run this runner is driving (issue 1070).
#[derive(Debug)]
pub struct ArmedHalt {
    /// Where the run's own row stood.
    pub row: RowAtHalt,
    /// For a run whose first write was under way
    /// ([`RowAtHalt::FirstWriteUnderWay`]), and only then: what the run
    /// waits for, once its row has landed, before it ends.
    pub word: Option<DropWord>,
}

/// What a drop owes a run whose halt it armed while the run's first write
/// was under way: word of whether the drop was published (issue 1070).
///
/// The coordination consumer's fold of the drop marks only a row that
/// exists, and while the first write is under way the fold may come before
/// the row. So the run, once its row has landed and it has taken the halt,
/// waits for this word: told the drop is published ([`Self::published`]),
/// it marks its row finished as the fold does, conditionally, so whichever
/// of the two lands first is the mark; dropped without it (the publish
/// failed, or the request was cut off before it returned), it leaves the
/// row in flight, as a drop that is not on the record must. The wait and
/// the mark are the run's own, on the task that drives it, so they do not
/// depend on the request that armed the halt living on after the publish.
#[derive(Debug)]
pub struct DropWord(mpsc::Sender<String>);

impl DropWord {
    /// The drop is published: the run marks its row finished in
    /// `final_phase` once the row has landed. Synchronous, so nothing
    /// separates a publish that returned from this.
    pub fn published(self, final_phase: &str) {
        // Room for one word: a second drop's, armed during the same
        // write, adds nothing to the first's.
        let _ = self.0.try_send(final_phase.to_owned());
    }
}

impl LiveRegistry {
    /// The agent this runner is driving `invocation_id` for, or `None`
    /// when it is not driving it at all. Liveness and identity in one
    /// answer, from the one authority that has both.
    pub(crate) fn agent_for(&self, invocation_id: &Uuid) -> Option<AgentId> {
        self.active
            .lock()
            .expect("active set poisoned")
            .get(invocation_id)
            .cloned()
    }

    /// Whether this runner is currently driving `invocation_id`.
    pub(crate) fn is_active(&self, invocation_id: &Uuid) -> bool {
        self.agent_for(invocation_id).is_some()
    }

    /// Arm a halt for the next step boundary. Returns false without
    /// changing state when this runner is not driving the invocation —
    /// the test-and-set that keeps a halt from outliving its drive.
    pub(crate) fn request_halt(&self, invocation_id: Uuid) -> bool {
        self.arm_halt(invocation_id).is_some()
    }

    /// Arm a halt, as [`Self::request_halt`] does, and say where the run's
    /// own row stood: `None` when this runner is not driving it.
    pub(crate) fn arm_halt(&self, invocation_id: Uuid) -> Option<ArmedHalt> {
        // One lock for both halves: between a separate `is_active` and
        // the insert the drive could end, leaving an armed halt behind
        // for the next drive of the same id to consume. The same lock
        // orders the arm against the beginning of the run's first write.
        let active = self.active.lock().expect("active set poisoned");
        if !active.contains_key(&invocation_id) {
            return None;
        }
        self.halt_requested
            .lock()
            .expect("halt set poisoned")
            .insert(invocation_id);
        // The write's beginning is under the `active` lock held here; its
        // end is not, so the order of these two reads is what makes the
        // report exact. The end notes the row first, then leaves
        // `first_writes` ([`Self::end_first_write`]). Read `first_writes`
        // first: a write still there is under way. One gone has either not
        // begun, and cannot begin while this lock is held, or has ended,
        // and if it landed its note is already there for the read after.
        // In the other order a write could land and end between the two
        // reads, and a row that is there would be reported never written.
        let mut first_writes = self.first_writes.lock().expect("first-write set poisoned");
        if let Some(words) = first_writes.get_mut(&invocation_id) {
            let words = words.get_or_insert_with(|| {
                let (word, heard) = mpsc::channel(1);
                DropWords { word, heard }
            });
            return Some(ArmedHalt {
                row: RowAtHalt::FirstWriteUnderWay,
                word: Some(DropWord(words.word.clone())),
            });
        }
        drop(first_writes);
        let row = if self.wrote_row(invocation_id) {
            RowAtHalt::Written
        } else {
            RowAtHalt::NeverWritten
        };
        Some(ArmedHalt { row, word: None })
    }

    /// Begin the run's first WAL write, unless a halt is armed: then the
    /// halt is taken, and the write is not to be made (`false`). Under the
    /// `active` lock a halt is armed under, so a halt armed after this
    /// sees the write under way.
    pub(crate) fn begin_first_write(&self, invocation_id: Uuid) -> bool {
        let _active = self.active.lock().expect("active set poisoned");
        if self
            .halt_requested
            .lock()
            .expect("halt set poisoned")
            .remove(&invocation_id)
        {
            return false;
        }
        self.first_writes
            .lock()
            .expect("first-write set poisoned")
            .insert(invocation_id, None);
        true
    }

    /// The run's first write has ended: `landed`, or failed. A row that
    /// landed is noted before the write leaves `first_writes`, the order
    /// [`Self::arm_halt`]'s reads rely on; one that failed is not noted.
    /// Returns what the run hears the drops that armed a halt during the
    /// write say, if any did: every such drop's word ends once it is given
    /// or dropped, so reading it ends.
    pub(crate) fn end_first_write(
        &self,
        invocation_id: Uuid,
        landed: bool,
    ) -> Option<mpsc::Receiver<String>> {
        if landed {
            self.note_row_written(invocation_id);
        }
        let words = self
            .first_writes
            .lock()
            .expect("first-write set poisoned")
            .remove(&invocation_id)
            .flatten()?;
        // Only the drops' senders are left, so the receiver ends with them.
        drop(words.word);
        Some(words.heard)
    }

    /// Whether the run's first write is under way: for tests that must
    /// arm a halt in that instant.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn first_write_under_way(&self, invocation_id: Uuid) -> bool {
        self.first_writes
            .lock()
            .expect("first-write set poisoned")
            .contains_key(&invocation_id)
    }

    /// Note that this drive's run has a WAL row of its own: its first
    /// write landed, or it resumed from one.
    pub(crate) fn note_row_written(&self, invocation_id: Uuid) {
        self.wrote_row
            .lock()
            .expect("written set poisoned")
            .insert(invocation_id);
    }

    /// Whether this drive's run has had a WAL row of its own.
    pub(crate) fn wrote_row(&self, invocation_id: Uuid) -> bool {
        self.wrote_row
            .lock()
            .expect("written set poisoned")
            .contains(&invocation_id)
    }

    /// Consume a halt, if one is armed.
    pub(crate) fn take_halt(&self, invocation_id: Uuid) -> bool {
        self.halt_requested
            .lock()
            .expect("halt set poisoned")
            .remove(&invocation_id)
    }

    /// Register a drive of `invocation_id` for `agent`, for as long as
    /// the returned guard lives.
    pub(crate) fn enter<'a>(
        &'a self,
        invocation_id: Uuid,
        agent_id: AgentId,
        rounds: &'a super::rounds::RoundLedger,
        timeouts: &'a super::timeouts::TimeoutLedger,
    ) -> ActiveInvocation<'a> {
        self.register(invocation_id, agent_id, false, rounds, timeouts)
    }

    /// Register a drive resumed from the run's row, as [`Self::enter`]
    /// does, noting the row in the same hold of the `active` lock a halt is
    /// armed under: an arm finds the drive with its row or not at all,
    /// never live with no row (issue 1070).
    pub(crate) fn enter_from_row<'a>(
        &'a self,
        invocation_id: Uuid,
        agent_id: AgentId,
        rounds: &'a super::rounds::RoundLedger,
        timeouts: &'a super::timeouts::TimeoutLedger,
    ) -> ActiveInvocation<'a> {
        self.register(invocation_id, agent_id, true, rounds, timeouts)
    }

    fn register<'a>(
        &'a self,
        invocation_id: Uuid,
        agent_id: AgentId,
        from_row: bool,
        rounds: &'a super::rounds::RoundLedger,
        timeouts: &'a super::timeouts::TimeoutLedger,
    ) -> ActiveInvocation<'a> {
        let mut active = self.active.lock().expect("active set poisoned");
        if from_row {
            self.note_row_written(invocation_id);
        }
        active.insert(invocation_id, agent_id);
        drop(active);
        ActiveInvocation {
            live: self,
            rounds,
            timeouts,
            id: invocation_id,
        }
    }
}

/// RAII entry in a [`LiveRegistry`]: removed on drop, so a panic or
/// early return can never leave a phantom "live" marker that would
/// block operator resume forever.
///
/// It clears the halt request on the same edge. A halt armed against an
/// invocation that then completes, fails, or panics before reaching a
/// step boundary would otherwise sit in the set for the daemon's
/// lifetime — and because `resume` re-drives the *same* invocation id, a
/// later drive of that id would consume the stale halt and suspend for
/// no reason. Tying both to the drive's lifetime makes that
/// unrepresentable.
pub(crate) struct ActiveInvocation<'a> {
    live: &'a LiveRegistry,
    rounds: &'a super::rounds::RoundLedger,
    /// The consecutive-timeout run (#547) — invocation-scoped, so it
    /// is forgotten on the same edge as the Round counter.
    timeouts: &'a super::timeouts::TimeoutLedger,
    id: Uuid,
}

impl Drop for ActiveInvocation<'_> {
    fn drop(&mut self) {
        self.live
            .active
            .lock()
            .expect("active set poisoned")
            .remove(&self.id);
        self.live
            .halt_requested
            .lock()
            .expect("halt set poisoned")
            .remove(&self.id);
        self.live
            .wrote_row
            .lock()
            .expect("written set poisoned")
            .remove(&self.id);
        self.live
            .first_writes
            .lock()
            .expect("first-write set poisoned")
            .remove(&self.id);
        self.rounds.forget(self.id);
        self.timeouts.forget(self.id);
    }
}

#[cfg(test)]
mod tests;
