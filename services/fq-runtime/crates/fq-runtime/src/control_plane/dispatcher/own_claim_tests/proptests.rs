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
//!
//! And at the end, once faults stop:
//!
//! 3. **Liveness.** A trigger with nothing on record that the broker can
//!    still deliver, or that a restart lets a replay reach, is started by
//!    that copy.
//! 4. **Every claim settles.** After a restart, a replay of the trigger
//!    starts nothing, is acked, and finds the claim marked started, naming
//!    the run on record: no step left a claim that a later delivery or
//!    restart cannot settle.
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
//! not lost arrives, and delivers nothing settled except a replay. The
//! model's only judgement is when a copy may arrive; every decision about a
//! copy is the dispatcher's own.
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
    /// An ack that was not lost has arrived: nothing is redelivered.
    acked: bool,
    /// The daemon has restarted, so a recreated durable can replay.
    restarted: bool,
    /// The next copy's answers are lost.
    lose_next: bool,
}

const MAX_DELIVER: u32 = TRIGGER_MAX_DELIVER as u32;

/// Hand a copy to `handle`, delivery `delivered`, with `run` queued for a
/// run it starts, and note what the broker received.
async fn deliver(arena: &Arena, broker: &mut Broker, run: Run, delivered: u32) -> Answered {
    arena.worker.queue(run);
    let lost = std::mem::take(&mut broker.lose_next);
    let answered = arena.deliver(delivered, lost).await;
    // A run this copy did not start is not the next copy's to start.
    arena.worker.script.lock().unwrap().clear();
    if !lost && answered.acks > 0 {
        broker.acked = true;
    }
    answered
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

/// Play `steps`, then stop the faults and settle the trigger.
async fn play(arena: &mut Arena, steps: &[Step]) -> Result<(), TestCaseError> {
    let mut broker = Broker::default();
    for (i, step) in steps.iter().enumerate() {
        match *step {
            Step::Deliver(run) => {
                if broker.acked || broker.delivered >= MAX_DELIVER {
                    continue;
                }
                broker.delivered += 1;
                let delivered = broker.delivered;
                deliver(arena, &mut broker, run, delivered).await;
            }
            Step::Replay(run) => {
                if !broker.restarted {
                    continue;
                }
                deliver(arena, &mut broker, run, 1).await;
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
            deliver(arena, &mut broker, Run::Completes, delivered).await;
        } else {
            arena.restart().await;
            deliver(arena, &mut broker, Run::Completes, 1).await;
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
    let replay = deliver(arena, &mut broker, Run::Completes, 1).await;
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
    check(arena, "the settling replay").await
}

/// **A trigger starts once and its claim settles, whatever happens to its
/// copies.** The property, over random lives of one trigger.
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
            play(&mut arena, &steps).await
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
