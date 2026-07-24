//! Test-only helpers shared across the runtime crate's `mod tests`
//! blocks. All `#[cfg(test)]` and not part of the public API. The
//! private-broker guard is the `fq-test-support` crate, re-exported here
//! as `nats` so this crate's tests keep a single `test_support::nats`
//! handle; integration tests dev-depend on `fq-test-support` directly
//! (#233).
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

// The private-broker guard lives in the standalone `fq-test-support` crate so
// integration tests and other workspaces can share it (#233). Re-export it as
// `nats` so this crate's own `#[cfg(test)]` code keeps using
// `test_support::nats::{NatsServer, test_nats}` unchanged.
#[cfg(test)]
pub use fq_test_support as nats;

// Everything except the mock LLM server leans on dev-dependencies
// (fq-test-support, tempfile, …), so only the mock is exposed through
// the `test-support` feature; the rest stays crate-test-only.
#[cfg(test)]
pub mod events;
pub mod mock_anthropic;
#[cfg(test)]
pub mod oracle;
#[cfg(test)]
pub mod runtime;
#[cfg(test)]
pub mod sim;
#[cfg(test)]
pub mod stepper;
