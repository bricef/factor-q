//! `[tools]` — what a tool call is allowed, and the per-tool sections
//! for the tools that manage part of it themselves.
//!
//! Its own module because the section grew a policy: the three keys at
//! the top level bound *every* tool call at the one place all of them
//! pass through, and the ceiling has to be reconciled with the per-tool
//! section beneath it. That reconciliation ([`ToolsConfig::validate`])
//! is the reason this is not just a pair of structs.

use std::time::Duration;

use serde::Deserialize;

use super::ConfigError;

/// Tool configuration — `[tools]` in `fqd.toml`.
///
/// The three keys at this level bound **every** tool call, built-in or
/// MCP, at the one place all of them pass through (`run_tool`). Before
/// them only `exec` had a deadline, so an MCP server that accepted a
/// request and never answered parked the invocation forever (review
/// finding B2, <https://github.com/bricef/factor-q/issues/547>).
/// Per-tool subsections — today only `[tools.exec]` — configure the
/// tools that manage their own timeout, within this ceiling.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolsConfig {
    /// The deadline applied to a tool that does not manage one of its
    /// own, in seconds. Default 120, matching `[tools.exec]
    /// default_timeout_secs`: a tool call is a single bounded action —
    /// a file read, an MCP request — and two minutes is already
    /// generous for one. A tool that legitimately needs longer is
    /// `exec`, which asks for its own.
    #[serde(default = "default_tool_default_timeout_secs")]
    pub default_timeout_secs: u64,
    /// The hard ceiling on any single tool call, in seconds. A tool's
    /// own request (`exec`'s `timeout_secs`) is clamped down to this,
    /// never rejected.
    ///
    /// Default 900, deliberately at the top of the range a real `exec`
    /// call needs — a fleet agent running a full `just ci` — rather
    /// than at the general default. This value may not sit *below*
    /// `[tools.exec] max_timeout_secs`; the daemon refuses to start if
    /// it does, because a lower general ceiling would silently override
    /// the one per-tool timeout that already worked.
    #[serde(default = "default_tool_max_timeout_secs")]
    pub max_timeout_secs: u64,
    /// How many tool calls may time out **in a row** before the
    /// invocation itself fails. Default 3.
    ///
    /// A timed-out call on its own is reported to the model as a tool
    /// error, so an agent can route around a slow tool. That is the
    /// right answer once and the wrong answer forever: against a dead
    /// MCP server every call times out, and an agent that keeps
    /// retrying spends its whole budget one deadline at a time. Any
    /// call that *returns* — success, or a tool-reported error — resets
    /// the count, because it proves the machinery is alive.
    #[serde(default = "default_max_consecutive_timeouts")]
    pub max_consecutive_timeouts: u32,
    #[serde(default)]
    pub exec: ExecToolConfig,
}

fn default_tool_default_timeout_secs() -> u64 {
    120
}

fn default_tool_max_timeout_secs() -> u64 {
    900
}

fn default_max_consecutive_timeouts() -> u32 {
    3
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: default_tool_default_timeout_secs(),
            max_timeout_secs: default_tool_max_timeout_secs(),
            max_consecutive_timeouts: default_max_consecutive_timeouts(),
            exec: ExecToolConfig::default(),
        }
    }
}

impl ToolsConfig {
    /// The three call bounds as the runner takes them.
    pub fn call_limits(&self) -> crate::tools::ToolCallLimits {
        crate::tools::ToolCallLimits {
            default_timeout: Duration::from_secs(self.default_timeout_secs),
            max_timeout: Duration::from_secs(self.max_timeout_secs),
            max_consecutive_timeouts: self.max_consecutive_timeouts,
        }
    }

    /// Three orderings that must hold, all checked rather than
    /// silently clamped, because a settings pair that disagrees is an
    /// operator mistake with an invisible consequence.
    ///
    /// **The general ceiling must be at least the `exec` ceiling.**
    /// `[tools] max_timeout_secs = 60` under a `[tools.exec]
    /// max_timeout_secs = 900` would cap every `exec` call at a minute
    /// while the `exec` section still said 900, and the operator would
    /// read the wrong number for as long as the daemon ran.
    ///
    /// **In each section, the default must not exceed its own
    /// ceiling.** A `default_timeout_secs` above `max_timeout_secs` is
    /// clamped everywhere it is used, so the file states a deadline
    /// nothing honours; and in `[tools.exec]` specifically it would let
    /// the child outlive the host's backstop, inverting the ordering
    /// the backstop grace exists to guarantee.
    ///
    /// **`exec`'s teardown must fit inside the host's backstop.** The
    /// group kill and the output drain both run *after* the call's
    /// deadline has passed, and the host cancels the call
    /// [`BACKSTOP_GRACE`](crate::tools::ToolCallLimits::BACKSTOP_GRACE)
    /// after that same deadline. Set the two graces to 5s between them
    /// and the host drops the call exactly as `exec` is escalating to
    /// `SIGKILL` — the process group the teardown existed to end
    /// survives, which is the defect this whole section is guarding
    /// against (<https://github.com/bricef/factor-q/issues/552>).
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self.max_timeout_secs < self.exec.max_timeout_secs {
            return Err(ConfigError::ToolCeilingBelowExec {
                tools_max: self.max_timeout_secs,
                exec_max: self.exec.max_timeout_secs,
            });
        }
        for (section, default, max) in [
            ("[tools]", self.default_timeout_secs, self.max_timeout_secs),
            (
                "[tools.exec]",
                self.exec.default_timeout_secs,
                self.exec.max_timeout_secs,
            ),
        ] {
            if default > max {
                return Err(ConfigError::ToolDefaultAboveMax {
                    section,
                    default,
                    max,
                });
            }
        }
        let backstop_secs = crate::tools::ToolCallLimits::BACKSTOP_GRACE.as_secs();
        if self.exec.teardown_budget_secs() >= backstop_secs {
            return Err(ConfigError::ExecTeardownExceedsBackstop {
                kill_grace: self.exec.kill_grace_secs,
                drain_grace: self.exec.drain_grace_secs,
                teardown: self.exec.teardown_budget_secs(),
                backstop: backstop_secs,
            });
        }
        Ok(())
    }
}

/// Timeouts for the built-in `exec` tool — `[tools.exec]` in `fqd.toml`.
///
/// The `fq-tools` crate keeps its own conservative defaults (30s default
/// / 300s max) so the primitive is safe in isolation; the runtime raises
/// them here (120s / 600s) because a fleet agent running a full `just ci`
/// legitimately needs headroom the crate-level ceiling would clamp away.
/// Tunable parameters are configuration, not code (Design Principle 8).
#[derive(Debug, Clone, Deserialize)]
pub struct ExecToolConfig {
    /// Timeout applied when a caller does not request one, in seconds.
    #[serde(default = "default_exec_default_timeout_secs")]
    pub default_timeout_secs: u64,
    /// Hard ceiling on any single `exec` call, in seconds. A
    /// caller-supplied `timeout_secs` above this is clamped down, not
    /// rejected, to avoid trapping an agent in a retry loop.
    #[serde(default = "default_exec_max_timeout_secs")]
    pub max_timeout_secs: u64,
    /// How long a timed-out process group gets to exit on `SIGTERM`
    /// before it is `SIGKILL`ed, in seconds. Default 2.
    ///
    /// **Invariant**: `kill_grace_secs + drain_grace_secs` must be
    /// strictly below the host's backstop grace (`BACKSTOP_GRACE` in
    /// `crate::tools`, 5s), the delay after a call's deadline at which
    /// the host cancels it. The two graces run back to back *after*
    /// the deadline has already passed, so their sum is exactly how
    /// long `exec` still needs; at or past the backstop the host drops
    /// the call mid-teardown and the group being killed can outlive
    /// it. Checked at load by [`ToolsConfig`]'s validation rather than
    /// clamped, because an operator who raised a grace deliberately
    /// should be told it does not fit, not quietly given a shorter
    /// one.
    #[serde(default = "default_exec_kill_grace_secs")]
    pub kill_grace_secs: u64,
    /// How long output capture may continue after the child is gone,
    /// in seconds. Default 2. Bounds the drain so a descendant that
    /// inherited the pipe and left the process group cannot hold the
    /// tool open (<https://github.com/bricef/factor-q/issues/176>);
    /// the kernel pipe buffer flushes in far less. Bound by the same
    /// invariant as `kill_grace_secs`.
    #[serde(default = "default_exec_drain_grace_secs")]
    pub drain_grace_secs: u64,
}

fn default_exec_default_timeout_secs() -> u64 {
    120
}

fn default_exec_max_timeout_secs() -> u64 {
    600
}

fn default_exec_kill_grace_secs() -> u64 {
    2
}

fn default_exec_drain_grace_secs() -> u64 {
    2
}

impl Default for ExecToolConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: default_exec_default_timeout_secs(),
            max_timeout_secs: default_exec_max_timeout_secs(),
            kill_grace_secs: default_exec_kill_grace_secs(),
            drain_grace_secs: default_exec_drain_grace_secs(),
        }
    }
}

impl ExecToolConfig {
    /// Convert to the `fq-tools` [`ExecConfig`](fq_tools::builtin::ExecConfig),
    /// mapping the two timeouts and the two teardown graces, and
    /// preserving the crate's defaults for the fields this section does
    /// not expose (`max_output_bytes`, `default_path`).
    pub fn to_exec_config(&self) -> fq_tools::builtin::ExecConfig {
        fq_tools::builtin::ExecConfig {
            default_timeout: Duration::from_secs(self.default_timeout_secs),
            max_timeout: Duration::from_secs(self.max_timeout_secs),
            kill_grace: Duration::from_secs(self.kill_grace_secs),
            drain_grace: Duration::from_secs(self.drain_grace_secs),
            ..fq_tools::builtin::ExecConfig::default()
        }
    }

    /// How long a timed-out `exec` call still needs after its deadline:
    /// the group kill, then the bounded output drain, back to back.
    fn teardown_budget_secs(&self) -> u64 {
        self.kill_grace_secs.saturating_add(self.drain_grace_secs)
    }
}
