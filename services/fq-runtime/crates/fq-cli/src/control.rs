//! The client half of the daemon control plane: `fq down` and `fq reload`.
//!
//! Split out of `lib.rs` (#189). Each publishes one control message and then
//! reports what it can honestly observe — `fq down` waits, bounded, for the
//! daemon's own `fq.system.shutdown` event rather than checking a process.
//! The daemon half is `listeners.rs`.

use anyhow::Context;
use fq_runtime::EventBus;
use fq_runtime::events::EventPayload;
use futures::StreamExt;

use crate::cli::GlobalArgs;

/// `fq down` (issue #63): cleanly stop a running daemon and confirm it
/// exited. Subscribes to `fq.system.shutdown` *before* publishing the
/// control-down message so the daemon's exit event can't be missed in
/// the gap, publishes the down request (drain mode, or `--now` to skip
/// the drain), then waits — bounded — for a `SystemShutdown` event and
/// reports the runtime that stopped.
///
/// Confirmation is scoped to what v1 can honestly observe: the daemon's
/// own clean-exit event (published after the worker is deregistered), not
/// an OS-level process check — there is no PID/supervisor registry yet
/// (the `fq up`/supervisor story is explicitly out of scope for this
/// ticket). A timeout is a loud, actionable error rather than a false
/// "stopped".
pub(crate) async fn down_daemon(global: &GlobalArgs, now: bool) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    // Subscribe to the daemon shutdown event BEFORE publishing the down
    // request, so a daemon that stops fast can't exit in the window
    // between publish and subscribe and leave us waiting forever.
    let mut shutdown_stream = bus
        .subscribe(fq_runtime::events::subjects::SYSTEM_SHUTDOWN.to_string())
        .await
        .context("failed to subscribe to system shutdown events")?;

    // Also watch worker heartbeats (`fq.worker.*.heartbeat`): a running
    // daemon publishes one on start and every ~10s, which lets us tell
    // "no daemon is listening" (fast-fail) apart from "a daemon is
    // stopping" (wait out the drain) below.
    let mut heartbeat_stream = bus
        .subscribe("fq.worker.*.heartbeat".to_string())
        .await
        .context("failed to subscribe to worker heartbeats")?;

    bus.publish_control_down(now)
        .await
        .context("failed to publish control down")?;

    // Progress narration goes to stderr: it is printed before we know
    // whether a daemon is even running, and stdout must stay clean for
    // the final confirmation / a machine consumer (issue #190).
    if now {
        eprintln!(
            "Published stop (--now) on {}.",
            fq_runtime::bus::CONTROL_DOWN_SUBJECT
        );
        eprintln!(
            "A running `fq run` daemon will tear down cleanly, deregister its worker, \
             and exit immediately; in-flight invocations are resumed by recovery \
             on the next start."
        );
    } else {
        eprintln!(
            "Published stop on {}.",
            fq_runtime::bus::CONTROL_DOWN_SUBJECT
        );
        eprintln!(
            "A running `fq run` daemon will drain in-flight invocations to a step boundary, \
             deregister its worker, and exit."
        );
    }

    // Bound the confirmation wait by the drain deadline (plus headroom for
    // the daemon's own teardown/publish) in drain mode; `--now` should be
    // near-instant but gets the same generous ceiling so a busy daemon is
    // not misreported as hung.
    let wait = std::time::Duration::from_millis(config.drain_deadline_ms)
        + std::time::Duration::from_secs(10);
    // Liveness gate: a running daemon emits a worker heartbeat on start
    // and every ~10s (`worker::heartbeat::DEFAULT_INTERVAL_MS`). If neither
    // its shutdown nor any heartbeat arrives within ~2 intervals (capped by
    // the full wait), nothing is listening — fast-fail instead of blocking
    // out the whole deadline.
    let liveness_window = std::time::Duration::from_secs(20).min(wait);
    eprintln!(
        "Waiting up to {}s for the daemon to confirm it has stopped...",
        wait.as_secs()
    );

    enum Confirm {
        NoDaemon,
        StreamClosed,
        TimedOut,
    }

    let start = tokio::time::Instant::now();
    let liveness_deadline = start + liveness_window;
    let full_deadline = start + wait;
    let mut seen_daemon = false;

    let result = loop {
        // Hold to the short liveness gate until we see a sign of life;
        // after that, wait out the full drain-deadline ceiling.
        let deadline = if seen_daemon {
            full_deadline
        } else {
            liveness_deadline
        };
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                break if seen_daemon {
                    Confirm::TimedOut
                } else {
                    Confirm::NoDaemon
                };
            }
            msg = shutdown_stream.next() => match msg {
                Some(Ok(event)) => {
                    if let EventPayload::SystemShutdown(p) = &event.payload {
                        println!(
                            "✓ Daemon stopped (runtime {}, reason={}, clean={}).",
                            p.runtime_id, p.reason, p.clean
                        );
                        return Ok(());
                    }
                }
                // A single undeserialisable event must not end the wait —
                // keep listening for the shutdown event.
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "skipping undeserialisable event while waiting");
                }
                None => break Confirm::StreamClosed,
            },
            hb = heartbeat_stream.next() => {
                // Any heartbeat proves a daemon is up and (having received
                // the down request) stopping; wait out the full deadline for
                // its shutdown event. A closed heartbeat stream is benign.
                if hb.is_some() {
                    seen_daemon = true;
                }
            }
        }
    };

    match result {
        Confirm::NoDaemon => anyhow::bail!(
            "no running `fq run` daemon detected — no worker heartbeat on \
             `fq.worker.*.heartbeat` within {}s, so `fq down` is a no-op. \
             Is the daemon running? (`fq status`)",
            liveness_window.as_secs()
        ),
        Confirm::StreamClosed => anyhow::bail!(
            "the shutdown event stream closed before the daemon confirmed it stopped; \
             check `fq status` / `fq workers list` for the daemon's state"
        ),
        Confirm::TimedOut => anyhow::bail!(
            "timed out after {}s: a daemon was heartbeating but did not confirm it \
             stopped — check `fq status` and `fq workers list` for a lingering worker.",
            wait.as_secs()
        ),
    }
}

pub(crate) async fn reload_daemon(global: &GlobalArgs) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
    bus.publish_control_reload()
        .await
        .context("failed to publish control reload")?;
    println!(
        "Published reload signal on {}.",
        fq_runtime::bus::CONTROL_RELOAD_SUBJECT
    );
    println!(
        "A running `fq run` daemon will re-read {} and swap its registry for the next trigger.",
        config.agents.directory.display()
    );
    Ok(())
}
