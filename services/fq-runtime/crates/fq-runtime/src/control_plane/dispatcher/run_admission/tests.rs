//! The worker permits as the daemon's resumes use them (#994): a place
//! taken with [`WorkerPermits::queue`](super::WorkerPermits::queue) before
//! the dispatcher pulls a trigger is ahead of that trigger, however late
//! the task that holds it first runs; and a drain or a shutdown ends the
//! wait, without starting anything on a permit the drain itself frees.
//!
//! The "recovered runs" here are places taken exactly as startup recovery
//! takes them, with the test holding the permits where recovery's tasks
//! would run the resume. `fq-daemon`'s recovery tests drive the real path.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::super::tests::{
    CappedWorker, publish_triggers, recovered_claims, registry_with, stop, unique_agent_id,
    unique_consumer_name,
};
use super::super::{DispatcherError, SharedRegistry, TriggerDispatcher};
use super::{Place, RunAdmission, RunPermit, Waiter};
use crate::bus::EventBus;
use crate::control_plane::ControlPlaneStore;
use crate::llm::fixture::FixtureClient;
use crate::worker::{DrainReason, DrainRequest, Worker};

/// A dispatcher on its own consumer for `agent`, sharing `admission` the
/// way the daemon's does.
struct Sharing {
    shutdown: oneshot::Sender<()>,
    run: JoinHandle<Result<(), DispatcherError>>,
    _claims: tempfile::TempDir,
}

async fn dispatcher_sharing(
    bus: &EventBus,
    agent: &str,
    registry: SharedRegistry,
    worker: Arc<dyn Worker>,
    admission: &RunAdmission,
) -> Sharing {
    let claims = tempfile::tempdir().unwrap();
    let store = Arc::new(
        ControlPlaneStore::open(&claims.path().join("cp.db"))
            .await
            .unwrap(),
    );
    let dispatcher = TriggerDispatcher::new(
        bus.clone(),
        registry,
        worker,
        Arc::new(FixtureClient::new()),
        admission.workers().max_concurrent(),
    )
    .with_caps_and_claims(
        admission.clone(),
        recovered_claims(&store).await,
        "worker-a",
    );
    let consumer = unique_consumer_name();
    let filter = crate::events::subjects::trigger(agent);
    let (shutdown, shutdown_rx) = oneshot::channel();
    let run = tokio::spawn(async move {
        dispatcher
            .run_on_consumer(&consumer, Some(&filter), shutdown_rx)
            .await
    });
    Sharing {
        shutdown,
        run,
        _claims: claims,
    }
}

/// The permit a place was admitted with, or a failure naming the place.
async fn admitted(place: Place) -> RunPermit {
    assert!(place.is_admitted(), "expected a free permit, got {place:?}");
    place.permit().await.expect("an admitted place starts")
}

/// **The order after a restart.** At a worker cap of 1, two runs startup
/// recovery found are queued before the dispatcher starts: the first
/// takes the permit, the second its place behind it. The trigger the
/// dispatcher then pulls does not start until both have given the permit
/// back.
///
/// The second run's task is first polled only after the trigger is
/// already waiting — as late as a spawned task can be. It still goes
/// first, because its place was taken when it was queued, not when its
/// task first ran; that is what makes the order hold by construction.
#[tokio::test]
async fn runs_queued_before_the_dispatcher_starts_go_before_its_first_trigger() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("after-recovery");
    let (_agents, registry) = registry_with(&agent);
    let worker = CappedWorker::new();
    let admission = RunAdmission::new(worker.clone(), 1);

    let first = admitted(
        admission
            .workers()
            .queue(Waiter::Recovered(Uuid::now_v7()))
            .await,
    )
    .await;
    let second = admission
        .workers()
        .queue(Waiter::Recovered(Uuid::now_v7()))
        .await;
    assert!(second.is_queued(), "the second waits for the only permit");

    let dispatcher = dispatcher_sharing(&bus, &agent, registry, worker.clone(), &admission).await;
    publish_triggers(&bus, &agent, 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        worker.started(),
        0,
        "the trigger was pulled and must wait: a recovered run holds the only worker permit"
    );

    let (second_running, second_has_permit) = oneshot::channel();
    let (end_second, second_ends) = oneshot::channel::<()>();
    let second = tokio::spawn(async move {
        let permit = second.permit().await.expect("nothing is stopping");
        let _ = second_running.send(());
        let _ = second_ends.await;
        drop(permit);
    });
    drop(first);
    tokio::time::timeout(Duration::from_secs(5), second_has_permit)
        .await
        .expect(
            "the second recovered run takes the permit the first gave back — a timeout here \
             is the trigger having jumped it",
        )
        .expect("the second run's task is alive");
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        worker.started(),
        0,
        "the trigger still waits, behind both recovered runs"
    );

    end_second
        .send(())
        .expect("the second run is waiting to end");
    second.await.expect("the second run's task joins");
    worker.wait_for_starts(1, Duration::from_secs(5)).await;
    worker.let_finish(10);
    stop(dispatcher.shutdown, dispatcher.run).await;
}

/// **A drain ends a queued wait.** The run ahead keeps its permit — a
/// drain suspends runs, it does not take permits from them — and the
/// queued resume lets go within a keepalive tick rather than waiting for
/// a permit that may never come. Nothing queues after it.
#[tokio::test]
async fn a_queued_resume_lets_go_on_a_drain() {
    let worker = CappedWorker::new();
    let admission = RunAdmission::new(worker.clone(), 1);
    let workers = admission.workers();
    let running = admitted(workers.queue(Waiter::Recovered(Uuid::now_v7())).await).await;
    let queued = workers.queue(Waiter::Recovered(Uuid::now_v7())).await;
    assert!(queued.is_queued());
    let queued = tokio::spawn(queued.permit());

    worker
        .request_drain(DrainRequest::new(DrainReason::Deploy))
        .await;
    let permit = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("a queued wait lets go within a keepalive tick of a drain")
        .expect("the wait's task joins");
    assert!(permit.is_none(), "a drained daemon starts nothing");
    assert!(
        workers
            .queue(Waiter::Operator(Uuid::now_v7()))
            .await
            .is_stopping(),
        "nothing queues once the daemon is draining"
    );
    drop(running);
}

/// **A permit the drain frees starts nothing.** Suspending runs give their
/// permits back, and the head of the queue is woken by exactly that —
/// before its own keepalive tick has seen the drain. The permit must not
/// start it, and must not be kept.
#[tokio::test]
async fn a_permit_the_drain_frees_starts_no_queued_resume() {
    let worker = CappedWorker::new();
    let admission = RunAdmission::new(worker.clone(), 1);
    let workers = Arc::clone(admission.workers());
    let running = admitted(workers.queue(Waiter::Recovered(Uuid::now_v7())).await).await;
    let queued = workers.queue(Waiter::Operator(Uuid::now_v7())).await;
    let queued = tokio::spawn(queued.permit());
    // Let the task take over the wait, so the grant wakes it.
    tokio::task::yield_now().await;

    worker
        .request_drain(DrainRequest::new(DrainReason::Deploy))
        .await;
    drop(running);
    let permit = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("the wait ends")
        .expect("the wait's task joins");
    assert!(
        permit.is_none(),
        "a queued resume started on the permit the drain itself freed"
    );
    assert_eq!(workers.in_flight(), 0, "and the permit was not kept");
}

/// **A shutdown ends a queued wait.** The dispatcher's loop seeing its
/// shutdown signal is what lets every wait go — its own and the daemon's
/// queued resumes alike, because they wait on the same permits.
#[tokio::test]
async fn a_queued_resume_lets_go_when_the_dispatcher_shuts_down() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("shutdown-queued");
    let (_agents, registry) = registry_with(&agent);
    let worker = CappedWorker::new();
    let admission = RunAdmission::new(worker.clone(), 1);
    let workers = admission.workers();
    let running = admitted(workers.queue(Waiter::Recovered(Uuid::now_v7())).await).await;
    let queued = workers.queue(Waiter::Recovered(Uuid::now_v7())).await;
    assert!(queued.is_queued());
    let queued = tokio::spawn(queued.permit());

    let dispatcher = dispatcher_sharing(&bus, &agent, registry, worker.clone(), &admission).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop(dispatcher.shutdown, dispatcher.run).await;

    let permit = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("a queued wait lets go within a keepalive tick of the shutdown")
        .expect("the wait's task joins");
    assert!(permit.is_none(), "a stopped daemon starts nothing");
    assert!(
        workers
            .queue(Waiter::Operator(Uuid::now_v7()))
            .await
            .is_stopping(),
        "nothing queues once the dispatcher has stopped"
    );
    drop(running);
}

/// **A drop and a start cannot interleave.** A drop holds the queued
/// resume while it publishes; a permit that comes meanwhile does not start
/// the run, and once the drop has gone through the run never starts and
/// the permit goes straight back.
#[tokio::test]
async fn a_held_queued_resume_does_not_start_and_a_drop_ends_it() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let running = admitted(workers.queue(Waiter::Recovered(Uuid::now_v7())).await).await;
    let id = Uuid::now_v7();
    let queued = workers.queue(Waiter::Operator(id)).await;
    let queued = tokio::spawn(queued.permit());

    let hold = workers
        .hold_for_drop(&id.to_string())
        .await
        .expect("a queued resume is there to hold");
    assert!(!hold.is_starting());
    drop(running);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !queued.is_finished(),
        "the permit came while the drop held the run: it must not start yet"
    );
    hold.dropped();
    let permit = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("the run's wait ends once the drop has gone through")
        .expect("the wait's task joins");
    assert!(permit.is_none(), "a dropped run never starts");
    assert_eq!(workers.in_flight(), 0, "and its permit went straight back");
    assert!(
        workers.hold_for_drop(&id.to_string()).await.is_none(),
        "nor is it listed any more"
    );
}

/// **A drop that fails leaves the run as it was.** Released without
/// `dropped`, the hold starts the run on the permit it was waiting for.
#[tokio::test]
async fn a_queued_resume_starts_when_its_drop_fails() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let running = admitted(workers.queue(Waiter::Recovered(Uuid::now_v7())).await).await;
    let id = Uuid::now_v7();
    let queued = tokio::spawn(workers.queue(Waiter::Recovered(id)).await.permit());

    let hold = workers.hold_for_drop(&id.to_string()).await.expect("held");
    drop(running);
    drop(hold);
    let permit = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("the run starts")
        .expect("the wait's task joins");
    assert!(permit.is_some(), "a failed drop does not stop the run");
    let hold = workers.hold_for_drop(&id.to_string()).await.expect("held");
    assert!(
        hold.is_starting(),
        "a started run is left to the runner's drop path"
    );
}

/// **One invocation is listed once.** A second resume of an invocation
/// already queued, or already running, is refused rather than queued
/// beside it — a drop would hold only one of the two — and once the first
/// has ended the invocation can be listed again.
#[tokio::test]
async fn a_second_listing_of_one_invocation_is_refused_until_the_first_ends() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let id = Uuid::now_v7();
    let first = workers.queue(Waiter::Operator(id)).await;
    assert!(first.is_admitted());

    let second = workers.queue(Waiter::Operator(id)).await;
    assert!(second.is_already_listed(), "while it is queued: {second:?}");
    assert!(second.permit().await.is_none(), "and nothing starts");
    let hold = workers
        .hold_for_drop(&id.to_string())
        .await
        .expect("the first listing is untouched");
    assert!(!hold.is_starting());
    drop(hold);

    let running = first.permit().await.expect("the first starts");
    assert!(
        workers
            .queue(Waiter::Operator(id))
            .await
            .is_already_listed(),
        "nor while it runs"
    );
    drop(running);
    assert!(
        workers.queue(Waiter::Operator(id)).await.is_admitted(),
        "once it has ended, the invocation can be listed again"
    );
}

/// **A run the drop finds waiting leaves the queue within a tick**, while
/// the run ahead of it still holds the only permit: it does not wait for a
/// permit only to give it straight back.
#[tokio::test]
async fn a_dropped_resume_leaves_the_queue_within_a_tick() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let running = admitted(workers.queue(Waiter::Recovered(Uuid::now_v7())).await).await;
    let id = Uuid::now_v7();
    let queued = tokio::spawn(workers.queue(Waiter::Recovered(id)).await.permit());

    workers
        .hold_for_drop(&id.to_string())
        .await
        .expect("held")
        .dropped();
    let permit = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .expect("a dropped run leaves the queue without waiting for a permit")
        .expect("the wait's task joins");
    assert!(permit.is_none());
    drop(running);
}

/// **A drop and a deferred run's resume cannot interleave either** (issue
/// 1070). A run deferred for a rate limit is listed from the moment it is
/// put down. A drop holding it as its delay runs out does not wake it, as
/// a drop still in progress is not one yet, so the resume is due and its
/// permit comes; it does not start while the drop holds it, and once the
/// drop has gone through it never starts.
#[tokio::test]
async fn a_deferral_held_by_a_drop_as_its_delay_runs_out_never_starts() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let id = Uuid::now_v7();
    let deferral = workers
        .list_deferred(id)
        .expect("listed when it is put down");

    let hold = workers
        .hold_for_drop(&id.to_string())
        .await
        .expect("a deferred run is there to hold");
    assert!(!hold.is_starting());
    assert!(
        deferral.sleep(Duration::from_millis(50)).await,
        "the drop has not gone through: the delay runs out"
    );
    let resume = tokio::spawn(deferral.permit());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !resume.is_finished(),
        "its permit is free, but the drop holds it: it must not start yet"
    );
    hold.dropped();
    let permit = tokio::time::timeout(Duration::from_secs(2), resume)
        .await
        .expect("the resume's wait ends once the drop has gone through")
        .expect("the wait's task joins");
    assert!(permit.is_none(), "a dropped run never starts");
    assert_eq!(workers.in_flight(), 0, "and its permit went straight back");
    assert!(
        workers.hold_for_drop(&id.to_string()).await.is_none(),
        "nor is it listed any more"
    );
}

/// **The other order: the resume starts first.** Its permit came and it
/// started before a drop could hold it. The drop's hold says so, and the
/// run is the runner's to stop: a plain drop of it is refused, and `--live`
/// halts it. A drop cannot un-start it, and while it runs it cannot be put
/// down a second time beside itself.
#[tokio::test]
async fn a_deferral_whose_resume_started_first_is_left_to_the_runner() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let id = Uuid::now_v7();
    let deferral = workers.list_deferred(id).expect("listed");
    assert!(deferral.sleep(Duration::from_millis(10)).await);
    let running = deferral
        .permit()
        .await
        .expect("nothing holds it: it starts");

    let hold = workers
        .hold_for_drop(&id.to_string())
        .await
        .expect("listed while it runs");
    assert!(
        hold.is_starting(),
        "a started run is left to the runner's drop path"
    );
    hold.dropped();
    assert!(
        workers
            .hold_for_drop(&id.to_string())
            .await
            .expect("still listed")
            .is_starting(),
        "a drop cannot un-start it"
    );
    assert!(
        matches!(
            workers.list_deferred(id),
            Err(super::NotListed::AlreadyListed)
        ),
        "nor can it be put down again while it runs"
    );
    drop(running);
    assert!(
        workers.list_deferred(id).is_ok(),
        "once its run has ended, it can be put down again"
    );
}

/// **A drop wakes a sleeping deferral at once**, however long its delay:
/// the sleep ends saying the resume is not due, and the resume never
/// starts.
#[tokio::test]
async fn a_dropped_deferral_wakes_from_its_sleep_at_once() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let id = Uuid::now_v7();
    let deferral = workers.list_deferred(id).expect("listed");
    let sleeping = tokio::spawn(async move {
        let due = deferral.sleep(Duration::from_secs(3_600)).await;
        (due, deferral)
    });

    workers
        .hold_for_drop(&id.to_string())
        .await
        .expect("held")
        .dropped();
    let (due, deferral) = tokio::time::timeout(Duration::from_secs(2), sleeping)
        .await
        .expect("the drop wakes the sleep")
        .expect("the sleep's task joins");
    assert!(!due, "a dropped deferral is not due");
    assert!(
        deferral.permit().await.is_none(),
        "and its resume never starts"
    );
}

/// A run that puts itself down on its first start, under the id the
/// dispatcher named, for an hour; a resume of it is counted.
struct DefersOnStart {
    named: std::sync::Mutex<Option<Uuid>>,
    resumed: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Worker for DefersOnStart {
    async fn run_invocation(
        &self,
        _agent: &crate::agent::Agent,
        _llm: &dyn crate::llm::LlmClient,
        _trigger: crate::trigger::Trigger,
        _delivery_attempt: Option<u32>,
        mut durable_start: crate::worker::DurableStart,
    ) -> Result<crate::worker::InvocationOutcome, crate::worker::ExecutorError> {
        let invocation_id = durable_start
            .invocation_id()
            .expect("the dispatcher names the run it starts");
        *self.named.lock().unwrap() = Some(invocation_id);
        durable_start.fire();
        Ok(crate::worker::InvocationOutcome::Deferred {
            invocation_id,
            resume_after: Duration::from_secs(3_600),
        })
    }

    async fn resume_invocation(
        &self,
        _agent: &crate::agent::Agent,
        _llm: &dyn crate::llm::LlmClient,
        invocation_id: Uuid,
    ) -> Result<crate::worker::InvocationOutcome, crate::worker::ExecutorError> {
        self.resumed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(crate::worker::ExecutorError::WorkerStore(format!(
            "{invocation_id} is not to be resumed here"
        )))
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> crate::worker::DrainState {
        crate::worker::DrainState::Running
    }
}

/// **A trigger's deferral is listed in the permits the daemon shares**
/// (issue 1070): the ones the operator surface's drop holds, so `fq
/// invocation drop` reaches a run a trigger started and a rate limit put
/// down. Held and dropped there, it gives its agent's slot back at once,
/// and it is never resumed.
#[tokio::test]
async fn a_triggers_deferral_is_listed_in_the_permits_the_daemon_shares() {
    let server = crate::test_support::nats::test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent = unique_agent_id("trigger-deferral");
    let (_agents, registry) = registry_with(&agent);
    let worker = Arc::new(DefersOnStart {
        named: std::sync::Mutex::new(None),
        resumed: std::sync::atomic::AtomicUsize::new(0),
    });
    let admission = RunAdmission::new(worker.clone(), 1);
    let dispatcher = dispatcher_sharing(&bus, &agent, registry, worker.clone(), &admission).await;
    publish_triggers(&bus, &agent, 1).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let hold = loop {
        let named = *worker.named.lock().unwrap();
        if let Some(id) = named
            && let Some(hold) = admission.workers().hold_for_drop(&id.to_string()).await
        {
            break hold;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the trigger's run is put down, and its deferral listed in the shared permits"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(!hold.is_starting(), "asleep, not starting");
    let agent_id = crate::agent::AgentId::new(&agent).unwrap();
    assert_eq!(
        admission.agents().in_flight(&agent_id),
        1,
        "the sleeping run holds its agent's slot"
    );
    hold.dropped();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while admission.agents().in_flight(&agent_id) != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the dropped run gives its slot back at once"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        worker.resumed.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "and is never resumed"
    );
    stop(dispatcher.shutdown, dispatcher.run).await;
}

/// **A drop that held nothing is remembered** (issue 1070): a drop that
/// went through in the instant before its run was put down for a rate
/// limit found nothing listed. The deferral that follows is refused, so
/// the run is never put down; a run of another invocation is not touched.
#[tokio::test]
async fn a_deferral_of_a_run_a_drop_went_through_for_is_refused() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let id = Uuid::now_v7();
    workers.note_dropped(&id.to_string());
    assert!(
        matches!(workers.list_deferred(id), Err(super::NotListed::Dropped)),
        "the dropped run is not put down"
    );
    assert!(
        workers.list_deferred(Uuid::now_v7()).is_ok(),
        "another run is"
    );
    workers.note_dropped("not an id");
}

/// **A drop noted while its run is listed drops it**, as a held drop
/// does: a sleeping deferral is woken to end, and its resume never starts.
#[tokio::test]
async fn a_drop_noted_while_its_run_sleeps_wakes_it_and_it_never_starts() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let id = Uuid::now_v7();
    let deferral = workers.list_deferred(id).expect("listed");
    let sleeping = tokio::spawn(async move {
        let due = deferral.sleep(Duration::from_secs(3_600)).await;
        (due, deferral)
    });
    workers.note_dropped(&id.to_string());
    let (due, deferral) = tokio::time::timeout(Duration::from_secs(2), sleeping)
        .await
        .expect("the noted drop wakes the sleep")
        .expect("the sleep's task joins");
    assert!(!due, "a dropped deferral is not due");
    assert!(deferral.permit().await.is_none(), "and never starts");
}

/// **A drop is remembered for an hour, and then forgotten**: the memory
/// holds at most the last hour's drops. Past it, a deferral of the
/// invocation is listed again.
#[tokio::test(start_paused = true)]
async fn a_drop_is_remembered_for_an_hour_then_forgotten() {
    let admission = RunAdmission::new(CappedWorker::new(), 1);
    let workers = Arc::clone(admission.workers());
    let id = Uuid::now_v7();
    workers.note_dropped(&id.to_string());
    tokio::time::advance(super::DROPS_REMEMBERED_FOR - Duration::from_secs(1)).await;
    assert!(
        matches!(workers.list_deferred(id), Err(super::NotListed::Dropped)),
        "still remembered just inside the hour"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        workers.list_deferred(id).is_ok(),
        "forgotten once the hour is out"
    );
}
