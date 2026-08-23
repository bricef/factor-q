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
//!
//! The report's declared shapes live in [`fq_runtime::surface`]: the
//! dashboard renders this report too, and a declared shape with two
//! clients is a shared definition rather than a private one.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
use fq_runtime::surface::{StatusParams, StatusRegistry, StatusReport};
use fq_runtime::views::Views;

use crate::version::FQ_VERSION;

/// Register `control.status` on the daemon's edge.
pub(crate) fn register_status_report(
    registry: &mut fq_edge::EdgeRegistry,
    views: Arc<Views>,
    bus: fq_runtime::EventBus,
    agents: fq_runtime::SharedRegistry,
    // Where this daemon's stores are. Taken from the config it was
    // started with, so the answer describes the process reporting it.
    db_paths: std::sync::Arc<fq_runtime::RuntimeDbPaths>,
    legacy_events_db: std::sync::Arc<std::path::PathBuf>,
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
            let db_paths = db_paths.clone();
            let legacy_events_db = legacy_events_db.clone();
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
                    stores: fq_ops::surface::StatusStores {
                        worker_path: db_paths.worker.display().to_string(),
                        control_plane_path: db_paths.control_plane.display().to_string(),
                        projection_path: db_paths.projection.display().to_string(),
                        // Checked per call: a migration removes it, and
                        // a report that cached its absence would keep
                        // claiming a file the daemon has since dealt with.
                        legacy_events_db: legacy_events_db
                            .exists()
                            .then(|| legacy_events_db.display().to_string()),
                        initialised: db_paths.all_exist(),
                    },
                    streams,
                    registry: StatusRegistry::from(snapshot.as_ref()),
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
