//! An operator's drop of a run deferred for a rate limit (issue 1070).
//!
//! A run whose model is rate-limited past what the retry layer waits is
//! put down: its WAL row stays in flight, it keeps the agent slot it was
//! admitted under, and the deferral queue hands it back for a resume once
//! its delay has run, to wait for a worker permit like any run. The runner
//! is not driving it meanwhile, so a plain `fq invocation drop` takes it,
//! as it takes a resume queued for a permit: the drop is published, and
//! the worker's row is marked only once the coordination consumer folds
//! it.
//!
//! The run here is a recovered one, resumed by recovery's own task, which
//! the model's first call rate-limits; a real dispatcher over the real
//! runner drains the deferral queue. No coordination consumer runs until a
//! test asks for the fold, so before then the drop's mark is never on the
//! worker's row: the consumer lagging, held as a state rather than raced.
//! The race the other way, a resume that has started before the drop, is
//! the last test.

use super::*;
use fq_edge::wire::WireError;
use fq_runtime::events::HostNoticePayload;
use fq_runtime::llm::{ModelThrottle, ThrottleBounds, ThrottleConfig};
use fq_runtime::watermark::Watermark;
use fq_runtime::worker::DeferralQueue;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use tokio::sync::Semaphore;

/// How long the runner defers the run for.
const DELAY: Duration = Duration::from_secs(2);

/// How long the run is deferred for in the test that drops it asleep: long
/// enough that the drop returns well inside it on a machine busy enough
/// for the drop's own SQLite commit and publish to take a second and a
/// half, which a review measured while the workspace compiled.
const ASLEEP: Duration = Duration::from_secs(6);

/// Long enough for a run the dispatcher has started to reach its model
/// call, and for a dropped run waiting for a worker permit to see the drop
/// (within one 250ms tick).
const SETTLE: Duration = Duration::from_millis(1_500);

/// Ceiling on every wait that ends when something happens. Reaching it is
/// a failure, never a branch.
const PATIENCE: Duration = Duration::from_secs(30);

/// The model the recovered run asks. Its first call is rate-limited with
/// no `Retry-After`; there is no retry layer in front of the runner here,
/// so that is a rate limit the retry layer gave up on, and the runner
/// defers the run. Every later call ends the run with `report_outcome`,
/// after the test lets it return when the model is held.
struct RateLimitedFirst {
    /// The first call is held until the test releases it, and then
    /// rate-limited.
    first_held: bool,
    held: bool,
    calls: AtomicUsize,
    entered: Semaphore,
    release: Semaphore,
}

impl RateLimitedFirst {
    fn new(held: bool) -> Arc<Self> {
        Arc::new(Self {
            first_held: false,
            held,
            calls: AtomicUsize::new(0),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    /// A model whose first call is held until the test releases it, and
    /// comes back rate-limited then; every later call answers at once.
    fn held_first() -> Arc<Self> {
        Arc::new(Self {
            first_held: true,
            held: false,
            calls: AtomicUsize::new(0),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    /// Wait inside a call until the test releases it.
    async fn hold(&self) {
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("release semaphore open")
            .forget();
    }

    /// The calls made after the one the rate limit refused.
    fn calls_after_the_deferral(&self) -> usize {
        self.calls.load(SeqCst).saturating_sub(1)
    }

    /// Wait until a resume is inside its model call.
    async fn entered(&self) {
        tokio::time::timeout(DELAY + PATIENCE, self.entered.acquire())
            .await
            .expect("the resume reached its model call")
            .expect("entry semaphore open")
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

#[async_trait::async_trait]
impl LlmClient for RateLimitedFirst {
    async fn chat(
        &self,
        _request: fq_runtime::ChatRequest,
    ) -> Result<ChatResponse, fq_runtime::LlmError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            if self.first_held {
                self.hold().await;
            }
            return Err(fq_runtime::LlmError::RateLimited {
                model: "claude-haiku".to_string(),
                retry_after: None,
            });
        }
        if self.held {
            self.hold().await;
        }
        Ok(the_invocation_is_done())
    }
}

/// Whether the resumed run's model call is held until the test releases
/// it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Resumed {
    Answered,
    Held,
}

/// Give `start` a runner that defers a rate-limited run for `delay`: the
/// throttle's escalating default, when the provider names no wait.
fn defer_for(start: &mut NextStart, delay: Duration) {
    let throttle = ModelThrottle::new(
        ThrottleConfig {
            enabled: false,
            default_pause_ms: delay.as_millis() as u64,
            ..ThrottleConfig::default()
        },
        ThrottleBounds {
            ceiling: 1,
            max_pause: delay,
        },
    );
    start.runner = throttled_runner_over(
        Arc::new(start.bus.clone()),
        start.worker_store.clone(),
        start.worker_id.clone(),
        Arc::new(throttle),
    );
}

/// The next start over one recovered run whose resume a rate limit
/// deferred for `delay`, with the dispatcher that drains the deferral
/// queue, and the operator surface.
struct DeferredRun {
    start: NextStart,
    admission: RunAdmission,
    surface: fq_edge::EdgeRegistry,
    model: Arc<RateLimitedFirst>,
    /// Every event published for [`CAPPED_AGENT`] since before the resume.
    events: std::pin::Pin<
        Box<
            dyn futures::Stream<Item = Result<fq_runtime::events::Event, fq_runtime::bus::BusError>>
                + Send,
        >,
    >,
    stop_dispatching: oneshot::Sender<()>,
    dispatching: tokio::task::JoinHandle<Result<(), DispatcherError>>,
    /// The queue a deferral is put down on, which the dispatcher drains.
    deferrals: DeferralQueue,
    /// How long the run was put down for.
    delay: Duration,
    /// An instant by which the run had been put down: its delay runs out
    /// no later than `delay` after it.
    put_down_by: Instant,
}

impl DeferredRun {
    /// Returns once the run sleeps on its deferral, holding its agent's
    /// slot.
    async fn new(resumed: Resumed, delay: Duration) -> Self {
        let mut run = Self::arena(RateLimitedFirst::new(resumed == Resumed::Held), delay).await;
        for task in run.resume_by_recovery().await {
            tokio::time::timeout(PATIENCE, task)
                .await
                .expect("the resume task ends once the run is deferred")
                .expect("its task joins");
        }
        run.put_down_by = Instant::now();
        assert_eq!(
            run.model.calls.load(SeqCst),
            1,
            "the resume asked the model once, and was told to wait"
        );
        assert_eq!(
            run.admission.agents().in_flight(&capped_agent()),
            1,
            "a deferred run still holds its agent's slot while it sleeps"
        );
        run
    }

    /// Everything but the resume: the next start's stores and runner,
    /// deferring for `delay`, the operator surface, and the dispatcher
    /// draining the deferral queue.
    async fn arena(model: Arc<RateLimitedFirst>, delay: Duration) -> Self {
        let mut start = NextStart::over_one_run_in_flight().await;
        defer_for(&mut start, delay);
        let admission = RunAdmission::new(start.runner.clone(), 1);
        let surface = start.operator_surface(&admission).await;
        let llm: Arc<dyn LlmClient> = model.clone();
        let events = start
            .bus
            .subscribe(format!("fq.agent.{CAPPED_AGENT}.>"))
            .await
            .expect("subscribe");

        let (deferrals, due) = DeferralQueue::new();
        let dispatcher = TriggerDispatcher::new(
            start.bus.clone(),
            shared_registry(capped_agent_registry(start.scratch.path())),
            start.runner.clone(),
            llm.clone(),
            1,
        )
        .with_caps_and_claims(
            admission.clone(),
            start.claims.take().expect("one dispatcher per start"),
            start.worker_id.as_str(),
        )
        .with_deferrals(deferrals.clone(), due);
        let (stop_dispatching, stopped) = oneshot::channel();
        let dispatching = tokio::spawn(dispatcher.run(stopped));
        Self {
            start,
            admission,
            surface,
            model,
            events,
            stop_dispatching,
            dispatching,
            deferrals,
            delay,
            put_down_by: Instant::now(),
        }
    }

    /// Hand the recovered run to recovery's own resume task, as `assemble`
    /// does, and return the task.
    async fn resume_by_recovery(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        let llm: Arc<dyn LlmClient> = self.model.clone();
        spawn_resume_tasks(
            std::mem::take(&mut self.start.recoverable),
            &capped_agent_registry(self.start.scratch.path()),
            &self.start.runner,
            &llm,
            &self.start.bus,
            &self.start.worker_store,
            &self.deferrals,
            &self.admission,
        )
        .await
        .handles
    }

    /// `fq invocation drop --live`, through the daemon's operator surface.
    async fn drop_it_live(&self) -> Result<serde_json::Value, WireError> {
        invoke(
            &self.surface,
            fq_ops::Invocation::Drop,
            serde_json::json!({
                "invocation_id": self.start.invocation.to_string(),
                "reason": "test",
                "live": true,
            }),
        )
        .await
    }

    /// `fq invocation drop`, plain, through the daemon's operator surface.
    async fn drop_it(&self) -> Result<serde_json::Value, WireError> {
        invoke(
            &self.surface,
            fq_ops::Invocation::Drop,
            serde_json::json!({ "invocation_id": self.start.invocation.to_string(), "reason": "test" }),
        )
        .await
    }

    /// How long after `since` the run's agent slot was given back, waiting
    /// until `since + within` at most: `None` if it was not by then.
    async fn slot_given_back(&self, since: Instant, within: Duration) -> Option<Duration> {
        loop {
            if self.admission.agents().in_flight(&capped_agent()) == 0 {
                return Some(since.elapsed());
            }
            if since.elapsed() >= within {
                return None;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Publish a barrier after everything published so far, and return
    /// what was published for the run before it. One connection carries
    /// every publish, so the barrier arrives last.
    async fn published(&mut self) -> Vec<fq_runtime::events::Event> {
        let barrier = self.barrier().await.0;
        let mut published = Vec::new();
        loop {
            let event = tokio::time::timeout(PATIENCE, self.events.next())
                .await
                .expect("the barrier arrives")
                .expect("the subscription is open")
                .expect("the event deserialises");
            if event.envelope.invocation_id == barrier {
                return published;
            }
            if event.envelope.invocation_id == self.start.invocation {
                published.push(event);
            }
        }
    }

    async fn barrier(&self) -> (Uuid, u64) {
        let barrier = Uuid::now_v7();
        let seq = self
            .start
            .bus
            .publish(&fq_runtime::events::Event::new(
                capped_agent(),
                barrier,
                EventPayload::HostNotice(HostNoticePayload {
                    kind: "test_barrier".to_string(),
                    body: String::new(),
                }),
            ))
            .await
            .expect("publish the barrier");
        (barrier, seq)
    }

    /// Let the coordination consumer fold everything published so far,
    /// the drop included, and stop it.
    async fn fold(&self) {
        let (mark, coordination): (_, Watermark) = fq_runtime::watermark::channel();
        let consumer = fq_runtime::CoordinationConsumer::new(
            self.start.bus.clone(),
            self.start.cp_store.clone(),
        )
        .with_worker_store(self.start.worker_store.clone())
        .with_self_worker_id(self.start.worker_id.as_str().to_string())
        .with_watermark(mark);
        let (stop, stopped) = oneshot::channel();
        let folding = tokio::spawn(async move { consumer.run(stopped).await });
        let seq = self.barrier().await.1;
        coordination
            .wait_for(seq, PATIENCE)
            .await
            .expect("the coordination consumer applies everything up to the barrier");
        let _ = stop.send(());
        tokio::time::timeout(PATIENCE, folding)
            .await
            .expect("the coordination consumer stops")
            .expect("its task joins")
            .expect("it exits cleanly");
    }

    /// The worker's row, which no archive-ack consumer here deletes.
    async fn row(&self) -> fq_runtime::worker::InvocationStateRow {
        self.start
            .worker_store
            .get_invocation_state(&self.start.invocation.to_string())
            .await
            .unwrap()
            .expect("the worker's row is kept")
    }

    async fn stop(self) {
        let _ = self.stop_dispatching.send(());
        tokio::time::timeout(PATIENCE, self.dispatching)
            .await
            .expect("the dispatcher stops")
            .expect("its task joins")
            .expect("it exits cleanly");
    }
}

/// The run's own endings among `events`: a `completed` or a `failed`,
/// which the drop never publishes.
fn own_endings(events: &[fq_runtime::events::Event]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::Completed(_) => Some("completed"),
            EventPayload::Failed(_) => Some("failed"),
            _ => None,
        })
        .collect()
}

/// **A drop takes a deferred run while it sleeps.** The run was put down
/// for [`ASLEEP`] and nothing drives it, so a plain drop takes it. When the
/// delay runs out it is not resumed: it asks the model nothing more and
/// publishes no ending of its own, and its agent's slot is given back at
/// the drop rather than at the end of the delay. Once the drop is folded,
/// the worker's row carries the drop's mark and no ending of the run's.
///
/// Every time here is measured against when the delay runs out, not
/// against when the drop was asked for, so how long the drop call itself
/// takes on a busy machine moves nothing.
#[tokio::test]
async fn a_deferred_run_dropped_while_it_sleeps_is_never_resumed() {
    let mut run = DeferredRun::new(Resumed::Answered, ASLEEP).await;
    let delay_over = run.put_down_by + run.delay;

    let dropped = run.drop_it().await;
    let returned_at = Instant::now();
    assert!(
        dropped.is_ok(),
        "a deferred run is not being driven, so a plain drop takes it: {dropped:?}"
    );
    assert!(
        returned_at < delay_over,
        "the test's premise: the drop returned while the run still slept, {:?} before its delay \
         ran out",
        delay_over.saturating_duration_since(returned_at)
    );
    let given_back = run.slot_given_back(returned_at, PATIENCE).await;
    // Past the delay, with time for a resume to have asked the model.
    tokio::time::sleep_until((delay_over + SETTLE).into()).await;

    let calls = run.model.calls_after_the_deferral();
    assert_eq!(
        calls, 0,
        "a dropped deferred run must make no model call when its delay runs out, but it made \
         {calls}: the drop does not reach a run deferred for a rate limit, so its resume asks \
         the model again before the runner notices its row (one more call, the bound the first \
         change set; with the fold held here, it ran on to its end)"
    );
    let endings = own_endings(&run.published().await);
    assert!(
        endings.is_empty(),
        "nor does it publish an ending of its own: {endings:?}"
    );
    assert!(
        given_back.is_some_and(|after| returned_at + after < delay_over),
        "its agent's slot is given back at the drop, not at the end of the delay: given back \
         {given_back:?} after the drop returned, the delay running out {:?} after it",
        delay_over.saturating_duration_since(returned_at)
    );
    assert!(!run.start.runner.is_active(&run.start.invocation));

    run.fold().await;
    let row = run.row().await;
    assert!(row.terminal_at.is_some(), "the drop's fold marks the row");
    assert_eq!(row.phase, "failed");
    assert!(
        row.terminal_outcome.is_none(),
        "the row stores no ending of the run's own, as the stream carries none"
    );
    run.stop().await;
}

/// **A drop takes a deferred run whose delay has run out while it waits
/// for a worker permit.** The delay elapses first: the resume is due, and
/// waits behind the run holding the only permit. The drop then takes it as
/// it takes any queued resume. It leaves the queue within a tick, giving
/// its agent's slot back while the permit is still held, and when the
/// permit is freed it does not start.
#[tokio::test]
async fn a_deferred_run_dropped_while_its_resume_waits_for_a_permit_never_starts() {
    let mut run = DeferredRun::new(Resumed::Answered, DELAY).await;
    let running = hold_the_only_permit(&run.admission).await;
    tokio::time::sleep(DELAY + SETTLE).await;
    assert_eq!(
        run.model.calls_after_the_deferral(),
        0,
        "the resume is due and waits: the only permit is held"
    );

    let dropped = run.drop_it().await;
    assert!(
        dropped.is_ok(),
        "a resume waiting for a permit is not being driven, so a plain drop takes it: {dropped:?}"
    );
    // The test holds the only permit, so only the drop can give the slot
    // back: waited for with patience, since the drop call alone can take
    // a second and a half on a busy machine.
    let given_back = run.slot_given_back(Instant::now(), PATIENCE).await;
    drop(running);
    tokio::time::sleep(SETTLE).await;

    let calls = run.model.calls_after_the_deferral();
    assert_eq!(
        calls, 0,
        "a dropped deferred run must make no model call when the permit it waited for is \
         freed, but it made {calls}: the drop does not reach a deferred run's resume, so it \
         asks the model again before the runner notices its row (one more call, the bound the \
         first change set; with the fold held here, it ran on to its end)"
    );
    let endings = own_endings(&run.published().await);
    assert!(
        endings.is_empty(),
        "nor does it publish an ending of its own: {endings:?}"
    );
    assert!(
        given_back.is_some(),
        "its agent's slot is given back while the permit is still held: the dropped run \
         leaves the queue rather than waiting for a permit only to give it back"
    );

    run.fold().await;
    let row = run.row().await;
    assert_eq!(
        (row.phase.as_str(), row.terminal_outcome.is_none()),
        ("failed", true),
        "the drop's mark, and no ending of the run's own"
    );
    run.stop().await;
}

/// **The race the other way: the resume starts first.** The delay runs
/// out and the resume is inside its model call when the drop arrives: the
/// runner is driving the run, so a plain drop is refused, as for any run
/// being driven, and publishes nothing. The run's own ending stands, on
/// the stream and on the worker's row alike.
#[tokio::test]
async fn a_deferred_run_whose_resume_has_started_is_refused_a_plain_drop() {
    let mut run = DeferredRun::new(Resumed::Held, DELAY).await;
    run.model.entered().await;

    let refused = run.drop_it().await;
    let Err(WireError::InvalidInput { message, .. }) = &refused else {
        panic!("a plain drop of a run being driven is refused: {refused:?}");
    };
    assert!(
        message.contains("currently running; use --live"),
        "and the refusal says so: {message}"
    );
    run.model.release();
    await_in_flight(&run.admission, 0).await;

    let published = run.published().await;
    assert_eq!(own_endings(&published), ["completed"]);
    assert!(
        !published
            .iter()
            .any(|event| matches!(event.payload, EventPayload::InvocationOperatorRecovered(_))),
        "the refused drop published nothing"
    );
    run.fold().await;
    let row = run.row().await;
    assert_eq!(
        (row.phase.as_str(), row.terminal_outcome.is_some()),
        ("completed", true),
        "the worker's row stores the run's own ending, the one on the stream"
    );
    run.stop().await;
}

/// **An operator's resume that a rate limit puts down is listed for a drop
/// to reach.** `fq invocation resume` re-drives an ambiguous run, which
/// its model then tells to wait. The re-drive gives its worker permit
/// back before it is deferred, as recovery's resumes and the dispatcher's
/// do, so its deferral is listed in the permits the daemon shares with
/// the drop, still holding its agent's slot, and its resume comes due.
/// A re-drive still holding its own listing when it is put down would be
/// a second run of one invocation, and would not be put down at all.
#[tokio::test]
async fn an_operator_resume_a_rate_limit_puts_down_is_listed_for_a_drop() {
    let mut start = NextStart::over(Seed::Ambiguous).await;
    defer_for(&mut start, DELAY);
    let admission = RunAdmission::new(start.runner.clone(), 1);
    let model = RateLimitedFirst::new(false);
    let (deferrals, mut due) = DeferralQueue::new();
    let control = crate::resume::ResumeControl::new(
        start.bus.clone(),
        start.worker_store.clone(),
        start.cp_store.clone(),
        start.runner.clone(),
        shared_registry(capped_agent_registry(start.scratch.path())),
        model.clone(),
        deferrals,
        admission.clone(),
    );
    let id = start.invocation.to_string();
    let answer = crate::resume::handle_resume_request(
        &control,
        crate::resume::InvocationResumeRequest {
            invocation_id: id.clone(),
            reason: Some("test".to_string()),
        },
    )
    .await;
    assert!(answer.ok, "the resume is accepted: {}", answer.message);

    let deadline = Instant::now() + PATIENCE;
    loop {
        let put_down = model.calls.load(SeqCst) == 1
            && admission
                .workers()
                .hold_for_drop(&id)
                .await
                .is_some_and(|hold| !hold.is_starting());
        if put_down {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the re-drive was told to wait and is listed, waiting, for a drop to reach"
        );
        tokio::time::sleep(POLL).await;
    }
    assert_eq!(
        admission.agents().in_flight(&capped_agent()),
        1,
        "the deferral holds its agent's slot"
    );
    let resume = tokio::time::timeout(DELAY + PATIENCE, due.recv())
        .await
        .expect("its resume comes due once the delay has run")
        .expect("the queue holds a sender");
    assert_eq!(resume.invocation_id, start.invocation);
    drop(resume);
    assert_eq!(admission.agents().in_flight(&capped_agent()), 0);
}

/// **A drop in the instant a run is put down holds it too.** The runner
/// has stopped driving the run, which a rate limit deferred, and its
/// deferral is not listed yet: a plain drop in that instant finds nothing
/// to hold and is taken as idle work. The deferral that follows must not
/// put the run down: its agent's slot goes back at once, and when the
/// delay would have run out nothing resumes it.
///
/// The instant is held open by doing what recovery's resume task does, a
/// step at a time, with the drop between the run's return and its
/// deferral.
#[tokio::test]
async fn a_drop_in_the_instant_a_run_is_put_down_holds_it_too() {
    let mut run = DeferredRun::arena(RateLimitedFirst::new(false), ASLEEP).await;
    let registry = capped_agent_registry(run.start.scratch.path());
    let agent = registry
        .get_loaded(&capped_agent())
        .expect("the agent is loaded")
        .agent
        .clone();
    let slot = run
        .admission
        .agents()
        .enter(agent.id(), agent.max_concurrent());
    let llm: Arc<dyn LlmClient> = run.model.clone();
    let resumed = run
        .start
        .runner
        .resume(&agent, llm.as_ref(), run.start.invocation)
        .await;
    let Ok(fq_runtime::InvocationOutcome::Deferred {
        invocation_id,
        resume_after,
    }) = resumed
    else {
        panic!("the model's rate limit defers the run: {resumed:?}");
    };
    assert!(!run.start.runner.is_active(&invocation_id));

    let dropped = run.drop_it().await;
    assert!(
        dropped.is_ok(),
        "nothing drives or lists the run, so a plain drop takes it: {dropped:?}"
    );
    let put_down_at = Instant::now();
    run.deferrals.defer(
        run.admission.workers(),
        invocation_id,
        capped_agent(),
        resume_after,
        slot,
    );
    let delay_over = put_down_at + resume_after;
    let given_back = run.slot_given_back(put_down_at, PATIENCE).await;
    tokio::time::sleep_until((delay_over + SETTLE).into()).await;

    let calls = run.model.calls_after_the_deferral();
    assert_eq!(
        calls, 0,
        "a run dropped in the instant it was put down must make no model call when its delay \
         runs out, but it made {calls}: the drop found nothing to hold, the deferral was put \
         down anyway and the dispatcher resumed it"
    );
    let endings = own_endings(&run.published().await);
    assert!(endings.is_empty(), "no ending of its own: {endings:?}");
    assert!(
        given_back.is_some_and(|after| put_down_at + after < delay_over),
        "its agent's slot goes back at once, not at the end of the delay: given back \
         {given_back:?} after the deferral"
    );
    run.fold().await;
    let row = run.row().await;
    assert_eq!(
        (row.phase.as_str(), row.terminal_outcome.is_none()),
        ("failed", true),
        "the drop's mark, and no ending of the run's own"
    );
    run.stop().await;
}

/// **A live drop of a run its model then rate-limits holds it too.** The
/// run is inside its model call, so a plain drop is refused and a live one
/// arms the halt. The call then comes back rate-limited before the drop is
/// folded: the run puts itself down with no step boundary between, and
/// says so on the stream after the drop. The deferral must not put it down:
/// its agent's slot goes back at once, and nothing resumes it.
#[tokio::test]
async fn a_live_drop_of_a_run_its_model_then_rate_limits_holds_it_too() {
    let mut run = DeferredRun::arena(RateLimitedFirst::held_first(), ASLEEP).await;
    let resumes = run.resume_by_recovery().await;
    run.model.entered().await;

    let refused = run.drop_it().await;
    assert!(
        matches!(&refused, Err(WireError::InvalidInput { message, .. })
            if message.contains("currently running; use --live")),
        "a plain drop of a run being driven is refused: {refused:?}"
    );
    let dropped = run.drop_it_live().await;
    assert!(dropped.is_ok(), "a live drop halts it: {dropped:?}");
    run.model.release();
    for task in resumes {
        tokio::time::timeout(PATIENCE, task)
            .await
            .expect("the resume task ends once the run is put down")
            .expect("its task joins");
    }
    let put_down_at = Instant::now();
    let delay_over = put_down_at + ASLEEP;
    let given_back = run.slot_given_back(put_down_at, PATIENCE).await;
    tokio::time::sleep_until((delay_over + SETTLE).into()).await;

    let calls = run.model.calls_after_the_deferral();
    assert_eq!(
        calls, 0,
        "a run dropped live as its model rate-limited it must make no model call when its \
         delay runs out, but it made {calls}: the halt was never seen, the run was put down \
         after the drop, and the dispatcher resumed it"
    );
    let published = run.published().await;
    assert!(
        own_endings(&published).is_empty(),
        "no ending of its own: {:?}",
        own_endings(&published)
    );
    let at = |kind: fn(&EventPayload) -> bool| published.iter().position(|e| kind(&e.payload));
    let drop_at = at(|p| matches!(p, EventPayload::InvocationOperatorRecovered(_)));
    let deferral_at = at(|p| matches!(p, EventPayload::InvocationDeferred(_)));
    assert!(
        drop_at.is_some() && deferral_at > drop_at,
        "the run announces its deferral after the drop, with no step boundary to see the halt \
         at: drop at {drop_at:?}, deferral at {deferral_at:?}"
    );
    assert!(
        given_back.is_some_and(|after| put_down_at + after < delay_over),
        "its agent's slot goes back at once, not at the end of the delay: given back \
         {given_back:?} after it was put down"
    );
    run.fold().await;
    let row = run.row().await;
    assert_eq!(
        (row.phase.as_str(), row.terminal_outcome.is_none()),
        ("failed", true),
        "the drop's mark, and no ending of the run's own"
    );
    run.stop().await;
}

/// **A refused drop is not remembered.** A plain drop of a run inside its
/// model call is refused and publishes nothing. When the call then comes
/// back rate-limited, the run is put down and resumed when its delay runs
/// out, as any deferred run is. Only a drop that went through is
/// remembered: one remembered before it was known to have gone through
/// would leave a run that was refused a drop, and then rate-limited
/// within the hour, never put down or resumed.
#[tokio::test]
async fn a_refused_drop_is_not_remembered() {
    let mut run = DeferredRun::arena(RateLimitedFirst::held_first(), DELAY).await;
    let resumes = run.resume_by_recovery().await;
    run.model.entered().await;

    let refused = run.drop_it().await;
    assert!(
        matches!(&refused, Err(WireError::InvalidInput { message, .. })
            if message.contains("currently running; use --live")),
        "a plain drop of a run being driven is refused: {refused:?}"
    );
    run.model.release();
    for task in resumes {
        tokio::time::timeout(PATIENCE, task)
            .await
            .expect("the resume task ends once the run is put down")
            .expect("its task joins");
    }
    let deadline = Instant::now() + DELAY + PATIENCE;
    while run.model.calls.load(SeqCst) < 2 {
        assert!(
            Instant::now() < deadline,
            "the run, put down for the rate limit, is resumed when its delay runs out and asks \
             the model again: the refused drop held nothing over it"
        );
        tokio::time::sleep(POLL).await;
    }
    await_in_flight(&run.admission, 0).await;
    assert_eq!(own_endings(&run.published().await), ["completed"]);
    run.stop().await;
}
