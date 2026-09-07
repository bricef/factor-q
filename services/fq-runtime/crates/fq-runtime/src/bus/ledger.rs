//! What this process's consumer loops report about their parse
//! boundary, for the health surface to read.
//!
//! JetStream can say how far a durable has been delivered and what is
//! pending; it cannot say *why* a consumer stopped pulling, or how many
//! of the messages it acked were never events at all. Those two facts
//! are learned at the loop's parse boundary and nowhere else, so the
//! loop writes them here and `fq doctor` / `fq status` read them back
//! beside the JetStream figures — a halted consumer looks, from the
//! broker's side, like one that is merely behind.
//!
//! The ledger rides the bus handle because that is what every consumer
//! loop already holds and what the health probe already reads with:
//! one connection, one ledger, and no consumer or report is handed
//! anything new. Each `EventBus::connect` starts an empty one, which
//! is also what keeps tests on private brokers from seeing each
//! other's halts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

pub use fq_ops::health::UnsupportedEvent;

/// One consumer's parse-boundary record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsumerRecord {
    /// Messages acked because they were not an event in any version,
    /// since the loop started.
    pub malformed_acked: u64,
    /// The event the loop halted on, once it has.
    pub halted_on: Option<UnsupportedEvent>,
}

/// The per-consumer records, shared by every clone of the bus.
#[derive(Debug, Clone, Default)]
pub struct ConsumerLedger {
    records: Arc<Mutex<HashMap<String, ConsumerRecord>>>,
}

impl ConsumerLedger {
    /// A loop is starting under `consumer`: its record starts empty, so
    /// the figures describe this loop and not an earlier one on the
    /// same durable.
    pub fn start(&self, consumer: &str) {
        self.lock()
            .insert(consumer.to_string(), ConsumerRecord::default());
    }

    /// The loop acked a message that was not an event in any version.
    pub fn note_malformed(&self, consumer: &str) {
        self.lock()
            .entry(consumer.to_string())
            .or_default()
            .malformed_acked += 1;
    }

    /// The loop halted on an event it cannot read.
    pub fn halt(&self, consumer: &str, on: UnsupportedEvent) {
        self.lock()
            .entry(consumer.to_string())
            .or_default()
            .halted_on = Some(on);
    }

    /// `consumer`'s record; empty for a durable no loop in this process
    /// has run.
    pub fn record(&self, consumer: &str) -> ConsumerRecord {
        self.lock().get(consumer).cloned().unwrap_or_default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ConsumerRecord>> {
        // A poisoned lock means a panic while holding it, and the map
        // is still a map: report what it holds rather than nothing.
        self.records.lock().unwrap_or_else(|e| e.into_inner())
    }
}
