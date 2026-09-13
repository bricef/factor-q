//! `[maintenance]` — the daemon-side maintenance consumer (#257).

use std::time::Duration;

use serde::Deserialize;

/// Whether this daemon runs the maintenance tasks a scheduler asks
/// for, and how long one is allowed to take.
///
/// **Default on**, which is the one decision in this file worth
/// arguing. Design Principle 8 makes it configuration; it does not say
/// which way it points. Off-by-default would make the first scheduled
/// job on a new instance do *nothing*, silently: fq-cron would publish
/// happily, the stream would accept the message, and the only symptom
/// would be maintenance that never ran — the failure mode a `ping`
/// task exists to rule out, reintroduced as a default. On-by-default
/// costs a durable consumer that idles until something is published,
/// which is a few bytes of broker state and no work at all. The knob
/// stays because a second daemon against one broker must not run every
/// sweep twice: on a multi-daemon deployment exactly one keeps it
/// true.
#[derive(Debug, Clone, Deserialize)]
pub struct MaintenanceConfig {
    /// Run the maintenance consumer. Default `true`. When false the
    /// daemon creates no durable and consumes nothing; the stream is
    /// still ensured, so a scheduler publishing to it is not broken by
    /// a daemon that declines to listen, and the commands age out.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Ack window for the maintenance durable, in ms. Default 60000.
    ///
    /// Its own knob rather than `[bus] ack_wait_ms`, for the reason
    /// the summariser has one: that window is sized for a handler that
    /// writes to SQLite and returns, and a maintenance task is not
    /// that shape — a pricing refresh fetches over the network, an
    /// audit walks a store. A task slower than its window is
    /// redelivered *underneath the run still in flight*; the run-id
    /// ledger stops that becoming a second run, but the right fix is a
    /// window that fits the work, and this is where it is set.
    #[serde(default = "default_ack_wait_ms")]
    pub ack_wait_ms: u64,
}

impl MaintenanceConfig {
    /// The ack window as a duration.
    pub fn ack_wait(&self) -> Duration {
        Duration::from_millis(self.ack_wait_ms)
    }
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            ack_wait_ms: default_ack_wait_ms(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_ack_wait_ms() -> u64 {
    60_000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is on, and an omitted section is the default — a
    /// daemon that has never heard of maintenance still consumes it.
    #[test]
    fn an_absent_section_enables_the_consumer() {
        let config = MaintenanceConfig::default();
        assert!(config.enabled);
        assert_eq!(config.ack_wait(), Duration::from_secs(60));
    }

    #[test]
    fn each_key_can_be_set_on_its_own() {
        let config: MaintenanceConfig = toml::from_str("enabled = false").unwrap();
        assert!(!config.enabled);
        assert_eq!(
            config.ack_wait_ms, 60_000,
            "the other key keeps its default"
        );

        let config: MaintenanceConfig = toml::from_str("ack_wait_ms = 5000").unwrap();
        assert!(config.enabled);
        assert_eq!(config.ack_wait(), Duration::from_secs(5));
    }
}
