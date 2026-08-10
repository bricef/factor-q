//! The daemon's remaining control-plane listener: the task that answers
//! `fq.control.resume`.
//!
//! Split out of `run_daemon` (#189). It is a best-effort core-NATS
//! subscription that resubscribes on loss, because a control channel is a
//! convenience and losing it must never tear the runtime down — which is why
//! its handle is not watched as a daemon-fatal arm.
//!
//! Two siblings stood here until cohort 4.3: the `fq.control.reload` and
//! `fq.control.down` listeners. Both are gone, along with their subjects —
//! the machinery verbs are declared commands on the edge now
//! (`control_commands.rs`), so the daemon answers them on the transport it
//! already serves and an operator's request either lands or errors, rather
//! than being published into a channel that may own no subscriber. Resume
//! follows them in cohort 4.3's item 13, which is a separate flip.

use futures::StreamExt;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::resume::{
    InvocationResumeRequest, InvocationResumeResponse, ResumeControl, handle_resume_request,
};

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
