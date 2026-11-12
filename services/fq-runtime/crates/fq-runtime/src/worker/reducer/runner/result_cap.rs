//! The cap on a tool result: the one place a tool's answer is bounded
//! before anything records it
//! (<https://github.com/bricef/factor-q/issues/827>).
//!
//! Split from `runner.rs`, which sits on a size budget that only
//! tightens. Entered from `run_tool` and nowhere else, as soon as the
//! tool's outcome is known and **before** the WAL write, so the WAL row,
//! the `tool.result` event and the conversation the model is sent next
//! all carry the same text. That text then rides in every later
//! `llm.request` and in the state blob, which is why it is bounded once,
//! here, and not by each tool.
//!
//! Before this only `exec` bounded its output. `file_read` returned a
//! whole file and an MCP result joined every text part, and the broker
//! refuses any message over its `max_payload` — so one large result,
//! re-sent every turn, left an invocation unable to publish anything
//! again.
//!
//! An error is capped too, because an error message can be a stack
//! dump. That is why the error is rendered here: the cap applies to the
//! message the model reads, and that message only exists once the error
//! has been classified.
//!
//! Truncation is the interim. Where this is meant to go is #948 — output
//! over a threshold stored in the content-addressed store and handed to
//! the model as a reference — and this function is the seam that work
//! replaces. Until then, and until a ranged `file_read` exists (#947),
//! the marker says how much was dropped and does not offer the rest.

use fq_tools::{ToolError, ToolResult};

use crate::events::ToolErrorKind;
use crate::worker::reducer::emit::classify_tool_error;

/// A tool's outcome as everything downstream records it: a result with
/// its output capped, or an error classified into its kind and a capped
/// message. `max_bytes` is `[tools] max_result_bytes` and bounds the
/// text with its truncation marker.
pub(super) fn cap_outcome(
    outcome: Result<ToolResult, ToolError>,
    max_bytes: usize,
) -> Result<ToolResult, (ToolErrorKind, String)> {
    match outcome {
        Ok(result) => Ok(ToolResult {
            output: fq_tools::cap_output(result.output, max_bytes),
            is_error: result.is_error,
        }),
        Err(err) => {
            let (kind, message) = classify_tool_error(&err);
            Err((kind, fq_tools::cap_output(message, max_bytes)))
        }
    }
}
