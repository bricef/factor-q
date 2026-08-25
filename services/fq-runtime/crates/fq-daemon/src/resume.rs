//! `fq invocation resume` as a protocol: the request and response both sides
//! serialise, and the daemon-side handler that services one.
//!
//! Split out of `lib.rs` (#189). The daemon owns the worker store and is the
//! only process allowed to mutate the WAL, so every decision below has to be
//! made here.
//!
//! It used to be reached by a bespoke NATS request/reply — the last thing a
//! client spoke to the broker for, and the reason `fq` linked one at all. It
//! is a declared command on the authenticated edge now, registered by
//! [`register_resume_command`]; the transport is the edge's, and a refusal is
//! a wire error rather than a flag inside a successful answer.

use std::sync::Arc;

use fq_edge::wire::WireError;
pub(crate) use fq_ops::surface::{InvocationResumeRequest, InvocationResumeResponse};
use fq_runtime::agent::AgentId;
use fq_runtime::events::{Event, EventPayload};
use fq_runtime::llm::LlmClient;
use fq_runtime::{ControlPlaneStore, EventBus, SharedRegistry};
use uuid::Uuid;

// `InvocationResumeResponse` is `fq_ops::surface`'s now, and an inherent impl has to
// sit with its type. These are the daemon's use of it, so they
// become free functions here rather than travelling to a crate
// that has no reason to know what a page cap or a refusal means.
/// Precondition refusal: nothing was injected yet, so there are no
/// completed call ids to report.
pub(crate) fn rejected(message: impl Into<String>) -> InvocationResumeResponse {
    InvocationResumeResponse {
        ok: false,
        message: message.into(),
        completed_call_ids: Vec::new(),
    }
}

/// Daemon-side dependencies for servicing `fq invocation resume`
/// requests (#373), grouped so the registered command holds one handle
/// rather than closing over six.
pub struct ResumeControl {
    pub(crate) bus: EventBus,
    pub(crate) worker_store: Arc<fq_runtime::WorkerStore>,
    pub(crate) cp_store: Arc<ControlPlaneStore>,
    pub(crate) runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
    pub(crate) registry: SharedRegistry,
    pub(crate) llm: Arc<dyn LlmClient>,
}

impl ResumeControl {
    /// Assemble the handle `invocation.resume` runs against.
    ///
    /// Public because the fields are not: an integration test builds
    /// one to register the surface, and widening six fields to let it
    /// would make every one of them part of the crate's API rather
    /// than just the act of constructing the whole.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bus: EventBus,
        worker_store: Arc<fq_runtime::WorkerStore>,
        cp_store: Arc<ControlPlaneStore>,
        runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
        registry: SharedRegistry,
        llm: Arc<dyn LlmClient>,
    ) -> Self {
        ResumeControl {
            bus,
            worker_store,
            cp_store,
            runner,
            registry,
            llm,
        }
    }
}

/// Service one `fq invocation resume` request on the daemon side: enforce
/// the Ambiguous-only precondition, durably inject interrupted results,
/// publish the `invocation.operator_resumed` audit event, and re-drive the
/// invocation through the existing SafeReplay recovery path (#373).
///
/// Every refusal returns a distinct message because the CLI surfaces it
/// verbatim — the operator must be able to tell terminal / live /
/// already-resumed / unknown apart without reading daemon logs.
pub(crate) async fn handle_resume_request(
    control: &ResumeControl,
    req: InvocationResumeRequest,
) -> InvocationResumeResponse {
    use fq_runtime::control_plane::OwnerStatus;
    use fq_runtime::events::InvocationOperatorResumedPayload;
    use fq_runtime::worker::{DispatchStatus, RecoveryCategory, categorise};

    let id = &req.invocation_id;
    let state = match control.worker_store.get_invocation_state(id).await {
        Ok(Some(state)) => state,
        Ok(None) => {
            // No WAL row. The control plane can still tell a finished
            // invocation apart from a never-seen id, so the operator
            // learns which mistake they made.
            let owner = control
                .cp_store
                .get_invocation_owner(id)
                .await
                .ok()
                .flatten();
            return rejected(match owner.map(|o| o.status) {
                Some(OwnerStatus::Completed | OwnerStatus::Failed) => {
                    format!("invocation {id} is terminal and cannot be resumed")
                }
                _ => format!("unknown invocation {id}"),
            });
        }
        Err(err) => {
            return rejected(format!("failed to inspect invocation: {err}"));
        }
    };
    if state.terminal_at.is_some() {
        return rejected(format!("invocation {id} is terminal and cannot be resumed"));
    }
    // The worker store alone cannot see an operator drop (#374): drop
    // writes coordination only, so the WAL state row still reads
    // non-terminal here. Coordination is the authority on operator
    // terminal decisions — consult it before touching the WAL, or
    // resume would happily overrule a drop.
    let owner = control
        .cp_store
        .get_invocation_owner(id)
        .await
        .ok()
        .flatten();
    if let Some(owner) = &owner
        && matches!(owner.status, OwnerStatus::Completed | OwnerStatus::Failed)
    {
        return rejected(format!(
            "invocation {id} is terminal (completed, failed, or operator-dropped) \
             and cannot be resumed"
        ));
    }
    // Ambiguous-shaped is not crashed: the dispatched mark lands
    // BEFORE tool execution (by design), so a healthy invocation
    // mid-tool has the same dispatched-without-completed WAL as a
    // crashed one. Liveness comes from the process that actually
    // knows — this daemon's own runner. v1 runs one daemon per
    // store, so "not driven by this runner" IS orphaned; the
    // coordination owner rows can't answer with zero lag (and carry
    // placeholder worker ids for crashed runs — the resume e2e
    // pinned both). Cross-worker liveness is the #107/#374 story.
    if let Ok(uuid) = uuid::Uuid::parse_str(id)
        && control.runner.is_active(&uuid)
    {
        return rejected(format!(
            "invocation {id} is executing on this daemon right now — resume \
             is for crashed invocations; drain or wait for it instead"
        ));
    }

    let tools = control
        .worker_store
        .list_tool_dispatches_for_invocation(id)
        .await;
    let llms = control
        .worker_store
        .list_llm_dispatches_for_invocation(id)
        .await;
    let (Ok(tools), Ok(llms)) = (tools, llms) else {
        return rejected("failed to inspect invocation WAL");
    };

    // Interrupted-result injection only reconciles tool calls — a stuck
    // LLM dispatch has no tool_dispatch row to complete, so resume cannot
    // help and must say so rather than mislabel the state.
    if llms.iter().any(|l| l.status == DispatchStatus::Dispatched) {
        return rejected(format!(
            "invocation {id} is ambiguous in an LLM dispatch; \
             interrupted-result injection applies only to tool calls"
        ));
    }
    if categorise(&state, &tools, &llms) != RecoveryCategory::Ambiguous {
        // Non-ambiguous splits into live (a worker still owns it) vs
        // already-recovered — distinct errors per #373's precondition.
        let live = control
            .cp_store
            .get_invocation_owner(id)
            .await
            .ok()
            .flatten()
            .is_some_and(|o| o.status == OwnerStatus::InFlight);
        return rejected(if live {
            format!("invocation {id} is live; only Ambiguous invocations can be resumed")
        } else {
            format!("invocation {id} is not Ambiguous (it may already have been resumed)")
        });
    }

    let ids = match control.worker_store.inject_interrupted_results(id).await {
        Ok(ids) => ids,
        Err(err) => {
            return rejected(format!("failed to inject interrupted result: {err}"));
        }
    };

    // From here on the injection is durable, so even failure responses
    // carry the completed call ids: the WAL changed whether or not the
    // re-drive could be started.
    let (Ok(agent_id), Ok(invocation_id)) =
        (AgentId::new(state.agent_id.clone()), Uuid::parse_str(id))
    else {
        return InvocationResumeResponse {
            ok: false,
            message: "stored invocation identity is invalid".into(),
            completed_call_ids: ids,
        };
    };

    // Audit is best-effort: the durable injection is the source of truth,
    // and a lost event must not abort the resume.
    let event = Event::new(
        agent_id.clone(),
        invocation_id,
        EventPayload::InvocationOperatorResumed(InvocationOperatorResumedPayload {
            completed_call_ids: ids.clone(),
            reason: req.reason,
        }),
    );
    if let Err(err) = control.bus.publish(&event).await {
        tracing::warn!(error = %err, "failed to publish invocation.operator_resumed");
    }

    let Some(agent) = control.registry.read().await.get(&agent_id).cloned() else {
        return InvocationResumeResponse {
            ok: false,
            message: format!("agent {} is not loaded", state.agent_id),
            completed_call_ids: ids,
        };
    };
    // Detached like startup recovery: the reply must not wait on the
    // re-driven invocation, which can run for minutes.
    let runner = control.runner.clone();
    let llm = control.llm.clone();
    tokio::spawn(async move {
        if let Err(err) = runner.resume(&agent, llm.as_ref(), invocation_id).await {
            tracing::error!(error = %err, %invocation_id, "operator resume failed");
        }
    });
    InvocationResumeResponse {
        ok: true,
        message: "resume accepted".into(),
        completed_call_ids: ids,
    }
}

/// Declare `invocation.resume` on the edge.
///
/// Its own function for the same reason the reports have theirs: the
/// registry assembles the surface, and one op's declaration plus its
/// handler is a unit that reads better beside the logic it calls than
/// inside a list of every other op.
pub(crate) fn register_resume_command(
    registry: &mut fq_edge::EdgeRegistry,
    resume_control: std::sync::Arc<ResumeControl>,
) -> anyhow::Result<()> {
    let decl = fq_ops::Command::new::<fq_ops::surface::InvocationResumeRequest>(
        fq_ops::Invocation::Resume,
        fq_ops::Authority {
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Invocation,
        },
        "Resume a crashed invocation: reconcile its stuck tool calls with an \
         interrupted result, then re-drive it through the recovery path.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Only an invocation that crashed mid-tool-call can be resumed. A \
         refusal is an error, not a receipt: it names which precondition \
         failed — terminal, live, already resumed, or unknown — because \
         those are four different things for an operator to do next. \
         Success is a receipt; the re-drive runs detached, so the invocation \
         is still working when this returns.",
    );
    registry
        .command::<fq_ops::surface::InvocationResumeRequest, _, _>(
            decl,
            move |input: fq_ops::surface::InvocationResumeRequest| {
                let control = resume_control.clone();
                async move {
                    let id = input.invocation_id.clone();
                    let answer = crate::resume::handle_resume_request(&control, input).await;
                    // A refusal is an error here, not a successful
                    // response carrying `ok: false`. That flag was the
                    // request/reply shape, where the only channel was
                    // the payload; on the edge a caller that ignores it
                    // would read a refusal as a resume. The four
                    // preconditions keep their distinct messages —
                    // terminal, live, already resumed, unknown — because
                    // they are four different things to do next.
                    if !answer.ok {
                        // `InvalidInput`, following `invocation.drop`:
                        // "refusals are verdicts on this request". Not
                        // `Conflict`, whose contract is narrower than it
                        // looks — it means the work is already done and
                        // *must* name the atom the first call produced.
                        // Only "already resumed" is that; terminal and
                        // live are verdicts on current state, and an
                        // unknown id is a `NotFound`.
                        //
                        // Telling those apart needs a typed refusal out
                        // of `handle_resume_request`, which returns one
                        // message string across seven sites. Until then
                        // this is the variant the sibling verb settled
                        // on, and the message carries which it was.
                        return Err(WireError::InvalidInput {
                            op: "invocation.resume".into(),
                            message: answer.message,
                        });
                    }
                    Ok(fq_ops::Receipt::naming(
                        fq_ops::Domain::Invocation,
                        serde_json::json!({ "invocation_id": id }),
                    ))
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}
