//! The machinery verbs, daemon-side: `control.reload` and
//! `control.down` (plan Phase 4, verbs 3 and 4).
//!
//! Both used to be a core-NATS publish the client made and a listener
//! the daemon ran — fire-and-forget in both directions, with no ack and
//! no way for the daemon to refuse. They are declared commands on the
//! authenticated edge now, and the `fq.control.*` subjects they rode
//! are gone: NATS is the internal event log and coordination
//! substrate, not an external control surface (domain-model amendment,
//! 2026-08-05).
//!
//! What that buys, beyond one transport instead of two: a reload that
//! *fails* is now an error the operator sees rather than a log line on
//! the daemon nobody reads, and a down that reaches nobody is a
//! connection error rather than a publish into the void.
//!
//! Neither command appends an atom, so both answer with
//! [`fq_ops::Receipt::empty`]. The reload swaps live machinery — the
//! registry is not a fold of anything, so there is nothing to name and
//! no sequence to gate a read on. The down's own record is the
//! `system.shutdown` event the daemon publishes on its way out, which
//! is written *after* this handler has returned and so can never be in
//! its receipt.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::worker::{DrainReason, DrainRequest};
use fq_runtime::{AgentRegistry, SharedRegistry};

/// How a `control.down` asked this daemon to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DownMode {
    /// Suspend in-flight invocations at their next step boundary,
    /// bounded by `drain_deadline_ms`, then exit.
    Drain,
    /// Skip the drain: clean teardown, worker deregistered, immediate
    /// exit, in-flight invocations left for the next start's recovery.
    Now,
}

/// The daemon's stop switch, as the edge holds it.
///
/// It was a one-shot, on the reasoning that stopping happens once. It
/// does — but *how* it stops can still change while it is happening,
/// and the one-shot made that unsayable: the first `control.down` took
/// the sender and `fq down --now` against a daemon already draining
/// found it gone and did nothing, which is precisely when an operator
/// reaches for it (#509).
///
/// A watch instead, and the only transition it allows is an
/// escalation. A repeated plain `down` changes nothing, as before;
/// `--now` on a daemon already draining raises the mode and the
/// drain's bounded wait, which is watching this same channel, stops
/// waiting. Nothing can lower it: a `down` arriving after a `--now`
/// cannot un-escalate a stop that is already immediate.
#[derive(Clone)]
pub struct DownSignal(Arc<tokio::sync::watch::Sender<Option<DownMode>>>);

/// The reading half, held by the shutdown select and by the drain wait.
pub type DownReceiver = tokio::sync::watch::Receiver<Option<DownMode>>;

impl DownSignal {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(None).0))
    }

    pub fn subscribe(&self) -> DownReceiver {
        self.0.subscribe()
    }

    /// Ask for a stop, or escalate one already under way. Returns
    /// whether this call changed the mode.
    pub fn request(&self, mode: DownMode) -> bool {
        self.0.send_if_modified(|current| match current {
            Some(existing) if *existing >= mode => false,
            _ => {
                *current = Some(mode);
                true
            }
        })
    }
}

/// Wait until someone asks this daemon to stop, and report how.
///
/// A dropped sender is not a stop request: it means nobody is left to
/// make one, so this waits forever rather than reading the absence of
/// an operator as an instruction from one.
pub(crate) async fn wait_for_down(rx: &mut DownReceiver) -> DownMode {
    loop {
        if let Some(mode) = *rx.borrow_and_update() {
            return mode;
        }
        if rx.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

/// Wait until someone asks for the *immediate* stop — the escalation
/// the drain's bounded wait selects on.
pub(crate) async fn wait_for_down_now(rx: &mut DownReceiver) {
    loop {
        if *rx.borrow_and_update() == Some(DownMode::Now) {
            return;
        }
        if rx.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

/// What `control.reload` and `control.down` reach for. Split out of
/// [`crate::OperatorDeps`]'s wider set because these two verbs command
/// the machinery rather than reading it: the registry handle they swap,
/// where the definitions come from, the runner they ask to drain, and
/// the stop switch.
pub struct MachineryDeps {
    /// The hot-swapped registry the dispatcher reads and this reload
    /// replaces — the same handle the Agent view answers from, which is
    /// why `fq agent list` cannot disagree with what a reload installed.
    pub agents: SharedRegistry,
    pub agents_dir: PathBuf,
    pub default_model: Option<String>,
    /// The worker a drain-mode down asks to suspend in-flight work at
    /// its next step boundary before the daemon exits.
    pub worker: Arc<dyn fq_runtime::Worker>,
    pub down: DownSignal,
}

/// The typed input of `control.down` on the wire.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct DownCommandInput {
    /// Skip the drain: clean teardown, worker deregistered, immediate
    /// exit. Defaults false, which is also what a peer that predates
    /// the field sends.
    #[serde(default)]
    now: bool,
}

/// The typed input of `control.reload` on the wire. Empty, and declared
/// anyway: the daemon reloads from the directory *it* was started with,
/// never one the caller names — that choice is the whole point of the
/// verb, and this is where a future option (an `fqd.toml` refresh)
/// would appear.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ReloadCommandInput {}

/// Re-read the agents directory and atomically swap the shared registry
/// the dispatcher reads.
///
/// Failure policy: a reload never leaves the daemon worse off. A missing
/// directory or a load error keeps the *current* registry — a bad edit
/// cannot knock out a running daemon — and is reported back to the
/// caller, which is what the fire-and-forget publish could not do.
/// Per-file parse errors are partial success: the definitions that did
/// parse are installed (matching `AgentRegistry`'s own semantics) and
/// the rejected files are visible in `fq agent list`, which reads this
/// same registry. The swap only affects the NEXT trigger; in-flight
/// invocations keep the config they snapshotted at trigger time
/// (ADR-0020 refresh-between-invocations).
async fn reload_agents(
    shared: &SharedRegistry,
    agents_dir: &Path,
    default_model: Option<&str>,
) -> Result<(), String> {
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
            Ok(())
        }
        Err(err) => {
            tracing::error!(
                error = %err,
                dir = %agents_dir.display(),
                "agent reload failed; keeping the current registry"
            );
            Err(format!(
                "reload failed, keeping the definitions already loaded: {err}"
            ))
        }
    }
}

/// Register `control.reload` and `control.down` on the daemon's edge.
pub(crate) fn register_control_commands(
    registry: &mut fq_edge::EdgeRegistry,
    deps: MachineryDeps,
) -> anyhow::Result<()> {
    let MachineryDeps {
        agents,
        agents_dir,
        default_model,
        worker,
        down,
    } = deps;

    let decl = fq_ops::Command::new::<ReloadCommandInput>(
        fq_ops::Control::Reload,
        fq_ops::Authority {
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Control,
        },
        "Re-read the agent definitions from disk and swap the live registry.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Affects the NEXT trigger only: in-flight invocations keep the config \
         they snapshotted at trigger time. Answers when the swap has happened, \
         so a reload that could not read the directory is an error rather than \
         a silence — and in that case the definitions already loaded are kept. \
         Appends no atom: the registry is live machinery, not a fold.",
    );
    registry
        .command::<ReloadCommandInput, _, _>(decl, move |_input: ReloadCommandInput| {
            let agents = agents.clone();
            let agents_dir = agents_dir.clone();
            let default_model = default_model.clone();
            async move {
                reload_agents(&agents, &agents_dir, default_model.as_deref())
                    .await
                    .map_err(|message| WireError::Internal { message })?;
                Ok(fq_ops::Receipt::empty())
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    let decl = fq_ops::Command::new::<DownCommandInput>(
        fq_ops::Control::Down,
        fq_ops::Authority {
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Control,
        },
        "Stop this daemon, draining in-flight work to a step boundary.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Returns as soon as the stop is under way, not when it has finished — \
         the daemon cannot answer a call once it has exited, so confirmation is \
         the caller watching this edge stop answering. `now` skips the drain: \
         clean teardown, worker deregistered, immediate exit, with in-flight \
         invocations left for the next start's recovery. A repeated plain call \
         on a daemon already stopping changes nothing; `now` on one already \
         draining ESCALATES it — the bounded drain wait stops waiting and the \
         same clean teardown runs at once. Escalation is one-way: a plain call \
         after a `now` cannot put the drain back. Appends no atom — the \
         `system.shutdown` event is written after this has answered, and its \
         reason records the mode that actually ran.",
    );
    registry
        .command::<DownCommandInput, _, _>(decl, move |input: DownCommandInput| {
            let worker = worker.clone();
            let down = down.clone();
            async move {
                let mode = if input.now {
                    DownMode::Now
                } else {
                    DownMode::Drain
                };
                if !input.now {
                    tracing::info!(
                        "down requested; draining in-flight invocations to a step \
                         boundary, then exiting"
                    );
                    // Armed before the select is woken, exactly as the
                    // retired listener did: the dispatcher must already
                    // be draining when the teardown starts, or the
                    // bounded wait has nothing to wait for.
                    worker
                        .request_drain(DrainRequest::new(DrainReason::Operator))
                        .await;
                } else if worker.drain_status() == fq_runtime::worker::DrainState::Draining {
                    tracing::info!(
                        "down requested (--now) while draining; escalating — the drain \
                         stops waiting, the worker is still deregistered and \
                         system.shutdown is still published"
                    );
                } else {
                    tracing::info!(
                        "down requested (--now); tearing down cleanly, \
                         deregistering the worker, and exiting without draining"
                    );
                }
                down.request(mode);
                Ok(fq_ops::Receipt::empty())
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests;
