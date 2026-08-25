//! Which event types are **transient** — the one place that is
//! written down.
//!
//! A transient event is operational signal rather than history: it is
//! true for the next few seconds, superseded by the next one, and the
//! fact it carries already reaches an operator through the resource
//! that consumes it. `worker_heartbeat` is the first — a worker's 10s
//! liveness ping, ~13k rows a day, whose meaning is `worker.list`'s
//! `last_heartbeat_ms`.
//!
//! **Transients are published and consumed exactly as before.** The
//! heartbeat producer still publishes, the heartbeat consumer still
//! folds, and the control plane's liveness still works. What this
//! module decides is narrower and only about the *operator surface*:
//! a transient is not part of the external interface. Debugging one
//! means tapping the transport directly — that is the debug surface,
//! and it is a different thing.
//!
//! **Why one place.** The knowledge used to be spread across a
//! hardcoded early return in the projection's `insert_event` and a
//! `DELETE` in its migrations, with nothing connecting either to what
//! the surface claimed about itself. That is how `event.list` (from
//! the projection, which never indexed heartbeats) and `event.stream`
//! (from the log, which served them) came to answer different
//! populations for the same filter while the Event atom's declared
//! description said "one row per event". Three derivations of the list
//! below — the projection's skip, the stream's exclusion, and the
//! atom's declared text — make the two verbs agree by construction
//! rather than by two comments happening to say the same thing.

/// The transient event types, comma-separated — **the one place the
/// set is written**. Adding a type is one edit, here.
///
/// A macro expanding to a string literal rather than a `&[&str]`
/// const, because one of the derivations is a compile-time one: an
/// operator-surface declaration's `description` is a `&'static str`,
/// and `concat!` splices literals. A const would have left the surface
/// text restating the set by hand, which is the drift this module
/// exists to remove. Everything that wants the set as values reads
/// [`types`].
#[macro_export]
macro_rules! transient_event_types {
    () => {
        "worker_heartbeat"
    };
}

/// The transient event types, spelled as
/// [`EventPayload::event_type`](crate::events::EventPayload::event_type)
/// spells them.
pub fn types() -> impl Iterator<Item = &'static str> {
    crate::transient_event_types!().split(", ")
}

/// Whether an event type is transient. Takes the type name rather
/// than a payload so a store holding rows of extracted fields — where
/// the type is a column and the payload is long gone — can ask the
/// same question as a caller holding a whole event.
pub fn includes(event_type: &str) -> bool {
    types().any(|t| t == event_type)
}

#[cfg(test)]
mod tests {
    use crate::events::{Event, EventPayload, WorkerHeartbeatPayload};
    use crate::worker::WorkerId;

    /// Every name in the list must name a payload variant that really
    /// exists. The set is written as text so the surface can splice
    /// it, and text does not fail to compile — so a typo would
    /// silently mean "nothing is transient", with the projection
    /// indexing heartbeats again and no test complaining anywhere
    /// near here.
    #[test]
    fn every_transient_name_is_a_real_event_type() {
        let known: Vec<&'static str> = vec![
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: WorkerId::new("w").unwrap(),
            })
            .event_type(),
        ];
        for name in super::types() {
            assert!(
                known.contains(&name),
                "`{name}` is listed as transient but no payload variant answers to it; \
                 add its constructor to `known` when you add the type"
            );
        }
    }

    /// The predicate an event answers, and the one a row of extracted
    /// fields answers, must be the same predicate.
    #[test]
    fn a_heartbeat_is_transient_and_a_trigger_is_not() {
        let heartbeat = Event::system(
            uuid::Uuid::now_v7(),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: WorkerId::new("w").unwrap(),
            }),
        );
        assert!(heartbeat.payload.is_transient());
        assert!(super::includes(heartbeat.payload.event_type()));
        assert!(!super::includes("triggered"));
        assert!(!super::includes(""));
    }
}
