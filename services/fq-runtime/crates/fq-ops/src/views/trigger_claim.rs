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
    /// The trigger was dealt with and no invocation started: it was
    /// refused, failed for good before its first durable write, was
    /// requeued from a hold, or ran out of deliveries and was
    /// dead-lettered, as `consumed_why` says. A redelivery of the same
    /// message meets this state and is acknowledged and dropped.
    Consumed,
    /// A state this build cannot interpret. No daemon sends it: it is what
    /// a reader holds so that one claim it cannot interpret does not fail
    /// the answer that carries it, and everything else in that answer with
    /// it.
    #[serde(other)]
    #[schemars(skip)]
    Unknown,
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
    /// How the trigger was consumed, when `state` is `consumed`: one of a
    /// small set of stable spellings, such as `failed_for_good` or
    /// `dead_lettered`. A reader that meets one it does not know shows it
    /// as it is. Absent in any other state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_why: Option<String>,
    /// When the claim recorded that an operator's drop of the invocation it
    /// names wrote that invocation's coordination status, epoch ms, when
    /// `state` is `consumed`. The claim says no run started, so that status,
    /// and the archive record the drop writes with it, record the drop,
    /// not a start. `consumed_why` still says how the trigger was dealt
    /// with. Absent when no drop was recorded, and in any other state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_dropped_at_ms: Option<i64>,
    /// The worker id of the daemon process that holds the claim, or held
    /// it when the invocation started. A daemon takes a new id every time
    /// it starts, so any id other than the running daemon's names a
    /// process that has exited.
    pub claimant: String,
    /// When the claim was taken, epoch ms. A claim one process adopted
    /// from another that had exited carries the time of the adoption.
    pub claimed_at_ms: i64,
}
