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
use tokio::time::timeout_at;

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
    /// The deadline is measured over the whole wait, servicing
    /// included — a tool is no less stuck for the host having been busy
    /// — but it is an *arm of the same `select!`*, never a `timeout`
    /// wrapped around the loop. That distinction is the whole design.
    ///
    /// A `timeout` around the loop looks equivalent and is not, because
    /// it can fire while a branch body is mid-`await`, dropping it.
    /// Three things break when the body it drops is
    /// `handle_server_request`:
    ///
    /// - the provider call inside it is abandoned after the request
    ///   went out, so its **cost is never recorded** — and cost
    ///   information is the one thing this system never loses;
    /// - the server's sampling request is never answered, so a
    ///   well-behaved server waits on a reply that is not coming;
    /// - a `publish_chained` dropped between the bus accepting an event
    ///   and `*cursor = Some(id)` **forks the event chain**: the event
    ///   is on the bus, and the next one records the wrong parent.
    ///
    /// As an arm, the deadline can only win *between* servicings: a
    /// `select!` branch body runs to completion once chosen, so an
    /// in-flight `handle_server_request` finishes and the timeout is
    /// taken on the next pass. The tool future is the only thing
    /// dropped, which is the one drop that is safe — a `Tool::execute`
    /// that needs to clean up is told its deadline through
    /// [`ToolContext::deadline`](fq_tools::ToolContext::deadline) and
    /// acts on it first (the MCP adapter sends
    /// `notifications/cancelled` there).
    ///
    /// The cost of that choice is that the deadline cannot interrupt a
    /// single wedged server request. It does not need to: the model
    /// call inside one carries `[worker] llm_timeout_secs` (#546), so
    /// the servicing is itself bounded.
    ///
    /// **A servicing that outlives the backstop hands the tool its full
    /// grace back.** The gap between `allowed` and `armed` is not slack
    /// in the deadline, it is the tool's teardown budget — `exec`'s
    /// group kill and output drain, the MCP adapter's
    /// `notifications/cancelled` — and a servicing bounded only by
    /// `llm_timeout_secs` would otherwise spend all of it while the tool
    /// future sits unpolled. The tool would then get exactly one poll
    /// before an already-elapsed expiry won and dropped it: `exec`'s
    /// child would die by `kill_on_drop` and the output it had captured
    /// would reach the model as a bare "timed out" instead. So after
    /// each servicing an expiry that has already passed is re-armed at
    /// `now + (armed − allowed)`. The tool's *own* deadline never moves,
    /// only the host's backstop, and only ever to one grace after the
    /// last servicing — a chatty server buys the tool no extra running
    /// time, just the teardown it was always owed (#617).
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
        // The tool's teardown budget, held apart from the deadline
        // itself because a servicing can give it back (see below).
        let grace = deadline.armed.saturating_sub(deadline.allowed);
        let mut expires_at = tokio::time::Instant::now() + deadline.armed;
        let timed_out = || {
            warn!(
                agent_id = %agent_id,
                invocation_id = %invocation_id,
                tool = %tool_name,
                deadline_secs = deadline.allowed.as_secs(),
                "tool call passed its deadline and the host backstop fired; abandoning the call"
            );
            Ok(Err(ToolError::TimedOut {
                after: deadline.allowed,
                // The host stopped waiting; it stopped nothing else.
                output: None,
            }))
        };

        let Some(channel) = sampling else {
            // Nothing else is being awaited, so nothing else can be
            // dropped: a plain deadline on the tool future is exactly
            // right.
            return match timeout_at(expires_at, tool.execute(ctx, params)).await {
                Ok(result) => Ok(result),
                Err(_) => timed_out(),
            };
        };

        let tool_fut = tool.execute(ctx, params);
        tokio::pin!(tool_fut);
        let expiry = tokio::time::sleep_until(expires_at);
        tokio::pin!(expiry);
        loop {
            tokio::select! {
                // Ordered, not random. The tool first: if it finished
                // during the last servicing, that answer beats a
                // deadline that has since passed. Then the deadline,
                // so a chatty server cannot hold the call past it by
                // always having another request ready. Requests last.
                biased;
                result = &mut tool_fut => break Ok(result),
                _ = &mut expiry => break timed_out(),
                maybe_req = channel.recv() => match maybe_req {
                    Some((server, request)) => {
                        let mut ctx = InvocationCtx::new(
                            llm, agent_id, invocation_id, totals, cursor,
                        );
                        // Runs to completion — see the note above on
                        // why this must never be a dropped future.
                        self.handle_server_request(&mut ctx, agent, &server, request)
                            .await?;
                        // The host was busy, not the tool. Give back the
                        // teardown budget this servicing consumed,
                        // measured from the moment the tool is polled
                        // again, so a co-operative tool still gets to
                        // stop its work and answer (#617).
                        let now = tokio::time::Instant::now();
                        if now >= expires_at {
                            expires_at = now + grace;
                            expiry.as_mut().reset(expires_at);
                        }
                    }
                    // All servers' channels closed: nothing left to
                    // service, so the tool alone, still bounded.
                    None => break match timeout_at(expires_at, &mut tool_fut).await {
                        Ok(result) => Ok(result),
                        Err(_) => timed_out(),
                    },
                }
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
