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

/// Wait, on a test thread, until `ready` holds.
fn wait_until(ready: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "never ready");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Long enough for a thread let go to reach the next lock it waits on.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// **An arm that meets the end of a first write that landed reports the
/// row** (issue 1070). The end is not under the `active` lock the arm
/// holds: it notes the row, then the write leaves the set of writes under
/// way. Staged with the registry's own locks, as the review staged it: the
/// test holds that set, so the arm, its halt inserted, waits on it; the
/// run's end then notes the row and takes the write out, and lets go. The
/// arm, reading the set first, finds the write gone and the row noted. In
/// the other order it read no row before the end, then no write after it,
/// and reported a row that had landed as never written.
#[test]
fn an_arm_that_meets_the_end_of_a_landed_first_write_reports_the_row() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();
    let _drive = live.enter(id, agent("builder"), &rounds, &timeouts);
    assert!(live.begin_first_write(id));

    let mut writes = live.first_writes.lock().unwrap();
    std::thread::scope(|s| {
        let arm = s.spawn(|| live.arm_halt(id).map(|halt| halt.row));
        wait_until(|| live.halt_requested.lock().unwrap().contains(&id));
        std::thread::sleep(SETTLE);
        // The run's end, as `end_first_write` makes it.
        live.note_row_written(id);
        writes.remove(&id);
        drop(writes);
        assert_eq!(
            arm.join().unwrap(),
            Some(RowAtHalt::Written),
            "the row landed before the arm read where it stood"
        );
    });
}

/// **A first write leaves the writes under way only once its row is
/// noted** (issue 1070), the order the arm's reads rely on. Staged: the
/// test holds the noted rows, so the run's end waits at its note; an arm
/// then answers without that lock, and finds the write still under way.
/// Had the write left first, the arm would find neither a write nor a note
/// and wait on the note held here.
#[test]
fn a_first_write_is_noted_before_it_leaves_the_writes_under_way() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();
    let _drive = live.enter(id, agent("builder"), &rounds, &timeouts);
    assert!(live.begin_first_write(id));

    let noted = live.wrote_row.lock().unwrap();
    let (answered, report) = std::thread::scope(|s| {
        let end = s.spawn(|| live.end_first_write(id, true).is_some());
        std::thread::sleep(SETTLE);
        let arm = s.spawn(|| live.arm_halt(id).map(|halt| halt.row));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !arm.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let answered = arm.is_finished();
        drop(noted);
        let report = arm.join().unwrap();
        assert!(end.join().unwrap(), "the arm's word is there to hear");
        (answered, report)
    });
    assert!(
        answered,
        "the arm waited on the note: the write had left the writes under way before its row \
         was noted"
    );
    assert_eq!(report, Some(RowAtHalt::FirstWriteUnderWay));
}

/// **No halt is armed between the first write's look for one and its
/// beginning** (issue 1070): both are made in one hold of the `active`
/// lock the arm takes. Staged: the test holds the writes under way, so the
/// beginning, having looked for a halt and found none, waits to record
/// itself; an arm started then waits on `active`, and arms nothing until
/// the write has begun, which it then reports.
#[test]
fn no_halt_is_armed_between_the_first_writes_look_and_its_beginning() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();
    let _drive = live.enter(id, agent("builder"), &rounds, &timeouts);

    let writes = live.first_writes.lock().unwrap();
    let (armed_meanwhile, begun, report) = std::thread::scope(|s| {
        let begin = s.spawn(|| live.begin_first_write(id));
        std::thread::sleep(SETTLE);
        let arm = s.spawn(|| live.arm_halt(id).map(|halt| halt.row));
        std::thread::sleep(SETTLE);
        let armed_meanwhile = live.halt_requested.lock().unwrap().contains(&id);
        drop(writes);
        (armed_meanwhile, begin.join().unwrap(), arm.join().unwrap())
    });
    assert!(
        !armed_meanwhile,
        "a halt was armed after the first write looked for one and before it began: the write \
         goes ahead with the halt armed, and the arm may report no row to come"
    );
    assert!(begun, "no halt was armed when the write looked");
    assert_eq!(report, Some(RowAtHalt::FirstWriteUnderWay));
}

/// **A halt never finds a resumed drive live without its row** (issue
/// 1070): the row is noted in the same hold of the `active` lock that
/// registers the drive. Staged: the test holds the noted rows, so the
/// registration waits at its note; an arm started then waits on `active`
/// and arms nothing until the drive is registered with its row.
#[test]
fn a_halt_never_finds_a_resumed_drive_live_without_its_row() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();

    let noted = live.wrote_row.lock().unwrap();
    let (armed_meanwhile, report) = std::thread::scope(|s| {
        let entered = s.spawn(|| {
            let drive = live.enter_from_row(id, agent("builder"), &rounds, &timeouts);
            std::mem::forget(drive);
        });
        std::thread::sleep(SETTLE);
        let arm = s.spawn(|| live.arm_halt(id).map(|halt| halt.row));
        std::thread::sleep(SETTLE);
        let armed_meanwhile = live.halt_requested.lock().unwrap().contains(&id);
        drop(noted);
        entered.join().unwrap();
        (armed_meanwhile, arm.join().unwrap())
    });
    assert!(
        !armed_meanwhile,
        "a halt was armed on a resumed drive registered before its row was noted"
    );
    assert_eq!(report, Some(RowAtHalt::Written));
}

/// **A first write under way is forgotten with its drive** (issue 1070).
/// A drive that ends during its first write, as a panic or a cancelled
/// task ends it, leaves no entry behind: a fresh drive of the same id
/// would otherwise read as having its first write under way, and a halt
/// armed on it would wait for a word no write will hear.
#[test]
fn a_first_write_under_way_is_forgotten_with_its_drive() {
    let live = LiveRegistry::default();
    let rounds = super::super::rounds::RoundLedger::default();
    let timeouts = super::super::timeouts::TimeoutLedger::default();
    let id = Uuid::now_v7();
    {
        let _drive = live.enter(id, agent("builder"), &rounds, &timeouts);
        assert!(live.begin_first_write(id));
        assert!(live.first_write_under_way(id));
    }
    assert!(
        !live.first_write_under_way(id),
        "the first write's entry outlived its drive"
    );
    let _drive = live.enter(id, agent("builder"), &rounds, &timeouts);
    assert_eq!(
        live.arm_halt(id)
            .map(|halt| (halt.row, halt.word.is_some())),
        Some((RowAtHalt::NeverWritten, false)),
        "a fresh drive of the same id has no write under way"
    );
}
