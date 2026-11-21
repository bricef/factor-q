//! `invocation.drop`'s handler: moved out of `operator_registry`, which
//! stands at the function cap, and taking the runner as a
//! [`DriveHalts`], so a test can drive the whole handler with a runner
//! that loses the arm.

use std::sync::Arc;

use fq_edge::wire::WireError;

use super::drop_archive;
use super::drop_halt::{DriveHalts, DropCommandInput, arm_drop_halt};

/// What the drop's handler writes to and asks, besides the runner.
pub(super) struct DropDeps {
    pub(super) bus: fq_runtime::EventBus,
    pub(super) projection: Arc<fq_runtime::control_plane::projection::ProjectionStore>,
    pub(super) control_plane: Arc<fq_runtime::control_plane::store::ControlPlaneStore>,
    /// The worker permits a queued run waits on, which the drop holds.
    pub(super) workers: Arc<fq_runtime::control_plane::dispatcher::WorkerPermits>,
    pub(super) worker_store: Arc<fq_runtime::WorkerStore>,
}

/// Drop `input`'s invocation for the caller `call` names: hold it if it is
/// queued, arm a halt on it if this daemon drives it and `live` is set,
/// say what archive to expect, publish the drop, and return the receipt.
pub(super) async fn drop_command(
    deps: &DropDeps,
    runner: &(impl DriveHalts + Sync),
    input: DropCommandInput,
    call: &fq_edge::CallContext,
) -> Result<fq_ops::Receipt, WireError> {
    // Who asked: the principal of the token the call came in
    // on, as the edge hands it; none for a call made in-process,
    // which records nothing (issue 1070).
    let requested_by = call.principal().map(str::to_string);
    // A run queued for a worker permit or deferred for a rate
    // limit is not driven: held from here to the receipt, it
    // cannot start; marked dropped, never; and remembered, for
    // one in the instant of being put down, which nothing holds.
    let queued = deps.workers.hold_for_drop(&input.invocation_id).await;
    // Liveness first: a refused drop must leave no trace, so
    // nothing is published until the halt is armed (or the
    // invocation is known idle). The arm hands back who the
    // halted invocation belongs to, and the write below
    // resolves from that rather than from a projection this
    // daemon may be ahead of — an armed halt and a NotFound
    // can never be the same answer (#107).
    let armed = arm_drop_halt(runner, queued.as_ref(), &input)?;
    let driving_agent = armed.as_ref().map(|armed| armed.agent.clone());
    let archive = drop_archive::after_the_drop(
        &deps.worker_store,
        &deps.control_plane,
        armed
            .as_ref()
            .and_then(|armed| armed.halt.as_ref().map(|halt| halt.row)),
        &input.invocation_id,
    )
    .await;
    let record = fq_runtime::control_plane::operator::DropRecord {
        reason: input.reason.clone(),
        requested_by,
        archive: Some(archive),
    };
    // The edge implementing an op IS the runtime boundary, not a
    // call point to flip.
    // allow-runtime-internals: daemon-side command handler.
    let result = fq_runtime::control_plane::operator::drop_invocation(
        &deps.bus,
        &deps.projection,
        &deps.control_plane,
        &input.invocation_id,
        &record,
        driving_agent.as_ref(),
    )
    .await
    .map_err(|e| match e {
        // allow-runtime-internals: daemon-side error mapping, same handler.
        fq_runtime::control_plane::operator::DropError::UnknownInvocation(id) => {
            WireError::NotFound {
                op: "invocation.drop".into(),
                message: format!("no invocation `{id}`"),
            }
        }
        other => WireError::InvalidInput {
            op: "invocation.drop".into(),
            message: other.to_string(),
        },
    })?;
    // Published: a run halted during its first write marks its
    // row once the write lands (issue 1070).
    if let Some(armed) = armed {
        armed.published();
    }
    if let Some(queued) = queued {
        queued.dropped();
    }
    deps.workers.note_dropped(&input.invocation_id);
    Ok(fq_ops::Receipt {
        // The drop event, named by the identity `event.get`
        // takes — the same `event_id` its listing carries,
        // so the receipt walks to the whole event.
        atoms: vec![fq_ops::AtomRef {
            domain: fq_ops::Domain::Event,
            key: serde_json::json!({ "event_id": result.event_id }),
        }],
        // The position is still here, where being a log
        // coordinate is the point: it is what a gated read
        // passes as `min_seq` for read-your-writes.
        watermarks: [(fq_ops::Domain::Event, result.event_seq)]
            .into_iter()
            .collect(),
    })
}

#[cfg(test)]
mod tests;
