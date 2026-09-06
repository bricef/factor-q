//! The redelivery policy every durable consumer on this bus answers a
//! transient failure with (review finding B4).
//!
//! One value carries the whole redelivery story, because the four
//! numbers only make sense together: how long the server waits for an
//! ack before redelivering on its own (`ack_wait`), how fast *we* ask
//! for a redelivery after a handler failed (`nak_initial` doubling to
//! `nak_max`), how often that failure is worth a log line
//! (`log_interval`), and how many redeliveries of the same message mean
//! the consumer is no longer making progress
//! (`stuck_after_redeliveries`).
//!
//! Before this, `HandlerError::Transient` answered with `Nak(None)` —
//! "redeliver immediately" — on durables with unlimited redelivery, so
//! a persistent transient fault (a full disk under the projection, a
//! failed ack publish in the coordination consumer) became a hot loop
//! at broker round-trip speed with a frozen watermark behind it. The
//! escalation is what turns that into a bounded, visible retry: the
//! event still cannot be lost, because `max_deliver` on the
//! event-stream durables stays unlimited, but the retry costs one
//! round-trip a minute instead of thousands.
//!
//! Every number is configuration — `[bus]` in `fqd.toml` — because
//! tuning them is an operator's job, not a rebuild (Design Principle
//! 8). The defaults here are what an unconfigured daemon runs.

use std::time::Duration;

/// First redelivery delay after a handler's transient failure.
pub const DEFAULT_NAK_INITIAL: Duration = Duration::from_secs(1);

/// Ceiling on the escalating redelivery delay. Also the natural log
/// rate once the escalation caps: one redelivery a minute is one line
/// a minute.
pub const DEFAULT_NAK_MAX: Duration = Duration::from_secs(60);

/// How long the server waits for an ack before redelivering by itself.
/// This is the NATS server's own default, made explicit: every durable
/// carried it implicitly, which meant nothing in this tree said what it
/// was or that it had been considered. Sized so a healthy handler never
/// trips it — the control-plane handlers are single SQLite writes, and
/// the dispatcher acks at the invocation's first WAL write, seconds in.
pub const DEFAULT_ACK_WAIT: Duration = Duration::from_secs(30);

/// Floor on the gap between two error lines about the same stuck
/// consumer once the delay has stopped escalating.
pub const DEFAULT_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Redeliveries of the still-unacked messages past which a consumer is
/// reported unhealthy rather than merely retrying. Five is roughly
/// thirty seconds of continuous failure under the default escalation
/// (1+2+4+8+16), which is long enough that a broker blip or a moment of
/// store contention has cleared and short enough that an operator
/// running `fq doctor` during an incident sees it.
pub const DEFAULT_STUCK_AFTER_REDELIVERIES: u64 = 5;

/// How a durable consumer paces redelivery, logs about it, and decides
/// it is stuck. See the module doc for why the four live together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerRedeliveryPolicy {
    /// Delay on the first NAK; doubles per redelivery from there.
    pub nak_initial: Duration,
    /// Ceiling the doubling saturates at.
    pub nak_max: Duration,
    /// Explicit `ack_wait` on every durable this bus creates.
    pub ack_wait: Duration,
    /// Minimum gap between two error lines about one consumer's
    /// redeliveries, once the delay stops escalating.
    pub log_interval: Duration,
    /// Redeliveries past which health calls the consumer stuck.
    pub stuck_after_redeliveries: u64,
}

impl Default for ConsumerRedeliveryPolicy {
    fn default() -> Self {
        Self {
            nak_initial: DEFAULT_NAK_INITIAL,
            nak_max: DEFAULT_NAK_MAX,
            ack_wait: DEFAULT_ACK_WAIT,
            log_interval: DEFAULT_LOG_INTERVAL,
            stuck_after_redeliveries: DEFAULT_STUCK_AFTER_REDELIVERIES,
        }
    }
}

impl ConsumerRedeliveryPolicy {
    /// The delay to NAK with on delivery number `delivered`, counted
    /// the way JetStream counts it (the first delivery is 1). The
    /// initial delay doubles once per redelivery and saturates at
    /// [`Self::nak_max`], so a handler failing forever settles into one
    /// retry per cap interval instead of a broker-speed loop.
    pub fn nak_delay(&self, delivered: u64) -> Duration {
        // 31 doublings is already past any sane cap; clamping here
        // keeps the shift and the multiply in range for any input the
        // server could hand us.
        let steps = u32::try_from(delivered.saturating_sub(1)).unwrap_or(u32::MAX);
        let factor = 1u32 << steps.min(31);
        self.nak_initial
            .checked_mul(factor)
            .unwrap_or(self.nak_max)
            .min(self.nak_max)
    }

    /// Whether delivery number `delivered` is a fresh escalation step —
    /// its delay differs from the previous delivery's. Every step is
    /// new information about how bad the fault is; past the cap they
    /// stop differing and [`RedeliveryLog`] falls back to the interval.
    pub fn escalates_at(&self, delivered: u64) -> bool {
        delivered <= 1 || self.nak_delay(delivered) != self.nak_delay(delivered.saturating_sub(1))
    }
}

/// The rate limit on a consumer's redelivery error log: the escalation
/// ladder once, then at most one line per
/// [`ConsumerRedeliveryPolicy::log_interval`].
///
/// Held per consumer loop, so two consumers failing at once still each
/// say so. The clock is passed in rather than read here, which is what
/// makes the limit testable without sleeping through a minute.
///
/// **An escalation step only counts as new information once.** The
/// limiter is per loop while the delivery count is per *message*, so a
/// consumer whose handler fails on everything sees delivery 1 of a
/// fresh message over and over. Admitting each of those as a first
/// escalation step would make the rate `steps × arrival rate` — under a
/// persistent `SQLITE_FULL` on a busy stream, exactly the flood the
/// limiter exists to stop. So a step is admitted only when its delivery
/// count is higher than any logged since the last interval line: the
/// ladder is climbed once and the window then governs.
#[derive(Debug)]
pub struct RedeliveryLog {
    policy: ConsumerRedeliveryPolicy,
    last_logged: Option<std::time::Instant>,
    /// The highest delivery count logged since the window last opened.
    high_water: u64,
}

impl RedeliveryLog {
    pub fn new(policy: ConsumerRedeliveryPolicy) -> Self {
        Self {
            policy,
            last_logged: None,
            high_water: 0,
        }
    }

    /// Whether this redelivery gets a line, recording the decision.
    /// `now` is the caller's clock reading.
    pub fn admit(&mut self, delivered: u64, now: std::time::Instant) -> bool {
        let due = match self.last_logged {
            // Nothing said yet: the first failure always speaks.
            None => true,
            // The window has elapsed. This line reopens it, so the
            // ladder may be climbed again — a fault that outlives an
            // interval deserves to show its escalation afresh.
            Some(last) if now.duration_since(last) >= self.policy.log_interval => true,
            // Inside the window: only a step past everything already
            // said, which is what makes the ladder cost one line each
            // rather than one line per message per step.
            Some(_) => self.policy.escalates_at(delivered) && delivered > self.high_water,
        };
        if due {
            self.last_logged = Some(now);
            self.high_water = delivered;
        }
        due
    }
}

#[cfg(test)]
mod tests;
