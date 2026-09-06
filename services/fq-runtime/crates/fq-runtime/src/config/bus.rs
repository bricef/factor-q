//! `[bus]` — how durable consumers on the event bus handle redelivery.
//!
//! Its own module for the reason `[edge]` has one: five numbers that
//! each have to explain themselves, and a parent that stays a table of
//! contents. The numbers themselves — what they mean together and why
//! these defaults — are documented once, on
//! [`crate::bus::ConsumerRedeliveryPolicy`]; this is the TOML face of
//! that value.

use serde::Deserialize;

use crate::bus::retry::{
    ConsumerRedeliveryPolicy, DEFAULT_ACK_WAIT, DEFAULT_LOG_INTERVAL, DEFAULT_NAK_INITIAL,
    DEFAULT_NAK_MAX, DEFAULT_STUCK_AFTER_REDELIVERIES,
};

/// `[bus]` in `fqd.toml`: the redelivery policy applied to every
/// durable consumer this daemon creates.
///
/// Tuning it is an operator's job, not a rebuild (Design Principle 8).
/// A daemon with no `[bus]` table runs the defaults, which are the
/// values these fields document.
#[derive(Debug, Clone, Deserialize)]
pub struct BusConfig {
    /// Delay on the first NAK after a handler's transient failure, in
    /// milliseconds. Doubles per redelivery from there.
    #[serde(default = "default_nak_initial_ms")]
    pub nak_initial_ms: u64,
    /// Ceiling the doubling saturates at, in milliseconds. A handler
    /// failing forever settles into one retry per this interval.
    #[serde(default = "default_nak_max_ms")]
    pub nak_max_ms: u64,
    /// Explicit `ack_wait` on every durable, in milliseconds: how long
    /// the server waits for an ack before redelivering by itself. Size
    /// it so a healthy handler never trips it.
    #[serde(default = "default_ack_wait_ms")]
    pub ack_wait_ms: u64,
    /// Floor on the gap between two error lines about one consumer's
    /// redeliveries once the delay has stopped escalating, in
    /// milliseconds.
    #[serde(default = "default_log_interval_ms")]
    pub log_interval_ms: u64,
    /// Redeliveries of a consumer's unacked messages past which
    /// `control.status` and `fq doctor` report it stuck rather than
    /// merely retrying.
    #[serde(default = "default_stuck_after_redeliveries")]
    pub stuck_after_redeliveries: u64,
}

impl BusConfig {
    /// The typed policy this table describes.
    pub fn policy(&self) -> ConsumerRedeliveryPolicy {
        ConsumerRedeliveryPolicy {
            nak_initial: std::time::Duration::from_millis(self.nak_initial_ms),
            nak_max: std::time::Duration::from_millis(self.nak_max_ms),
            ack_wait: std::time::Duration::from_millis(self.ack_wait_ms),
            log_interval: std::time::Duration::from_millis(self.log_interval_ms),
            stuck_after_redeliveries: self.stuck_after_redeliveries,
        }
    }
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            nak_initial_ms: default_nak_initial_ms(),
            nak_max_ms: default_nak_max_ms(),
            ack_wait_ms: default_ack_wait_ms(),
            log_interval_ms: default_log_interval_ms(),
            stuck_after_redeliveries: default_stuck_after_redeliveries(),
        }
    }
}

fn default_nak_initial_ms() -> u64 {
    DEFAULT_NAK_INITIAL.as_millis() as u64
}

fn default_nak_max_ms() -> u64 {
    DEFAULT_NAK_MAX.as_millis() as u64
}

fn default_ack_wait_ms() -> u64 {
    DEFAULT_ACK_WAIT.as_millis() as u64
}

fn default_log_interval_ms() -> u64 {
    DEFAULT_LOG_INTERVAL.as_millis() as u64
}

fn default_stuck_after_redeliveries() -> u64 {
    DEFAULT_STUCK_AFTER_REDELIVERIES
}
