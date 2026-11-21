//! `invocation.drop`'s input, and the precondition a live drop is armed
//! under: moved from `operator_surface.rs`, which the size cap holds at
//! 800 production lines.

/// The typed input of `invocation.drop` on the wire.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(super) struct DropCommandInput {
    pub(super) invocation_id: String,
    #[serde(default)]
    pub(super) reason: Option<String>,
    /// Halt the invocation first if this daemon is actively driving
    /// it. Without it, live work is refused outright — the kill
    /// switch is opt-in, never implied. Defaults false, which
    /// is also what every peer that predates the field sends.
    #[serde(default)]
    pub(super) live: bool,
}

/// What the live drop asks of the runner: which agent it is driving a
/// run for, and to arm a halt on that run. The runner is the one
/// implementation; a test gives another, to stand in for the instant
/// between the two answers.
pub(super) trait DriveHalts {
    fn active_agent(&self, invocation_id: &uuid::Uuid) -> Option<fq_runtime::AgentId>;
    fn arm_halt(&self, invocation_id: uuid::Uuid) -> Option<fq_runtime::worker::ArmedHalt>;
}

impl DriveHalts for fq_runtime::ReducerRunner<fq_runtime::Harness> {
    fn active_agent(&self, invocation_id: &uuid::Uuid) -> Option<fq_runtime::AgentId> {
        fq_runtime::ReducerRunner::active_agent(self, invocation_id)
    }

    fn arm_halt(&self, invocation_id: uuid::Uuid) -> Option<fq_runtime::worker::ArmedHalt> {
        fq_runtime::ReducerRunner::arm_halt(self, invocation_id)
    }
}

/// The live-drop precondition, daemon-side (plan Phase 4, verb 18):
/// the runner that would be driving this invocation is the only
/// zero-lag authority on whether it *is*, so the check and the halt
/// happen here — in the same handler that then writes the terminal
/// event, leaving no window between deciding and acting.
///
/// Returns **the agent the invocation belongs to**, when this daemon was
/// driving it, and the halt, when one could be armed. That is not a convenience: the runner is the
/// same authority that just justified stopping real work, and it is
/// ahead of every durable record (the projection is folded
/// asynchronously; nothing on the dispatch path writes an owner row).
/// Carrying its answer forward is what makes the drop's resolution
/// infallible whenever the halt was armed — so a call can never stop an
/// invocation and then report it unknown (#107).
///
/// Refusals are `InvalidInput`: they are verdicts on this request (a
/// bare drop of live work), not on the invocation's existence. The
/// operator reads the message verbatim, so each one names its remedy.
/// Every refusal here precedes the arm, so a refused drop stops nothing.
///
/// A run whose drive ends between the two answers, driven at the first
/// and gone by the arm, is not refused (issue 1070): the drop goes ahead
/// with no halt, as for any run this daemon is not driving. If the run
/// ended, the drop's fold records the request beside that ending; if its
/// drive stopped for another reason, the fold ends it as it ends any run
/// that has not ended. Refusing it would lose the request. The agent is
/// the runner's first answer, so the drop's resolution still cannot come
/// back unknown.
///
/// `queued` is the caller's hold on a run waiting for a worker permit
/// (#994), or deferred for a rate limit, if this is one. Held, it cannot
/// start; one whose permit came just before the hold is starting, and is
/// refused like the live run it is about to be.
pub(super) fn arm_drop_halt(
    runner: &impl DriveHalts,
    queued: Option<&fq_runtime::control_plane::dispatcher::QueuedHold>,
    input: &DropCommandInput,
) -> Result<Option<Armed>, fq_edge::wire::WireError> {
    let refuse = |message: String| fq_edge::wire::WireError::InvalidInput {
        op: "invocation.drop".into(),
        message,
    };
    let Ok(id) = uuid::Uuid::parse_str(&input.invocation_id) else {
        return Err(refuse(format!(
            "invalid invocation id `{}`",
            input.invocation_id
        )));
    };
    let Some(agent) = runner.active_agent(&id) else {
        if queued.is_some_and(|q| q.is_starting()) {
            return Err(refuse(format!(
                "invocation {id} is starting to run; retry with --live to halt and drop it"
            )));
        }
        return Ok(None);
    };
    if !input.live {
        return Err(refuse(format!(
            "invocation {id} is currently running; use --live to halt and drop it"
        )));
    }
    // The arm can lose to the invocation finishing between the
    // liveness check and here: no halt, and the drop goes ahead on a run
    // that has ended, whose fold keeps that ending and records the
    // request beside it.
    let halt = runner.arm_halt(id);
    Ok(Some(Armed { agent, halt }))
}

/// A run this daemon was driving: whose it is, and the halt armed on it,
/// with where the run's own row stood and, for a run whose first write was
/// under way, the word the run waits for (issue 1070). No halt when the
/// run ended before one could be armed.
pub(super) struct Armed {
    pub(super) agent: fq_runtime::AgentId,
    pub(super) halt: Option<fq_runtime::worker::ArmedHalt>,
}

impl Armed {
    /// The drop is published. A run whose first write was under way when
    /// the halt was armed is told so, and marks its own row once the write
    /// has landed, as the drop's fold does: the fold may have come before
    /// the row existed. Called with nothing awaited since the publish
    /// returned, so a request cut off after it cannot lose the word; an
    /// `Armed` dropped without this call tells the run the drop is not on
    /// the record, and its row is left as it is.
    pub(super) fn published(self) {
        if let Some(word) = self.halt.and_then(|halt| halt.word) {
            word.published("failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runner that drives the run at the first answer, and has let it go
    /// by the second: the run ended in between.
    struct EndsBetweenTheAnswers;

    impl DriveHalts for EndsBetweenTheAnswers {
        fn active_agent(&self, _: &uuid::Uuid) -> Option<fq_runtime::AgentId> {
            Some(fq_runtime::AgentId::new("agent").unwrap())
        }

        fn arm_halt(&self, _: uuid::Uuid) -> Option<fq_runtime::worker::ArmedHalt> {
            None
        }
    }

    fn input(live: bool) -> DropCommandInput {
        DropCommandInput {
            invocation_id: uuid::Uuid::now_v7().to_string(),
            reason: None,
            live,
        }
    }

    /// **A live drop of a run that ends before the halt is armed goes
    /// ahead** (issue 1070): no halt, the agent from the runner's first
    /// answer, and the drop records the request beside the ending the run
    /// reached. Refusing it would lose the request.
    #[test]
    fn a_live_drop_of_a_run_that_ends_before_the_arm_goes_ahead() {
        match arm_drop_halt(&EndsBetweenTheAnswers, None, &input(true)) {
            Ok(Some(armed)) => assert_eq!(armed.agent.as_str(), "agent"),
            Ok(None) => panic!("the drop went ahead, but without the agent the runner named"),
            Err(refused) => panic!(
                "a live drop of a run that ended before the halt could be armed was refused, \
                 which loses the request: {refused:?}"
            ),
        }
    }

    /// A bare drop of a run this daemon drives is refused, as before: the
    /// kill switch is opt-in.
    #[test]
    fn a_bare_drop_of_a_run_this_daemon_drives_is_refused() {
        let refused = arm_drop_halt(&EndsBetweenTheAnswers, None, &input(false));
        assert!(
            matches!(&refused, Err(fq_edge::wire::WireError::InvalidInput { message, .. })
                if message.contains("use --live")),
            "{:?}",
            refused.err()
        );
    }
}
