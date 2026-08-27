//! Stopping: which signal arrived, and what a finished task means.
//!
//! Split out of `daemon.rs` because neither of these reads any of the
//! daemon's state — they answer about the process, not about the
//! runtime it is hosting.

/// Wait for an OS shutdown signal and report which one fired.
///
/// The caller maps the two signals to different shutdown paths (ADR-0027):
///
/// - **SIGTERM** — what process managers, `docker stop`, systemd, and
///   orchestrators send to stop a service — triggers a **graceful drain**:
///   in-flight invocations suspend at a step boundary and the daemon exits,
///   bounded by `drain_deadline_ms`. The orchestrator's own SIGKILL grace
///   period must be ≥ that deadline, because nothing shortens the drain
///   once it starts: this function returns on the *first* SIGTERM and
///   stops reading the stream, tokio never unregisters its handler, and
///   nothing here restores `SIG_DFL` — so a second SIGTERM is absorbed
///   silently and SIGKILL is the only way out of a wedged drain (#509).
///   See the deploy plan for per-orchestrator grace settings.
/// - **SIGINT (Ctrl-C)** — interactive stop — is a fast clean shutdown that
///   does not wait out in-flight work (crash-recovery resumes it).
///
/// Either way the daemon exits cleanly (worker deregistered), unlike the
/// abrupt default SIGTERM disposition that orphans the worker + in-flight
/// invocations as recovery cruft.
///
/// Returns a static reason string for the `system.shutdown` event:
/// `"ctrl_c"`, `"sigterm"`, or `"signal_error"` when a listener could
/// not be installed or errored.
pub(crate) async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "failed to install SIGTERM handler; listening for Ctrl-C only"
                );
                return match tokio::signal::ctrl_c().await {
                    Ok(()) => "ctrl_c",
                    Err(err) => {
                        tracing::error!(error = %err, "failed to listen for Ctrl-C");
                        "signal_error"
                    }
                };
            }
        };
        tokio::select! {
            res = tokio::signal::ctrl_c() => match res {
                Ok(()) => "ctrl_c",
                Err(err) => {
                    tracing::error!(error = %err, "failed to listen for Ctrl-C");
                    "signal_error"
                }
            },
            _ = sigterm.recv() => "sigterm",
        }
    }
    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => "ctrl_c",
            Err(err) => {
                tracing::error!(error = %err, "failed to listen for Ctrl-C");
                "signal_error"
            }
        }
    }
}

/// Convert a joined task result into a short error message. A
/// clean early-exit (task returned Ok(())) is reported as a
/// descriptive string so operators see *something* explaining
/// why a task stopped without being asked to.
pub(crate) fn describe_task_result<E: std::fmt::Display>(
    name: &str,
    result: Result<Result<(), E>, tokio::task::JoinError>,
) -> String {
    match result {
        Ok(Ok(())) => format!("{name} exited before a shutdown signal was sent"),
        Ok(Err(err)) => format!("{name} failed: {err}"),
        Err(join_err) => format!("{name} task panicked: {join_err}"),
    }
}
