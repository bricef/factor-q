//! The sweeper's held ending, over a recording sink: when it goes out,
//! how often, under which id, and how big it is.

use std::collections::HashSet;
use std::sync::Arc;

use fq_ops::health::ArchiveHoldReason;
use uuid::Uuid;

use super::*;
use crate::agent::AgentId;
use crate::bus::EventSink;
use crate::events::{
    CompletedPayload, EventPayload, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
    TaskStatus,
};
use crate::test_support::sim::RecordingSink;
use crate::worker::WorkerId;
use crate::worker::store::{WorkerStore, terminal_outcome};

const AGENT: &str = "held-ending-agent";
const WORKER: &str = "held-ending-worker";

struct Fixture {
    store: Arc<WorkerStore>,
    sink: Arc<RecordingSink>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            WorkerStore::open(&dir.path().join("worker.db"))
                .await
                .unwrap(),
        );
        Self {
            store,
            sink: Arc::new(RecordingSink::new()),
            _dir: dir,
        }
    }

    /// A new daemon run's sweeper: it starts with no holds.
    fn sweeper(&self) -> ArchiveRetrySweeper {
        ArchiveRetrySweeper::new_with_sink(
            Arc::clone(&self.sink) as Arc<dyn EventSink>,
            WorkerId::new(WORKER).unwrap(),
            Arc::clone(&self.store),
        )
    }

    /// A finished row in `phase` that stored `stored(its id)` as its
    /// ending and never stamped it published.
    async fn finished_row(&self, phase: &str, stored: impl FnOnce(Uuid) -> Event) -> Uuid {
        let invocation_id = Uuid::now_v7();
        let row = InvocationStateRow {
            invocation_id: invocation_id.to_string(),
            agent_id: AGENT.to_string(),
            schema_version: 1,
            phase: phase.to_string(),
            state_blob: vec![1, 2, 3],
            step_index: 2,
            started_at: 1_000,
            updated_at: 5_000,
            terminal_at: Some(5_000),
            workspace_ref: None,
            archive_status: None,
            archive_published_at: None,
            trigger_source: None,
            trigger_subject: None,
            trigger_payload: None,
            terminal_outcome: Some(terminal_outcome::encode(&stored(invocation_id)).unwrap()),
            terminal_published_at: None,
        };
        self.store.upsert_invocation_state(&row).await.unwrap();
        invocation_id
    }

    fn held_endings(&self) -> Vec<Event> {
        self.sink
            .events()
            .into_iter()
            .filter(|event| matches!(event.payload, EventPayload::InvocationEndingHeld(_)))
            .collect()
    }
}

async fn pass(sweeper: &ArchiveRetrySweeper) -> Result<(), ArchiveRetryError> {
    sweeper.sweep_once(&mut HashSet::new()).await
}

/// A `failed` this build will not republish: an envelope version it does
/// not read.
fn unreadable_failed(invocation_id: Uuid) -> Event {
    let mut event = Event::new(
        AgentId::new(AGENT).unwrap(),
        invocation_id,
        EventPayload::Failed(FailedPayload {
            error_kind: FailureKind::MaxIterations,
            error_message: "exceeded max iterations (2)".to_string(),
            phase: FailurePhase::Reducer,
            partial_totals: InvocationTotals::default(),
            detail: None,
        }),
    );
    event.envelope.schema_version = crate::events::SCHEMA_VERSION - 1;
    event
}

/// A `completed` whose summary is `summary_bytes` long.
fn completed_of(invocation_id: Uuid, summary_bytes: usize) -> Event {
    Event::new(
        AgentId::new(AGENT).unwrap(),
        invocation_id,
        EventPayload::Completed(CompletedPayload {
            task_status: TaskStatus::Success,
            result_summary: Some("s".repeat(summary_bytes)),
            total_llm_calls: 1,
            total_tool_calls: 0,
            total_cost: 0.0,
            total_duration_ms: 10,
        }),
    )
}

fn held_payload(event: &Event) -> &InvocationEndingHeldPayload {
    match &event.payload {
        EventPayload::InvocationEndingHeld(held) => held,
        other => panic!("not a held ending: {other:?}"),
    }
}

/// Held for a stored event it will not republish: one held ending, saying
/// how the run ended and when, for the worker holding it, under the id
/// derived from the run's; and only once in the daemon run, however many
/// passes hold the row.
#[tokio::test]
async fn a_refused_ending_goes_out_once_per_daemon_run() {
    let fixture = Fixture::new().await;
    let invocation_id = fixture.finished_row("failed", unreadable_failed).await;
    let sweeper = fixture.sweeper();
    for _ in 0..3 {
        pass(&sweeper).await.unwrap();
    }
    let held = fixture.held_endings();
    assert_eq!(held.len(), 1, "one held ending in the run: {held:?}");
    let event = &held[0];
    assert_eq!(event.envelope.invocation_id, invocation_id);
    assert_eq!(event.envelope.agent_id.as_str(), AGENT);
    assert_eq!(event.envelope.event_id, event_id(invocation_id, 5_000));
    assert_eq!(
        held_payload(event),
        &InvocationEndingHeldPayload {
            outcome: HeldOutcome::Failed,
            ended_at_ms: 5_000,
            started_at_ms: 1_000,
            worker_id: WorkerId::new(WORKER).unwrap(),
            reason: EndingHeldReason::StoredEventRefused,
            // A row with no model or tool calls on record.
            totals: Some(crate::events::RecordedTotals {
                total_llm_calls: 0,
                total_tool_calls: 0,
                total_cost: 0.0,
            }),
        }
    );
    assert_eq!(
        fixture.sink.events().len(),
        1,
        "nothing else goes out for the row"
    );

    // The next daemon run sends it again, under the same id.
    pass(&fixture.sweeper()).await.unwrap();
    let held = fixture.held_endings();
    assert_eq!(held.len(), 2);
    assert_eq!(held[1].envelope.event_id, held[0].envelope.event_id);
}

/// Held because the broker refuses the stored `completed` for its size:
/// the held ending goes out on the pass that holds the row, and not on
/// the passes that skip it after.
#[tokio::test]
async fn a_too_large_ending_goes_out_on_the_pass_that_holds_it() {
    let fixture = Fixture::new().await;
    fixture.sink.set_max_payload(2_048);
    let invocation_id = fixture
        .finished_row("completed", |id| completed_of(id, 4_096))
        .await;
    let sweeper = fixture.sweeper();
    for _ in 0..3 {
        pass(&sweeper).await.unwrap();
    }
    assert!(
        matches!(
            sweeper.holds().snapshot()[..],
            [fq_ops::health::ArchiveHold {
                reason: ArchiveHoldReason::PayloadTooLarge { .. },
                ..
            }]
        ),
        "held for size: {:?}",
        sweeper.holds().snapshot()
    );
    let held = fixture.held_endings();
    assert_eq!(held.len(), 1, "{held:?}");
    let held = held_payload(&held[0]);
    assert_eq!(held.outcome, HeldOutcome::Completed);
    assert_eq!(held.reason, EndingHeldReason::PayloadTooLarge);
    assert_eq!(
        held.totals, None,
        "the stored completed reports a model call the rows do not hold: no figure rather \
         than a second one"
    );
    assert_eq!(
        fixture.held_endings()[0].envelope.invocation_id,
        invocation_id
    );
}

/// A bus error on the held ending ends the pass, as any bus error does,
/// and the next pass sends it, for a row held for size too, which the
/// next pass does not offer to the broker again.
#[tokio::test]
async fn a_held_ending_the_bus_failed_goes_out_on_the_next_pass() {
    for too_large in [false, true] {
        let fixture = Fixture::new().await;
        if too_large {
            fixture.sink.set_max_payload(2_048);
            fixture
                .finished_row("completed", |id| completed_of(id, 4_096))
                .await;
        } else {
            fixture.finished_row("failed", unreadable_failed).await;
        }
        let sweeper = fixture.sweeper();
        fixture.sink.fail_publish_at(0);
        assert!(
            matches!(pass(&sweeper).await, Err(ArchiveRetryError::Publish(_))),
            "the bus error ends the pass"
        );
        assert!(fixture.held_endings().is_empty());
        fixture.sink.clear_fault();
        pass(&sweeper).await.unwrap();
        pass(&sweeper).await.unwrap();
        assert_eq!(
            fixture.held_endings().len(),
            1,
            "too_large {too_large}: sent once, on the pass after the error"
        );
    }
}

/// A row held because its `invocation.archived` is too large published
/// its ending already: no held ending goes out for it.
#[tokio::test]
async fn an_archive_held_for_size_sends_no_held_ending() {
    let fixture = Fixture::new().await;
    // The ending fits, the archive's state does not.
    fixture.sink.set_max_payload(2_048);
    let invocation_id = Uuid::now_v7();
    let row = InvocationStateRow {
        invocation_id: invocation_id.to_string(),
        agent_id: AGENT.to_string(),
        schema_version: 1,
        phase: "completed".to_string(),
        state_blob: vec![7; 4_096],
        step_index: 2,
        started_at: 1_000,
        updated_at: 5_000,
        terminal_at: Some(5_000),
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
        terminal_outcome: Some(terminal_outcome::encode(&completed_of(invocation_id, 8)).unwrap()),
        terminal_published_at: None,
    };
    fixture.store.upsert_invocation_state(&row).await.unwrap();
    pass(&fixture.sweeper()).await.unwrap();
    let kinds: Vec<_> = fixture
        .sink
        .events()
        .iter()
        .map(|event| event.payload.event_type())
        .collect();
    assert_eq!(kinds, ["completed"], "the run's own ending went out");
    assert!(fixture.held_endings().is_empty());
}

/// Its size does not depend on the stored event, and is small: a fixed
/// part of a few hundred bytes, plus the agent and worker ids, with the
/// largest totals a run can report. The bus's own count, header included.
#[tokio::test]
async fn a_held_ending_is_small_whatever_the_stored_event_is() {
    let mut sizes = Vec::new();
    for summary_bytes in [8, 4_000_000] {
        let fixture = Fixture::new().await;
        fixture.sink.set_max_payload(4_096);
        fixture
            .finished_row("completed", |id| completed_of(id, summary_bytes))
            .await;
        pass(&fixture.sweeper()).await.unwrap();
        let events = fixture.sink.events();
        let event = events.last().expect("something went out");
        if summary_bytes > 4_096 {
            assert!(matches!(
                event.payload,
                EventPayload::InvocationEndingHeld(_)
            ));
            sizes.push(serde_json::to_vec(event).unwrap().len());
        }
    }
    // The largest totals a run can report, and a long agent id.
    let long_agent = "a".repeat(128);
    let mut event = Event::new(
        AgentId::new(long_agent.clone()).unwrap(),
        Uuid::now_v7(),
        EventPayload::InvocationEndingHeld(InvocationEndingHeldPayload {
            outcome: HeldOutcome::Completed,
            ended_at_ms: i64::MAX,
            started_at_ms: i64::MAX,
            worker_id: WorkerId::new(WORKER).unwrap(),
            reason: EndingHeldReason::StoredEventRefused,
            totals: Some(crate::events::RecordedTotals {
                total_llm_calls: u32::MAX,
                total_tool_calls: u32::MAX,
                total_cost: f64::MAX,
            }),
        }),
    );
    event.envelope.event_id = event_id(event.envelope.invocation_id, i64::MAX);
    let body = serde_json::to_vec(&event).unwrap().len();
    let bound = 900 + long_agent.len() + WORKER.len();
    assert!(
        body <= bound,
        "a held ending is {body} bytes, over its bound of {bound}"
    );
    crate::bus::size::check_event_size(&event, body, 1_024 + long_agent.len())
        .expect("a held ending fits a broker far below any max_payload in use");
    assert!(sizes.iter().all(|size| *size < 1_024), "{sizes:?}");
}

/// The id is the same for every copy of one run's held ending, is a v7
/// dated when the run ended, so it sorts after everything the run did, is
/// never another of the run's ids, and tells two runs apart.
#[test]
fn the_held_ending_s_id_is_the_run_s_own_and_no_other() {
    let mut seen = HashSet::new();
    let millis = |id: Uuid| {
        let (secs, nanos) = id.get_timestamp().unwrap().to_unix();
        secs as i64 * 1_000 + i64::from(nanos) / 1_000_000
    };
    for n in 0..1_000i64 {
        let invocation_id = Uuid::now_v7();
        // The run's own events, minted while it ran, before it ended.
        let triggered = Uuid::now_v7();
        let ended_at_ms = millis(triggered) + 1 + n;
        let id = event_id(invocation_id, ended_at_ms);
        assert_eq!(
            id,
            event_id(invocation_id, ended_at_ms),
            "derived, so the same each time"
        );
        assert_ne!(id, invocation_id);
        assert_eq!(id.get_version_num(), 7);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
        assert_eq!(millis(id), ended_at_ms, "dated when the run ended");
        assert!(
            id > invocation_id && id > triggered,
            "sorts after the run's start and what it did"
        );
        assert!(seen.insert(id), "two runs, one id");
    }
}

/// Which holds keep the run's own ending off the stream: a stored event
/// refused, or a stored ending refused for size, named by the `event_type`
/// the refused event itself has. Not a refused archive, nor a malformed
/// row.
#[test]
fn only_a_held_ending_s_own_holds_send_one() {
    let too_large = |event: &Event| ArchiveHoldReason::PayloadTooLarge {
        event_type: event.payload.event_type().to_string(),
        size_bytes: 2_000_000,
        limit_bytes: 1_048_576,
    };
    let id = Uuid::now_v7();
    assert_eq!(
        reason_of(&too_large(&completed_of(id, 8))),
        Some(EndingHeldReason::PayloadTooLarge)
    );
    assert_eq!(
        reason_of(&too_large(&unreadable_failed(id))),
        Some(EndingHeldReason::PayloadTooLarge)
    );
    let archived = Event::new(
        AgentId::new(AGENT).unwrap(),
        id,
        EventPayload::InvocationArchived(crate::events::InvocationArchivedPayload {
            worker_id: WorkerId::new(WORKER).unwrap(),
            final_phase: "completed".to_string(),
            final_state_blob: vec![],
            started_at_ms: 0,
            terminal_at_ms: 0,
        }),
    );
    assert_eq!(reason_of(&too_large(&archived)), None);
    assert_eq!(
        reason_of(&ArchiveHoldReason::StoredEventRefused {
            detail: String::new()
        }),
        Some(EndingHeldReason::StoredEventRefused)
    );
    assert_eq!(
        reason_of(&ArchiveHoldReason::MalformedRow {
            detail: String::new()
        }),
        None
    );
}

/// A size-held row whose per-call rows agree with its stored ending: the
/// held ending carries the recorded totals, which are the stored ending's.
#[tokio::test]
async fn a_too_large_ending_carries_the_totals_its_stored_ending_agrees_with() {
    let fixture = Fixture::new().await;
    fixture.sink.set_max_payload(2_048);
    let invocation_id = fixture
        .finished_row("completed", |id| {
            let mut event = completed_of(id, 4_096);
            if let EventPayload::Completed(completed) = &mut event.payload {
                completed.total_llm_calls = 2;
                completed.total_tool_calls = 1;
                completed.total_cost = 0.1 + 0.2;
            }
            event
        })
        .await;
    let inv = invocation_id.to_string();
    for (n, cost) in [(0, 0.1), (1, 0.2)] {
        let request = format!("call-{n}");
        let origin = crate::events::LlmCallOrigin::AgentTurn;
        let store = &fixture.store;
        store
            .write_llm_intent(&inv, &request, "claude-sim", "{}", &origin, 1_000 + n)
            .await
            .unwrap();
        store
            .write_llm_dispatched(&inv, &request, 1_100 + n)
            .await
            .unwrap();
        store
            .write_llm_completed(&inv, &request, "{}", false, cost, 1_200 + n)
            .await
            .unwrap();
    }
    fixture
        .store
        .write_tool_intent(&inv, "tool-0", "sim_tool", "{}", 1_300)
        .await
        .unwrap();
    fixture
        .store
        .write_tool_dispatched(&inv, "tool-0", 1_400)
        .await
        .unwrap();
    fixture
        .store
        .write_tool_completed(&inv, "tool-0", "{}", false, 1_500)
        .await
        .unwrap();

    pass(&fixture.sweeper()).await.unwrap();
    let held = fixture.held_endings();
    assert_eq!(
        held_payload(&held[0]).totals,
        Some(crate::events::RecordedTotals {
            total_llm_calls: 2,
            total_tool_calls: 1,
            total_cost: 0.1 + 0.2,
        })
    );
}

/// A stored ending whose summed cost a fast float parser would misread:
/// three calls whose costs sum, left to right, to 1.5229507399999997,
/// which serde_json without `float_roundtrip` read back as
/// 1.5229507399999995. With it, the stored figure reads back as written,
/// so the rows agree with the stored ending exactly and the held ending
/// carries the figure the run published, to the bit.
#[tokio::test]
async fn totals_agree_with_a_stored_cost_a_fast_parser_would_misread() {
    let costs = [0.386611969, 0.434366916, 0.7019718549999999];
    let total: f64 = costs.iter().sum();
    let read_back: f64 = serde_json::from_str(&serde_json::to_string(&total).unwrap()).unwrap();
    assert_eq!(
        total.to_bits(),
        read_back.to_bits(),
        "the figure reads back as it was written"
    );

    let fixture = Fixture::new().await;
    fixture.sink.set_max_payload(2_048);
    let invocation_id = fixture
        .finished_row("completed", |id| {
            let mut event = completed_of(id, 4_096);
            if let EventPayload::Completed(completed) = &mut event.payload {
                completed.total_llm_calls = 3;
                completed.total_tool_calls = 0;
                completed.total_cost = total;
            }
            event
        })
        .await;
    let inv = invocation_id.to_string();
    for (n, cost) in costs.into_iter().enumerate() {
        let request = format!("call-{n}");
        let at = 1_000 + n as i64;
        let origin = crate::events::LlmCallOrigin::AgentTurn;
        let store = &fixture.store;
        store
            .write_llm_intent(&inv, &request, "claude-sim", "{}", &origin, at)
            .await
            .unwrap();
        store
            .write_llm_dispatched(&inv, &request, at)
            .await
            .unwrap();
        store
            .write_llm_completed(&inv, &request, "{}", false, cost, at)
            .await
            .unwrap();
    }

    pass(&fixture.sweeper()).await.unwrap();
    let held = fixture.held_endings();
    let totals = held_payload(&held[0])
        .totals
        .expect("the rows agree with the stored ending");
    assert_eq!(totals.total_cost.to_bits(), total.to_bits());
    assert_eq!(totals.total_llm_calls, 3);
}

/// A broker whose `max_payload` refuses even the held ending: the pass
/// does not end at that row. Held rows sort first, so a refusal that ended
/// the pass would leave every row behind it unarchived, on every pass. The
/// refused held ending is not offered again in the run; the row behind is
/// archived on each pass.
#[tokio::test]
async fn a_held_ending_too_large_itself_does_not_end_the_pass() {
    let fixture = Fixture::new().await;
    // An agent id long enough that the held row's held ending is over the
    // limit, while the healthy row's archive, under a short one, is not.
    let long_agent = "a".repeat(1_200);
    let held = Uuid::now_v7();
    let mut row = InvocationStateRow {
        invocation_id: held.to_string(),
        agent_id: long_agent.clone(),
        schema_version: 1,
        phase: "failed".to_string(),
        state_blob: vec![1, 2, 3],
        step_index: 2,
        started_at: 1_000,
        updated_at: 5_000,
        terminal_at: Some(5_000),
        workspace_ref: None,
        archive_status: None,
        archive_published_at: None,
        trigger_source: None,
        trigger_subject: None,
        trigger_payload: None,
        terminal_outcome: None,
        terminal_published_at: None,
    };
    let mut stored = unreadable_failed(held);
    stored.envelope.agent_id = AgentId::new(long_agent).unwrap();
    row.terminal_outcome = Some(terminal_outcome::encode(&stored).unwrap());
    fixture.store.upsert_invocation_state(&row).await.unwrap();
    let healthy = Uuid::now_v7();
    let mut healthy_row = row.clone();
    healthy_row.invocation_id = healthy.to_string();
    healthy_row.agent_id = AGENT.to_string();
    healthy_row.terminal_outcome = None;
    fixture
        .store
        .upsert_invocation_state(&healthy_row)
        .await
        .unwrap();
    // Archived once already and awaiting its ack, so it sorts behind the
    // held row, which never was.
    fixture
        .store
        .set_archive_pending(&healthy.to_string(), 1_000)
        .await
        .unwrap();
    let order = fixture.store.list_archive_pending().await.unwrap();
    assert_eq!(
        order[0].invocation_id,
        held.to_string(),
        "the held row first"
    );
    fixture.sink.set_max_payload(1_024);

    let sweeper = fixture.sweeper();
    for _ in 0..3 {
        pass(&sweeper)
            .await
            .expect("a refused held ending does not end the pass");
    }
    let refused: Vec<_> = fixture
        .sink
        .refused()
        .iter()
        .map(|event| (event.envelope.invocation_id, event.payload.event_type()))
        .collect();
    assert_eq!(
        refused,
        [(held, "invocation_ending_held")],
        "offered once in the run, and refused"
    );
    let archived = fixture
        .sink
        .events()
        .iter()
        .filter(|event| {
            event.envelope.invocation_id == healthy
                && matches!(event.payload, EventPayload::InvocationArchived(_))
        })
        .count();
    assert_eq!(archived, 3, "the row behind is archived on every pass");
}
