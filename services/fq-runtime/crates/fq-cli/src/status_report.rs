//! `control.status`, daemon-side (plan Phase 4, verb 14): machinery
//! state in one report — which build is running, what its streams and
//! its live registry look like, how far its projection has folded, and
//! what recovery state it is in.
//!
//! **The accumulation point, so the shape is heterogeneous on
//! purpose.** The domain model settles where the next piece of "how is
//! the machinery itself doing" goes: here, by growing this one schema,
//! rather than by adding a verb for each new thing worth knowing. That
//! is why the fields below do not share a subject — a version string, a
//! JetStream probe, a registry census, one projection count, one
//! recovery fold. There is no resource behind them to make them
//! homogeneous: machinery state is not a fold of atoms, and the
//! synthetic it is scoped to has no Get for this report to duplicate
//! (ADR-0006 Appendix D). The same stretch `control.doctor` makes, and
//! the model admits it for both by name.
//!
//! **Every field here is something only the daemon can answer**, which
//! is the line the client half is built on. The JetStream probe needs
//! the connection this process already holds; the registry is the
//! in-memory handle `fq reload` swaps, not the caller's disk; the row
//! count and the recovery counts are reads of stores the daemon owns.
//! What is *not* here — configuration paths, and whether the files at
//! them exist — never needed a daemon and is answered client-side, so
//! `fq status` keeps saying something when nothing is running
//! ([`crate::status`]).
//!
//! Two sub-reads (`event_count`, `recovery`) stay internal to the
//! composite rather than becoming declared reports, for the reason
//! `control.doctor` states at length: a report's authority is Read on
//! its own scope and never on its inputs, so declaring them would buy
//! this report nothing and spend names against the curation gate.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
use fq_runtime::views::Views;

use crate::version::FQ_VERSION;

/// The typed parameters of `control.status`. Empty, and declared
/// anyway: the report is small enough that every part of it is worth
/// having, and this is where a future narrowing would appear.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct StatusParams {}

/// The daemon's live agent registry, censused: what it would run right
/// now, and what it could not load.
#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq, Default,
)]
pub(crate) struct StatusRegistry {
    /// Definitions the registry holds — the agents this daemon would
    /// run if triggered right now.
    pub(crate) agents: i64,
    /// One entry per definition file the registry rejected, phrased as
    /// the daemon phrased it; each message names the file. A daemon
    /// with load errors is running fewer agents than its directory
    /// describes, which is rarely what the operator intended.
    pub(crate) load_errors: Vec<String>,
}

impl StatusRegistry {
    /// Census one registry snapshot.
    pub(crate) fn of(registry: &fq_runtime::AgentRegistry) -> Self {
        StatusRegistry {
            agents: registry.len() as i64,
            load_errors: registry.errors().iter().map(|e| e.to_string()).collect(),
        }
    }
}

/// `control.status`'s declared output, and what `fq status --json`
/// nests under `daemon`.
#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Debug, Clone, PartialEq, Eq,
)]
pub(crate) struct StatusReport {
    /// The build this daemon is running: semver plus the commit it was
    /// built from, so a deploy check can confirm the live process is on
    /// the expected revision.
    pub(crate) version: String,
    /// JetStream health for the runtime's core streams and their
    /// primary durable consumers — message counts, byte totals and how
    /// far each consumer has got. Probed at the daemon, over the
    /// connection it already holds.
    pub(crate) streams: Vec<fq_runtime::health::StreamHealth>,
    /// The live agent registry, censused.
    pub(crate) registry: StatusRegistry,
    /// Rows in the daemon's projection index — how much of the event
    /// log has been folded into readable state.
    pub(crate) projection_rows: i64,
    /// Ambiguous invocations awaiting triage and workers past the
    /// stale threshold, with their ids.
    pub(crate) recovery: fq_runtime::views::RecoveryView,
}

/// Register `control.status` on the daemon's edge.
pub(crate) fn register_status_report(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
    bus: fq_runtime::EventBus,
    agents: fq_runtime::SharedRegistry,
) -> anyhow::Result<()> {
    let decl = fq_ops::Report::new::<StatusParams, StatusReport>(
        fq_ops::ControlReport::Status,
        "Machinery state: the daemon's build, its stream health, its live registry, \
         its projection position and its recovery counts.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "What is running and what it is running on. Answers `which build is this`, \
         `are the streams and their consumers keeping up`, `which agent definitions did \
         this daemon load and which did it reject`, `how far has the projection folded` \
         and `is anything waiting on operator recovery`. It does not judge: stale workers \
         and ambiguous invocations are reported as counts, and deciding whether they \
         amount to a problem is `control.doctor`'s job. \
         The registry census is the daemon's in-memory handle — what it would run right \
         now, not what is on any caller's disk — so it reflects a reload without a \
         restart. Stream figures are a probe at the instant of the call, not a fold, so \
         two calls a second apart legitimately differ. \
         This report cannot describe a daemon that is not running, and a caller that \
         cannot reach one has learned the single most important thing about the \
         machinery rather than failed to learn anything: absence is the finding.",
    );
    registry
        .report::<StatusParams, StatusReport, _, _>(decl, move |_params: StatusParams| {
            let views = views.clone();
            let bus = bus.clone();
            let agents = agents.clone();
            async move {
                let internal = |e: fq_runtime::views::ViewsError| WireError::Internal {
                    message: e.to_string(),
                };
                let streams = fq_runtime::health::probe_core_streams(&bus.jetstream()).await;
                let projection_rows = views.event_count().await.map_err(internal)?;
                let now_ms = chrono::Utc::now().timestamp_millis();
                let recovery = views
                    .recovery(now_ms, DEFAULT_STALE_THRESHOLD_MS)
                    .await
                    .map_err(internal)?;
                // Clone the inner Arc out of the lock so the wire work
                // never holds it — the dispatcher's discipline, and the
                // Agent view's.
                let snapshot = agents.read().await.clone();
                Ok(StatusReport {
                    version: FQ_VERSION.to_string(),
                    streams,
                    registry: StatusRegistry::of(&snapshot),
                    projection_rows,
                    recovery,
                })
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests;
