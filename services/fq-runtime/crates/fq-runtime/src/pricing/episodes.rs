//! Which pricing conditions this daemon has already reported, so a
//! standing condition is raised once and closed once.
//!
//! Two of the pricing signals describe a *condition* rather than
//! something that happened: `pricing.fetch_failed` is true for as long
//! as the source will not answer, and `pricing.stale` for as long as the
//! table the daemon is serving is past `[pricing] max_age`. The load
//! recomputes both on every refresh, so publishing whatever it computed
//! meant re-raising the same condition on a cadence: at four refreshes a
//! day, a week of a broken upstream is twenty-eight `pricing.stale`
//! alerts. With no acknowledgement (#736's v1 decision) an alert is open
//! until a later signal resolves it, so twenty-eight of them is
//! twenty-eight things a pane says are open, none of which can ever
//! close — and a count that only grows is a count nobody reads
//! (<https://github.com/bricef/factor-q/pull/745>, review C-5/E-7).
//!
//! So the condition is **edge-triggered**. This value remembers, per
//! condition, the `event_id` of the signal that raised it; while a
//! condition is open the load's recurrence of it is dropped, and the
//! first load that ends the condition emits a notification of the same
//! kind naming that id in `resolves`. A recovery carries the kind of the
//! signal it closes — the topic has not changed, only its state — and is
//! a notification, because recovering is not itself alarming.
//!
//! ## What a restart does
//!
//! The memory is in this process and nowhere else. A daemon that
//! restarts while a condition holds raises it again on its startup load,
//! which is honest: that *is* a new episode as far as anything in this
//! process can know, and a fresh alert naming the current age is more
//! use than silence. The cost is the other direction — an alert raised
//! by the previous run stays open for ever, because the run that could
//! have resolved it is gone. That is accepted rather than fixed: the
//! alternative is persisting signal ids beside the cache and resolving
//! an alert this process never saw, and reading that back wrongly (a
//! restored cache directory, a copied deployment) would resolve alerts
//! that are still true, which is the worse failure. The same value is
//! shared by the startup load and the scheduled refresh, so a table
//! found stale at boot and still stale at the next refresh is one
//! episode, not two.

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::events::operator_signal::kinds;
use crate::events::{OperatorSignalPayload, PendingSignal, SignalKind};

use super::live::AcceptedLoad;

/// The pricing conditions this daemon has raised and not yet resolved.
///
/// Cheap to clone — every clone reads and writes the same memory. Built
/// where the daemon loads its prices, used by that load and by every
/// scheduled refresh after it.
#[derive(Clone, Debug, Default)]
pub struct PricingEpisodes(Arc<Mutex<OpenEpisodes>>);

/// The raising signal's id, per condition. `None` is "not currently
/// reported".
#[derive(Debug, Default)]
struct OpenEpisodes {
    stale: Option<Uuid>,
    fetch_failed: Option<Uuid>,
}

/// A pricing signal that describes a condition rather than an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Episode {
    /// The served table is past `[pricing] max_age`.
    Stale,
    /// The document did not land.
    FetchFailed,
}

impl Episode {
    /// Which condition a signal reports, or `None` for a signal that
    /// reports something that happened (a refused change) and is
    /// therefore news every time.
    fn of(kind: &SignalKind) -> Option<Self> {
        match kind.as_str() {
            kinds::PRICING_STALE => Some(Self::Stale),
            kinds::PRICING_FETCH_FAILED => Some(Self::FetchFailed),
            _ => None,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::Stale => kinds::PRICING_STALE,
            Self::FetchFailed => kinds::PRICING_FETCH_FAILED,
        }
    }

    /// Whether `load` shows the condition has ended.
    ///
    /// A load that accepted a document ends both: a fetch that
    /// succeeded is not failing, and a table accepted moments ago is not
    /// stale. Staleness asks for *both* halves rather than just "the
    /// load reported none", because a load that could not fetch and
    /// found no provenance to age reports none either, and resolving the
    /// alert on that would close it while it is still true.
    fn ended(self, load: &AcceptedLoad) -> bool {
        match self {
            Self::Stale => load.fetch_error.is_none() && load.staleness.is_none(),
            Self::FetchFailed => load.fetch_error.is_none(),
        }
    }

    /// The line the recovery carries.
    fn recovery(self) -> &'static str {
        match self {
            Self::Stale => "the pricing table has refreshed and is inside its window again",
            Self::FetchFailed => "the pricing document fetched again; the table is current",
        }
    }
}

impl OpenEpisodes {
    fn get(&self, episode: Episode) -> Option<Uuid> {
        match episode {
            Episode::Stale => self.stale,
            Episode::FetchFailed => self.fetch_failed,
        }
    }

    fn set(&mut self, episode: Episode, raised: Option<Uuid>) {
        match episode {
            Episode::Stale => self.stale = raised,
            Episode::FetchFailed => self.fetch_failed = raised,
        }
    }
}

impl PricingEpisodes {
    pub fn new() -> Self {
        Self::default()
    }

    /// What a load should actually publish: its signals, with a
    /// recurrence of an already-reported condition dropped and a
    /// recovery added for each condition this load ended.
    ///
    /// Order: the load's own signals first, then the recoveries, so a
    /// reader of the log sees what this refresh found before what it
    /// closed.
    pub fn edges(&self, load: &AcceptedLoad, signals: Vec<PendingSignal>) -> Vec<PendingSignal> {
        let mut open = self.0.lock().expect("pricing episodes lock poisoned");
        let mut published = Vec::with_capacity(signals.len());
        for signal in signals {
            match Episode::of(signal.kind()) {
                // Already reported and still true: the condition has not
                // changed, so there is nothing to say.
                Some(episode) if open.get(episode).is_some() => continue,
                Some(episode) => {
                    open.set(episode, Some(signal.event_id));
                    published.push(signal);
                }
                None => published.push(signal),
            }
        }
        for episode in [Episode::Stale, Episode::FetchFailed] {
            let Some(raised) = open.get(episode) else {
                continue;
            };
            if !episode.ended(load) {
                continue;
            }
            open.set(episode, None);
            published.push(PendingSignal::new(
                OperatorSignalPayload::notification(
                    SignalKind::registered(episode.kind()),
                    episode.recovery(),
                )
                .resolving(raised),
            ));
        }
        published
    }
}

#[cfg(test)]
mod tests;
