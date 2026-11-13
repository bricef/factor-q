//! The daemon's startup recovery: reconciling worker rows against operator
//! terminal decisions, classifying what was in flight when the last process
//! stopped, recording on each one's trigger claim that it durably started,
//! restoring coordination ownership, and re-driving the safe cases.
//!
//! Split out of `run_daemon` (#189) as three straight lifts, called at exactly
//! the points they used to occupy — the order is load-bearing. Reconciliation
//! runs before classification so a pre-existing orphan cannot be reported
//! ambiguous again, and the resume tasks are spawned only after the runner
//! that drives them exists, and queued for a worker permit before the
//! dispatcher is constructed, which puts every one of them ahead of every
//! trigger (#994). The claim marks must land before the trigger
//! dispatcher consumes anything (#959), or it could meet the redelivery of a
//! trigger whose invocation is in flight while its claim still read
//! `claimed`, and start the trigger a second time. That order is held by
//! type rather than by call order: the [`TriggerClaims`] a dispatcher needs
//! to arbitrate exist only as what the recording returns.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use fq_runtime::agent::{AgentId, AgentRegistry};
use fq_runtime::control_plane::dispatcher::{RunAdmission, TriggerClaims, Waiter};
use fq_runtime::events::{Event, EventPayload};
use fq_runtime::llm::LlmClient;
use fq_runtime::worker::{ClassifiedInvocation, ExecutorError, ResumeError, WorkerId};
use fq_runtime::{ControlPlaneStore, EventBus, WorkerStore};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Publish `invocation.ambiguous` at most once per invocation (#64).
///
/// Claims the worker store's one-shot stamp (`ambiguous_reported_at`)
/// before publishing, so a restart that re-classifies the same
/// invocation as ambiguous — or re-fails the same resume — does not
/// re-fire the event. `stamp_key` is the store's invocation-id string
/// (normally equal to `invocation_id`; the scan path passes the raw
/// row id so a malformed stored uuid still stamps its own row).
///
/// Claim-then-publish is deliberately at-most-once: a publish failure
/// after a successful claim is logged and not retried. A claim *error*
/// (store unavailable) publishes anyway — it doesn't prove the event
/// was already sent, and a possible duplicate beats re-silencing the
/// failure mode #64 exists to make loud.
///
/// Returns what it did, so a caller's own log line can say so.
async fn publish_ambiguous_once(
    worker_store: &fq_runtime::WorkerStore,
    bus: &EventBus,
    agent_id: AgentId,
    invocation_id: Uuid,
    stamp_key: &str,
    payload: fq_runtime::events::InvocationAmbiguousPayload,
) -> AmbiguousReport {
    let now_ms = chrono::Utc::now().timestamp_millis();
    match worker_store
        .mark_ambiguous_reported(stamp_key, now_ms)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                invocation_id = %invocation_id,
                "invocation.ambiguous already reported for this invocation; not re-firing"
            );
            return AmbiguousReport::AlreadyReported;
        }
        Err(err) => {
            tracing::error!(
                invocation_id = %invocation_id,
                error = %err,
                "failed to claim ambiguous-report stamp; publishing anyway (may duplicate)"
            );
        }
    }
    let event = Event::new(
        agent_id,
        invocation_id,
        EventPayload::InvocationAmbiguous(payload),
    );
    if let Err(err) = bus.publish(&event).await {
        tracing::error!(
            invocation_id = %invocation_id,
            error = %err,
            "failed to publish invocation.ambiguous (stamp already claimed; will not retry)"
        );
        return AmbiguousReport::PublishFailed;
    }
    AmbiguousReport::Published
}

/// What [`publish_ambiguous_once`] did for one invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AmbiguousReport {
    /// It published `invocation.ambiguous`.
    Published,
    /// An earlier start reported the invocation, so nothing was published.
    AlreadyReported,
    /// It went to publish, and the publish failed. Logged, not retried.
    PublishFailed,
}

impl AmbiguousReport {
    /// The end of a log line saying what happened to the report.
    fn describe(self) -> &'static str {
        match self {
            Self::Published => "emitted invocation.ambiguous",
            Self::AlreadyReported => {
                "invocation.ambiguous was reported on an earlier start and is not emitted again"
            }
            Self::PublishFailed => "invocation.ambiguous could not be published",
        }
    }
}

/// Reconcile worker rows left live by operator terminal transitions made
/// before this binary was deployed. Runs before classification so a
/// pre-existing orphan cannot be reported ambiguous again.
pub(crate) async fn reconcile_terminal_owners(
    worker_store: &WorkerStore,
    cp_store: &ControlPlaneStore,
    now_ms: i64,
) -> anyhow::Result<()> {
    for row in worker_store
        .find_in_flight_invocations()
        .await
        .context("failed to scan worker rows for terminal-owner reconciliation")?
    {
        if let Some(owner) = cp_store
            .get_invocation_owner(&row.invocation_id)
            .await
            .context("failed to read owner during worker reconciliation")?
            && matches!(
                owner.status,
                fq_runtime::control_plane::OwnerStatus::Completed
                    | fq_runtime::control_plane::OwnerStatus::Failed
            )
        {
            worker_store
                .mark_invocation_operator_terminal(
                    &row.invocation_id,
                    owner.status.as_str(),
                    now_ms,
                )
                .await
                .context("failed to reconcile terminal worker row")?;
        }
    }
    Ok(())
}

/// What startup recovery hands the rest of the boot.
pub(crate) struct Recovered {
    /// The invocations to re-drive.
    pub recoverable: Vec<ClassifiedInvocation>,
    /// The ids whose workspaces the startup prune must keep: every
    /// in-flight invocation, resumable *or* ambiguous, because resume
    /// continues from its workspace and triage may need to inspect it.
    pub in_flight_ids: HashSet<String>,
    /// The trigger claims the dispatcher arbitrates on. They exist only as
    /// the result of recording every in-flight invocation's durable start
    /// on its claim, so no dispatcher can arbitrate before that (#959).
    pub claims: TriggerClaims,
}

/// Classify what was in flight when the last process stopped, publish the
/// `system.recovery` roll-up, surface the cases that cannot be recovered as
/// `invocation.ambiguous`, report every in-flight invocation to the control
/// plane as durably started, and restore coordination ownership for the
/// ones that can be recovered.
pub(crate) async fn classify_in_flight(
    worker_store: &Arc<WorkerStore>,
    cp_store: &Arc<ControlPlaneStore>,
    bus: &EventBus,
    runtime_id: Uuid,
    worker_id: &WorkerId,
) -> anyhow::Result<Recovered> {
    // Worker recovery: scan in-flight invocations from the worker store,
    // classify each, restore ownership for auto-resumed cases, and emit
    // `invocation.ambiguous` events for cases that cannot be recovered.
    let classified = fq_runtime::worker::scan_in_flight(worker_store.as_ref())
        .await
        .context("failed to scan in-flight invocations")?;
    let mut counts = fq_runtime::worker::CategoryCounts::default();
    for inv in &classified {
        counts.record(inv.category.clone());
    }
    // Always emit system.recovery so historical recovery
    // counts are queryable through the projection (even when
    // there's nothing to recover — counts would all be zero
    // and that's still informational for `fq events query`).
    let recovery_event = Event::system(
        runtime_id,
        EventPayload::SystemRecovery(fq_runtime::events::SystemRecoveryPayload {
            runtime_id,
            worker_id: worker_id.as_str().to_string(),
            safe_resume: counts.safe_resume,
            safe_replay: counts.safe_replay,
            ambiguous: counts.ambiguous,
            total: counts.total(),
        }),
    );
    if let Err(err) = bus.publish(&recovery_event).await {
        tracing::warn!(error = %err, "failed to publish system.recovery event");
    }
    if counts.total() > 0 {
        println!(
            "  in-flight:        {} ({} safe-resume, {} safe-replay, {} ambiguous)",
            counts.total(),
            counts.safe_resume,
            counts.safe_replay,
            counts.ambiguous,
        );
        for inv in &classified {
            if let Some((entity, call_id)) = inv.ambiguous_context() {
                // Re-validate the agent_id pulled from the store
                // before publishing it. If the stored value somehow
                // fails AgentId validation, skip the recovery event
                // and surface the problem in logs — better than
                // panicking or emitting a malformed event.
                let agent_id = match AgentId::new(inv.state.agent_id.clone()) {
                    Ok(id) => id,
                    Err(err) => {
                        tracing::error!(
                            stored_agent_id = %inv.state.agent_id,
                            error = %err,
                            "stored agent_id fails validation; skipping ambiguous-recovery event"
                        );
                        continue;
                    }
                };
                let event_invocation_id = uuid::Uuid::parse_str(&inv.state.invocation_id)
                    .unwrap_or_else(|_| {
                        // Fall back to a fresh uuid if the
                        // stored id ever isn't valid (shouldn't
                        // happen — every id is a v7 uuid).
                        uuid::Uuid::now_v7()
                    });
                publish_ambiguous_once(
                    worker_store.as_ref(),
                    bus,
                    agent_id,
                    event_invocation_id,
                    &inv.state.invocation_id,
                    fq_runtime::events::InvocationAmbiguousPayload {
                        stuck_entity: entity.to_string(),
                        stuck_call_id: call_id,
                        note:
                            "worker startup categorisation found a `dispatched` row without `completed`"
                                .to_string(),
                    },
                )
                .await;
                // The coordination consumer (spawned below)
                // picks up the `invocation.ambiguous` event, when
                // this start published one rather than an earlier
                // start, and upserts the
                // coordination_invocation_owner row. v1
                // collapsed-process used to write directly
                // here; that's now the consumer's job, which
                // matches v2's split-process expectation
                // (worker emits, control-plane writes).
            }
        }
    }

    // Every in-flight invocation — resumable *or* ambiguous — keeps its
    // workspace: resume continues from it, and `fq invocation resume` may
    // need to inspect it. The startup prune below sweeps workspaces of
    // everything else (terminal or unknown).
    let in_flight_ids: std::collections::HashSet<String> = classified
        .iter()
        .map(|c| c.state.invocation_id.clone())
        .collect();

    // Every in-flight invocation made its first WAL write, so its trigger
    // has durably started, whether or not the process that ran it lived to
    // record that on the trigger's claim. The worker reports its in-flight
    // set and the control plane records it on the claims, so the redelivery
    // of such a still-unacked trigger is dropped rather than started a
    // second time (#959). Ambiguous invocations count: an operator decides
    // their fate, not a redelivery. In this process the report is not a
    // message but a direct write into the control-plane store, and it must
    // land before the dispatcher consumes a trigger: the claims the
    // dispatcher needs are what the recording returns, and an error here
    // stops the boot.
    let in_flight = fq_runtime::worker::InFlightReport::scan(worker_store.as_ref())
        .await
        .context("failed to read the in-flight invocations to report")?;
    let claims = TriggerClaims::recover(cp_store.clone(), &in_flight)
        .await
        .context("failed to record a recovered invocation's durable start on its claim")?;

    // Restore the coordination rows before spawning resumes. A row can be
    // absent after a daemon crash, but the list/dashboard projection keys on
    // it; without this upsert a recovered invocation runs invisibly.
    let mut recoverable = Vec::new();
    for invocation in classified {
        if matches!(
            invocation.category,
            fq_runtime::worker::RecoveryCategory::SafeResume
                | fq_runtime::worker::RecoveryCategory::SafeReplay
        ) {
            cp_store
                .upsert_invocation_ownership(
                    &invocation.state.invocation_id,
                    worker_id.as_str(),
                    invocation.state.started_at,
                    fq_runtime::control_plane::OwnerStatus::InFlight,
                )
                .await
                .context("failed to restore recovered invocation ownership")?;
            recoverable.push(invocation);
        }
    }
    Ok(Recovered {
        recoverable,
        in_flight_ids,
        claims,
    })
}

/// Queue one auto-resume per safe-resume / safe-replay invocation for a
/// worker permit, in the order the scan found them, and spawn the task
/// that runs it once its permit comes. Ambiguous cases were already
/// surfaced; safe cases proceed automatically, and a failure in one does
/// not stop the others.
///
/// Called before the dispatcher exists, so every resume queued here is
/// ahead of every trigger for a permit: the first
/// `max_concurrent_invocations` start at once, the rest wait their turn,
/// and the first trigger waits behind them all (#994).
///
/// The handles come back so a graceful drain (ADR-0027) can wait for them
/// to suspend at a step boundary — or, still queued, to let go of their
/// place — before exiting. On a signal-driven shutdown they stay
/// detached, as they always have. So does the count the boot banner
/// prints of the ones that wait.
// Eight shared runtime handles, each of which a resume task genuinely
// needs; grouping them into a struct would only move the same list one
// line up. `ResumeControl::new` next door carries the same allow for
// the same reason.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_resume_tasks(
    recoverable: Vec<ClassifiedInvocation>,
    registry: &AgentRegistry,
    resume_runner: &Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
    llm: &Arc<dyn LlmClient>,
    bus: &EventBus,
    worker_store: &Arc<WorkerStore>,
    // A resumed invocation can come back `Deferred` (#278) — its model
    // is still rate-limited — and goes back on the queue the dispatcher
    // drains, rather than being logged and forgotten.
    deferrals: &fq_runtime::worker::DeferralQueue,
    // The worker permit each resume queues for (#994), and the agent
    // count it enters (#718). The count is taken at once and never gated:
    // recovery exists to finish what was already started.
    admission: &RunAdmission,
) -> ResumeTasks {
    let resume_count = recoverable.len();
    let mut waiting = 0;
    // Track the resume tasks' handles so a graceful drain (ADR-0027) can
    // wait for them to suspend at a step boundary before exiting. On a
    // signal-driven shutdown they stay detached (abandoned, as before).
    let mut resume_handles: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(resume_count);
    for inv in recoverable {
        let inv_id = match uuid::Uuid::parse_str(&inv.state.invocation_id) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(
                    invocation_id = %inv.state.invocation_id,
                    error = %err,
                    "invalid invocation_id; skipping resume"
                );
                continue;
            }
        };
        let agent_id = match fq_runtime::AgentId::new(&inv.state.agent_id) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(
                    agent_id = %inv.state.agent_id,
                    error = %err,
                    "invalid agent_id; skipping resume"
                );
                continue;
            }
        };
        let loaded = match registry.get_loaded(&agent_id) {
            Some(l) => l,
            None => {
                tracing::warn!(
                    agent_id = %inv.state.agent_id,
                    "agent not in registry; skipping resume — drop the invocation manually"
                );
                continue;
            }
        };
        let agent = loaded.agent.clone();
        let runner = resume_runner.clone();
        let llm_arc = llm.clone();
        let bus = bus.clone();
        let wstore = worker_store.clone();
        let deferrals = deferrals.clone();
        // Taken before the task is spawned, so the count is right the
        // instant `assemble` returns — the dispatcher must not admit a
        // trigger in the window before a resume task is scheduled. It is
        // held while the resume waits for its permit, as a trigger's slot
        // is: a slot is intent to run.
        let agent_slot = admission.agents().enter(agent.id(), agent.max_concurrent());
        // Its place in the permits' queue, taken here and now rather than
        // when the task is first polled: that is what puts it ahead of
        // the dispatcher's first trigger by construction.
        let place = admission.workers().queue(Waiter::Recovered(inv_id)).await;
        waiting += usize::from(place.is_queued());
        resume_handles.push(tokio::spawn(async move {
            let agent_slot = agent_slot;
            // `None`, logged by the admission, is a drain or shutdown
            // before its turn came — the WAL row is untouched, and the
            // next start's recovery finds it in flight again — or an
            // operator's drop of the queued run, which then never starts.
            let Some(permit) = place.permit().await else {
                return;
            };
            let resumed = runner.resume(&agent, llm_arc.as_ref(), inv_id).await;
            // The run is over: its worker goes back now, not after the
            // outcome is handled — the failure arm awaits a store write
            // and a publish — and a drop no longer finds it "starting".
            drop(permit);
            match resumed {
                // The slot goes on the queue with the resume (#718): a
                // recovered invocation that defers again is still one of
                // this agent's in-flight runs, and taking a second slot
                // when it wakes would be a second entry for one run.
                Ok(fq_runtime::InvocationOutcome::Deferred {
                    invocation_id,
                    resume_after,
                }) => deferrals.defer(invocation_id, agent_id, resume_after, agent_slot),
                Ok(outcome) => tracing::info!(
                    invocation_id = %inv_id,
                    ?outcome,
                    "resume completed"
                ),
                // Ended by some other route while it waited for its permit,
                // and the archive hand-off may have deleted the row since.
                // (An `fq invocation drop` of the queued run does not get
                // here: the drop holds it, and it never starts.) Nothing is
                // ambiguous about it, and reporting it so would turn an
                // ended invocation back into one awaiting triage.
                Err(ExecutorError::Resume(
                    ResumeError::AlreadyTerminal { .. } | ResumeError::NoStateRow { .. },
                )) => tracing::info!(
                    invocation_id = %inv_id,
                    "recovered invocation ended before its turn to resume; nothing to resume"
                ),
                Err(err) => {
                    let row = ResumedRow::read(wstore.as_ref(), &inv_id.to_string()).await;
                    report_resume_error(row, wstore.as_ref(), &bus, agent_id, inv_id, err).await;
                }
            }
        }));
    }
    let tasks = ResumeTasks {
        handles: resume_handles,
        waiting,
    };
    tasks.announce(resume_count, admission.workers().max_concurrent());
    tasks
}

/// What the worker row says, once a resume has returned an error, about
/// the invocation it resumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResumedRow {
    /// Still in flight: the resume could not be carried out, and the run
    /// did not reach a terminal event.
    InFlight,
    /// Terminal: the invocation has ended. Usually the run reached its
    /// terminal event, stored with the terminal write or published before
    /// it, and the error came after (its archive refused, say). It can
    /// also be an operator's drop, which marks the row terminal while the
    /// run is going. Either way the error does not undo the ending.
    Terminal,
    /// Gone: the invocation has ended. Only the archive hand-off deletes
    /// a row, once the control plane has acknowledged an archive, and an
    /// archive goes out only for a terminal row. Usually this resume
    /// ended the run; if the resume failed reading the row, whatever
    /// ended it came first.
    Archived,
}

impl ResumedRow {
    /// Read `invocation_id`'s row.
    async fn read(
        worker_store: &WorkerStore,
        invocation_id: &str,
    ) -> Result<Self, fq_runtime::WorkerStoreError> {
        let row = worker_store.get_invocation_state(invocation_id).await?;
        Ok(match row {
            None => Self::Archived,
            Some(row) if row.terminal_at.is_some() => Self::Terminal,
            Some(_) => Self::InFlight,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::InFlight => "in_flight",
            Self::Terminal => "terminal",
            Self::Archived => "archived",
        }
    }
}

/// Report a recovered invocation whose resume returned `err`, given what
/// its worker row said afterwards (`row`, read by [`ResumedRow::read`]).
///
/// The error alone cannot say whether the run ended (issue 1005). A run
/// that ends in an ordinary `failed` returns `InvocationFailed` when the
/// reducer decides it, the provider's error when a model call fails
/// part-way through a step, and a bus error when its `failed` went out
/// but its `invocation.archived` was refused; each has published or
/// stored its terminal event. The worker row says whether the invocation
/// ended: terminal, or deleted by the archive hand-off, means it did, and
/// nothing about it is ambiguous. Reporting it would give it a second
/// ending in the event log, and turn its ownership row to `ambiguous` for
/// an operator to triage a run that needs nothing. Not reported, its
/// ownership row reads `failed` once its archive is acknowledged, and
/// `in_flight` until then: for as long as the archive is refused (issue
/// 1024).
///
/// Only a resume that could not be carried out — the row still in flight,
/// or unreadable — is `invocation.ambiguous`, at most once
/// ([`publish_ambiguous_once`]); the log line says whether this start
/// published it or an earlier one already had.
async fn report_resume_error(
    row: Result<ResumedRow, fq_runtime::WorkerStoreError>,
    worker_store: &WorkerStore,
    bus: &EventBus,
    agent_id: AgentId,
    invocation_id: Uuid,
    err: ExecutorError,
) {
    match row {
        Ok(row @ (ResumedRow::Terminal | ResumedRow::Archived)) => {
            tracing::warn!(
                invocation_id = %invocation_id,
                agent_id = %agent_id,
                error = %err,
                row = row.label(),
                "resume returned an error, but the invocation has ended; not reporting it \
                 ambiguous"
            );
            return;
        }
        Ok(ResumedRow::InFlight) => {}
        // Not knowing is not knowing it ended: report it, as
        // `publish_ambiguous_once` does when it cannot read its stamp.
        Err(read) => tracing::error!(
            invocation_id = %invocation_id,
            error = %read,
            "could not read the worker row of an invocation whose resume failed; \
             treating the resume as not carried out"
        ),
    }
    let report = publish_ambiguous_once(
        worker_store,
        bus,
        agent_id.clone(),
        invocation_id,
        &invocation_id.to_string(),
        fq_runtime::events::InvocationAmbiguousPayload {
            stuck_entity: "recovery".to_string(),
            stuck_call_id: invocation_id.to_string(),
            note: format!("automatic resume failed: {err}"),
        },
    )
    .await;
    tracing::error!(
        invocation_id = %invocation_id,
        agent_id = %agent_id,
        error = %err,
        "resume failed; {}",
        report.describe()
    );
}

/// What [`spawn_resume_tasks`] started.
pub(crate) struct ResumeTasks {
    /// One per resume, for a graceful drain to wait on.
    pub handles: Vec<JoinHandle<()>>,
    /// How many of them were queued behind a full worker cap: the boot
    /// banner's `resumes waiting` count.
    pub waiting: usize,
}

impl ResumeTasks {
    /// The boot banner's lines for them, out of `spawned` recovered runs.
    fn announce(&self, spawned: usize, max_concurrent: usize) {
        if spawned > 0 {
            println!("  resume tasks:     {spawned} spawned");
        }
        if self.waiting > 0 {
            println!(
                "  resumes waiting:  {} for a worker permit (max_concurrent_invocations = \
                 {max_concurrent})",
                self.waiting
            );
        }
    }
}

#[cfg(test)]
mod tests;
