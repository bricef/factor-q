//! SQLite projection of the factor-q event stream.
//!
//! The projection is a materialised view over NATS events, optimised
//! for metadata queries (filter by agent, event type, time range) and
//! cost aggregation. Per the design in
//! `docs/design/storage-and-scaling.md`, NATS is the source of truth:
//!
//! - The projection stores envelope fields plus denormalised columns
//!   for common query filters. It does not store full payloads.
//! - Row sizes are stable regardless of event body size, so growth
//!   is predictable.
//! - The projection can always be rebuilt from NATS by dropping the
//!   database and replaying the stream from `deliver_all`.
//!
//! [`ProjectionStore`] owns the SQLite connection pool and exposes
//! `insert_event`, `query_events`, and `cost_summary`. The store is
//! safe to share across threads (both the writer task and reader
//! tasks can hold clones of the pool).
//!
//! [`ProjectionConsumer`] wraps a durable JetStream consumer, loops
//! over delivered events, calls `insert_event` for each, and acks.
//! It runs until a shutdown signal fires and returns cleanly.

pub mod consumer;
pub mod store;

pub use consumer::{ConsumerError, ProjectionConsumer};
pub use store::{CostSummary, EventRow, ProjectionStore, StoreError};
