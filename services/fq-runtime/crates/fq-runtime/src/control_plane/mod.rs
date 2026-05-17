//! Control-plane role.
//!
//! Per `docs/design/data-architecture.md` §3, the control-plane
//! is the global view of the runtime: trigger ingestion and
//! routing, projection over the audit log, schedules, pending
//! waits, coordination state, and the operator-facing surface.
//!
//! In v1 the control-plane and worker share a single `fq run`
//! process; this module enforces the role boundary at compile
//! time so v2 (separate deployment) is a process split rather
//! than a redesign.
//!
//! Today's control-plane components:
//!
//! - [`dispatcher`] — the trigger dispatcher, which consumes
//!   `fq.trigger.>` from NATS and hands invocations to a
//!   [`crate::worker::Worker`].
//! - [`projection`] — the SQLite projection over the audit log
//!   and the consumer that materialises events into it.
//! - [`coordination_consumer`] — subscribes to
//!   `fq.agent.*.invocation.*` and maintains the
//!   coordination_invocation_owner / coordination_worker tables.
//!   Handles `invocation.ambiguous` (step 7) and
//!   `invocation.archived` (step 8); for the latter, writes the
//!   `invocation_archive` row and publishes
//!   `fq.worker.{worker_id}.invocation.archive_acked` back to
//!   the originating worker.
//! - [`heartbeat_consumer`] — subscribes to
//!   `fq.worker.*.heartbeat` and updates
//!   `coordination_worker.last_heartbeat`.
//!
//! Future control-plane work (schedules, pending waits,
//! retention sweep) lands here as the `data-architecture-v1`
//! plan steps progress.

pub mod coordination_consumer;
pub mod dispatcher;
pub mod heartbeat_consumer;
pub mod projection;
pub mod store;

pub use coordination_consumer::{CoordinationConsumer, CoordinationConsumerError};
pub use heartbeat_consumer::{HeartbeatConsumer, HeartbeatConsumerError};
pub use store::{
    CONTROL_PLANE_SCHEMA_VERSION, ControlPlaneStore, ControlPlaneStoreError, InvocationArchiveRow,
    OwnerRow, OwnerStatus, PendingWaitRow, ScheduleEntryRow, WorkerRow, WorkerStatus,
};
