//! How many requests a mock provider had open at once — the measurement
//! #278's burst criterion is judged on. A mock records, for every
//! arrival, how many requests were already in flight and how many rate
//! limits it had served by then, so a test can say "after the provider
//! first said no, the fleet never exceeded N" from the provider's side
//! of the wire, which is the side that counts.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One request's arrival, as the provider saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrival {
    /// Requests open at the instant this one arrived, itself included.
    pub in_flight: usize,
    /// Rate-limit responses the mock had already sent when this one
    /// arrived. Zero means the caller could not yet have known the
    /// provider was throttling.
    pub rate_limits_served_before: usize,
}

#[derive(Debug, Default)]
pub struct ConcurrencyGauge {
    in_flight: AtomicUsize,
    rate_limits_served: AtomicUsize,
    arrivals: Mutex<Vec<Arrival>>,
}

impl ConcurrencyGauge {
    /// Record an arrival and hold its slot open until the guard drops —
    /// which the handler does once the response is on its way.
    pub fn enter(&self) -> InFlight<'_> {
        let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.arrivals.lock().unwrap().push(Arrival {
            in_flight,
            rate_limits_served_before: self.rate_limits_served.load(Ordering::SeqCst),
        });
        InFlight { gauge: self }
    }

    /// A 429 is about to go out.
    pub fn rate_limit_served(&self) {
        self.rate_limits_served.fetch_add(1, Ordering::SeqCst);
    }

    /// Every arrival, in order.
    pub fn arrivals(&self) -> Vec<Arrival> {
        self.arrivals.lock().unwrap().clone()
    }
}

/// One open request's slot; dropping it is the request finishing.
pub struct InFlight<'a> {
    gauge: &'a ConcurrencyGauge,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.gauge.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}
