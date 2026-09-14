//! The table the daemon is *serving*, and the one thing you can do to
//! it besides read it: replace it whole.
//!
//! A [`PricingTable`] is a value — [`accept`](super::accept::accept)
//! takes one and returns another — and until the periodic refresh
//! (<https://github.com/bricef/factor-q/issues/344>) the daemon only ever
//! had one, built at startup and handed out as an `Arc`. A refresh needs
//! the holders of that `Arc` to see a *different* table without being
//! rebuilt, and every one of them is on the cost path of a running
//! invocation.
//!
//! So the handle is an `Arc<RwLock<Arc<PricingTable>>>` and the reads are
//! copies:
//!
//! - **The swap is one pointer write.** A reader takes the read lock long
//!   enough to clone the inner `Arc` and lets go; a writer takes the
//!   write lock long enough to store a new one. A reader therefore holds
//!   either the whole old table or the whole new one and never a table
//!   half way between — the invariant an in-place `HashMap` behind a lock
//!   could not offer, because a refresh is hundreds of inserts and a
//!   reader between two of them sees a table that never existed.
//! - **The lookups return owned values**, not borrows into the table, so
//!   no caller holds a lock — or a table — across an `await`. Everything
//!   the price path reads is `Copy` ([`ModelPricing`], a window) or a
//!   short `String` (the provenance version), which is why this costs
//!   nothing and is what makes the handle a drop-in for the `Arc` it
//!   replaced.
//!
//! **There is no `price(model)` accessor, and that is deliberate.** A
//! priced call reads two things from the table — what the model costs
//! and which table said so — and a cost row whose figure and citation
//! come from two snapshots is a record of a table that never priced it.
//! Under the cost-retention principle the citation is the only thing
//! that makes a retained figure a record, so a wrong one is worse than
//! none. Per-field accessors made that mistake the easy one to make and
//! three call sites made it, so the handle offers [`current`] and
//! nothing else: a caller takes one snapshot and reads the price, the
//! window and the version off it. The type is then what keeps the rule
//! rather than a comment asking callers to remember it
//! (<https://github.com/bricef/factor-q/pull/745>, review D-1).
//!
//! [`current`]: ServedPricing::current
//!
//! `std::sync::RwLock` rather than `tokio`'s: every critical section here
//! is an `Arc` clone, so a reader never blocks on anything and an async
//! lock would buy a scheduler round trip per price lookup. And
//! `RwLock<Arc<_>>` rather than a third-party atomic pointer cell, to
//! keep a dependency out of the price path for a lock this uncontended.

use std::sync::{Arc, RwLock};

use crate::events::PricingProvenance;

use super::PricingTable;

/// A shared handle on the pricing table this daemon is serving.
///
/// Cheap to clone — every clone reads and swaps the same table. Built
/// from the table the startup load produced, handed to the reducer
/// runner and the summariser, and swapped by the pricing refresh.
#[derive(Clone)]
pub struct ServedPricing(Arc<RwLock<Arc<PricingTable>>>);

impl ServedPricing {
    /// Serve `table` until something swaps it.
    pub fn new(table: PricingTable) -> Self {
        Self::from(Arc::new(table))
    }

    /// The table being served right now, as an `Arc` the caller owns.
    ///
    /// Whatever a later swap does, this snapshot stays whole and
    /// readable — which is what lets a price lookup and the cost
    /// calculation that follows it agree, even across a refresh that
    /// landed between them.
    pub fn current(&self) -> Arc<PricingTable> {
        Arc::clone(&self.0.read().expect("served pricing lock poisoned"))
    }

    /// Serve `next` from now on, and return what was being served.
    ///
    /// The returned handle is what a caller compares against to say what
    /// changed; readers that already took a snapshot keep theirs.
    pub fn swap(&self, next: PricingTable) -> Arc<PricingTable> {
        let next = Arc::new(next);
        let mut slot = self.0.write().expect("served pricing lock poisoned");
        std::mem::replace(&mut slot, next)
    }

    /// The provenance of the table being served, when it has one.
    pub fn provenance(&self) -> Option<PricingProvenance> {
        self.current().provenance().cloned()
    }

    /// How many models the served table prices.
    pub fn len(&self) -> usize {
        self.current().len()
    }

    /// True when the served table prices nothing.
    pub fn is_empty(&self) -> bool {
        self.current().is_empty()
    }
}

impl From<Arc<PricingTable>> for ServedPricing {
    fn from(table: Arc<PricingTable>) -> Self {
        Self(Arc::new(RwLock::new(table)))
    }
}

impl From<PricingTable> for ServedPricing {
    fn from(table: PricingTable) -> Self {
        Self::new(table)
    }
}

impl std::fmt::Debug for ServedPricing {
    /// The table itself is hundreds of entries and nothing wants it in a
    /// log line; its size and its provenance are what identify it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let table = self.current();
        f.debug_struct("ServedPricing")
            .field("entries", &table.len())
            .field("version", &table.version())
            .finish()
    }
}

#[cfg(test)]
mod tests;
