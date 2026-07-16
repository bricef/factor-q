//! Test-only helpers shared across the runtime crate's `mod tests`
//! blocks. Most are `#[cfg(test)]` and not part of the public API; the
//! exception is [`nats`], exposed to integration tests via the
//! `test-support` feature (#233).
//!
//! The two submodules cover the two reusable patterns called out
//! in `docs/plans/closed/2026-04-28-data-architecture-v1.md`:
//!
//! - [`events`] — subscribe to a NATS subject, run an action,
//!   collect emitted events, and assert structural properties of
//!   the captured sequence. Used in NATS-gated tests across the
//!   executor, the reducer runner, and the new control-plane /
//!   worker tests as they land.
//! - [`stepper`] — drive a [`crate::Reducer`] through individual
//!   steps with full control, for tests that suspend mid-flight,
//!   inject specific [`crate::worker::reducer::types::CapabilityResult`]s,
//!   or verify state shape after every step.

// `nats` is dependency-free (std + serde_json), so it can be exposed to
// integration tests via the `test-support` feature, not only `cfg(test)`
// (#233). Every other submodule pulls a dev-dependency (axum, tempfile, …)
// that is unavailable when the lib is built as a dependency, so they stay
// `cfg(test)`.
#[cfg(any(test, feature = "test-support"))]
pub mod nats;

#[cfg(test)]
pub mod events;
#[cfg(test)]
pub mod mock_anthropic;
#[cfg(test)]
pub mod oracle;
#[cfg(test)]
pub mod runtime;
#[cfg(test)]
pub mod sim;
#[cfg(test)]
pub mod stepper;
