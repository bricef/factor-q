//! Reducer-model agent harness.
//!
//! The agent harness is structured as a pure synchronous reducer:
//! a single `step(StepInput) -> StepOutput` function with no I/O,
//! no async, and no hidden state. The host (`runner` submodule)
//! drives the loop, executes the actions the reducer requests,
//! and feeds the results back on the next step.
//!
//! This is the native realisation of the boundary described in
//! `docs/design/wasm-boundary-design.md`. WASM packaging is
//! deliberately out of scope here: the architectural claim the
//! shape makes is that a state-enum reducer is a tractable shape
//! for the agent harness. WASM is a packaging question that
//! becomes relevant only after the reducer claim is in place.
//!
//! Two key behavioural guarantees are tested in [`runner`]:
//!
//! - **Canonical event sequence**: a scripted scenario produces
//!   the documented sequence of canonical events
//!   (`triggered → llm.request → llm.dispatched → llm.response
//!   → tool.* → completed`) in order, every time.
//! - **Suspend/resume**: an invocation can be paused at any step
//!   boundary, the opaque state blob persisted, the reducer
//!   instance dropped, and a fresh instance resumed from the blob
//!   with no observable difference in final output.

pub mod harness;
pub mod runner;
pub mod types;

pub use harness::Harness;
pub use runner::{ReducerContext, ReducerRunner, RunnerConfig};
pub use types::{
    CapabilityResult, EmittedEvent, HarnessError, HarnessErrorKind, LogEntry, LogLevel,
    ModelRequest, ModelResponse, NextAction, Reducer, StepInput, StepOutput, ToolCallRequest,
    ToolCallResult, TriggerPayload, TriggerSourceKind,
};
