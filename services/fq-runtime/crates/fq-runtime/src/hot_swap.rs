//! One shared value, replaced whole while readers are reading it.
//!
//! Three places in this runtime hold something that is rebuilt from
//! outside — the agent registry (`fq reload`), the tool registry (an MCP
//! server's `tools/list_changed`), the pricing table (the scheduled
//! refresh) — and every one of them had grown its own
//! `Arc<RwLock<Arc<T>>>`, with its own paragraph re-arguing the nesting
//! and its own clone-and-drop read discipline. Three copies of one
//! four-line cell is a value waiting to be named
//! (<https://github.com/bricef/factor-q/pull/745>, review D-2).
//!
//! The nesting is not an accident; each layer has a job:
//!
//! - **outer `Arc`** — shares the one lock across the tasks that hold
//!   it, which `tokio::spawn` requires to be owned and `'static`.
//! - **`RwLock`** — lets a writer replace the value while readers read.
//! - **inner `Arc<T>`** — lets a reader snapshot the current value with
//!   a refcount bump and drop the lock immediately, rather than holding
//!   it across the work the value is for, or deep-cloning the value on
//!   every read.
//!
//! Two properties follow, and they are the reason every one of the three
//! wanted this shape rather than an in-place `HashMap` behind a lock:
//!
//! - **A swap is one pointer write**, so a reader holds either the whole
//!   old value or the whole new one and never something half way
//!   between. A registry rebuilt entry by entry under a reader would be
//!   a state that never existed.
//! - **A snapshot outlives the swap that replaces it.** Work already in
//!   flight finishes against the value it started with; the next reader
//!   picks up the new one. That is the ADR-0020
//!   refresh-between-invocations rule, expressed as a type.
//!
//! `std::sync::RwLock` rather than `tokio`'s: every critical section is
//! an `Arc` clone, so a reader never blocks on anything and an async
//! lock would buy a scheduler round trip per read. And `RwLock<Arc<_>>`
//! rather than a third-party atomic pointer cell (`arc-swap`), to keep a
//! dependency out of the hot paths this sits on for a lock this
//! uncontended.

use std::sync::{Arc, RwLock};

/// A shared `T` that can be replaced whole.
///
/// Cheap to clone — every clone reads and swaps the same cell. Read with
/// [`current`](Self::current), replace with [`swap`](Self::swap).
pub struct HotSwap<T>(Arc<RwLock<Arc<T>>>);

impl<T> HotSwap<T> {
    /// Hold `value` until something swaps it.
    pub fn new(value: impl Into<Arc<T>>) -> Self {
        Self(Arc::new(RwLock::new(value.into())))
    }

    /// The value held right now, as an `Arc` the caller owns.
    ///
    /// Whatever a later swap does, this snapshot stays whole and
    /// readable. A caller that needs two facts to agree with each other
    /// reads them off one snapshot rather than calling this twice.
    pub fn current(&self) -> Arc<T> {
        Arc::clone(&self.0.read().expect("hot-swap lock poisoned"))
    }

    /// Hold `next` from now on, and return what was held.
    ///
    /// The returned handle is what a caller compares against to say what
    /// changed; readers that already took a snapshot keep theirs.
    pub fn swap(&self, next: impl Into<Arc<T>>) -> Arc<T> {
        let next = next.into();
        let mut slot = self.0.write().expect("hot-swap lock poisoned");
        std::mem::replace(&mut slot, next)
    }
}

/// Hand-written rather than derived: `#[derive(Clone)]` would add a
/// `T: Clone` bound, and nothing here clones a `T` — cloning the handle
/// is bumping a refcount on a cell that shares whatever `T` is.
impl<T> Clone for HotSwap<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> From<Arc<T>> for HotSwap<T> {
    fn from(value: Arc<T>) -> Self {
        Self::new(value)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for HotSwap<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("HotSwap").field(&self.current()).finish()
    }
}

#[cfg(test)]
mod tests;
