//! Operator-issued actions against control-plane state.
//!
//! These are the verbs the operator CLI (`fq invocation
//! drop` and friends) calls into. Lifted here so the test
//! harness can drive the same code path the CLI uses, with
//! no duplication.

use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{BusError, EventBus};
use crate::control_plane::projection::ProjectionStore;
use crate::control_plane::projection::store::StoreError;
use crate::control_plane::store::{ControlPlaneStore, ControlPlaneStoreError};
use crate::events::{Event, EventPayload, InvocationOperatorRecoveredPayload};

/// Outcome of a successful [`drop_invocation`].
#[derive(Debug, Clone)]
pub struct DropResult {
    pub invocation_id: String,
    pub agent_id: String,
    pub event_id: String,
    pub reason: Option<String>,
}

/// Failure modes for [`drop_invocation`].
#[derive(Debug, thiserror::Error)]
pub enum DropError {
    #[error("invocation {0} not found: no projection event and no coordination owner row")]
    UnknownInvocation(String),
    #[error("invalid agent id from projection: {0}")]
    InvalidAgentId(String),
    #[error("invalid invocation id `{id}`: {source}")]
    InvalidInvocationId {
        id: String,
        #[source]
        source: uuid::Error,
    },
    #[error("projection store error: {0}")]
    Store(#[from] StoreError),
    #[error("control-plane store error: {0}")]
    ControlPlane(#[from] ControlPlaneStoreError),
    #[error("event bus error: {0}")]
    Bus(#[from] BusError),
}

/// Operator-issued drop. Looks up the agent for the given
/// invocation from the projection, builds an
/// `invocation.operator_recovered` event with
/// `action="drop"` and `final_phase="failed"`, and publishes
/// it. The control-plane's coordination consumer is
/// responsible for writing the archive row and flipping the
/// owner status.
pub async fn drop_invocation(
    bus: &EventBus,
    proj_store: &ProjectionStore,
    control_store: &ControlPlaneStore,
    invocation_id: &str,
    reason: Option<&str>,
) -> Result<DropResult, DropError> {
    // Older/synthetic recovery rows may have no projection event and therefore
    // no agent. Clear those rows directly; normal rows retain the existing
    // event-driven terminal/archive transition.
    let agent_id_str = match proj_store.agent_id_for_invocation(invocation_id).await? {
        Some(agent_id) => agent_id,
        None => {
            // No projection event names an agent — this is either an
            // agent-less recovery row or an id that never existed.
            // `delete_invocation_owner` tells them apart by whether it
            // actually removed a row: a truly-unknown id must still error
            // rather than emit a phantom operator-recovered event
            // (ADR-0026 — the event log is the system of record).
            if !control_store.delete_invocation_owner(invocation_id).await? {
                return Err(DropError::UnknownInvocation(invocation_id.to_string()));
            }
            "operator".to_string()
        }
    };
    let agent_id =
        AgentId::new(agent_id_str.clone()).map_err(|e| DropError::InvalidAgentId(e.to_string()))?;
    let inv_uuid = Uuid::parse_str(invocation_id).map_err(|e| DropError::InvalidInvocationId {
        id: invocation_id.to_string(),
        source: e,
    })?;

    let event = Event::new(
        agent_id,
        inv_uuid,
        EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
            action: "drop".to_string(),
            final_phase: "failed".to_string(),
            reason: reason.map(|s| s.to_string()),
        }),
    );
    let event_id = event.envelope.event_id.to_string();
    bus.publish(&event).await?;

    Ok(DropResult {
        invocation_id: invocation_id.to_string(),
        agent_id: agent_id_str,
        event_id,
        reason: reason.map(|s| s.to_string()),
    })
}
