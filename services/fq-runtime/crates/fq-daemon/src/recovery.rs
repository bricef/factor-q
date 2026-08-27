//! The daemon's startup recovery: reconciling worker rows against operator
//! terminal decisions, classifying what was in flight when the last process
//! stopped, restoring coordination ownership, and re-driving the safe cases.
//!
//! Split out of `run_daemon` (#189) as three straight lifts, called at exactly
//! the points they used to occupy — the order is load-bearing. Reconciliation
//! runs before classification so a pre-existing orphan cannot be reported
//! ambiguous again, and the resume tasks are spawned only after the runner
//! that drives them exists.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use fq_runtime::agent::{AgentId, AgentRegistry};
use fq_runtime::events::{Event, EventPayload};
use fq_runtime::llm::LlmClient;
use fq_runtime::worker::{ClassifiedInvocation, WorkerId};
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
async fn publish_ambiguous_once(
    worker_store: &fq_runtime::WorkerStore,
    bus: &EventBus,
    agent_id: AgentId,
    invocation_id: Uuid,
    stamp_key: &str,
    payload: fq_runtime::events::InvocationAmbiguousPayload,
) {
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
            return;
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

/// Classify what was in flight when the last process stopped, publish the
/// `system.recovery` roll-up, surface the cases that cannot be recovered as
/// `invocation.ambiguous`, and restore coordination ownership for the ones
/// that can.
///
/// Returns the invocations to re-drive, and the ids whose workspaces the
/// startup prune must keep — every in-flight invocation, resumable *or*
/// ambiguous, because resume continues from its workspace and triage may
/// need to inspect it.
pub(crate) async fn classify_in_flight(
    worker_store: &Arc<WorkerStore>,
    cp_store: &ControlPlaneStore,
    bus: &EventBus,
    runtime_id: Uuid,
    worker_id: &WorkerId,
) -> anyhow::Result<(Vec<ClassifiedInvocation>, HashSet<String>)> {
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
                // picks up the `invocation.ambiguous` event we
                // just published and upserts the
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
    Ok((recoverable, in_flight_ids))
}

/// Spawn one detached auto-resume task per safe-resume / safe-replay
/// invocation. Ambiguous cases were already surfaced; safe cases proceed
/// automatically, and a failure in one does not stop the others.
///
/// The handles come back so a graceful drain (ADR-0027) can wait for them
/// to suspend at a step boundary before exiting. On a signal-driven
/// shutdown they stay detached, as they always have.
pub(crate) fn spawn_resume_tasks(
    recoverable: Vec<ClassifiedInvocation>,
    registry: &AgentRegistry,
    resume_runner: &Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>>,
    llm: &Arc<dyn LlmClient>,
    bus: &EventBus,
    worker_store: &Arc<WorkerStore>,
) -> Vec<JoinHandle<()>> {
    let resume_count = recoverable.len();
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
        resume_handles.push(tokio::spawn(async move {
            match runner.resume(&agent, llm_arc.as_ref(), inv_id).await {
                Ok(outcome) => tracing::info!(
                    invocation_id = %inv_id,
                    ?outcome,
                    "resume completed"
                ),
                Err(err) => {
                    let note = format!("automatic resume failed: {err}");
                    tracing::error!(invocation_id = %inv_id, agent_id = %agent_id, error = %err, "resume failed; emitting invocation.ambiguous");
                    publish_ambiguous_once(
                        wstore.as_ref(),
                        &bus,
                        agent_id,
                        inv_id,
                        &inv_id.to_string(),
                        fq_runtime::events::InvocationAmbiguousPayload {
                            stuck_entity: "recovery".to_string(),
                            stuck_call_id: inv_id.to_string(),
                            note,
                        },
                    )
                    .await;
                }
            }
        }));
    }
    if resume_count > 0 {
        println!("  resume tasks:     {resume_count} spawned");
    }
    resume_handles
}

#[cfg(test)]
mod tests;
