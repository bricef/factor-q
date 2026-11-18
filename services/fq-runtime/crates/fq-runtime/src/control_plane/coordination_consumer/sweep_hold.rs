//! Holding the coordination consumer's tick sweeps until startup recovery
//! has decided (issue 1070).
//!
//! The daemon starts this consumer before its startup recovery, so that
//! recovery decides over the events the daemon that stopped left unfolded.
//! The consumer's tick runs two sweeps that report what has not moved for
//! too long: the stale-worker sweep, which marks a worker whose heartbeat
//! lapsed and publishes `worker.orphaned`, and the stuck-invocation sweep,
//! which publishes `invocation.stuck` for a WAL row past its threshold. At
//! a start both would find what the stopped daemon left, its worker row
//! and its runs in flight, which recovery is about to resume or find
//! ended. A report about them before recovery has decided is a false
//! alarm, so the daemon holds the sweeps until it has.
//!
//! A held tick sweeps nothing and the first tick after the release sweeps
//! as usual. The hold lasts only while the boot decides, released once
//! recovery has decided whether or not it held any run back, so a report
//! is late by at most that long and none is lost: both sweeps read
//! standing state, the roster and the WAL, and that first tick finds it as
//! it then is. A run recovery did not resume is then reported stuck once
//! past its threshold, as any run that has not moved is.

use tokio::sync::watch;

/// A hold on the tick sweeps, and the release that ends it.
pub fn hold_sweeps() -> (SweepHold, SweepRelease) {
    let (released, rx) = watch::channel(false);
    (SweepHold { released: rx }, SweepRelease { released })
}

/// What the consumer reads on each tick: whether its sweeps are held.
#[derive(Clone, Debug)]
pub struct SweepHold {
    released: watch::Receiver<bool>,
}

impl SweepHold {
    /// Whether the sweeps are still held.
    pub fn is_held(&self) -> bool {
        !*self.released.borrow()
    }
}

/// Ends the hold when released, or when dropped: a boot that fails, or a
/// task that stops before it decides, does not leave the sweeps held for
/// the life of the process.
#[derive(Debug)]
pub struct SweepRelease {
    released: watch::Sender<bool>,
}

impl SweepRelease {
    /// Release the sweeps: the next tick runs them. Dropping the release
    /// does the same; this names the moment.
    pub fn release(self) {}
}

impl Drop for SweepRelease {
    fn drop(&mut self) {
        self.released.send_replace(true);
    }
}
