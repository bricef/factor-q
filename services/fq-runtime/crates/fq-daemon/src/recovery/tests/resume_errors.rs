//! What startup recovery reports about a resume that returned an error
//! (issue 1005).
//!
//! `ReducerRunner::resume` returns an error for two different things. A
//! run that ended in an ordinary `failed` has published `failed` and
//! `invocation.archived` and marked its WAL row terminal: its record is
//! complete, and nothing about it needs an operator. That is a failure the
//! reducer decides (the iteration ceiling), which comes back as
//! `InvocationFailed`, and one that ends a step part-way (a model call the
//! provider refuses), which comes back as the `Llm` error itself. A resume
//! that could not be carried out leaves the row in flight, and only an
//! operator can say what happens to it next. Only that one is
//! `invocation.ambiguous`.
//!
//! Each test runs what the daemon runs around a resume: the real runner
//! driven by recovery's own resume task, the coordination consumer that
//! folds lifecycle events into the ownership row `fq doctor` and
//! `fq invocation list` read, and the worker's archive-ack consumer that
//! deletes the WAL row once the archive is acknowledged, all on one
//! private broker.

use super::*;
use fq_runtime::control_plane::OwnerStatus;
use fq_runtime::events::{AssistantPart, HostNoticePayload, LlmCallOrigin};
use fq_runtime::watermark::Watermark;

/// [`CAPPED_AGENT`] as these tests define it: one model turn and no tools.
/// A turn that does not end the run is its last, so the step after it is
/// the reducer ending the run in `failed` at the iteration ceiling.
fn one_turn_registry(dir: &std::path::Path) -> AgentRegistry {
    let dir = dir.join("one-turn");
    std::fs::create_dir_all(&dir).unwrap();
    let definition = dir.join(format!("{CAPPED_AGENT}.md"));
    std::fs::write(
        &definition,
        format!(
            "---\nname: {CAPPED_AGENT}\nmodel: claude-haiku\nbudget: 1.0\nmax_iterations: 1\n\
             ---\n\nTest agent."
        ),
    )
    .unwrap();
    let mut registry = AgentRegistry::new();
    registry.load_file(&definition);
    assert!(registry.errors().is_empty(), "{:?}", registry.errors());
    registry
}

/// How the model answers every call a resumed run makes.
#[derive(Clone, Copy, Debug)]
enum Reply {
    /// A turn of plain text, which does not end the run.
    Text,
    /// The provider refuses the request: the step fails part-way.
    Refused,
}

#[async_trait::async_trait]
impl LlmClient for Reply {
    async fn chat(
        &self,
        _request: fq_runtime::ChatRequest,
    ) -> Result<ChatResponse, fq_runtime::LlmError> {
        match self {
            Reply::Text => Ok(ChatResponse {
                parts: vec![AssistantPart::Text {
                    text: "still thinking it over".to_string(),
                }],
                stop_reason: fq_runtime::events::StopReason::EndTurn,
                usage: fq_runtime::events::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cache_write_5m_tokens: None,
                    cache_write_1h_tokens: None,
                    reasoning_tokens: None,
                },
                reported_cost_usd: None,
            }),
            Reply::Refused => Err(fq_runtime::LlmError::Rejected(
                "400 the provider refuses this request (test)".to_string(),
            )),
        }
    }
}

/// A daemon's next start over one invocation left in flight, with the
/// consumers that run beside recovery's resume task.
struct Recovering {
    start: NextStart,
    /// Every event published for [`CAPPED_AGENT`] since before the first
    /// resume.
    events: std::pin::Pin<
        Box<
            dyn futures::Stream<Item = Result<fq_runtime::events::Event, fq_runtime::bus::BusError>>
                + Send,
        >,
    >,
    /// How far the coordination consumer has applied the event stream.
    coordination: Watermark,
    stops: Vec<oneshot::Sender<()>>,
    consumers: Vec<tokio::task::JoinHandle<()>>,
}

impl Recovering {
    async fn new() -> Self {
        Self::over(Seed::Resumable(1)).await
    }

    async fn over(seed: Seed) -> Self {
        let start = NextStart::over(seed).await;
        let events = start
            .bus
            .subscribe(format!("fq.agent.{CAPPED_AGENT}.>"))
            .await
            .expect("subscribe");
        let (mark, coordination) = fq_runtime::watermark::channel();
        let coordination_consumer =
            fq_runtime::CoordinationConsumer::new(start.bus.clone(), start.cp_store.clone())
                .with_worker_store(start.worker_store.clone())
                .with_self_worker_id(start.worker_id.as_str().to_string())
                .with_watermark(mark);
        let archive_acks = fq_runtime::ArchiveAckConsumer::new(
            start.bus.clone(),
            start.worker_id.clone(),
            start.worker_store.clone(),
        );
        let (coordination_stop, coordination_stopped) = oneshot::channel();
        let (acks_stop, acks_stopped) = oneshot::channel();
        let consumers = vec![
            tokio::spawn(async move {
                coordination_consumer
                    .run(coordination_stopped)
                    .await
                    .expect("the coordination consumer exits cleanly");
            }),
            tokio::spawn(async move {
                archive_acks
                    .run(acks_stopped)
                    .await
                    .expect("the archive-ack consumer exits cleanly");
            }),
        ];
        tokio::time::sleep(Duration::from_millis(50)).await;
        Self {
            start,
            events,
            coordination,
            stops: vec![coordination_stop, acks_stop],
            consumers,
        }
    }

    fn invocation(&self) -> Uuid {
        self.start.invocation
    }

    /// Hand what recovery found to recovery's own resume task, with the
    /// model answering `reply`, and wait for the task to end. Returns the
    /// events published for the invocation meanwhile, once the
    /// coordination consumer has applied every one of them.
    async fn resume(&mut self, reply: Reply) -> Vec<fq_runtime::events::Event> {
        let admission = RunAdmission::new(self.start.runner.clone(), 1);
        let llm: Arc<dyn LlmClient> = Arc::new(reply);
        let tasks = spawn_resume_tasks(
            std::mem::take(&mut self.start.recoverable),
            &one_turn_registry(self.start.scratch.path()),
            &self.start.runner,
            &llm,
            &self.start.bus,
            &self.start.worker_store,
            &fq_runtime::worker::DeferralQueue::new().0,
            &admission,
        )
        .await;
        assert_eq!(tasks.handles.len(), 1, "one resume task");
        for task in tasks.handles {
            tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .expect("the resume task ends")
                .expect("its task joins");
        }
        self.settle().await
    }

    /// Ask for the invocation as `fq invocation resume` does, with the
    /// model answering `reply` if it is re-driven. Returns the answer and
    /// the admission any re-drive runs under.
    async fn ask_operator_resume(
        &self,
        reply: Reply,
    ) -> (crate::resume::InvocationResumeResponse, RunAdmission) {
        let admission = RunAdmission::new(self.start.runner.clone(), 1);
        let control = crate::resume::ResumeControl::new(
            self.start.bus.clone(),
            self.start.worker_store.clone(),
            self.start.cp_store.clone(),
            self.start.runner.clone(),
            shared_registry(capped_agent_registry(self.start.scratch.path())),
            Arc::new(reply),
            fq_runtime::worker::DeferralQueue::new().0,
            admission.clone(),
        );
        let answer = crate::resume::handle_resume_request(
            &control,
            crate::resume::InvocationResumeRequest {
                invocation_id: self.invocation().to_string(),
                reason: Some("test".to_string()),
            },
        )
        .await;
        (answer, admission)
    }

    /// Resume the invocation as `fq invocation resume` does, with the model
    /// answering `reply`, and wait for the re-drive to end. Returns what was
    /// published for the invocation meanwhile, as [`Self::resume`] does.
    async fn operator_resume(&mut self, reply: Reply) -> Vec<fq_runtime::events::Event> {
        let (answer, admission) = self.ask_operator_resume(reply).await;
        assert!(answer.ok, "the resume is accepted: {}", answer.message);
        // The re-drive runs detached; its agent's count is given back when
        // its task ends.
        await_in_flight(&admission, 0).await;
        self.settle().await
    }

    /// `fq invocation resume` for the invocation, which must be refused:
    /// returns the refusal's message.
    async fn operator_resume_refused(&self) -> String {
        let (answer, admission) = self.ask_operator_resume(Reply::Text).await;
        assert!(
            !answer.ok,
            "the resume is refused, not accepted: {}",
            answer.message
        );
        assert_eq!(
            admission.agents().in_flight(&capped_agent()),
            0,
            "and nothing is re-driven"
        );
        answer.message
    }

    /// Drop the invocation as `fq invocation drop` does, through the
    /// daemon's operator surface, and wait for the coordination consumer
    /// to apply what it published.
    async fn operator_drop(&mut self) {
        let admission = RunAdmission::new(self.start.runner.clone(), 1);
        let surface = self.start.operator_surface(&admission).await;
        let dropped = invoke(
            &surface,
            fq_ops::Invocation::Drop,
            serde_json::json!({ "invocation_id": self.invocation().to_string(), "reason": "test" }),
        )
        .await;
        assert!(dropped.is_ok(), "the drop is accepted: {dropped:?}");
        self.settle().await;
    }

    /// From here on, the runner publishes through `sink` in front of the
    /// bus. Recovery itself still publishes on the bus.
    fn publish_through(&mut self, sink: Arc<dyn fq_runtime::bus::EventSink>) {
        self.start.runner = runner_over(
            sink,
            self.start.worker_store.clone(),
            self.start.worker_id.clone(),
        );
    }

    /// Publish a barrier event after everything the resume published, wait
    /// for the coordination consumer to apply it, and return what came
    /// before it for the invocation. One connection carries every publish,
    /// so the barrier arrives last.
    async fn settle(&mut self) -> Vec<fq_runtime::events::Event> {
        let barrier = Uuid::now_v7();
        let seq = self
            .start
            .bus
            .publish(&fq_runtime::events::Event::new(
                AgentId::new(CAPPED_AGENT).unwrap(),
                barrier,
                EventPayload::HostNotice(HostNoticePayload {
                    kind: "test_barrier".to_string(),
                    body: String::new(),
                }),
            ))
            .await
            .expect("publish the barrier");
        self.coordination
            .wait_for(seq, Duration::from_secs(10))
            .await
            .expect("the coordination consumer applies everything up to the barrier");
        let invocation = self.invocation();
        let mut published = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), self.events.next())
                .await
                .expect("the barrier arrives within 10s")
                .expect("the subscription is open")
                .expect("the event deserialises");
            if event.envelope.invocation_id == barrier {
                return published;
            }
            if event.envelope.invocation_id == invocation {
                published.push(event);
            }
        }
    }

    /// The next start's recovery over the same stores: what it finds in
    /// flight, it hands the next [`Self::resume`].
    async fn restart(&mut self) {
        let recovered = self.next_start_recovers().await;
        assert_eq!(
            recovered,
            [self.invocation().to_string()],
            "the next start finds the invocation in flight and resumes it"
        );
    }

    /// The next start's recovery over the same stores. Returns the ids it
    /// would resume, and keeps them for the next [`Self::resume`].
    async fn next_start_recovers(&mut self) -> Vec<String> {
        let runtime_id = Uuid::now_v7();
        let recovered = classify_in_flight(
            &self.start.worker_store,
            &self.start.cp_store,
            &self.start.bus,
            runtime_id,
            &WorkerId::new(runtime_id.to_string()).unwrap(),
        )
        .await
        .expect("the next start's recovery");
        let ids = recovered
            .recoverable
            .iter()
            .map(|c| c.state.invocation_id.clone())
            .collect();
        self.start.recoverable = recovered.recoverable;
        ids
    }

    /// What the invocation's worker row says now.
    async fn row(&self) -> ResumedRow {
        ResumedRow::read(&self.start.worker_store, &self.invocation().to_string())
            .await
            .expect("read the worker row")
    }

    /// The invocation's ownership status, as `fq invocation list` and
    /// `fq doctor` read it.
    async fn owner(&self) -> Option<OwnerStatus> {
        self.start
            .cp_store
            .get_invocation_owner(&self.invocation().to_string())
            .await
            .expect("read the ownership row")
            .map(|owner| owner.status)
    }

    async fn stop(self) {
        for stop in self.stops {
            let _ = stop.send(());
        }
        for consumer in self.consumers {
            tokio::time::timeout(Duration::from_secs(10), consumer)
                .await
                .expect("the consumer stops")
                .expect("its task joins");
        }
    }
}

/// The bus, refusing every `invocation.archived` as the broker refuses one
/// larger than its `max_payload`, and passing everything else through.
struct RefusesArchives(EventBus);

#[async_trait::async_trait]
impl fq_runtime::bus::EventSink for RefusesArchives {
    async fn publish(
        &self,
        event: &fq_runtime::events::Event,
    ) -> Result<u64, fq_runtime::bus::BusError> {
        if matches!(event.payload, EventPayload::InvocationArchived(_)) {
            return Err(fq_runtime::bus::BusError::Publish(
                "the broker refuses this archive (test)".to_string(),
            ));
        }
        self.0.publish(event).await
    }
}

/// `value` as it is written on the wire: a snake_case name.
fn wire(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("serialises")
        .as_str()
        .expect("a name")
        .to_string()
}

/// The lifecycle events among `events`, by name, in publish order. A
/// `failed` carries its kind and phase, which say which path ended the
/// run: `failed (max_iterations, reducer)` is the reducer deciding it,
/// `failed (llm_error, llm_request)` a model call failing mid-step.
fn lifecycle(events: &[fq_runtime::events::Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::Completed(_) => Some("completed".to_string()),
            EventPayload::Failed(failed) => Some(format!(
                "failed ({}, {})",
                wire(failed.error_kind),
                wire(failed.phase)
            )),
            EventPayload::InvocationArchived(_) => Some("invocation.archived".to_string()),
            EventPayload::InvocationAmbiguous(_) => Some("invocation.ambiguous".to_string()),
            _ => None,
        })
        .collect()
}

/// A resumed run that ends in `failed` published `failed` (as `failed`
/// names it, see [`lifecycle`]) and `invocation.archived`, and its
/// ownership row says it failed: it is not reported ambiguous. Both are
/// asserted at once, so a failure shows both.
async fn assert_ended_in_failed_and_not_reported(
    recovering: &Recovering,
    events: &[fq_runtime::events::Event],
    failed: &str,
) {
    assert_eq!(
        (lifecycle(events), recovering.owner().await),
        (
            vec![failed.to_string(), "invocation.archived".to_string()],
            Some(OwnerStatus::Failed)
        ),
        "a resumed run that ended in `failed` publishes its ending and its archive and is \
         owned as failed. A second ending, `invocation.ambiguous`, turns the ownership row \
         to `ambiguous` and sends an operator after a run that needs nothing (issue 1005)"
    );
}

/// **A resumed run the reducer ends in `failed` is not reported
/// ambiguous.** The invocation is resumed at startup and its one model
/// turn does not end it, so the reducer ends it at the iteration ceiling:
/// `failed`, then `invocation.archived`, and the resume returns
/// `InvocationFailed`.
#[tokio::test]
async fn a_resumed_run_the_reducer_ends_in_failed_is_not_reported_ambiguous() {
    let mut recovering = Recovering::new().await;
    let events = recovering.resume(Reply::Text).await;
    assert_ended_in_failed_and_not_reported(
        &recovering,
        &events,
        "failed (max_iterations, reducer)",
    )
    .await;
    recovering.stop().await;
}

/// **A resumed run that fails part-way through a step is not reported
/// ambiguous.** The provider refuses the resumed run's model call, which
/// ends the run in `failed`, then `invocation.archived`, and the resume
/// returns the provider's error rather than `InvocationFailed`: what
/// recovery goes by has to be whether the run ended, not which error came
/// back.
#[tokio::test]
async fn a_resumed_run_that_fails_mid_step_is_not_reported_ambiguous() {
    let mut recovering = Recovering::new().await;
    let events = recovering.resume(Reply::Refused).await;
    assert_ended_in_failed_and_not_reported(
        &recovering,
        &events,
        "failed (llm_error, llm_request)",
    )
    .await;
    recovering.stop().await;
}

/// **A resumed run whose archive is refused is not reported ambiguous
/// either.** The run fails part-way through a step and publishes `failed`,
/// its row goes terminal, and then the broker refuses its
/// `invocation.archived`, so the resume returns that bus error. The run
/// has ended, and recovery does not report it.
///
/// Its ownership row reads `in_flight`, as recovery restored it at
/// startup. The coordination consumer moves that row on
/// `invocation.archived`, never on `failed`, so it reads `failed` only
/// once the archive is delivered, and until then the run is counted in
/// flight (issue 1024). Before issue 1005 it read `ambiguous` instead.
#[tokio::test]
async fn a_resumed_run_whose_archive_is_refused_is_not_reported_ambiguous() {
    let mut recovering = Recovering::new().await;
    let bus = recovering.start.bus.clone();
    recovering.publish_through(Arc::new(RefusesArchives(bus)));
    let events = recovering.resume(Reply::Refused).await;
    assert_eq!(
        (
            lifecycle(&events),
            recovering.owner().await,
            recovering.row().await
        ),
        (
            vec!["failed (llm_error, llm_request)".to_string()],
            Some(OwnerStatus::InFlight),
            ResumedRow::Terminal
        ),
        "the run ended in `failed`, its archive refused: no `invocation.ambiguous`, and the \
         ownership row is left `in_flight` until an archive is delivered (issue 1024)"
    );
    recovering.stop().await;
}

/// A next start over a WAL holding a completed model call whose stored
/// reply this build cannot read back. Its resume stops while it rebuilds
/// the run, before any step: the run does not end and its row stays in
/// flight.
async fn a_start_over_an_unreadable_reply() -> Recovering {
    let mut recovering = Recovering::new().await;
    let id = recovering.invocation().to_string();
    let store = recovering.start.worker_store.clone();
    store
        .write_llm_intent(
            &id,
            "req-0",
            "claude-haiku",
            "{}",
            &LlmCallOrigin::AgentTurn,
            1_001,
        )
        .await
        .unwrap();
    store
        .write_llm_dispatched(&id, "req-0", 1_002)
        .await
        .unwrap();
    store
        .write_llm_completed(&id, "req-0", "not a reply", false, 0.0001, 1_003)
        .await
        .unwrap();
    recovering.restart().await;
    recovering
}

/// **A resume that cannot proceed is reported ambiguous, once.** Only an
/// operator can settle the invocation, and recovery says so with
/// `invocation.ambiguous`: the ownership row turns `ambiguous`. The next
/// start finds it in flight again, its resume stops the same way, and it
/// is not reported a second time.
///
/// From that next start on, the ownership row reads `in_flight` (issue
/// 1025): startup recovery restores every invocation it resumes to
/// `in_flight` before the resume runs, and the one-shot stamp stops the
/// report that would have turned it back. So the invocation leaves
/// `fq invocation list --status=ambiguous` and `fq doctor`'s ambiguous
/// count at the second start, still unsettled.
#[tokio::test]
async fn a_resume_that_cannot_proceed_is_reported_ambiguous_once() {
    let mut recovering = a_start_over_an_unreadable_reply().await;

    let first = recovering.resume(Reply::Text).await;
    assert_eq!(
        lifecycle(&first),
        ["invocation.ambiguous"],
        "a resume that cannot proceed is reported ambiguous"
    );
    assert_eq!(recovering.owner().await, Some(OwnerStatus::Ambiguous));
    assert_eq!(recovering.row().await, ResumedRow::InFlight, "not ended");

    recovering.restart().await;
    // Issue 1025: what this start's recovery does to the ownership row.
    assert_eq!(
        recovering.owner().await,
        Some(OwnerStatus::InFlight),
        "recovery restores a resumed invocation to `in_flight`, ambiguous or not"
    );
    let second = recovering.resume(Reply::Text).await;
    assert_eq!(
        lifecycle(&second),
        Vec::<&str>::new(),
        "the next start's resume stops the same way and is not reported again"
    );
    assert_eq!(
        recovering.owner().await,
        Some(OwnerStatus::InFlight),
        "so the row stays `in_flight` (issue 1025)"
    );
    recovering.stop().await;
}

/// **An invocation recovery reported ambiguous is settled by a drop, not
/// a resume.** `fq invocation resume` reconciles a stuck dispatch, and
/// this invocation has none, so it is refused on both sides of the next
/// start: while the ownership row reads `ambiguous`, as not ambiguous in
/// its WAL, and once recovery has put the row back to `in_flight`, as
/// live. `fq invocation drop` ends it, and the next start has nothing to
/// resume.
#[tokio::test]
async fn an_invocation_recovery_reported_ambiguous_is_settled_by_a_drop() {
    let mut recovering = a_start_over_an_unreadable_reply().await;
    let first = recovering.resume(Reply::Text).await;
    assert_eq!(lifecycle(&first), ["invocation.ambiguous"]);

    let refused = recovering.operator_resume_refused().await;
    assert!(
        refused.contains("is not Ambiguous (it may already have been resumed)"),
        "{refused}"
    );

    recovering.restart().await;
    let second = recovering.resume(Reply::Text).await;
    assert_eq!(lifecycle(&second), Vec::<&str>::new());
    let refused = recovering.operator_resume_refused().await;
    assert!(
        refused.contains("is live; only Ambiguous invocations can be resumed"),
        "{refused}"
    );

    // The drop's own event may go out under another agent id than this
    // subscription's (see `drops_published`); what it did is read off the
    // two rows it moves instead.
    recovering.operator_drop().await;
    assert_eq!(
        (recovering.owner().await, recovering.row().await),
        (Some(OwnerStatus::Failed), ResumedRow::Terminal),
        "the drop ends it: owned as failed, and its worker row terminal"
    );
    assert_eq!(
        recovering.next_start_recovers().await,
        Vec::<String>::new(),
        "and the next start has nothing to resume"
    );
    recovering.stop().await;
}

/// **A resume whose row cannot be read afterwards is reported
/// ambiguous.** Not knowing whether the run ended is not knowing that it
/// did, so recovery reports it, as `publish_ambiguous_once` does when it
/// cannot read its stamp. The store gives no way to make its read fail
/// here, so the resume task's read result is handed in directly.
#[tokio::test]
async fn a_resume_whose_row_cannot_be_read_is_reported_ambiguous() {
    let mut recovering = Recovering::new().await;
    report_resume_error(
        Err(fq_runtime::WorkerStoreError::Backend(
            "disk I/O error (test)".to_string(),
        )),
        &recovering.start.worker_store,
        &recovering.start.bus,
        AgentId::new(CAPPED_AGENT).unwrap(),
        recovering.invocation(),
        ExecutorError::WorkerStore("disk I/O error (test)".to_string()),
    )
    .await;
    let events = recovering.settle().await;
    assert_eq!(
        (lifecycle(&events), recovering.owner().await),
        (
            vec!["invocation.ambiguous".to_string()],
            Some(OwnerStatus::Ambiguous)
        ),
        "an unreadable row is reported"
    );
    recovering.stop().await;
}

/// **`fq invocation resume` makes no such report.** An operator's re-drive
/// of an ambiguous invocation that then fails part-way through a step
/// publishes the ending and the archive and nothing more, and the
/// ownership row moves from `ambiguous` to failed. The operator's path
/// logs a re-drive that returned an error and never announced one as
/// ambiguous; this holds it there beside recovery's.
#[tokio::test]
async fn an_operator_resume_whose_run_ends_in_failed_is_not_reported_ambiguous() {
    let mut recovering = Recovering::over(Seed::Ambiguous).await;
    let events = recovering.operator_resume(Reply::Refused).await;
    assert_ended_in_failed_and_not_reported(
        &recovering,
        &events,
        "failed (llm_error, llm_request)",
    )
    .await;
    recovering.stop().await;
}

/// **The worker row, not the error, says whether a resumed run ended.**
/// Still in flight is a resume that was not carried out; terminal is a run
/// that reached its terminal event; and gone, after a resume that ran, is
/// a run the archive hand-off deleted once it ended. That last is the
/// order in which the daemon's archive-ack consumer acts before the resume
/// task reads the row, which the tests above leave to the race and which
/// here is set out directly, with the deletion that consumer makes.
#[tokio::test]
async fn the_worker_row_says_whether_a_resumed_run_ended() {
    let start = NextStart::over_one_run_in_flight().await;
    let store = start.worker_store.as_ref();
    let id = start.invocation.to_string();
    assert_eq!(
        ResumedRow::read(store, &id).await.unwrap(),
        ResumedRow::InFlight,
        "the row recovery found in flight"
    );

    let mut row = store
        .get_invocation_state(&id)
        .await
        .unwrap()
        .expect("the WAL row");
    row.phase = "failed".to_string();
    row.terminal_at = Some(2_000);
    row.updated_at = 2_000;
    store.upsert_invocation_state(&row).await.unwrap();
    assert_eq!(
        ResumedRow::read(store, &id).await.unwrap(),
        ResumedRow::Terminal,
        "the run's terminal write"
    );

    store.delete_invocation_state(&id).await.unwrap();
    assert_eq!(
        ResumedRow::read(store, &id).await.unwrap(),
        ResumedRow::Archived,
        "no row: the archive hand-off deleted it"
    );
}
