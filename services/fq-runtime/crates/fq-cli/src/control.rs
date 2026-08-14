//! The client half of the daemon control plane: `fq down` and `fq reload`.
//!
//! Both are declared commands on the authenticated edge now (plan Phase 4,
//! verbs 3 and 4). They used to publish on `fq.control.*` and hope; the
//! subjects are gone, and with them two client-side bus subscriptions that
//! core NATS could drop silently. The daemon half is `control_commands.rs`.
//!
//! **What `fq down` can honestly confirm changed with the transport, and
//! is worth stating.** It used to publish, then wait for the daemon's own
//! `fq.system.shutdown` event, and report the runtime id and clean flag
//! that event carried. There is no subscription left to hear that on — and
//! there could not be one, since the whole point is that the daemon has
//! stopped. What replaces it is stronger as a liveness proof and weaker as
//! a description: the command is answered by the daemon, and then its edge
//! stops answering, which is the process itself going away rather than a
//! message it sent on the way out. Exit 0 still means "confirmed stopped"
//! — `ops/dogfood/deploy.sh` depends on exactly that.

use std::time::{Duration, Instant};

use crate::cli::GlobalArgs;
use crate::edge_call::{edge_client_for, edge_invoke};

/// How often the stop wait re-dials the daemon's edge.
const DOWN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Headroom past the drain deadline for the daemon's own teardown: the
/// stores close, the worker deregisters, the shutdown event publishes.
const DOWN_TEARDOWN_HEADROOM: Duration = Duration::from_secs(10);

/// `fq down` (issue #63): cleanly stop a running daemon and confirm it
/// exited — the operator-facing stop verb, so nobody reaches for
/// `pkill -INT`.
///
/// Three steps, and the ordering is the contract: dial the daemon (an
/// unreachable one is a fast, loud failure rather than a publish into the
/// void), command the stop, then wait — bounded — for its edge to stop
/// answering. A timeout is a loud, actionable error, never a false
/// "stopped".
pub(crate) async fn down_daemon(global: &GlobalArgs, now: bool) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    // The liveness gate, and it fails closed: with no daemon listening
    // there is nothing to dial, so `fq down` reports that instead of
    // waiting out a deadline for a confirmation nobody will send. The
    // old gate watched for a worker heartbeat over ~20s to tell "no
    // daemon" from "a daemon that is stopping"; a refused connection
    // answers the same question at once.
    let client = edge_client_for(global).await.map_err(|err| {
        anyhow::anyhow!(
            "no running `fq run` daemon reachable at {}: {err:#}\n\
             `fq down` is a no-op — is the daemon running? (`fq status`)",
            config.edge.bind
        )
    })?;

    // Progress narration goes to stderr: it is printed before the stop
    // is known to have taken, and stdout must stay clean for the final
    // confirmation / a machine consumer (issue #190).
    if now {
        eprintln!("Requested an immediate stop (--now).");
        eprintln!(
            "The daemon will tear down cleanly, deregister its worker, and exit at once; \
             in-flight invocations are resumed by recovery on the next start."
        );
    } else {
        eprintln!("Requested a graceful stop.");
        eprintln!(
            "The daemon will drain in-flight invocations to a step boundary, \
             deregister its worker, and exit."
        );
    }

    client
        .invoke(
            fq_ops::OpId::Verb(fq_ops::VerbId::Control(fq_ops::Control::Down)),
            serde_json::json!({ "now": now }),
        )
        .await?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // The command is answered; nothing else may travel this connection,
    // which is about to be torn down under us by the daemon exiting.
    drop(client);

    // Bound the confirmation wait by the drain deadline plus headroom in
    // drain mode; `--now` should be near-instant but gets the same
    // generous ceiling so a busy daemon is not misreported as hung.
    let wait = Duration::from_millis(config.drain_deadline_ms) + DOWN_TEARDOWN_HEADROOM;
    eprintln!(
        "Waiting up to {}s for the daemon to stop...",
        wait.as_secs()
    );
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        // A dial that fails is the daemon gone: its edge listener dies
        // with the process. Any error will do — a refused connection, a
        // closed handshake — because none of them is a daemon serving
        // operators.
        if edge_client_for(global).await.is_err() {
            let mode = if now { "now" } else { "drain" };
            println!("✓ Daemon stopped (mode={mode}).");
            return Ok(());
        }
        tokio::time::sleep(DOWN_POLL_INTERVAL).await;
    }
    anyhow::bail!(
        "timed out after {}s: the daemon accepted the stop but its edge is still \
         answering — check `fq status` and `fq workers list` for a lingering worker.",
        wait.as_secs()
    )
}

/// `fq reload`: ask the daemon to re-read its agent definitions and swap
/// the registry the dispatcher reads, without a restart.
///
/// The directory is the daemon's, not this client's — which is why the
/// confirmation no longer names one. A path printed here would be the
/// caller's own configured directory, and whether that is the directory
/// the daemon reads is precisely the skew `fq agent list` was moved to
/// remove (verb 9).
pub(crate) async fn reload_daemon(global: &GlobalArgs) -> anyhow::Result<()> {
    edge_invoke(
        global,
        fq_ops::OpId::Verb(fq_ops::VerbId::Control(fq_ops::Control::Reload)),
        serde_json::json!({}),
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Past tense, and it is earned: the command answers after the swap,
    // so a reload that could not read the directory came back as an
    // error above rather than as this line.
    println!("Reloaded the daemon's agent registry.");
    println!(
        "The swap affects the NEXT trigger; in-flight invocations keep the config \
         they snapshotted at trigger time."
    );
    println!("`fq agent list` shows what the daemon now holds.");
    Ok(())
}
