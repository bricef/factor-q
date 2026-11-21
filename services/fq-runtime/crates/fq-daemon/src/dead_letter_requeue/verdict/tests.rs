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
                claim: OfTheClaim::Marked(ClaimMark::Already),
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
            claim: OfTheClaim::NoAnswer,
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
        &OfTheClaim::Marked(ClaimMark::Now),
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
        &OfTheClaim::NoAnswer,
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
         started if that shows a `Live execution:` block, an `archived:` line or a trigger line \
         saying `durably started`; a `status:` other than `(no coordination row)` is a start only \
         under a trigger line that does not say `dropped by an operator`, since a run an operator \
         dropped before its first WAL write has one, ended by the drop, without having started. \
         A copy of the trigger can still start the run after you look. Once you have checked that it did not \
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
             --event-type triggered`"
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

/// The check's answer on a claim consumed as `why`, naming [`RUN`], with
/// nothing of that run on record and no operator's drop recorded.
fn consumed(why: ConsumedWhy) -> Found {
    Found::Look(Exhausted::Consumed(ConsumedClaim::Checked {
        invocation_id: Some(RUN.to_string()),
        why,
        operator_dropped: false,
    }))
}

/// As [`consumed`], for a claim that records an operator's drop of [`RUN`],
/// whose ownership row the check then did not read as a start.
fn dropped(why: ConsumedWhy) -> Found {
    Found::Look(Exhausted::Consumed(ConsumedClaim::Checked {
        invocation_id: Some(RUN.to_string()),
        why,
        operator_dropped: true,
    }))
}

/// Every reason but the two with an answer of their own: dead-lettered and
/// requeued from a hold.
fn consumed_without_a_dead_letter() -> [ConsumedWhy; 9] {
    [
        ConsumedWhy::RefusedSubject,
        ConsumedWhy::InvalidAgentId,
        ConsumedWhy::UnknownAgent,
        ConsumedWhy::InvalidPayload,
        ConsumedWhy::AgentRemoved,
        ConsumedWhy::FailedForGood,
        ConsumedWhy::HaltedInSetup,
        ConsumedWhy::ReturnedUnstarted,
        ConsumedWhy::Other("a_reason_from_a_newer_build".to_string()),
    ]
}

/// **Consumed as dead-lettered, the dead letter is requeued (issue 1055)**,
/// marked or not, with an operator's drop of the run recorded or not, and
/// with no word from the operator: nothing ran, and it is that trigger's own
/// dead letter, the one the dispatcher writes as it consumes the claim. A
/// drop of the run afterwards changes neither: the claim keeps its reason.
#[test]
fn a_trigger_consumed_as_dead_lettered_is_requeued() {
    for marked in [false, true] {
        for found in [consumed, dropped] {
            let dead = dead(marked);
            assert_eq!(
                verdict_on(&dead, found(ConsumedWhy::DeadLettered)),
                Verdict::Proceed
            );
            let answer = answered(
                &dead,
                true,
                false,
                verdict_on(&dead, found(ConsumedWhy::DeadLettered)),
            );
            assert!(answer.is_ok(), "{answer:?}");
        }
    }
}

/// **A trigger requeued from a hold is never requeued again (issue 1055)**,
/// marked or not, whatever the request says. Its replacement was published
/// under the same trigger id, so requeueing this dead letter would run the
/// work twice: a `Conflict`, as for a start on record, that says so and
/// how the work can still be run, as new work.
#[test]
fn a_trigger_requeued_from_a_hold_is_refused_whatever_the_flag() {
    for (marked, checked_not_started) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        for found in [consumed, dropped] {
            let dead = dead(marked);
            let answer = answered(
                &dead,
                true,
                checked_not_started,
                verdict_on(&dead, found(ConsumedWhy::RequeuedFromHold)),
            );
            let Err(WireError::Conflict { op, message }) = answer else {
                panic!("refused outright, as a Conflict: {answer:?}");
            };
            assert_eq!(op, OP);
            assert_eq!(
                message,
                "the dead letter at trigger sequence 12 (event \
             `019a0000-0000-7000-8000-000000000009`) is for a trigger requeued from a hold: its \
             claim says it was consumed without a run as `requeued_from_hold`, so its replacement \
             was already published under the same trigger id. Requeueing this dead letter would \
             run the work twice, so nothing was reserved or published, and `checked_not_started` \
             (`--checked-not-started`) does not change that: it is for a run the daemon cannot \
             tell about, and this trigger's work was handed to its replacement. To run the work \
             again anyway, publish it as new work with `trigger.publish`."
            );
        }
    }
}

/// **A trigger consumed for any other reason is refused, and the override
/// takes it (issue 1055).** The daemon writes no dead letter for those
/// answers, so this dead letter and the claim disagree, and one of them may
/// be about another trigger. The refusal says what the daemon found (no
/// start on record for the run the claim names), why that does not settle
/// it, and what to check before the override: that this dead letter's own
/// work did not run. It never says the run may have started: the daemon has
/// just read that it did not. For a halt in setup, what it read is the WAL
/// alone, and the refusal says so.
#[test]
fn a_trigger_consumed_for_another_reason_is_refused_and_the_override_takes_it() {
    for why in consumed_without_a_dead_letter() {
        for marked in [false, true] {
            let dead = dead(marked);
            let answer = answered(&dead, true, false, verdict_on(&dead, consumed(why.clone())));
            let Err(WireError::InvalidInput { op, message }) = answer else {
                panic!("{why}: refused, and the override is the edit: {answer:?}");
            };
            assert_eq!(op, OP);
            for expected in [
                format!("consumed without a run as `{why}`"),
                format!("no start on record for the run the claim names, `{RUN}`"),
                "this dead letter and the claim disagree, and one of them may be about another \
                 trigger"
                    .to_string(),
                "check that this dead letter's own work did not run".to_string(),
                "--trigger-seq 12 --checked-not-started".to_string(),
            ] {
                assert!(
                    message.contains(&expected),
                    "{why}: `{expected}` in {message}"
                );
            }
            assert!(
                !message.contains("may be for a trigger whose run started")
                    && !message.contains("may have started"),
                "{why}: the daemon read that the run did not start: {message}"
            );
            let answer = answered(&dead, true, true, verdict_on(&dead, consumed(why.clone())));
            assert!(answer.is_ok(), "{why}: on the operator's word: {answer:?}");
        }
    }

    let answer = answered(
        &dead(false),
        true,
        false,
        verdict_on(&dead(false), consumed(ConsumedWhy::FailedForGood)),
    );
    let Err(WireError::InvalidInput { message, .. }) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(
        message,
        "the dead letter at trigger sequence 12 (event `019a0000-0000-7000-8000-000000000009`) \
         is not requeued on the records alone: its trigger's claim says it was consumed without \
         a run as `failed_for_good`, and the daemon found no start on record for the run the \
         claim names, `019a0000-0000-7000-8000-000000000001`, in either the worker's WAL or the \
         ownership table. But the daemon writes no dead letter for a trigger it consumes as \
         `failed_for_good`, so this dead letter and the claim disagree, and one of them may be \
         about another trigger: what the claim says of its run settles nothing about this dead \
         letter's work. Nothing was reserved or published. Before you override, check that this \
         dead letter's own work did not run: list the agent's runs with `fq events query \
         --agent researcher --event-type triggered`, find any whose trigger payload is this dead \
         letter's with `fq events get <event-id>`, and check each with `fq invocation show \
         <id>`: it started if that shows a `Live execution:` block, an `archived:` line or a \
         trigger line saying `durably started`; a `status:` other than `(no coordination row)` \
         is a start only under a trigger line that does not say `dropped by an operator`, since \
         a run an operator dropped before its first WAL write has one, ended by the drop, \
         without having started. Once you have checked that its work did not run, ask again with \
         `checked_not_started` set: `fq dead-letters requeue researcher --trigger-seq 12 \
         --checked-not-started`. A start on record is refused whatever you pass."
    );

    // With an operator's drop of the run recorded, what the daemon read is the
    // WAL alone, whatever the reason, and the refusal says so.
    for why in consumed_without_a_dead_letter() {
        let answer = answered(
            &dead(false),
            true,
            false,
            verdict_on(&dead(false), dropped(why.clone())),
        );
        let Err(WireError::InvalidInput { message, .. }) = answer else {
            panic!("{why}: {answer:?}");
        };
        for expected in [
            format!("consumed without a run as `{why}`"),
            "`019a0000-0000-7000-8000-000000000001`, in the worker's WAL, the one record that \
             shows a start for a run the claim records an operator dropped, since the drop \
             writes the run's ownership row."
                .to_string(),
            format!("the daemon writes no dead letter for a trigger it consumes as `{why}`"),
        ] {
            assert!(
                message.contains(&expected),
                "{why}: `{expected}` in {message}"
            );
        }
        let answer = answered(
            &dead(false),
            true,
            true,
            verdict_on(&dead(false), dropped(why.clone())),
        );
        assert!(answer.is_ok(), "{why}: on the operator's word: {answer:?}");
    }
}

/// **A start under the run a dead letter's mark names is a start, for a
/// trigger consumed for a reason that writes no dead letter (issue 1055)**:
/// that run is the dead letter's own work, and it has a start on record.
/// The refusal says what is true of the claim: it answered, its answer
/// disagrees with this dead letter, and the start was found under the run
/// the mark names, not that no claim answered.
#[test]
fn a_start_under_the_marked_run_is_a_start_for_a_claim_that_disagrees() {
    let marked_run = || Some((MARKED_RUN.to_string(), "in the worker's WAL".to_string()));
    let verdict = || verdict_on(&dead(true), consumed(ConsumedWhy::FailedForGood));
    assert_eq!(
        with_the_marked_run(verdict(), marked_run()),
        Verdict::Started {
            invocation_id: MARKED_RUN.to_string(),
            on_record: "in the worker's WAL".to_string(),
            claim: OfTheClaim::Disagreed {
                why: ConsumedWhy::FailedForGood
            },
        }
    );

    for checked_not_started in [false, true] {
        let answer = answered(
            &dead(true),
            true,
            checked_not_started,
            with_the_marked_run(verdict(), marked_run()),
        );
        let Err(WireError::Conflict { message, .. }) = answer else {
            panic!("a start on record is a Conflict: {answer:?}");
        };
        assert!(
            message.ends_with(
                "Its claim answered that the trigger was consumed without a run as \
                 `failed_for_good`, which disagrees with this dead letter, since no dead letter is \
                 written for that answer: the start was found under the run the dead letter's \
                 mark names, and no claim was marked."
            ),
            "{message}"
        );
        assert!(!message.contains("No claim answered"), "{message}");
    }
}

/// **A consumed claim whose records could not be read cannot be told
/// (issue 1055).** For a claim consumed as dead-lettered that may clear;
/// for any other reason it does not, since read, the records would still
/// leave the claim and this dead letter disagreeing. Requeued from a hold,
/// it is refused outright all the same: the replacement went out whatever
/// the records hold.
#[test]
fn a_consumed_claim_whose_records_could_not_be_read_cannot_be_told() {
    let unchecked = |why| {
        Found::Look(Exhausted::Consumed(ConsumedClaim::Unchecked {
            invocation_id: RUN.to_string(),
            why,
            operator_dropped: false,
            unread: "the consumed trigger's ownership row could not be read: gone".to_string(),
        }))
    };
    for (consumed_why, clears) in [
        (ConsumedWhy::DeadLettered, true),
        (ConsumedWhy::FailedForGood, false),
    ] {
        let Verdict::CannotTell {
            why,
            invocation_id,
            may_clear,
        } = verdict_on(&dead(false), unchecked(consumed_why.clone()))
        else {
            panic!("a consumed claim whose records could not be read: cannot tell");
        };
        assert!(
            why.contains("could not be read") && why.contains(&format!("`{consumed_why}`")),
            "{why}"
        );
        assert_eq!(
            (invocation_id.as_deref(), may_clear),
            (Some(RUN), clears),
            "{consumed_why}"
        );
    }
    for checked_not_started in [false, true] {
        let answer = answered(
            &dead(false),
            true,
            checked_not_started,
            verdict_on(&dead(false), unchecked(ConsumedWhy::RequeuedFromHold)),
        );
        assert!(
            matches!(answer, Err(WireError::Conflict { .. })),
            "{answer:?}"
        );
    }
}
