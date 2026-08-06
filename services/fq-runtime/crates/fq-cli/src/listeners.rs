//! The daemon's control-plane listeners: the tasks that answer
//! `fq.control.reload`, `fq.control.down` and `fq.control.resume`.
//!
//! Split out of `run_daemon` (#189). All three share one shape and one
//! supervision posture: a best-effort core-NATS subscription that resubscribes
//! on loss, because a control channel is a convenience and losing it must
//! never tear the runtime down — which is why none of their handles is watched
//! as a daemon-fatal arm.
//!
//! Each spawner is called where its `tokio::spawn` used to stand, so the order
//! the listeners come up in is unchanged. That order has been load-bearing
//! before: a subject nobody owns yet answers "no responders", and a client
//! that reads that as "nothing is running" bypasses the very guard the
//! listener exists to enforce.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fq_runtime::worker::{DrainReason, DrainRequest};
use fq_runtime::{AgentRegistry, EventBus, SharedRegistry};
use futures::StreamExt;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::resume::{
    InvocationResumeRequest, InvocationResumeResponse, ResumeControl, handle_resume_request,
};

/// Re-read the agents directory and atomically swap the shared
/// registry the dispatcher reads. Invoked by the daemon's
/// control-reload listener on each `fq.control.reload` message.
///
/// Failure policy: a reload never leaves the daemon worse off. A
/// missing directory or a load error is logged and the *current*
/// registry is kept — a bad edit can't knock out a running daemon.
/// Per-file parse errors are logged but the successfully-parsed
/// agents are still installed (matching `AgentRegistry`'s
/// partial-success semantics). The swap only affects the NEXT
/// trigger; in-flight invocations keep the config they snapshotted
/// at trigger time (ADR-0020 refresh-between-invocations).
async fn reload_agents(shared: &SharedRegistry, agents_dir: &Path, default_model: Option<&str>) {
    // allow-runtime-internals: this IS the reload — the daemon re-reading its own registry.
    match AgentRegistry::load_from_directory(agents_dir, default_model) {
        Ok(registry) => {
            let count = registry.len();
            let error_count = registry.errors().len();
            for err in registry.errors() {
                tracing::warn!(error = %err, "agent load error during reload");
            }
            *shared.write().await = Arc::new(registry);
            tracing::info!(
                agents = count,
                errors = error_count,
                "reloaded agent definitions from disk"
            );
        }
        Err(err) => {
            tracing::error!(
                error = %err,
                dir = %agents_dir.display(),
                "agent reload failed; keeping the current registry"
            );
        }
    }
}

/// Spawn the control-reload listener. On each `fq.control.reload` message
/// it re-reads the agents directory and atomically swaps the shared
/// registry handle. Load failures (missing dir, all agents invalid) are
/// logged and the current registry is kept, so a bad edit can never leave
/// the daemon with no agents.
pub(crate) fn spawn_reload_listener(
    bus: EventBus,
    registry: SharedRegistry,
    agents_dir: PathBuf,
    default_model: Option<String>,
) -> (JoinHandle<()>, oneshot::Sender<()>) {
    let (reload_shutdown_tx, mut reload_shutdown_rx) = oneshot::channel::<()>();
    let reload_bus = bus;
    let reload_registry = registry;
    let reload_dir = agents_dir;
    let reload_default_model = default_model;
    let handle = tokio::spawn(async move {
        'resubscribe: loop {
            // allow-runtime-internals: the daemon's own control-plane listener.
            let mut sub = match reload_bus.subscribe_control_reload().await {
                Ok(sub) => sub,
                Err(err) => {
                    // Can't establish the subscription. Log and wait a
                    // beat before retrying rather than spinning or
                    // exiting — hot-reload is best-effort, its absence
                    // never justifies killing the daemon.
                    tracing::error!(
                        error = %err,
                        "failed to subscribe to control reload; retrying in 5s"
                    );
                    tokio::select! {
                        biased;
                        _ = &mut reload_shutdown_rx => {
                            tracing::info!("control-reload listener received shutdown signal");
                            break 'resubscribe;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    }
                }
            };
            loop {
                tokio::select! {
                    biased;
                    _ = &mut reload_shutdown_rx => {
                        tracing::info!("control-reload listener received shutdown signal");
                        break 'resubscribe;
                    }
                    msg = sub.next() => {
                        match msg {
                            Some(_) => {
                            reload_agents(
                                &reload_registry,
                                &reload_dir,
                                reload_default_model.as_deref(),
                            )
                            .await
                        }
                            None => {
                                // Subscription dropped. This is not a
                                // daemon-fatal condition — resubscribe
                                // and carry on so hot-reload recovers on
                                // its own.
                                tracing::warn!(
                                    "control-reload subscription ended; resubscribing"
                                );
                                continue 'resubscribe;
                            }
                        }
                    }
                }
            }
        }
    });
    (handle, reload_shutdown_tx)
}

/// Spawn the control-down listener (`fq down`, issue #63). On a
/// `fq.control.down` message it reads the body to pick the stop mode:
/// drain (suspend in-flight work to a step boundary, then exit) or `now`
/// (clean teardown + deregister + immediate exit). It requests the drain up
/// front in drain mode, then reports the chosen mode on the returned
/// receiver so the daemon's select can tear down and publish
/// `fq.system.shutdown` either way.
pub(crate) fn spawn_down_listener(
    bus: EventBus,
    worker: Arc<dyn fq_runtime::Worker>,
) -> (JoinHandle<()>, oneshot::Sender<()>, oneshot::Receiver<bool>) {
    let (down_requested_tx, down_requested_rx) = oneshot::channel::<bool>();
    let (down_listener_shutdown_tx, mut down_listener_shutdown_rx) = oneshot::channel::<()>();
    let down_bus = bus;
    let down_worker = worker;
    let handle = tokio::spawn(async move {
        let mut down_requested_tx = Some(down_requested_tx);
        'resubscribe: loop {
            // allow-runtime-internals: the daemon's own control-plane listener.
            let mut sub = match down_bus.subscribe_control_down().await {
                Ok(sub) => sub,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to subscribe to control down; retrying in 5s"
                    );
                    tokio::select! {
                        biased;
                        _ = &mut down_listener_shutdown_rx => break 'resubscribe,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    }
                }
            };
            tokio::select! {
                biased;
                _ = &mut down_listener_shutdown_rx => break 'resubscribe,
                msg = sub.next() => {
                    match msg {
                        Some(msg) => {
                            let now = fq_runtime::bus::down_mode_now_from_body(&msg.payload);
                            if now {
                                tracing::info!(
                                    "down requested (--now); tearing down cleanly, \
                                     deregistering the worker, and exiting without draining"
                                );
                            } else {
                                tracing::info!(
                                    "down requested; draining in-flight invocations to a step \
                                     boundary, then exiting"
                                );
                                down_worker
                                    .request_drain(DrainRequest::new(DrainReason::Operator))
                                    .await;
                            }
                            if let Some(tx) = down_requested_tx.take() {
                                let _ = tx.send(now);
                            }
                            break 'resubscribe;
                        }
                        None => {
                            tracing::warn!("control-down subscription ended; resubscribing");
                            continue 'resubscribe;
                        }
                    }
                }
            }
        }
    });
    (handle, down_listener_shutdown_tx, down_requested_rx)
}

/// Spawn the operator resume listener (`fq invocation resume`, #373). The
/// daemon owns the worker store and is the only process allowed to mutate
/// the WAL, and request/reply lets the CLI distinguish a stopped daemon
/// from a rejected precondition. All decision-making lives in
/// [`handle_resume_request`]; this task is transport only.
pub(crate) fn spawn_resume_listener(
    resume_control: ResumeControl,
) -> (JoinHandle<()>, oneshot::Sender<()>) {
    let (resume_listener_shutdown_tx, mut resume_listener_shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        'resubscribe: loop {
            // allow-runtime-internals: the daemon's own control-plane listener.
            let mut sub = match resume_control.bus.subscribe_control_resume().await {
                Ok(sub) => sub,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to subscribe to control resume; retrying in 5s"
                    );
                    tokio::select! {
                        biased;
                        _ = &mut resume_listener_shutdown_rx => break 'resubscribe,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    }
                }
            };
            loop {
                tokio::select! {
                    biased;
                    _ = &mut resume_listener_shutdown_rx => break 'resubscribe,
                    msg = sub.next() => {
                        let Some(msg) = msg else {
                            tracing::warn!("control-resume subscription ended; resubscribing");
                            continue 'resubscribe;
                        };
                        // Request/reply only: a message with no reply inbox
                        // has no caller waiting, so there is nothing to serve.
                        let Some(reply) = msg.reply.clone() else { continue };
                        let response =
                            match serde_json::from_slice::<InvocationResumeRequest>(&msg.payload) {
                                Ok(req) => handle_resume_request(&resume_control, req).await,
                                Err(err) => InvocationResumeResponse::rejected(format!(
                                    "invalid resume request: {err}"
                                )),
                            };
                        if let Ok(body) = serde_json::to_vec(&response) {
                            let _ = resume_control.bus.reply_control(reply.to_string(), body).await;
                        }
                    }
                }
            }
        }
    });
    (handle, resume_listener_shutdown_tx)
}

#[cfg(test)]
mod tests;
