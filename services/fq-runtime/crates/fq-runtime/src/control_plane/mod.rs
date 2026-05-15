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
//!
//! Future control-plane work (schedules, coordination tables,
//! pending waits, completed-invocation archive) lands here as
//! the `data-architecture-v1` plan steps progress.

pub mod coordination_consumer;
pub mod dispatcher;
pub mod projection;
pub mod store;

pub use coordination_consumer::{CoordinationConsumer, CoordinationConsumerError};
pub use store::{
    CONTROL_PLANE_SCHEMA_VERSION, ControlPlaneStore, ControlPlaneStoreError, InvocationArchiveRow,
    OwnerRow, OwnerStatus, PendingWaitRow, ScheduleEntryRow, WorkerRow, WorkerStatus,
};
