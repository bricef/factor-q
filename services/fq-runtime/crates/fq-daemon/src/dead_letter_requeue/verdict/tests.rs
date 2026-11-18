use super::*;
use fq_runtime::dead_letter::StartUnchecked;

const RUN: &str = "019a0000-0000-7000-8000-000000000001";
const MARKED_RUN: &str = "019a0000-0000-7000-8000-000000000002";

fn dead(marked: bool) -> DeadLetter {
    DeadLetter {
        event_id: "019a0000-0000-7000-8000-000000000009".to_string(),
        timestamp: chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
        agent_id: "researcher".to_string(),
        trigger_subject: "fq.trigger.researcher".to_string(),
        trigger_stream_seq: Some(12),
        source: "advisory".to_string(),
        trigger_payload: serde_json::json!({"n": 2}),
        error_message: "trigger exhausted after 5 deliveries (limit 5)".to_string(),
        start_unchecked: marked.then(|| StartUnchecked {
            reason: "a copy of the trigger is still handling its claim".to_string(),
            invocation_id: Some(MARKED_RUN.to_string()),
        }),
        unattributed: None,
        unattributed_kind: None,
    }
}

fn started() -> Exhausted {
    Exhausted::Started {
        invocation_id: RUN.to_string(),
        on_record: "on its claim, marked durably started".to_string(),
        mark: ClaimMark::Already,
    }
}

fn cannot_tell_verdict() -> Verdict {
    Verdict::CannotTell {
        why: "why".to_string(),
        invocation_id: Some(MARKED_RUN.to_string()),
        may_clear: false,
    }
}

/// **A start on record is refused, marked or not**, and the verdict
/// carries the run the claim names and what became of the claim.
#[test]
fn a_start_on_record_is_started_whatever_the_mark() {
    for marked in [false, true] {
        assert_eq!(
            verdict_on(&dead(marked), Found::Look(started())),
            Verdict::Started {
                invocation_id: RUN.to_string(),
                on_record: "on its claim, marked durably started".to_string(),
                mark: Some(ClaimMark::Already),
            }
        );
    }
}

/// **Nothing on record proceeds, marked or not**: a claim is there and its
/// run is on neither record.
#[test]
fn nothing_on_record_proceeds_whatever_the_mark() {
    for marked in [false, true] {
        assert_eq!(
            verdict_on(&dead(marked), Found::Look(Exhausted::NothingOnRecord)),
            Verdict::Proceed
        );
    }
}

/// **No claim, or none to consult**: nothing on record for an unmarked
/// dead letter, whose writer found none; cannot tell for a marked one,
/// naming the run its mark names, and not a reason that clears. The
/// reason says the run the mark names was looked up and nothing was found
/// (it is printed only once `with_the_marked_run` has found nothing).
#[test]
fn a_claim_that_is_gone_proceeds_unmarked_and_cannot_be_told_marked() {
    let gone: [fn() -> Found; 3] = [
        || Found::Look(Exhausted::NoClaim),
        || Found::NoClaimToConsult(EARLIER_STREAM),
        || Found::NoClaimToConsult(NO_SEQUENCE),
    ];
    for found in gone {
        assert_eq!(verdict_on(&dead(false), found()), Verdict::Proceed);
        let Verdict::CannotTell {
            why,
            invocation_id,
            may_clear,
        } = verdict_on(&dead(true), found())
        else {
            panic!("a marked dead letter whose claim is gone cannot be told");
        };
        assert!(
            why.starts_with(
                "it is marked `start_unchecked` (a copy of the trigger is still handling its \
                 claim), and "
            ),
            "{why}"
        );
        assert!(
            why.ends_with("; nothing was found on record under the run its mark names"),
            "{why}"
        );
        assert_eq!(invocation_id.as_deref(), Some(MARKED_RUN));
        assert!(!may_clear);
    }
}

/// **A start found under the run the mark names is a start on record.**
/// When the claim leaves the daemon unable to tell, the run the dead
/// letter's mark names is looked up by its own id, and a start found there
/// turns "cannot tell" into a refusal the override does not pass. No claim
/// was read, so none was marked. Nothing found leaves the verdict as it
/// was, and a verdict that is not "cannot tell" is not changed by it.
#[test]
fn a_start_found_under_the_marked_run_is_a_start_on_record() {
    let found = || {
        Some((
            MARKED_RUN.to_string(),
            "on the control plane's record as completed".to_string(),
        ))
    };
    assert_eq!(
        with_the_marked_run(cannot_tell_verdict(), found()),
        Verdict::Started {
            invocation_id: MARKED_RUN.to_string(),
            on_record: "on the control plane's record as completed".to_string(),
            mark: None,
        }
    );
    assert_eq!(
        with_the_marked_run(cannot_tell_verdict(), None),
        cannot_tell_verdict()
    );
    assert_eq!(
        with_the_marked_run(Verdict::Proceed, found()),
        Verdict::Proceed
    );
}

/// **A check that cannot decide is "cannot tell"**, naming the run the
/// claim names, else the one the mark names; the reason can clear.
#[test]
fn an_undecided_check_cannot_be_told() {
    let undecided = |invocation_id: Option<&str>| {
        Found::Look(Exhausted::Undecided {
            why: "a copy of the trigger is still handling its claim".to_string(),
            invocation_id: invocation_id.map(str::to_string),
        })
    };
    assert_eq!(
        verdict_on(&dead(false), undecided(Some(RUN))),
        Verdict::CannotTell {
            why: "a copy of the trigger is still handling its claim".to_string(),
            invocation_id: Some(RUN.to_string()),
            may_clear: true,
        }
    );
    assert_eq!(
        verdict_on(&dead(true), undecided(None)),
        Verdict::CannotTell {
            why: "a copy of the trigger is still handling its claim".to_string(),
            invocation_id: Some(MARKED_RUN.to_string()),
            may_clear: true,
        }
    );
    assert!(matches!(
        verdict_on(&dead(false), undecided(None)),
        Verdict::CannotTell {
            invocation_id: None,
            ..
        }
    ));
}

/// **No records at all is "cannot tell"**, even for an unmarked dead
/// letter: nothing was looked at.
#[test]
fn no_records_cannot_be_told() {
    for marked in [false, true] {
        assert!(matches!(
            verdict_on(&dead(marked), Found::NoRecords),
            Verdict::CannotTell {
                may_clear: false,
                ..
            }
        ));
    }
}

/// The start-on-record refusal names the dead letter, the run and where
/// to read it, says the override does not pass it, and says what became
/// of the claim, or that none was read.
#[test]
fn the_start_refusal_names_the_run_and_refuses_the_override() {
    let WireError::Conflict { op, message } = super::started(
        &dead(false),
        true,
        RUN,
        "on its claim, marked durably started",
        Some(&ClaimMark::Now),
    ) else {
        panic!("a start on record is a Conflict");
    };
    assert_eq!(op, OP);
    assert_eq!(
        message,
        "the dead letter at trigger sequence 12 (event \
         `019a0000-0000-7000-8000-000000000009`) is for a trigger whose run has a start on \
         record: invocation `019a0000-0000-7000-8000-000000000001`, on its claim, marked durably \
         started. Requeueing it would run the work twice, so nothing was reserved or published, \
         and `checked_not_started` (`--checked-not-started`) does not change that: it is for a \
         run the daemon cannot tell about, never one with a start on record. Read the run with \
         `fq invocation show 019a0000-0000-7000-8000-000000000001` (`invocation.get`). To run \
         the work again anyway, publish it as new work with `trigger.publish`. Its claim: marked \
         started now."
    );

    let by_id = super::started(
        &dead(true),
        true,
        MARKED_RUN,
        "on the control plane's record as completed",
        None,
    )
    .to_string();
    assert!(
        by_id.ends_with(
            "No claim answered for the trigger, so this was found under the run's own id, the \
             one the dead letter's mark names, and no claim was marked."
        ),
        "{by_id}"
    );
}

/// The cannot-tell refusal names the dead letter, why, the run to check
/// and what counts as started, and the command that overrides it; with
/// no run named, how to find it; when the request named no sequence, that
/// the dead letter was the agent's newest. What asking again can do is
/// said as it is: for a marked dead letter, less than for an unmarked one.
#[test]
fn the_cannot_tell_refusal_gives_the_check_and_the_override() {
    let WireError::InvalidInput { op, message } = cannot_tell(
        &dead(true),
        false,
        "the trigger's claim could not be read: disk I/O error",
        Some(RUN),
        true,
    ) else {
        panic!("cannot tell is an InvalidInput");
    };
    assert_eq!(op, OP);
    assert_eq!(
        message,
        "the newest dead letter of agent `researcher`, at trigger sequence 12 (event \
         `019a0000-0000-7000-8000-000000000009`), taken because the request gave no \
         `trigger_seq` (`--trigger-seq`), may be for a trigger whose run started, and the daemon \
         cannot tell: the trigger's claim could not be read: disk I/O error. Requeueing it would \
         run the work twice if the run started, so nothing was reserved or published. That can \
         clear, but asked again once it has, the daemon takes the verdict itself only while the \
         claim is there: if the claim has been given back by then, a dead letter marked like \
         this one is still one it cannot tell about, unless the run its mark names is on record. \
         Check the run with `fq invocation show 019a0000-0000-7000-8000-000000000001`: it \
         started if that shows a `Live execution:` block, an `archived:` line, a `status:` other \
         than `(no coordination row)` or a trigger line saying `durably started`. A copy of the \
         trigger can still start the run after you look. Once you have checked that it did not \
         start, ask again with `checked_not_started` set: `fq dead-letters requeue researcher \
         --trigger-seq 12 --checked-not-started`. A start on record is refused whatever you \
         pass."
    );

    let unmarked = cannot_tell(&dead(false), true, "why", Some(RUN), true).to_string();
    assert!(
        unmarked.contains(
            "That can clear: asked again once it has, the daemon takes the verdict itself."
        ),
        "{unmarked}"
    );

    let unnamed = cannot_tell(&dead(false), true, "why", None, false).to_string();
    assert!(
        unnamed.contains(
            "No run is named, since no claim naming one could be read and the dead letter's \
             mark names none: list the agent's runs with `fq events query --agent researcher \
             --type triggered`"
        ),
        "{unnamed}"
    );
    assert!(!unnamed.contains("That can clear"), "{unnamed}");
    assert!(
        unnamed.contains("the dead letter at trigger sequence 12"),
        "{unnamed}"
    );

    let no_sequence = cannot_tell(
        &DeadLetter {
            trigger_stream_seq: None,
            ..dead(true)
        },
        true,
        "why",
        Some(RUN),
        false,
    )
    .to_string();
    assert!(
        no_sequence
            .contains("the override, which names its dead letter by sequence, cannot reach it"),
        "{no_sequence}"
    );
    assert!(
        !no_sequence.contains("--checked-not-started`."),
        "no command is offered that would be refused; got {no_sequence}"
    );
}
