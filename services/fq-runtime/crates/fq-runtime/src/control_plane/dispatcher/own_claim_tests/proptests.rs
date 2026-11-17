//! One trigger's claim as a property, not as examples (issue 967).
//!
//! The tests beside this file are points; this is the space. A random life
//! of one trigger is played against the real arbitration: copies are
//! delivered, runs die before or after their first WAL write or fail
//! transiently, the claim store refuses marks or releases, a copy's
//! answers are lost, the daemon restarts and recovery runs, and copies are
//! replayed from a recreated durable. After every step, three things are
//! checked:
//!
//! 1. **No start over a start.** The trigger's work is never started while
//!    an earlier start of it is on record: the stub worker records every
//!    run started while the WAL already held one, and there must be none.
//! 2. **One run on record, and a marked claim names it.** The WAL holds at
//!    most one run of the trigger, and a claim marked started names that
//!    run.
//! 3. **A copy of a trigger that started is acked.** A copy that arrives
//!    while a start of its trigger is on record, and whose answers reach the
//!    broker, is acked, so the broker stops redelivering a trigger whose
//!    run started.
//!
//! And at the end, once faults stop:
//!
//! 4. **Liveness.** A trigger with nothing on record that the broker can
//!    still deliver, or that a restart lets a replay reach, is started by
//!    that copy.
//! 5. **Every claim settles.** After a restart, a replay of the trigger
//!    starts nothing, is acked, and finds the claim marked started, naming
//!    the run on record: no step left a claim that a later delivery or
//!    restart cannot settle.
//!
//! # Known open: a trigger whose run started, left to be dead-lettered
//!
//! One more thing is checked, and the property tolerates it today. When
//! the trigger's deliveries on the dispatcher's durable run out and no ack
//! has reached the broker, the dead-letter watch calls the trigger
//! exhausted, and if a start of it is on record that dead letter is false:
//! requeuing it runs the work twice. With invariant 3 holding, the only way
//! there is a copy that never came back: the last delivery started the run
//! (its task then died, or its mark gave up and its ack was lost), or every
//! ack after the start was lost, the last one included. No copy is left for
//! the dispatcher to settle, and the watch does not consult the records;
//! having it consult them is pull request 5 of issue 967. [`play`] reports
//! the case ([`KnownOpen`]) rather than failing on it, because a random
//! life reaches it now and then and the property must not fail on a gap
//! that is known and tracked.
//! [`a_trigger_whose_last_delivery_started_its_run_is_not_left_to_be_dead_lettered`]
//! asserts it on the two lives that show it and is ignored, with that
//! reason, until pull request 5 makes it pass.
//!
//! # Fidelity: what each step does
//!
//! The point of the property is the arbitration, so each step drives the
//! production code and nothing stands in for it but the run itself and the
//! broker's side of a delivery:
//!
//! | step | what happens | the code it drives |
//! | --- | --- | --- |
//! | deliver | the broker delivers the trigger again, while it holds it unacked and has deliveries left | `TriggerDispatcher::handle`, `claim_delivery` |
//! | replay | after a restart, a durable recreated on the stream delivers it, acked or not | the same |
//! | a run dies before its start | the task panics before the first WAL write | `handle`, on a task of its own as the consume loop runs it |
//! | a run dies after its start | the task panics in the poll that made the first WAL write | the same |
//! | a run fails transiently | the signal is dropped and the run errs before its first WAL write | `settle_before_durable_start`, `nak` |
//! | the store refuses updates | marks, re-takes and adoptions fail until healed or a restart | `mark_durable_started`'s retries and fallback |
//! | the store refuses releases | the release before an ack or a NAK fails | `ack`, `nak` |
//! | answers are lost | the next copy answers on a closed connection | the ack after a refused mark, a NAK, an ack |
//! | restart | a new claimant id, claims recovered over the WAL; a run in flight is resumed and finishes | `TriggerClaims::recover`, then a new dispatcher |
//!
//! The broker is modelled, not run: it counts deliveries up to
//! `TRIGGER_MAX_DELIVER`, considers the trigger settled once an ack that was
//! not lost arrives on the dispatcher's durable, and delivers nothing
//! settled except a replay. A replay comes from a recreated durable, so its
//! ack settles nothing on the dispatcher's. The model's only judgement is
//! when a copy may arrive; every decision about a copy is the dispatcher's
//! own.
//!
//! [`the_shrunk_dead_run_is_taken_again`] replays by hand the case this
//! property shrank to against the code before issue 967's last change.

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{Answered, Arena, Run, SEQ, heal, refuse_deletes, refuse_updates};
use crate::bus::TRIGGER_MAX_DELIVER;
use crate::test_support::nats::test_nats;
use crate::worker::WalRun;

/// One thing that happens in a trigger's life.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// The broker delivers the trigger (again), if it still holds it
    /// unacked and has deliveries left. A run this copy starts does as
    /// the [`Run`] says.
    Deliver(Run),
    /// After a restart, a durable recreated on the stream delivers the
    /// trigger, acked or not.
    Replay(Run),
    /// The claim store refuses updates until it heals or the daemon
    /// restarts.
    RefuseUpdates,
    /// The claim store refuses deletes until it heals or the daemon
    /// restarts.
    RefuseReleases,
    /// The next copy's answers are lost.
    LoseAnswers,
    /// The claim store heals.
    Heal,
    /// The daemon restarts, and recovery runs before it consumes anything.
    Restart,
}

fn run() -> impl Strategy<Value = Run> {
    prop_oneof![
        4 => Just(Run::Completes),
        2 => Just(Run::DiesBeforeStart),
        2 => Just(Run::DiesAfterStart),
        2 => Just(Run::FailsTransiently),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => run().prop_map(Step::Deliver),
        2 => run().prop_map(Step::Replay),
        1 => Just(Step::RefuseUpdates),
        2 => Just(Step::RefuseReleases),
        2 => Just(Step::LoseAnswers),
        1 => Just(Step::Heal),
        2 => Just(Step::Restart),
    ]
}

/// The broker's side of the trigger.
#[derive(Debug, Default)]
struct Broker {
    /// Deliveries of the trigger on the dispatcher's durable so far.
    delivered: u32,
    /// An ack that was not lost has arrived on the dispatcher's durable:
    /// nothing is redelivered.
    acked: bool,
    /// The last delivery on the dispatcher's durable was handled with no
    /// ack reaching the broker while a start of the trigger was on record.
    left_to_be_dead_lettered: bool,
    /// The daemon has restarted, so a recreated durable can replay.
    restarted: bool,
    /// The next copy's answers are lost.
    lose_next: bool,
}

const MAX_DELIVER: u32 = TRIGGER_MAX_DELIVER as u32;

/// Where a copy comes from.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// Delivery `n` on the dispatcher's durable.
    Durable(u32),
    /// A replay from a durable recreated on the stream.
    Replay,
}

/// What a life of the trigger left that is known to be wrong today.
#[derive(Debug, Default)]
struct KnownOpen {
    /// The trigger's deliveries on the dispatcher's durable ran out with no
    /// ack reaching the broker while a start of it was on record: the
    /// dead-letter watch, which does not consult the records, would call it
    /// exhausted (pull request 5 of issue 967).
    left_to_be_dead_lettered: bool,
}

/// Hand a copy to `handle`, with `run` queued for a run it starts, check
/// invariant 3, and note what the broker received.
async fn deliver(
    arena: &Arena,
    broker: &mut Broker,
    run: Run,
    source: Source,
) -> Result<Answered, TestCaseError> {
    let started_before = !arena.wal().await.is_empty();
    arena.worker.queue(run);
    let lost = std::mem::take(&mut broker.lose_next);
    let delivered = match source {
        Source::Durable(n) => n,
        Source::Replay => 1,
    };
    let answered = arena.deliver(delivered, lost).await;
    // A run this copy did not start is not the next copy's to start.
    arena.worker.script.lock().unwrap().clear();
    if started_before && !lost {
        prop_assert!(
            answered.acks > 0,
            "{:?} came while a start of its trigger was on record and was not acked: {:?}",
            source,
            answered
        );
    }
    if let Source::Durable(n) = source {
        if !lost && answered.acks > 0 {
            broker.acked = true;
        }
        if n == MAX_DELIVER && !broker.acked {
            broker.left_to_be_dead_lettered = !arena.wal().await.is_empty();
        }
    }
    Ok(answered)
}

/// The invariants that hold after every step.
async fn check(arena: &Arena, after: &str) -> Result<(), TestCaseError> {
    let over = arena.worker.started_over_a_start.lock().unwrap().clone();
    prop_assert!(
        over.is_empty(),
        "after {after}: the trigger was started again while a start of it was on record: {over:?}"
    );
    let wal = arena.wal().await;
    prop_assert!(
        wal.len() <= 1,
        "after {after}: more than one run of the trigger is on record: {wal:?}"
    );
    if let Some(row) = arena.claim(SEQ).await
        && row.started
    {
        prop_assert_eq!(
            row.invocation_id.clone(),
            wal.first().map(|(id, _)| id.clone()),
            "after {}: a claim marked started must name the run on record ({:?})",
            after,
            row
        );
    }
    Ok(())
}

/// Play `steps`, then stop the faults and settle the trigger. Returns what
/// the life left that is known to be wrong today.
async fn play(arena: &mut Arena, steps: &[Step]) -> Result<KnownOpen, TestCaseError> {
    let mut broker = Broker::default();
    for (i, step) in steps.iter().enumerate() {
        match *step {
            Step::Deliver(run) => {
                if broker.acked || broker.delivered >= MAX_DELIVER {
                    continue;
                }
                broker.delivered += 1;
                let delivered = broker.delivered;
                deliver(arena, &mut broker, run, Source::Durable(delivered)).await?;
            }
            Step::Replay(run) => {
                if !broker.restarted {
                    continue;
                }
                deliver(arena, &mut broker, run, Source::Replay).await?;
            }
            Step::RefuseUpdates => refuse_updates(&arena.claim_db).await,
            Step::RefuseReleases => refuse_deletes(&arena.claim_db).await,
            Step::LoseAnswers => broker.lose_next = true,
            Step::Heal => heal(&arena.claim_db).await,
            Step::Restart => {
                arena.restart().await;
                broker.restarted = true;
            }
        }
        check(arena, &format!("step {i}, {step:?}")).await?;
    }

    // The faults stop.
    heal(&arena.claim_db).await;
    broker.lose_next = false;

    // Liveness: nothing on record, and a copy the broker can still deliver
    // or a restart lets a replay bring, starts the trigger.
    if arena.wal().await.is_empty() {
        if !broker.acked && broker.delivered < MAX_DELIVER {
            broker.delivered += 1;
            let delivered = broker.delivered;
            deliver(
                arena,
                &mut broker,
                Run::Completes,
                Source::Durable(delivered),
            )
            .await?;
        } else {
            arena.restart().await;
            deliver(arena, &mut broker, Run::Completes, Source::Replay).await?;
        }
        let wal = arena.wal().await;
        prop_assert_eq!(
            wal.len(),
            1,
            "a trigger with nothing on record and a copy to deliver was not started"
        );
        check(arena, "the liveness delivery").await?;
    }

    // Settled: after a restart, a replay starts nothing, is acked, and finds
    // the claim marked started, naming the run on record.
    let started = arena.worker.starts().len();
    arena.restart().await;
    let replay = deliver(arena, &mut broker, Run::Completes, Source::Replay).await?;
    prop_assert_eq!(
        arena.worker.starts().len(),
        started,
        "a replay after a restart started the trigger again"
    );
    let wal = arena.wal().await;
    prop_assert_eq!(
        wal.len(),
        1,
        "one run of the trigger is on record: {:?}",
        wal
    );
    prop_assert_eq!(wal[0].1, WalRun::Finished);
    let claim = arena.claim(SEQ).await;
    prop_assert_eq!(
        claim
            .as_ref()
            .map(|row| (row.started, row.invocation_id.clone())),
        Some((true, Some(wal[0].0.clone()))),
        "the claim must end marked started, naming the run on record: {:?}",
        claim
    );
    prop_assert_eq!(replay.acks, 1, "the replay is acked: {:?}", replay);
    check(arena, "the settling replay").await?;
    Ok(KnownOpen {
        left_to_be_dead_lettered: broker.left_to_be_dead_lettered,
    })
}

/// **A trigger starts once and its claim settles, whatever happens to its
/// copies.** The property, over random lives of one trigger. A life that
/// leaves the trigger to be dead-lettered although its run started is the
/// known-open case the module doc describes, and is tolerated here.
#[test]
fn a_trigger_starts_once_and_its_claim_settles_whatever_happens_to_its_copies() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("a runtime");
    let server = test_nats();
    let url = server.url().to_string();
    let mut config = Config::with_cases(48);
    config.failure_persistence = None;
    let mut runner = TestRunner::new(config);
    let outcome = runner.run(&prop::collection::vec(step(), 1..8), |steps| {
        rt.block_on(async {
            let mut arena = Arena::new(&url, 1).await;
            play(&mut arena, &steps).await.map(|_known_open| ())
        })
    });
    if let Err(err) = outcome {
        panic!("{err}");
    }
}

/// **The case the property shrank to, kept as a regression.** Against the
/// code before issue 967's last change, the property shrank to one step:
/// a copy whose run dies before its first WAL write. Nothing is on record,
/// and every later copy met this daemon's own claim and was dropped, so the
/// liveness delivery started nothing. Played here through the property's
/// own steps and checks.
#[tokio::test]
async fn the_shrunk_dead_run_is_taken_again() {
    let (mut arena, _server) = super::arena().await;
    let outcome = play(&mut arena, &[Step::Deliver(Run::DiesBeforeStart)]).await;
    if let Err(err) = outcome {
        panic!("{err}");
    }
}

/// **A trigger whose last delivery started its run is not left to be
/// dead-lettered.** Known open, and ignored until pull request 5 of issue
/// 967 makes it pass. Two lives show it, the two the review of this change
/// named: four deliveries fail transiently and are NAKed, and the fifth and
/// last starts the run, whose task then dies in the poll of its first WAL
/// write; or whose mark is refused on every attempt and whose ack is lost.
/// Either way no copy follows for the dispatcher to settle, the broker
/// never receives an ack, and the dead-letter watch, which does not consult
/// the records, would call a trigger exhausted whose run started.
///
/// Ignored rather than written as an assertion of today's behaviour,
/// because the dead letter is the watch's decision and this harness models
/// the watch rather than running it: a test that asserted today's outcome
/// would stay green when the watch is fixed, and flag nothing. This one
/// states the outcome pull request 5 has to reach, is listed as ignored
/// with its reason in every run, and fails when run with `--ignored`; that
/// pull request makes the model's watch consult the records as the real
/// one will, and un-ignores it.
#[tokio::test]
#[ignore = "known open until pull request 5 of issue 967: the dead-letter watch does not consult \
            the records, so a run started by a trigger's last delivery is dead-lettered as \
            exhausted"]
async fn a_trigger_whose_last_delivery_started_its_run_is_not_left_to_be_dead_lettered() {
    let fails_four_times = [Step::Deliver(Run::FailsTransiently); 4];
    let lives: [Vec<Step>; 2] = [
        [&fails_four_times[..], &[Step::Deliver(Run::DiesAfterStart)]].concat(),
        [
            &fails_four_times[..],
            &[
                Step::RefuseUpdates,
                Step::LoseAnswers,
                Step::Deliver(Run::Completes),
            ],
        ]
        .concat(),
    ];
    let mut left = Vec::new();
    for steps in lives {
        let (mut arena, _server) = super::arena().await;
        let open = play(&mut arena, &steps)
            .await
            .unwrap_or_else(|err| panic!("{steps:?}: {err}"));
        if open.left_to_be_dead_lettered {
            left.push(steps);
        }
    }
    assert!(
        left.is_empty(),
        "the trigger's deliveries ran out with no ack reaching the broker while its run's start \
         was on record, so the dead-letter watch would call it exhausted, and requeuing that dead \
         letter would run the work twice, in these lives: {left:#?}"
    );
}
