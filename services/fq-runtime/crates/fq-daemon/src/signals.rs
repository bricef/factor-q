//! Stopping: which signal arrived, and what a finished task means.
//!
//! Split out of `daemon.rs` because neither of these reads any of the
//! daemon's state — they answer about the process, not about the
//! runtime it is hosting.

/// The OS signals that stop this daemon, held open for the whole run.
///
/// It used to be one function that awaited a signal and returned. That
/// shape is what made the second SIGTERM a dead letter (#509): the
/// stream was dropped the moment the first signal arrived, tokio's
/// handler stayed installed so the default disposition never came
/// back, and every later signal fell into a registration nobody was
/// reading. An operator watching a drain that would not finish had
/// SIGKILL and nothing else — which skips the deregistration and the
/// `system.shutdown` event, the two things the drain exists to
/// guarantee.
///
/// Holding the streams instead makes a second signal *mean* something.
/// The caller reads one to decide how to stop, and reads the same
/// streams again during the bounded drain wait, where another signal
/// escalates to the immediate-but-clean stop `fq down --now` already
/// implements.
///
/// The two signals differ in what they ask for (ADR-0027):
///
/// - **SIGTERM** — what process managers, `docker stop`, systemd and
///   orchestrators send to stop a service — triggers a **graceful
///   drain**: in-flight invocations suspend at a step boundary and the
///   daemon exits, bounded by `drain_deadline_ms`. A **second SIGTERM**
///   during that wait stops waiting and tears down immediately —
///   still cleanly: the worker is deregistered and `system.shutdown`
///   is published. Under compose, `docker kill -s TERM` is how an
///   operator sends it.
/// - **SIGINT (Ctrl-C)** — interactive stop — is a fast clean shutdown
///   that does not wait out in-flight work (crash-recovery resumes
///   it), and escalates a running drain the same way.
///
/// Either way the daemon exits cleanly, unlike the abrupt default
/// SIGTERM disposition that orphans the worker and its in-flight
/// invocations as recovery cruft. Nothing here ever restores
/// `SIG_DFL`: that would skip both guarantees, which is the whole
/// point of catching the signal in the first place. The orchestrator's
/// SIGKILL grace period should still be at least the drain deadline —
/// SIGKILL remains the only thing that can cut a stop short of a clean
/// one, and now it is no longer the only thing that can cut a *drain*
/// short.
pub(crate) struct ShutdownSignals {
    #[cfg(unix)]
    sigint: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sigterm: Option<tokio::signal::unix::Signal>,
    /// Set once a listener could not be installed or errored. There is
    /// nothing left to hear, so later reads wait forever rather than
    /// reporting the same failure in a hot loop.
    exhausted: bool,
}

impl ShutdownSignals {
    /// Install the handlers once, at startup.
    ///
    /// A handler that will not install is reported here and the stream
    /// is simply absent; [`Self::next`] falls back to whatever is left,
    /// and reports `"signal_error"` when nothing is.
    pub(crate) fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let install = |kind: SignalKind, name: &str| match signal(kind) {
                Ok(stream) => Some(stream),
                Err(err) => {
                    tracing::error!(error = %err, signal = name, "failed to install signal handler");
                    None
                }
            };
            Self {
                sigint: install(SignalKind::interrupt(), "SIGINT"),
                sigterm: install(SignalKind::terminate(), "SIGTERM"),
                exhausted: false,
            }
        }
        #[cfg(not(unix))]
        {
            Self { exhausted: false }
        }
    }

    /// Wait for the next shutdown signal and report which one fired.
    ///
    /// Returns a static reason string for the `system.shutdown` event:
    /// `"ctrl_c"`, `"sigterm"`, or `"signal_error"` when no listener
    /// could be installed. Callable repeatedly — that is the point.
    pub(crate) async fn next(&mut self) -> &'static str {
        if self.exhausted {
            return std::future::pending().await;
        }
        #[cfg(unix)]
        {
            match (self.sigint.as_mut(), self.sigterm.as_mut()) {
                (Some(sigint), Some(sigterm)) => tokio::select! {
                    _ = sigint.recv() => "ctrl_c",
                    _ = sigterm.recv() => "sigterm",
                },
                (Some(sigint), None) => {
                    sigint.recv().await;
                    "ctrl_c"
                }
                (None, Some(sigterm)) => {
                    sigterm.recv().await;
                    "sigterm"
                }
                (None, None) => {
                    self.exhausted = true;
                    "signal_error"
                }
            }
        }
        #[cfg(not(unix))]
        {
            match tokio::signal::ctrl_c().await {
                Ok(()) => "ctrl_c",
                Err(err) => {
                    tracing::error!(error = %err, "failed to listen for Ctrl-C");
                    self.exhausted = true;
                    "signal_error"
                }
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
