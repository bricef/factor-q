//! Reducer-model agent harness (native prototype).
//!
//! The agent harness is structured as a pure synchronous reducer:
//! a single `step(StepInput) -> StepOutput` function with no I/O,
//! no async, and no hidden state. The host (`runner` submodule)
//! drives the loop, executes the actions the reducer requests,
//! and feeds the results back on the next step.
//!
//! This is the native validation of the boundary described in
//! `docs/design/wasm-boundary-design.md`. WASM packaging is
//! deliberately out of scope here: the architectural claim under
//! test is that a state-enum reducer is a tractable shape for the
//! agent harness. WASM is a packaging question that becomes
//! relevant only after the reducer claim is validated.
//!
//! The two key tests are:
//!
//! - **Equivalence**: the same scripted scenario (same fixture
//!   LLM responses, same agent definition) produces the same
//!   sequence of canonical events through the reducer path as
//!   through the legacy [`crate::AgentExecutor`].
//! - **Suspend/resume**: an invocation can be paused at any step
//!   boundary, the opaque state blob persisted, the reducer
//!   instance dropped, and a fresh instance resumed from the blob
//!   with no observable difference in final output.

pub mod harness;
pub mod runner;
pub mod types;

pub use harness::Harness;
pub use runner::ReducerRunner;
pub use types::{
    CapabilityResult, EmittedEvent, HarnessError, HarnessErrorKind, LogEntry, LogLevel,
    ModelRequest, ModelResponse, NextAction, Reducer, StepInput, StepOutput, ToolCallRequest,
    ToolCallResult, TriggerPayload, TriggerSourceKind,
};
