//! SQLite projection of the factor-q event stream.
//!
//! The projection is a materialised view over NATS events, optimised
//! for metadata queries (filter by agent, event type, time range) and
//! cost aggregation. Per the design in
//! `docs/design/committed/storage-and-scaling.md`, NATS is the source of truth:
//!
//! - The projection stores envelope fields plus denormalised columns
//!   for common query filters. It does not store full payloads.
//! - Row sizes are stable regardless of event body size, so growth
//!   is predictable.
//! - The projection can always be rebuilt from NATS: the file carries
//!   a schema version, a bump rebuilds it on open, and `fq projection
//!   rebuild` does the same on demand — tables dropped and recreated,
//!   the durable consumer reset, the stream replayed from
//!   `deliver_all` ([`rebuild`]).
//!
//! [`ProjectionStore`] owns the SQLite connection pool and exposes
//! `insert_event`, `query_events`, and `cost_summary`. The store is
//! safe to share across threads (both the writer task and reader
//! tasks can hold clones of the pool).
//!
//! [`ProjectionConsumer`] wraps a durable JetStream consumer, loops
//! over delivered events, calls `insert_event` for each, and acks.
//! It runs until a shutdown signal fires and returns cleanly. The
//! daemon runs it through the [`rebuild::ProjectionSupervisor`], which
//! can stop and restart it around a rebuild.

pub mod consumer;
mod fields;
pub mod rebuild;
pub mod store;

pub use consumer::{ConsumerError, ProjectionConsumer};
pub use rebuild::{ProjectionRebuildHandle, ProjectionSupervisor, RebuildError};
pub use store::{
    CostSummary, EventLocation, EventRow, FailureSummary, PROJECTION_SCHEMA_VERSION,
    ProjectionStore, StoreError,
};
