//! The edge's tarpc contract: two methods for the whole surface. The
//! uniform envelopes are the single choke point for auth, audit,
//! versioning, and cost middleware; generated typed client wrappers
//! restore end-to-end static typing on top.

use crate::wire::{InvokeRequest, InvokeResponse, NextBatchRequest, StreamBatch, WireError};

#[tarpc::service]
pub trait Edge {
    /// Invoke one operation (Get/List, a command, or a report).
    async fn invoke(request: InvokeRequest) -> Result<InvokeResponse, WireError>;

    /// Long-poll the next batch of a stream operation from a sequence
    /// cursor.
    async fn next_batch(request: NextBatchRequest) -> Result<StreamBatch, WireError>;
}
