//! A trigger whose run finished while its claim still read `claimed`, and
//! the next daemon's start (issue 967).
//!
//! A run can finish under a claim that was never marked: the dispatcher
//! acks a delivery whose mark failed after its invocation had returned,
//! keeping the claim (`mark_durable_started`), and issue 967 has a refused
//! mark let the run go on. The next daemon claims under a new id, and its
//! dispatcher reads a `claimed` row under another id as a dead process's
//! claim on a trigger that never started, and adopts it. Two things bring
//! the trigger back to it: a redelivery, when the dead daemon's ack was
//! lost, and a replay, when the dispatcher's durable is deleted and
//! recreated or its state is lost in a broker restore, since the trigger
//! stream keeps a message for a day whether or not it was acked.
//!
//! Startup recovery marks every `claimed` row whose run has a start on
//! record before the dispatcher consumes anything: a run the worker's WAL
//! holds, in flight or finished, and a run the control plane holds an
//! ownership row for once the WAL has let it go. A claim with neither is
//! left for the adoption arm. Each test restarts over what the dead daemon
//! left, through `classify_in_flight` and then `TriggerDispatcher::run` on
//! the production durable, as the #959 tests beside these do.

use super::*;
use fq_runtime::control_plane::{InvocationArchiveRow, OwnerStatus};

/// How the dead daemon's run stood when the next daemon started.
#[derive(Clone, Copy, Debug)]
enum Run {
    /// Finished: its WAL row terminal, its archive not yet acknowledged.
    Finished,
    /// Archived: the control plane holds the archive row and the ownership
    /// row the hand-off writes, and the worker deleted its WAL row on the
    /// acknowledgement.
    Archived,
    /// Archived, and the archive row since swept by retention. The
    /// ownership row, which nothing sweeps, is all that is left of the run.
    ArchiveSwept,
}

/// Take the dead daemon's run of `invocation`, whose first WAL write has
/// landed, to `run`. The writes are the runner's terminal write, the
/// coordination consumer's archive hand-off and the worker's delete on its
/// acknowledgement, in that order and through the same store calls.
async fn the_run_ends(crash: &AfterTheCrash, invocation: &str, run: Run) {
    let mut terminal = first_wal_row(invocation);
    terminal.phase = "completed".to_string();
    terminal.step_index = 1;
    terminal.updated_at = 2_000;
    terminal.terminal_at = Some(2_000);
    crash
        .worker_store
        .upsert_invocation_state(&terminal)
        .await
        .unwrap();
    // Retention sweeps by `archived_at`, so the run whose archive is swept
    // is archived before the cutoff below and every other one after it.
    let archived_at = match run {
        Run::Finished => return,
        Run::Archived => 5_000,
        Run::ArchiveSwept => 3_000,
    };
    crash
        .cp_store
        .insert_archive(&InvocationArchiveRow {
            invocation_id: invocation.to_string(),
            agent_id: CRASH_AGENT.to_string(),
            final_phase: "completed".to_string(),
            final_state_blob: b"{}".to_vec(),
            started_at: 1_000,
            terminal_at: 2_000,
            archived_at,
        })
        .await
        .unwrap();
    crash
        .cp_store
        .upsert_invocation_ownership(invocation, DEAD_DAEMON, archived_at, OwnerStatus::Completed)
        .await
        .unwrap();
    assert_eq!(
        crash
            .worker_store
            .delete_invocation_state(invocation)
            .await
            .unwrap(),
        1,
        "the acknowledged archive takes the run out of the WAL"
    );
    if let Run::ArchiveSwept = run {
        assert_eq!(
            crash.cp_store.sweep_archive(4_000).await.unwrap(),
            1,
            "retention sweeps the archive row"
        );
    }
}

/// One more trigger the dead daemon pulled and claimed, naming
/// `invocation`, and left unacked.
async fn another_trigger(
    crash: &AfterTheCrash,
    invocation: &str,
) -> (async_nats::jetstream::Message, TriggerKey<'static>) {
    let published = crash
        .bus
        .publish_trigger(
            &AgentId::new(CRASH_AGENT).unwrap(),
            &serde_json::json!({ "input": format!("a trigger for {invocation}") }),
        )
        .await
        .expect("publish a trigger");
    let delivery = crash
        .consumer
        .batch()
        .max_messages(1)
        .expires(Duration::from_secs(5))
        .messages()
        .await
        .expect("pull")
        .next()
        .await
        .expect("the trigger within 5s")
        .expect("message ok");
    let key = TriggerKey {
        stream: TRIGGER_STREAM_NAME,
        stream_epoch: crash.bus.trigger_stream_epoch(),
        stream_seq: published.stream_seq,
    };
    assert_eq!(
        crash
            .cp_store
            .claim_trigger(key, DEAD_DAEMON, invocation, 0)
            .await
            .unwrap(),
        TriggerClaim::Won
    );
    (delivery, key)
}

/// The dead daemon acked `delivery` and kept its claim, as the dispatcher
/// does for a run whose mark failed (`ack_keeping_claim`). Returns once the
/// broker has the ack.
async fn acked_keeping_the_claim(
    consumer: &mut PullConsumer,
    delivery: &async_nats::jetstream::Message,
) {
    delivery.ack().await.expect("ack the delivery");
    let deadline = Instant::now() + POLL * MAX_POLLS;
    loop {
        let info = consumer.info().await.expect("consumer info").clone();
        if info.num_ack_pending == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the broker never took the ack: {info:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Delete the dispatcher's durable, as an operator recreating it does, or
/// a broker restore that loses its state. The next dispatcher creates it
/// again, and a durable created with no deliver policy starts from the
/// oldest message the stream holds: every trigger of the last day comes
/// back, acked or not.
async fn the_durable_is_lost(crash: &AfterTheCrash) {
    let stream = crash
        .bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream");
    assert!(
        stream
            .delete_consumer(CONSUMER_NAME)
            .await
            .expect("delete the dispatcher's durable")
            .success
    );
}

/// The next daemon, recovered and not yet dispatching: startup recovery
/// has run under a new runtime id, and its claims are in hand.
struct Recovering {
    worker_id: WorkerId,
    claims: TriggerClaims,
    /// The invocations recovery handed back to be resumed.
    recoverable: Vec<String>,
}

/// The next daemon's startup recovery, as `assemble` runs it.
async fn the_next_daemon_recovers(crash: &AfterTheCrash) -> Recovering {
    let runtime_id = Uuid::now_v7();
    let worker_id = WorkerId::new(runtime_id.to_string()).unwrap();
    let Recovered {
        recoverable,
        claims,
        ..
    } = classify_in_flight(
        &crash.worker_store,
        &crash.cp_store,
        &crash.bus,
        runtime_id,
        &worker_id,
    )
    .await
    .expect("startup recovery");
    Recovering {
        worker_id,
        claims,
        recoverable: recoverable
            .into_iter()
            .map(|c| c.state.invocation_id)
            .collect(),
    }
}

impl Recovering {
    /// Then its trigger dispatcher, on the production durable with the
    /// claims recovery returned, through `TriggerDispatcher::run` as
    /// `hosted.rs` runs it.
    fn dispatches(self, crash: &AfterTheCrash) -> Restarted {
        let worker = Arc::new(RestartedWorker::default());
        let dispatcher = TriggerDispatcher::new(
            crash.bus.clone(),
            shared_registry(crash_agent_registry(crash.scratch.path())),
            worker.clone(),
            Arc::new(FixtureClient::new()),
            1,
        )
        .with_trigger_claims(self.claims, self.worker_id.as_str());
        let (shutdown, shutdown_rx) = oneshot::channel();
        let run = tokio::spawn(dispatcher.run(shutdown_rx));
        Restarted {
            recoverable: self.recoverable,
            worker,
            shutdown,
            run,
        }
    }
}

/// The next daemon, recovered and dispatching. Whatever brings the dead
/// daemon's triggers back is set up before this is called.
async fn the_next_daemon(crash: &AfterTheCrash) -> Restarted {
    the_next_daemon_recovers(crash).await.dispatches(crash)
}

/// Wait until the recreated durable has delivered every message up to
/// `last_seq` and nothing is unacked: each replayed copy has been answered,
/// whichever way. A start acks its copy only once it is recorded, so a
/// start a replayed copy caused has been recorded by the time this returns.
async fn wait_until_the_replay_is_settled(crash: &AfterTheCrash, last_seq: u64) {
    let stream = crash
        .bus
        .jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("the trigger stream");
    let deadline = Instant::now() + POLL * MAX_POLLS;
    let mut last = None;
    for _ in 0..MAX_POLLS {
        // Absent until the dispatcher has created it again.
        if let Ok(info) = stream.consumer_info(CONSUMER_NAME).await {
            if info.delivered.stream_sequence >= last_seq
                && info.num_pending == 0
                && info.num_ack_pending == 0
            {
                return;
            }
            last = Some(info);
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("the replayed triggers were not settled in time: {last:?}");
}

fn duplicates_dropped(crash: &AfterTheCrash) -> u64 {
    crash
        .bus
        .consumer_ledger()
        .record(CONSUMER_NAME)
        .duplicate_dropped
}

/// The redelivery of a trigger whose run finished, after the next daemon's
/// recovery: dropped, its claim marked, nothing run a second time.
async fn a_redelivery_after(run: Run) {
    let mut crash = a_daemon_dies_holding_a_trigger(true).await;
    let finished = crash.dead_invocation.to_string();
    the_run_ends(&crash, &finished, run).await;
    // The ack that would have settled the delivery was lost: the broker
    // redelivers it at the end of its ack window, brought forward here as
    // in `restart`.
    crash
        .delivery
        .ack_with(AckKind::Nak(None))
        .await
        .expect("redeliver the trigger");

    let next = the_next_daemon(&crash).await;
    assert!(
        next.recoverable.is_empty(),
        "a finished run is not resumed: {:?}",
        next.recoverable
    );
    wait_until_the_redelivery_is_settled(&mut crash.consumer).await;
    // Margin on top of the ordering argument, for a start that would come
    // late.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        next.worker.starts(),
        Vec::<Option<Uuid>>::new(),
        "the redelivered trigger was adopted and started although its run had finished ({run:?}): \
         one trigger, two runs (issue 967)"
    );
    assert_eq!(
        claim_on(&crash).await,
        TriggerClaim::Started {
            invocation_id: finished
        },
        "recovery records the finished run's start on its claim"
    );
    assert_eq!(
        duplicates_dropped(&crash),
        1,
        "the redelivery is dropped as a duplicate of a started run, and counted"
    );
    next.stop().await;
}

/// **A trigger whose run finished, its WAL row terminal and not yet
/// archived, is not run again by a redelivery after a restart.**
#[tokio::test]
async fn a_finished_runs_redelivered_trigger_is_dropped_after_a_restart() {
    a_redelivery_after(Run::Finished).await;
}

/// **The same once the run was archived and its WAL row deleted:** the
/// control plane's ownership row is the start on record.
#[tokio::test]
async fn an_archived_runs_redelivered_trigger_is_dropped_after_a_restart() {
    a_redelivery_after(Run::Archived).await;
}

/// **And once retention has swept the archive row too.** The ownership
/// row is never swept, so it still records the run.
#[tokio::test]
async fn a_swept_archives_redelivered_trigger_is_dropped_after_a_restart() {
    a_redelivery_after(Run::ArchiveSwept).await;
}

/// **A replay after a restart starts each trigger's work once: the ones
/// whose runs finished are dropped, the one that never started is adopted
/// and run (issue 967).**
///
/// The dead daemon left four triggers claimed under its id. Three ran to
/// their end without their claims being marked, and it acked each,
/// keeping the claim: one finished and not yet archived, one archived,
/// one archived with the archive since swept. The fourth it claimed and
/// never started, and left unacked. Then the dispatcher's durable was
/// lost, and the next daemon's dispatcher created it again and was
/// handed every trigger still on the stream.
///
/// This is the route the review of PR 1021 reproduced: no ack is lost,
/// yet a finished trigger comes back. Recovery marks the three started
/// claims, so their copies are dropped; the fourth claim has no start on
/// record, is left `claimed`, and its copy is adopted and started once,
/// under the adopter's own id.
#[tokio::test]
async fn a_replay_after_a_restart_drops_finished_runs_and_adopts_the_trigger_that_never_started() {
    let mut crash = a_daemon_dies_holding_a_trigger(true).await;
    let finished = crash.dead_invocation.to_string();
    the_run_ends(&crash, &finished, Run::Finished).await;
    acked_keeping_the_claim(&mut crash.consumer, &crash.delivery).await;

    let mut started = vec![(crash.key, finished)];
    for run in [Run::Archived, Run::ArchiveSwept] {
        let invocation = Uuid::now_v7().to_string();
        let (delivery, key) = another_trigger(&crash, &invocation).await;
        crash
            .worker_store
            .upsert_invocation_state(&first_wal_row(&invocation))
            .await
            .unwrap();
        the_run_ends(&crash, &invocation, run).await;
        acked_keeping_the_claim(&mut crash.consumer, &delivery).await;
        started.push((key, invocation));
    }
    let never_started = Uuid::now_v7();
    let (_unacked, never_started_key) = another_trigger(&crash, &never_started.to_string()).await;

    the_durable_is_lost(&crash).await;
    let next = the_next_daemon(&crash).await;
    assert!(
        next.recoverable.is_empty(),
        "nothing was in flight, so nothing is resumed: {:?}",
        next.recoverable
    );
    wait_until_the_replay_is_settled(&crash, never_started_key.stream_seq).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let starts = next.worker.starts();
    assert_eq!(
        starts.len(),
        1,
        "only the trigger that never started runs; a start for any other is a finished run's \
         trigger started a second time by the replay (issue 967): {starts:?}"
    );
    let adopted = starts[0].expect("the dispatcher names the invocation it starts");
    assert_ne!(
        adopted, never_started,
        "an adopted claim runs under the adopter's own id"
    );
    let probe = |key| crash.cp_store.claim_trigger(key, "probe", "probe", 0);
    for (key, invocation) in started {
        assert_eq!(
            probe(key).await.unwrap(),
            TriggerClaim::Started {
                invocation_id: invocation
            },
            "recovery records each finished run's start on its claim"
        );
    }
    assert_eq!(
        probe(never_started_key).await.unwrap(),
        TriggerClaim::Started {
            invocation_id: adopted.to_string()
        },
        "the claim with no start on record was adopted and started once"
    );
    assert_eq!(
        duplicates_dropped(&crash),
        3,
        "each finished run's replayed copy is dropped as a duplicate, and counted"
    );
    next.stop().await;
}

/// **A run an operator dropped before its first WAL write is not run again
/// after a restart.** The dead daemon claimed the trigger, and an operator
/// dropped the run before it made its first WAL write: a live drop is
/// accepted from the moment the runner takes the run, and a drop of a run
/// whose process died resolves it through its `triggered` event, which is
/// published before that write. The drop leaves what the coordination
/// consumer writes for it, an archive row and a `failed` ownership row,
/// and no WAL row. Recovery marks the claim, so the redelivery is dropped
/// and the work never runs: the drop is the run's outcome, as it is for
/// the same drop on a live daemon, where the dispatcher consumes the
/// trigger rather than retry it.
#[tokio::test]
async fn a_run_dropped_before_its_first_wal_write_is_not_run_after_a_restart() {
    let mut crash = a_daemon_dies_holding_a_trigger(false).await;
    let dropped = crash.dead_invocation.to_string();
    // `handle_invocation_operator_recovered`'s writes, in its order. Its
    // third, marking the WAL row terminal, finds no row.
    crash
        .cp_store
        .insert_archive(&InvocationArchiveRow {
            invocation_id: dropped.clone(),
            agent_id: CRASH_AGENT.to_string(),
            final_phase: "failed".to_string(),
            final_state_blob: Vec::new(),
            started_at: 3_000,
            terminal_at: 3_000,
            archived_at: 3_000,
        })
        .await
        .unwrap();
    crash
        .cp_store
        .upsert_invocation_ownership(&dropped, CRASH_AGENT, 3_000, OwnerStatus::Failed)
        .await
        .unwrap();
    crash
        .delivery
        .ack_with(AckKind::Nak(None))
        .await
        .expect("redeliver the trigger");

    let next = the_next_daemon(&crash).await;
    wait_until_the_redelivery_is_settled(&mut crash.consumer).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        next.worker.starts(),
        Vec::<Option<Uuid>>::new(),
        "the redelivery ran the work an operator dropped"
    );
    assert_eq!(
        claim_on(&crash).await,
        TriggerClaim::Started {
            invocation_id: dropped
        },
        "recovery marks the claim of the dropped run"
    );
    assert_eq!(duplicates_dropped(&crash), 1);
    next.stop().await;
}

/// **A claim with no start on record is left exactly as it is.** The
/// dead daemon claimed the trigger and stopped before its first WAL
/// write; nothing of the run is on record anywhere. Recovery does not
/// touch the row, so the next daemon's dispatcher adopts it when the
/// replay brings the trigger back, and runs it once.
#[tokio::test]
async fn a_claim_with_no_start_on_record_is_left_for_the_replay_to_adopt() {
    let crash = a_daemon_dies_holding_a_trigger(false).await;
    let held = TriggerClaim::Held {
        claimant: DEAD_DAEMON.to_string(),
        invocation_id: Some(crash.dead_invocation.to_string()),
        claimed_at: 0,
    };
    assert_eq!(claim_on(&crash).await, held);

    let recovering = the_next_daemon_recovers(&crash).await;
    assert!(recovering.recoverable.is_empty());
    assert_eq!(
        claim_on(&crash).await,
        held,
        "recovery leaves a claim with no start on record exactly as it was"
    );

    the_durable_is_lost(&crash).await;
    let next = recovering.dispatches(&crash);
    wait_until_the_replay_is_settled(&crash, crash.key.stream_seq).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let starts = next.worker.starts();
    assert_eq!(
        starts.len(),
        1,
        "the trigger starts exactly once: {starts:?}"
    );
    let adopted = starts[0].expect("the dispatcher names the invocation it starts");
    assert_eq!(
        claim_on(&crash).await,
        TriggerClaim::Started {
            invocation_id: adopted.to_string()
        }
    );
    assert_eq!(duplicates_dropped(&crash), 0);
    next.stop().await;
}
