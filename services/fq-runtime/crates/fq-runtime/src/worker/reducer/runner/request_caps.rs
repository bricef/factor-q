//! How many requests an MCP server may send back to the host while its
//! tools run (#831, review finding A8).
//!
//! A granted server that holds one `tools/call` open can ask for a
//! sampling completion or an elicitation again and again, for as long as
//! the tool's deadline lets it, and every answer is a model call on the
//! agent's account. The invocation budget bounds that only in dollars,
//! only after each call, and only when the definition sets one. Two caps
//! bound it in requests, before anything is spent:
//!
//! - **per tool call**, `[worker] max_server_requests_per_tool_call`
//!   (default 8) — the loop guard. A server that needs more than a
//!   handful of completions to answer one tool call is doing the agent's
//!   job.
//! - **per invocation**, `[worker] max_server_requests_per_invocation`
//!   (default 100) — the backstop, for a server that stays under the
//!   first cap on every call of a long run. An honest heavy run, forty
//!   tool calls at two requests each, is 80.
//!
//! Sampling and elicitation draw on the same two counts: each is a model
//! call the server asked for, and a server granted both must not get
//! twice the allowance by alternating.
//!
//! **A request counts when it is admitted to spend.** The handlers run
//! every check that costs nothing first — the grant, the sub-budget, the
//! invocation budget, elicitation's inbound policies — and this one last,
//! ahead of the input evaluators, which are model calls themselves. So a
//! request declined for free never uses up the allowance, and a request
//! over a cap never reaches a model. Over a cap the server is declined
//! the way any refused request is, and the invocation carries on: the
//! decline is the server's to handle, not the agent's failure.
//!
//! **The counts are attempt-scoped.** They live on the run's
//! `SamplingChannel`, and only a live run has one. A resumed run is
//! handed none, because the per-invocation server connection died with
//! the attempt that started it (ADR-0018 §5), so it has no requests to
//! count and nothing for resume to reconstitute — unlike the spend, which
//! resume rebuilds from the WAL so the dollar ceilings span every attempt.

use std::fmt;

/// `[worker] max_server_requests_per_tool_call` when the file sets none.
const DEFAULT_PER_TOOL_CALL: u32 = 8;
/// `[worker] max_server_requests_per_invocation` when the file sets none.
const DEFAULT_PER_INVOCATION: u32 = 100;

/// The two caps on server-initiated requests, as the runner takes them
/// from `[worker]`. The default is that section's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerRequestLimits {
    /// Requests admitted while one tool call runs.
    pub per_tool_call: u32,
    /// Requests admitted across one invocation's tool calls.
    pub per_invocation: u32,
}

impl Default for ServerRequestLimits {
    fn default() -> Self {
        Self {
            per_tool_call: DEFAULT_PER_TOOL_CALL,
            per_invocation: DEFAULT_PER_INVOCATION,
        }
    }
}

/// How many server-initiated requests a live run has admitted: in the
/// tool call being serviced now, and in all.
#[derive(Debug, Default)]
pub(super) struct ServerRequestCount {
    tool_call: u32,
    invocation: u32,
    /// Whether a cap has refused a request yet. The first refusal is the
    /// one worth a warning; a server past a cap usually keeps asking.
    refused_before: bool,
}

impl ServerRequestCount {
    /// A tool call's servicing begins: its count starts again, the
    /// invocation's carries on.
    pub(super) fn begin_tool_call(&mut self) {
        self.tool_call = 0;
    }

    /// Admit one more request, or name the cap that refuses it. When
    /// both are reached the invocation's is named, because no later tool
    /// call lifts it.
    pub(super) fn admit(&mut self, limits: ServerRequestLimits) -> Result<(), CapReached> {
        let cap = if self.invocation >= limits.per_invocation {
            Cap::PerInvocation
        } else if self.tool_call >= limits.per_tool_call {
            Cap::PerToolCall
        } else {
            self.tool_call += 1;
            self.invocation += 1;
            return Ok(());
        };
        Err(CapReached {
            cap,
            limit: cap.limit(limits),
            first: !std::mem::replace(&mut self.refused_before, true),
        })
    }
}

/// Which of the two caps refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cap {
    PerToolCall,
    PerInvocation,
}

impl Cap {
    /// The `[worker]` key that sets this cap.
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::PerToolCall => "max_server_requests_per_tool_call",
            Self::PerInvocation => "max_server_requests_per_invocation",
        }
    }

    fn scope(self) -> &'static str {
        match self {
            Self::PerToolCall => "tool call",
            Self::PerInvocation => "invocation",
        }
    }

    fn limit(self, limits: ServerRequestLimits) -> u32 {
        match self {
            Self::PerToolCall => limits.per_tool_call,
            Self::PerInvocation => limits.per_invocation,
        }
    }
}

/// A request refused by a cap: which one, its value, and whether this is
/// the run's first refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CapReached {
    pub(super) cap: Cap,
    pub(super) limit: u32,
    pub(super) first: bool,
}

/// What the server is told: the cap, its value and the setting, so the
/// operator who reads it in a server's log knows which line to change.
impl fmt::Display for CapReached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "this {} has reached its cap of {} server-initiated requests (`[worker] {}`)",
            self.cap.scope(),
            self.limit,
            self.cap.key()
        )
    }
}

#[cfg(test)]
mod tests;
