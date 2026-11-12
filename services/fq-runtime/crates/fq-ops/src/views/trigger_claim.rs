//! A trigger delivery and the dispatcher's claim on it, as the operator
//! surface carries it: on the invocation view, as the delivery that
//! started a run, and in `control.doctor`, as a claim that never settled.

use serde::{Deserialize, Serialize};

/// How far the dispatch of one trigger delivery has got, as its claim
/// records it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriggerClaimState {
    /// Taken by a daemon process, and no durable start recorded: the
    /// trigger may be waiting to start, starting, or abandoned.
    Claimed,
    /// The invocation made its first durable write and the claim records
    /// it. A redelivery of the same message meets this state and starts
    /// nothing.
    DurablyStarted,
}

/// One trigger delivery and the dispatcher's claim on it: which message
/// on the trigger stream started (or is to start) an invocation.
///
/// A message is named by its stream, the incarnation of that stream, and
/// its sequence in it. Every redelivery of a message carries the same
/// three, which is why the claim is keyed on them rather than on the
/// trigger's own id: a trigger published without an id is given a new one
/// on each delivery.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct TriggerDeliveryView {
    /// The stream the trigger was delivered on.
    pub stream: String,
    /// Which incarnation of the stream: the time it was created, in
    /// nanoseconds since the Unix epoch, as a decimal string. A recreated
    /// stream numbers its messages from 1 again, so a sequence names a
    /// message only together with this. A string because it is an
    /// identity compared for equality and its values are past the range a
    /// JSON number read as a double holds exactly.
    pub stream_epoch: String,
    /// The message's sequence in that incarnation of the stream.
    pub stream_seq: u64,
    pub state: TriggerClaimState,
    /// The worker id of the daemon process that holds the claim, or held
    /// it when the invocation started. A daemon takes a new id every time
    /// it starts, so any id other than the running daemon's names a
    /// process that has exited.
    pub claimant: String,
    /// When the claim was taken, epoch ms. A claim one process adopted
    /// from another that had exited carries the time of the adoption.
    pub claimed_at_ms: i64,
}
