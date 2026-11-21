//! What one drop found, read from its own time (issue 1070), and the
//! words and values every surface writes it in.

use super::*;
use crate::views::TerminationReading;

const ENDED: i64 = 1_000;
const FIRST_DROP: i64 = 2_000;
const LATER_DROP: i64 = 9_000;

/// A row as the fold leaves it: `status`, the archive, who ended it and
/// when, and the first request on record, if any.
fn row(
    status: &str,
    archive: InvocationArchiveState,
    ended: Option<(InvocationEnder, i64)>,
    requested_at: Option<i64>,
) -> InvocationSummaryView {
    InvocationSummaryView {
        invocation_id: "inv".to_string(),
        agent_id: Some("agent".to_string()),
        worker_id: "worker".to_string(),
        status: status.to_string(),
        assigned_at_ms: 0,
        started_at_ms: 0,
        archived: archive == InvocationArchiveState::Archived,
        archive_state: archive,
        ended_by: ended.map(|(ender, _)| ender),
        ended_at_ms: ended.map(|(_, at)| at),
        termination: requested_at.map(|at| TerminationRequestView {
            requested_at_ms: at,
            requested_by: Some("on-call".to_string()),
            reason: Some("first".to_string()),
        }),
        summary: None,
    }
}

/// **A drop is reported by its own time, not by the first request's.**
/// The record keeps the first request, so its reading is the first one's;
/// what this drop found is read from this drop's own time against the
/// run's ending, and the request on record is named when it is not this
/// one.
#[test]
fn a_drop_is_read_from_its_own_time() {
    use InvocationArchiveState::{Archived, Pending, WillNotCome};
    use InvocationEnder::{Drop, Run};
    let cases = [
        (
            "the first drop of a running run",
            row(
                "failed",
                Pending,
                Some((Drop, FIRST_DROP)),
                Some(FIRST_DROP),
            ),
            FIRST_DROP,
            Some(DropCase::Dropped),
            None,
        ),
        (
            "a second drop of a dropped run",
            row(
                "failed",
                WillNotCome,
                Some((Drop, FIRST_DROP)),
                Some(FIRST_DROP),
            ),
            LATER_DROP,
            Some(DropCase::EndedByAnEarlierDrop),
            Some(FIRST_DROP),
        ),
        (
            "a drop of a run that completed long before",
            row("completed", Archived, Some((Run, ENDED)), Some(LATER_DROP)),
            LATER_DROP,
            Some(DropCase::AlreadyEnded),
            None,
        ),
        (
            "a second drop of a completed run with a request on record",
            row("completed", Archived, Some((Run, 3_000)), Some(FIRST_DROP)),
            LATER_DROP,
            Some(DropCase::AlreadyEnded),
            Some(FIRST_DROP),
        ),
        (
            "a drop requested while the run ran, whose own ending came first",
            row("completed", Pending, Some((Run, 3_000)), Some(FIRST_DROP)),
            FIRST_DROP,
            Some(DropCase::ReachedItsOwnEndingFirst),
            None,
        ),
        (
            "a drop at the instant the run ended",
            row("failed", Pending, Some((Run, FIRST_DROP)), Some(FIRST_DROP)),
            FIRST_DROP,
            Some(DropCase::AlreadyEnded),
            None,
        ),
        (
            "a drop of a row from before the record, which names no ender",
            row("completed", Archived, None, Some(LATER_DROP)),
            LATER_DROP,
            None,
            None,
        ),
    ];
    for (what, view, at, case, earlier) in cases {
        assert_eq!(view.drop_case(at), case, "{what}");
        assert_eq!(
            view.earlier_request(at)
                .map(|request| request.requested_at_ms),
            earlier,
            "{what}: the earlier request named"
        );
    }
    // The record's own reading is the first request's, whatever a later
    // drop found: a second drop of a completed run whose first request
    // came while it ran reads, on the record, as that first request did.
    let asked_while_running = row("completed", Archived, Some((Run, 3_000)), Some(FIRST_DROP));
    assert_eq!(
        asked_while_running.termination_reading(),
        Some(TerminationReading::ReachedItsOwnEndingFirst)
    );
    assert_eq!(
        asked_while_running.drop_case(LATER_DROP),
        Some(DropCase::AlreadyEnded)
    );
}

/// **A drop's ending is provisional until the run's archive lands**, and
/// so is any ending whose archive will not come; the run's own ending
/// with its archive pending or written is not.
#[test]
fn an_ending_is_provisional_while_the_run_can_still_replace_it() {
    use InvocationArchiveState::{Archived, Held, Pending, WillNotCome};
    use InvocationEnder::{Drop, Run};
    for (view, provisional) in [
        (row("failed", Pending, Some((Drop, 1)), Some(1)), true),
        (row("failed", Held, Some((Drop, 1)), Some(1)), true),
        (row("failed", WillNotCome, Some((Drop, 1)), Some(1)), true),
        (row("failed", Archived, Some((Drop, 1)), Some(1)), false),
        (row("completed", Pending, Some((Run, 1)), Some(2)), false),
        (row("completed", Archived, Some((Run, 1)), None), false),
        (row("failed", WillNotCome, Some((Run, 1)), None), true),
    ] {
        assert_eq!(
            view.ending_is_provisional(),
            provisional,
            "{} {:?} {:?}",
            view.status,
            view.archive_state,
            view.ended_by
        );
    }
}

/// **Every value a surface writes is pinned**: the JSON a script reads,
/// and the phrase a person reads, for what a drop found and for the
/// record's reading.
#[test]
fn every_case_and_reading_is_written_as_published() {
    for (case, json, phrase) in [
        (DropCase::Dropped, "dropped", "dropped"),
        (
            DropCase::EndedByAnEarlierDrop,
            "ended_by_an_earlier_drop",
            "already ended by an earlier drop",
        ),
        (
            DropCase::AlreadyEnded,
            "already_ended",
            "already ended when the termination was requested",
        ),
        (
            DropCase::ReachedItsOwnEndingFirst,
            "reached_its_own_ending_first",
            "requested while running; it reached its own ending first",
        ),
    ] {
        assert_eq!(serde_json::to_value(case).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<DropCase>(json.into()).unwrap(),
            case
        );
        assert_eq!(case.phrase(), phrase);
    }
    for (reading, json, phrase) in [
        (TerminationReading::Dropped, "dropped", "dropped"),
        (
            TerminationReading::AlreadyEnded,
            "already_ended",
            "already ended when the termination was requested",
        ),
        (
            TerminationReading::ReachedItsOwnEndingFirst,
            "reached_its_own_ending_first",
            "requested while running; it reached its own ending first",
        ),
    ] {
        assert_eq!(serde_json::to_value(reading).unwrap(), json);
        assert_eq!(reading.phrase(), phrase);
    }
    for (ender, json) in [
        (InvocationEnder::Run, "run"),
        (InvocationEnder::Drop, "drop"),
    ] {
        assert_eq!(serde_json::to_value(ender).unwrap(), json);
        assert_eq!(ender.as_str(), json);
    }
}
