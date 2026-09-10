//! The replay floor: the first stream sequence whose message declares
//! an envelope version this build reads
//! (<https://github.com/bricef/factor-q/issues/648>).
//!
//! A projection rebuild replays the stream into the recreated tables
//! through the same wire boundary every consumer uses, and that
//! boundary halts on the first message in a version this build does
//! not read (#409). Right for a live consumer; fatal for a replay that
//! starts at the beginning of a stream still holding history from
//! before an envelope bump — it halts on the first such message and
//! never completes. So the replay does not start at the beginning. It
//! starts at the **floor**, the first sequence whose message this
//! build reads. Everything below the floor is history the replay
//! cannot re-derive, and the rebuild keeps those rows as they were.
//!
//! The floor is a fact about the stream, found by binary search over
//! `[first_sequence, last_sequence]` with JetStream's get-message-by-
//! sequence and the wire boundary's own version probe
//! ([`declared_schema_version`]). The search assumes versions are
//! monotone along the stream — an envelope bump is one deploy moment,
//! and every message after it carries the new version — which is what
//! makes the halving sound. A sequence the stream no longer holds (a
//! deleted message, or one aged out from the front while the search
//! runs) or a message that declares no version at all (poison the
//! consumer would ack) is probed forward: the verdict for a position
//! is that of the first classifiable message at or after it.
//!
//! What the floor does not change: a version this build does not read
//! *after* the floor still halts the replay. That is a genuinely mixed
//! stream, and #409's rule stands. A stream whose versions are not
//! monotone — a rollback after a bump — has no single floor, and the
//! search lands on *a* position: every message it probed below it was
//! unreadable, and the first classifiable message at or above it is
//! readable. Whichever position that is, nothing is lost: the rows
//! below it are kept as they were, and an unreadable message above it
//! halts the replay where the consumer would have halted live.

use async_nats::jetstream::stream::{RawMessageErrorKind, Stream};

use super::stream_error;
use crate::control_plane::durable_consumer::DeliverFrom;
use crate::control_plane::projection::ConsumerError;
use crate::events::{SUPPORTED_SCHEMA_VERSIONS, declared_schema_version};

/// Where a replay starts, and the stream it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayFloor {
    /// The first sequence whose message this build reads: the first
    /// sequence when the stream holds nothing this build cannot read,
    /// and `last_sequence + 1` — the next sequence the stream will
    /// assign — when it holds nothing this build can, or nothing at
    /// all. Then there is nothing to replay and the durable waits for
    /// what comes next.
    pub floor: u64,
    /// The stream's first sequence when the floor was found.
    pub first_sequence: u64,
    /// The stream's last sequence when the floor was found.
    pub last_sequence: u64,
}

impl ReplayFloor {
    /// Whether the stream holds anything at or above the floor.
    pub fn nothing_to_replay(&self) -> bool {
        self.floor > self.last_sequence
    }

    /// Whether anything the stream holds lies below the floor — history
    /// the replay leaves as it is.
    pub fn skips_history(&self) -> bool {
        self.floor > self.first_sequence
    }

    /// Where the durable starts.
    pub fn deliver_from(&self) -> DeliverFrom {
        DeliverFrom::from_floor(self.floor)
    }
}

/// What the first classifiable message at or after a position is.
enum Verdict {
    /// The message at this sequence is one this build reads.
    Supported(u64),
    /// The message at this sequence declares a version this build does
    /// not read.
    Unsupported(u64),
    /// Nothing classifiable at or after the position.
    Nothing,
}

/// The verdict for `from`: the first message at or after it, up to
/// `last`, that declares a version. Deleted sequences and versionless
/// bytes are stepped over one at a time — the event stream is never
/// deleted from by hand and ages out only from the front, so a run of
/// them is short.
async fn classify_from(stream: &Stream, from: u64, last: u64) -> Result<Verdict, ConsumerError> {
    let mut seq = from;
    while seq <= last {
        match stream.get_raw_message(seq).await {
            Ok(message) => match declared_schema_version(&message.payload) {
                Some(version) if SUPPORTED_SCHEMA_VERSIONS.contains(&version) => {
                    return Ok(Verdict::Supported(seq));
                }
                Some(_) => return Ok(Verdict::Unsupported(seq)),
                // Not an event in any version: the consumer would ack
                // it, and it says nothing about where the floor is.
                None => {}
            },
            Err(err) if err.kind() == RawMessageErrorKind::NoMessageFound => {}
            Err(err) => return Err(stream_error(err)),
        }
        seq += 1;
    }
    Ok(Verdict::Nothing)
}

/// Find the replay floor of `stream` — see the module docs.
pub async fn replay_floor(stream: &mut Stream) -> Result<ReplayFloor, ConsumerError> {
    let state = &stream.info().await.map_err(stream_error)?.state;
    let (first_sequence, last_sequence) = (state.first_sequence, state.last_sequence);
    if state.messages == 0 {
        return Ok(ReplayFloor {
            floor: last_sequence + 1,
            first_sequence,
            last_sequence,
        });
    }

    // The predicate the search halves on — P(p): the first classifiable
    // message at or after p is one this build reads, or there is none.
    // Monotone along the stream under the version assumption, and true
    // at last + 1 vacuously. The floor is the first classifiable
    // message at or after the smallest p with P(p): every classifiable
    // message below that p is unsupported, and the first one at or
    // after it is supported.
    let mut lo = first_sequence;
    let mut hi = last_sequence + 1;
    // The supported message that answers for `hi`, once the search has
    // lowered `hi` onto one.
    let mut floor_at_hi: Option<u64> = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match classify_from(stream, mid, last_sequence).await? {
            Verdict::Supported(seq) => {
                hi = mid;
                floor_at_hi = Some(seq);
            }
            // Nothing in [mid, seq] can be the floor: seq is
            // unsupported and what precedes it from mid is
            // unclassifiable.
            Verdict::Unsupported(seq) => lo = seq + 1,
            Verdict::Nothing => {
                hi = mid;
                floor_at_hi = None;
            }
        }
    }
    Ok(ReplayFloor {
        floor: floor_at_hi.unwrap_or(last_sequence + 1),
        first_sequence,
        last_sequence,
    })
}

#[cfg(test)]
mod tests;
