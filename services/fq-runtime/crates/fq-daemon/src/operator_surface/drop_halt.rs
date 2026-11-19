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

/// The live-drop precondition, daemon-side (plan Phase 4, verb 18):
/// the runner that would be driving this invocation is the only
/// zero-lag authority on whether it *is*, so the check and the halt
/// happen here — in the same handler that then writes the terminal
/// event, leaving no window between deciding and acting.
///
/// Returns **the agent the halted invocation belongs to**, when this
/// daemon was driving it. That is not a convenience: the runner is the
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
/// `queued` is the caller's hold on a run waiting for a worker permit
/// (#994), or deferred for a rate limit, if this is one. Held, it cannot
/// start; one whose permit came just before the hold is starting, and is
/// refused like the live run it is about to be.
pub(super) fn arm_drop_halt(
    runner: &fq_runtime::ReducerRunner<fq_runtime::Harness>,
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
    // liveness check and here. Report that rather than publishing
    // a terminal event: the operator-recovered handler
    // deliberately overrides any prior status, so a run that
    // completed on its own would be archived as `failed`.
    let Some(halt) = runner.arm_halt(id) else {
        return Err(refuse(format!(
            "invocation {id} finished before the halt could be armed; \
             nothing dropped — re-check `fq invocation show {id}`"
        )));
    };
    Ok(Some(Armed { agent, halt }))
}

/// A halt armed on a run this daemon is driving: whose it is, where the
/// run's own row stood, and, for a run whose first write was under way,
/// the word the run waits for (issue 1070).
pub(super) struct Armed {
    pub(super) agent: fq_runtime::AgentId,
    pub(super) halt: fq_runtime::worker::ArmedHalt,
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
        if let Some(word) = self.halt.word {
            word.published("failed");
        }
    }
}
