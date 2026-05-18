//! Test-only helpers shared across the runtime crate's `mod tests`
//! blocks. Items here are gated on `#[cfg(test)]` and not part of
//! the runtime's public API.
//!
//! The two submodules cover the two reusable patterns called out
//! in `docs/plans/active/2026-04-28-data-architecture-v1.md`:
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

pub mod events;
pub mod mock_anthropic;
pub mod stepper;
