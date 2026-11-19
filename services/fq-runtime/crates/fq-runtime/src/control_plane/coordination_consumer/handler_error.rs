//! What the coordination consumer's shared loop does with a handler's
//! error: which errors are permanent, acknowledged and never retried, and
//! which leave the run's ownership row as it is for the handlers to do
//! the rest of the event.

use tracing::error;

use super::CoordinationConsumerError;
use crate::control_plane::durable_consumer::HandlerError;
use crate::control_plane::ownership::OwnershipError;
use crate::control_plane::store::ControlPlaneStoreError;
use crate::events::Event;

/// What the shared loop does with a handler's error. Two are permanent,
/// acked and never retried, since a retry would hold every later event
/// behind this one on a strict-order consumer for good:
///
/// - a row this build cannot read, which cannot be moved by
///   compare-and-set and is left as it is; the loop logs it at warn;
/// - a write the store refuses by its own rules, a check, a key, a NOT
///   NULL column or a trigger ([`ControlPlaneStoreError::Refused`]),
///   which no retry changes. It is logged here, at error, since a write
///   this consumer meant to make is left unmade (issue 1070), and the
///   loop logs it at warn too. A refused move of the ownership row leaves
///   the row as it is, and the handlers then do the rest of the event, as
///   for a row this build cannot read ([`leaves_the_row_as_it_is`]). No
///   write the consumer makes is meant to be refused, so this is a guard,
///   not a path the consumer takes.
///
/// Every other error, a contended row's included, is transient and
/// redelivered.
pub(super) fn as_handler_error(event: &Event, err: CoordinationConsumerError) -> HandlerError {
    if is_unreadable(&err) {
        return HandlerError::permanent(err);
    }
    if is_refused(&err) {
        error!(
            invocation_id = %event.envelope.invocation_id,
            agent_id = event.envelope.agent_id.as_str(),
            event_type = event.payload.event_type(),
            error = %err,
            "the control-plane store refused the event's write for good; the event is \
             acknowledged, and that write is not made"
        );
        return HandlerError::permanent(err);
    }
    HandlerError::transient(err)
}

/// Whether `err` is a write the control-plane store refused by its own
/// rules, which no retry changes.
fn is_refused(err: &CoordinationConsumerError) -> bool {
    matches!(
        err,
        CoordinationConsumerError::Store(ControlPlaneStoreError::Refused(_))
            | CoordinationConsumerError::Ownership(OwnershipError::Store(
                ControlPlaneStoreError::Refused(_)
            ))
    )
}

/// Whether `err` left the run's ownership row as it is, for a reason a
/// retry does not change: the row cannot be read, or the store refused its
/// move by its own rules. Either way the handlers do the rest of the event
/// (an archive recorded and acknowledged, a drop's stop of the run), as
/// issue 1024 settled for a row this build cannot read, and the event is
/// then refused as permanent.
pub(super) fn leaves_the_row_as_it_is(err: &CoordinationConsumerError) -> bool {
    is_unreadable(err)
        || matches!(
            err,
            CoordinationConsumerError::Ownership(OwnershipError::Store(
                ControlPlaneStoreError::Refused(_)
            ))
        )
}

/// Whether `err` is an ownership row this build cannot read, which every
/// handler meets the same way (`control_plane::ownership`, "A row this
/// build cannot read").
pub(super) fn is_unreadable(err: &CoordinationConsumerError) -> bool {
    matches!(
        err,
        CoordinationConsumerError::Ownership(OwnershipError::Unreadable { .. })
    )
}
