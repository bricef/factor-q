//! The deadline on a tool call, and what a run of missed deadlines
//! does to the invocation.
//!
//! Split from `runner.rs`, which sits on a size budget that only
//! tightens. Two methods, both entered from `run_tool` and nowhere
//! else: the await that bounds one call, and the judgement on the
//! streak that call belongs to.
//!
//! Before this existed, `run_tool` awaited `Tool::execute` with no
//! deadline at all. Only `exec` had one, so an MCP server that accepted
//! a request and never answered — or a `file_read` on a FIFO — parked
//! the invocation for as long as the daemon lived, with the drain
//! unable to suspend it and the heartbeat, which measures process
//! liveness rather than progress, reporting nothing wrong (review
//! finding B2, <https://github.com/bricef/factor-q/issues/547>).

use fq_tools::{Tool, ToolContext, ToolError, ToolResult};
use tokio::time::timeout;

use super::*;

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Run one tool to completion, to its deadline, or until the host
    /// backstop fires — whichever comes first — servicing any
    /// server-initiated requests that arrive while it runs.
    ///
    /// The `select!` over the sampling channel is why this is not a
    /// bare `tokio::time::timeout` at the call site. While a tool runs,
    /// the MCP server it belongs to may initiate requests back at us
    /// (sampling, elicitation) — those arrive *because* the agent
    /// called this tool, landing while we are parked at the await. The
    /// runner is the sole LLM arbiter, so it services them here rather
    /// than letting a second caller exist, and without blocking the
    /// tool (ADR-0018 §2). With no channel wired this is a plain await.
    ///
    /// The deadline covers the whole loop, servicing included: wall
    /// clock is wall clock, and a tool is no less stuck for the host
    /// having been busy.
    ///
    /// The outer `Err` is infrastructure (a server request that failed
    /// to publish); a timeout is an ordinary tool error in the inner
    /// `Result`, which is what makes it something the model reads and
    /// reacts to rather than a failed invocation.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn await_tool_under_deadline(
        &self,
        tool: &dyn Tool,
        ctx: &ToolContext<'_>,
        params: Value,
        deadline: crate::tools::CallDeadline,
        tool_name: &str,
        agent: &Agent,
        llm: &dyn LlmClient,
        agent_id: &AgentId,
        invocation_id: Uuid,
        totals: &mut InvocationTotals,
        cursor: &mut Option<Uuid>,
        sampling: Option<&mut SamplingChannel>,
    ) -> Result<Result<ToolResult, ToolError>, ExecutorError> {
        let running = async {
            match sampling {
                None => Ok(tool.execute(ctx, params).await),
                Some(channel) => {
                    let tool_fut = tool.execute(ctx, params);
                    tokio::pin!(tool_fut);
                    loop {
                        tokio::select! {
                            // Bias toward completing the tool: if both
                            // are ready, return the tool result rather
                            // than starving it behind a backlog of
                            // requests.
                            biased;
                            result = &mut tool_fut => break Ok(result),
                            maybe_req = channel.recv() => match maybe_req {
                                Some((server, request)) => {
                                    let mut ctx = InvocationCtx::new(
                                        llm, agent_id, invocation_id, totals, cursor,
                                    );
                                    self.handle_server_request(&mut ctx, agent, &server, request)
                                        .await?;
                                }
                                // All servers' channels closed: just
                                // await the tool to completion.
                                None => break Ok((&mut tool_fut).await),
                            }
                        }
                    }
                }
            }
        };

        match timeout(deadline.armed, running).await {
            Ok(outcome) => outcome,
            Err(_) => {
                warn!(
                    agent_id = %agent_id,
                    invocation_id = %invocation_id,
                    tool = %tool_name,
                    deadline_secs = deadline.allowed.as_secs(),
                    "tool call passed its deadline and the host backstop fired; abandoning the call"
                );
                Ok(Err(ToolError::TimedOut {
                    after: deadline.allowed,
                }))
            }
        }
    }

    /// Advance or clear the invocation's consecutive-timeout run, and
    /// fail the invocation when the run reaches
    /// `[tools] max_consecutive_timeouts`.
    ///
    /// One timeout is a fact the model can act on; a run of them means
    /// nothing is answering, and an agent left to keep trying spends
    /// its whole budget one deadline at a time. Ending the invocation
    /// is the cheaper failure — the trigger record and the WAL are
    /// intact, and a human or a retry sees a terminal `failed` naming
    /// the count instead of a budget quietly consumed.
    ///
    /// Any other outcome clears the run: a tool that returns an *error*
    /// has still answered, which is the thing a run of timeouts says is
    /// not happening.
    pub(super) async fn judge_timeout_streak(
        &self,
        kind: ToolErrorKind,
        agent_id: &AgentId,
        invocation_id: Uuid,
        totals: &mut InvocationTotals,
        start: Instant,
        cursor: &mut Option<Uuid>,
    ) -> Result<(), ExecutorError> {
        if kind != ToolErrorKind::Timeout {
            self.timeouts.record_answer(invocation_id);
            return Ok(());
        }
        let streak = self.timeouts.record_timeout(invocation_id);
        let limit = self.config.tool_limits.max_consecutive_timeouts;
        if limit == 0 || streak < limit {
            return Ok(());
        }
        totals.total_duration_ms = start.elapsed().as_millis() as u64;
        let kind = FailureKind::ToolError;
        let message = format!(
            "{streak} consecutive tool calls timed out (limit {limit}, \
             `[tools] max_consecutive_timeouts`); no tool is answering, so the invocation is \
             ended rather than left to spend its budget one deadline at a time"
        );
        self.emit_failed(
            agent_id,
            invocation_id,
            kind,
            message.clone(),
            FailurePhase::ToolCall,
            *totals,
            cursor,
        )
        .await?;
        Err(ExecutorError::InvocationFailed { kind, message })
    }
}
