//! When an invocation counts as **stuck** — derived from the call
//! deadlines rather than chosen.
//!
//! The threshold used to be a flat 30 seconds, borrowed from the
//! stale-worker sweep because an invocation quiet for as long as a
//! worker had not heartbeated felt like the same order of signal. It
//! was not. Minute-scale invocations are what this runtime is *for*,
//! and a model turn that takes four minutes is not a fault; any flat
//! number tight enough to catch a real wedge flags every one of them
//! (<https://github.com/bricef/factor-q/issues/37>).
//!
//! What changed is that every step is now individually bounded. A model
//! call has a deadline it is retried under a cap (`[worker]
//! llm_timeout_secs` × `[worker.llm_retry] timeout_max_attempts`), and a
//! tool call has a ceiling plus the host's backstop grace (`[tools]
//! max_timeout_secs` + `ToolCallLimits::BACKSTOP_GRACE`). Their sum is
//! the longest a single reducer step can legitimately take. So the
//! threshold is that sum, doubled:
//!
//! ```text
//! stuck_after = 2 × (timeout_max_attempts × llm_timeout_secs
//!                    + tools.max_timeout_secs
//!                    + backstop_grace)
//! ```
//!
//! Doubled rather than taken bare because the sum is a *worst* case, not
//! a budget: one step is allowed to hit it, and the reading is taken
//! from the WAL row's age, which lags a step that is still finishing.
//! Doubling buys a whole extra worst-case step before a slow-but-alive
//! invocation is called stuck. Being derived is the point — retune a
//! deadline and the safety net under it moves with it, which a constant
//! could not do.
//!
//! At the shipped defaults (600 s model, 2 attempts, 900 s tools, 5 s
//! grace) that is 2 × 2,105 s = **4,210 s**, a little over an hour.

use std::time::Duration;

use super::Config;

impl Config {
    /// How long an in-flight invocation may go without crossing a step
    /// boundary before the runtime reports it stuck.
    ///
    /// **The one definition.** `fq doctor`'s verdict, the periodic
    /// sweep that emits `invocation.stuck`, and the number both of them
    /// quote to an operator all come from this call, so the report and
    /// the event can never disagree about what "stuck" meant.
    pub fn stuck_after(&self) -> Duration {
        if let Some(secs) = self.worker.stuck_threshold_override_secs {
            return Duration::from_secs(secs);
        }
        let attempts = u64::from(self.worker.llm_retry.timeout_max_attempts.max(1));
        let llm = attempts.saturating_mul(self.worker.llm_timeout_secs);
        let tools = self
            .tools
            .max_timeout_secs
            .saturating_add(crate::tools::ToolCallLimits::BACKSTOP_GRACE.as_secs());
        Duration::from_secs(llm.saturating_add(tools).saturating_mul(2))
    }

    /// [`Config::stuck_after`] in milliseconds, the unit every
    /// timestamp in the stores and on the wire is in.
    pub fn stuck_after_ms(&self) -> i64 {
        self.stuck_after().as_millis().min(i64::MAX as u128) as i64
    }
}

#[cfg(test)]
mod tests;
