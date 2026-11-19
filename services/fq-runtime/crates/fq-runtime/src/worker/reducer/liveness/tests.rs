//! The live registry's own invariants — pure, no broker, no store.

use super::*;

fn agent(name: &str) -> AgentId {
    AgentId::new(name).expect("agent id")
}

/// Liveness and identity are one answer: while a drive is entered the
/// registry names its agent, and the moment the guard drops it names
/// nothing — no phantom "live" marker survives the drive.
#[test]
fn a_drive_is_live_and_named_for_exactly_its_own_lifetime() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();

    assert_eq!(live.agent_for(&id), None);
    assert!(!live.is_active(&id));

    {
        let _drive = live.enter(id, agent("researcher"), &rounds, &timeouts);
        assert!(live.is_active(&id));
        assert_eq!(
            live.agent_for(&id).map(AgentId::into_inner).as_deref(),
            Some("researcher")
        );
        // Concurrent drives are independent.
        let other = Uuid::now_v7();
        assert!(!live.is_active(&other));
    }

    assert!(!live.is_active(&id));
    assert_eq!(live.agent_for(&id), None);
}

/// A halt can only be armed against a live drive, is consumed once, and
/// never outlives the drive it was armed against — otherwise the next
/// drive of the same id (resume re-drives it) would suspend for no
/// reason.
#[test]
fn a_halt_needs_a_live_drive_and_dies_with_it() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();

    // Nothing to halt: refused, and nothing recorded.
    assert!(!live.request_halt(id));
    assert!(!live.take_halt(id));

    {
        let _drive = live.enter(id, agent("researcher"), &rounds, &timeouts);
        assert!(live.request_halt(id));
        // Consumed once.
        assert!(live.take_halt(id));
        assert!(!live.take_halt(id));
        // Armed again, then abandoned by the drive ending.
        assert!(live.request_halt(id));
    }

    assert!(
        !live.take_halt(id),
        "a halt must not survive the drive it was armed against"
    );

    // A fresh drive of the same id starts clean.
    let _drive = live.enter(id, agent("researcher"), &rounds, &timeouts);
    assert!(!live.take_halt(id));
}

/// The drive's note that it wrote its row (issue 1070) lives exactly as
/// long as the drive: forgotten when the drive ends, so a daemon does not
/// keep one id per run for its whole life, and a fresh drive of the same
/// id starts with no note until it writes or resumes from its row.
#[test]
fn a_written_row_is_noted_for_the_drive_and_forgotten_with_it() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();

    {
        let _drive = live.enter(id, agent("researcher"), &rounds, &timeouts);
        assert!(!live.wrote_row(id), "nothing written yet");
        live.note_row_written(id);
        assert!(live.wrote_row(id));
    }

    assert!(
        !live.wrote_row(id),
        "the note must not survive the drive that wrote the row"
    );
    let _drive = live.enter(id, agent("researcher"), &rounds, &timeouts);
    assert!(!live.wrote_row(id), "a fresh drive starts with no note");
}

/// **A halt says where the run's own row stands** (issue 1070): no row
/// and none to come before the first write begins; the write under way
/// from its beginning to its end; the row there once it has landed. A
/// write that failed leaves no row, and none to come from this drive.
#[test]
fn a_halt_says_whether_the_runs_row_is_to_come() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();
    let row = |live: &LiveRegistry| live.arm_halt(id).map(|halt| halt.row);
    assert_eq!(row(&live), None, "not driving: nothing armed");

    let _drive = live.enter(id, agent("builder"), &rounds, &timeouts);
    assert_eq!(row(&live), Some(RowAtHalt::NeverWritten));
    assert!(
        !live.begin_first_write(id),
        "the first write takes a halt armed before it, and is not made"
    );
    assert!(!live.take_halt(id), "the halt was taken");

    assert!(live.begin_first_write(id), "with no halt armed, it begins");
    assert_eq!(row(&live), Some(RowAtHalt::FirstWriteUnderWay));
    assert!(live.take_halt(id));
    live.end_first_write(id, false);
    assert_eq!(
        row(&live),
        Some(RowAtHalt::NeverWritten),
        "a write that failed left no row"
    );
    assert!(live.take_halt(id));

    assert!(live.begin_first_write(id));
    live.end_first_write(id, true);
    assert_eq!(row(&live), Some(RowAtHalt::Written));
}

/// **A resumed drive is live with its row, or not at all** (issue 1070):
/// it is entered from its row, and the arm never finds it live with none.
#[test]
fn a_resumed_drive_is_entered_with_its_row() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();
    {
        let _drive = live.enter_from_row(id, agent("builder"), &rounds, &timeouts);
        assert_eq!(
            live.arm_halt(id).map(|halt| halt.row),
            Some(RowAtHalt::Written)
        );
        assert!(live.take_halt(id));
    }
    assert!(!live.wrote_row(id), "forgotten with the drive");
}

/// **The drop's word reaches the run** (issue 1070). Only a halt that
/// finds the first write under way carries a word. The run hears it once
/// its write has ended: the final phase of a drop that was published, and
/// nothing from one that was not, whose word is dropped; and the hearing
/// ends with the words, so a run never waits on a drop that is gone.
#[tokio::test]
async fn the_drops_word_reaches_the_run_once_its_write_ends() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();
    let _drive = live.enter(id, agent("builder"), &rounds, &timeouts);
    let hear = |mut heard: tokio::sync::mpsc::Receiver<String>| async move {
        tokio::time::timeout(std::time::Duration::from_secs(1), heard.recv())
            .await
            .expect("the hearing ends with the words")
    };

    assert!(live.arm_halt(id).expect("armed").word.is_none());
    assert!(live.take_halt(id));

    assert!(live.begin_first_write(id));
    assert!(
        live.end_first_write(id, true).is_none(),
        "no halt during the write: nothing to hear"
    );

    live.wrote_row.lock().unwrap().clear();
    assert!(live.begin_first_write(id));
    let published = live.arm_halt(id).expect("armed").word.expect("a word");
    let not_published = live.arm_halt(id).expect("armed").word.expect("a word");
    drop(not_published);
    published.published("failed");
    let heard = live.end_first_write(id, true).expect("a drop armed a halt");
    assert_eq!(hear(heard).await.as_deref(), Some("failed"));
    assert!(live.take_halt(id));

    live.wrote_row.lock().unwrap().clear();
    assert!(live.begin_first_write(id));
    let word = live.arm_halt(id).expect("armed").word.expect("a word");
    let heard = live.end_first_write(id, true).expect("a drop armed a halt");
    drop(word);
    assert_eq!(hear(heard).await, None, "a drop not published says nothing");
}
