//! `control.projection_rebuild`, daemon-side: drop the projection's
//! tables, recreate them at this build's schema version, and replay
//! the event stream into them — on demand, under a running daemon.
//!
//! The same rebuild the daemon performs by itself when it opens a file
//! whose schema version is older than its own. Declared as a machinery
//! verb because that is what it is: the projection is derived, the
//! rebuild is a recovery of the machinery rather than a write to any
//! resource, and the authority to run it is the authority to command
//! the daemon (`Write` over `Control`, like `reload` and `down`).
//!
//! The handler does not touch the store. It asks the projection
//! supervisor — the task that owns the consumer — and waits for the
//! answer, because the consumer has to be stopped before the tables go
//! and started again after the durable is reset, and only the task
//! that runs it can do that in order. What comes back is the rebuild
//! as `control.status` will report it from now on; the command's
//! receipt is empty (nothing was appended to any log), so a caller
//! reads that report for the replay's progress.
//!
//! `confirm` is opt-in, never implied: the tables are dropped, and
//! while the sweep-exempt rows are carried across and the rest is
//! re-derived from the stream, the file is empty of everything else
//! until the replay catches up. A caller that did not say so is
//! refused before anything happens.

use fq_edge::wire::WireError;
use fq_runtime::control_plane::projection::rebuild::{ProjectionRebuildHandle, RebuildError};

/// The op's rendered name, quoted in the refusal it makes.
const OP: &str = "control.projection_rebuild";

/// The typed input of `control.projection_rebuild` on the wire.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ProjectionRebuildInput {
    /// Acknowledge that the projection's tables will be dropped and
    /// re-derived from the event stream. Refused when false: the
    /// rebuild is opt-in, never implied.
    #[serde(default)]
    confirm: bool,
    /// Why, for the record `control.status` reports. Optional.
    #[serde(default)]
    reason: Option<String>,
}

/// Register `control.projection_rebuild` on the daemon's edge.
pub(crate) fn register_projection_rebuild(
    registry: &mut fq_edge::EdgeRegistry,
    handle: ProjectionRebuildHandle,
) -> anyhow::Result<()> {
    let decl = fq_ops::Command::new::<ProjectionRebuildInput>(
        fq_ops::Control::ProjectionRebuild,
        fq_ops::Authority {
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Control,
        },
        "Rebuild the projection from the event stream: drop its tables, recreate them, replay.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "Requires `confirm`; refused otherwise. The projection is derived, so this is a \
         recovery of the machinery rather than a write to any resource: the daemon stops \
         its projection consumer, drops the projection tables and recreates them at this \
         build's schema version, resets the durable consumer so the stream replays from the \
         start of its retention, and starts the consumer again. Rows the retention sweep \
         exempts — cost-bearing events, invocation summaries and trigger records — are \
         carried across before the replay begins, so spend older than the stream's window \
         is kept; everything the stream still holds is re-derived whole, which is what \
         backfills a column that was NULL for history. Answers when the consumer is running \
         again, not when the replay has finished: until it catches up, reads answer over a \
         partial fold, and `control.status` reports the replay's progress under \
         `projection_rebuild`. Appends no atom.",
    );
    registry
        .command::<ProjectionRebuildInput, _, _>(decl, move |input: ProjectionRebuildInput| {
            let handle = handle.clone();
            async move {
                if !input.confirm {
                    return Err(WireError::InvalidInput {
                        op: OP.into(),
                        message: "a rebuild drops the projection's tables and re-derives them \
                                  from the event stream; pass `confirm` to proceed"
                            .into(),
                    });
                }
                tracing::info!(
                    reason = input.reason.as_deref().unwrap_or("none given"),
                    "projection rebuild requested by an operator"
                );
                handle
                    .rebuild(input.reason)
                    .await
                    .map_err(|err| match err {
                        RebuildError::NoSupervisor => WireError::Internal {
                            message: err.to_string(),
                        },
                        other => WireError::Internal {
                            message: format!("projection rebuild failed: {other}"),
                        },
                    })?;
                Ok(fq_ops::Receipt::empty())
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}
