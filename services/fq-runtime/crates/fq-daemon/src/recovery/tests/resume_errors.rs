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
        let start = NextStart::over_one_run_in_flight().await;
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
        assert_eq!(
            recovered
                .recoverable
                .iter()
                .map(|c| c.state.invocation_id.clone())
                .collect::<Vec<_>>(),
            [self.invocation().to_string()],
            "the next start finds the invocation in flight and resumes it"
        );
        self.start.recoverable = recovered.recoverable;
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

/// The lifecycle events among `events`, by name, in publish order.
fn lifecycle(events: &[fq_runtime::events::Event]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::Completed(_) => Some("completed"),
            EventPayload::Failed(_) => Some("failed"),
            EventPayload::InvocationArchived(_) => Some("invocation.archived"),
            EventPayload::InvocationAmbiguous(_) => Some("invocation.ambiguous"),
            _ => None,
        })
        .collect()
}

/// A resumed run that ends in `failed` published `failed` and
/// `invocation.archived`, and its ownership row says it failed: it is not
/// reported ambiguous. Both are asserted at once, so a failure shows both.
async fn assert_ended_in_failed_and_not_reported(
    recovering: &Recovering,
    events: &[fq_runtime::events::Event],
) {
    assert_eq!(
        (lifecycle(events), recovering.owner().await),
        (
            vec!["failed", "invocation.archived"],
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
    assert_ended_in_failed_and_not_reported(&recovering, &events).await;
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
    assert_ended_in_failed_and_not_reported(&recovering, &events).await;
    recovering.stop().await;
}

/// **A resume that cannot proceed is reported ambiguous, once.** The WAL
/// holds a completed model call whose stored reply this build cannot read
/// back, so the resume stops while it rebuilds the run, before any step:
/// the run does not end, its row stays in flight, and only an operator can
/// settle it. Recovery says so with `invocation.ambiguous`, and the
/// ownership row turns `ambiguous`. The next start finds it in flight
/// again, its resume stops the same way, and it is not reported a second
/// time.
#[tokio::test]
async fn a_resume_that_cannot_proceed_is_reported_ambiguous_once() {
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

    let first = recovering.resume(Reply::Text).await;
    assert_eq!(
        lifecycle(&first),
        ["invocation.ambiguous"],
        "a resume that cannot proceed is reported ambiguous"
    );
    assert_eq!(recovering.owner().await, Some(OwnerStatus::Ambiguous));
    let row = store
        .get_invocation_state(&id)
        .await
        .unwrap()
        .expect("the WAL row is still there");
    assert_eq!(row.terminal_at, None, "the run did not end");

    recovering.restart().await;
    let second = recovering.resume(Reply::Text).await;
    assert_eq!(
        lifecycle(&second),
        Vec::<&str>::new(),
        "the next start's resume stops the same way and is not reported again"
    );
    recovering.stop().await;
}
