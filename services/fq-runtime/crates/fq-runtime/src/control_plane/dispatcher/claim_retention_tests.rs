//! A started claim that the retention sweep keeps still refuses a replay.
//!
//! The trigger stream is a `Limits` stream that keeps a message for a day
//! after it is acked, so a durable that is deleted and recreated, or whose
//! state a broker restore loses, starts again from the oldest message on
//! the stream (`trigger_consumer_with_filter` sets no deliver policy, so
//! the default, all, applies). The `durably_started` claim rows are then
//! the only thing that stops those triggers from running a second time,
//! which is why the retention sweep may delete one only once its message
//! is gone (ADR-0032, "Retention of claim rows").
//!
//! This runs the real `handle` against a real broker and a real
//! control-plane store, with a stub worker. The replay is the broker's
//! own: the test deletes the durable and creates it again under the same
//! name, and the recreated durable delivers the trigger again from the
//! stream.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_nats::jetstream::Message;
use async_nats::jetstream::consumer::PullConsumer;
use chrono::Utc;
use futures::StreamExt;
use serde_json::json;
use uuid::Uuid;

use super::tests::{
    canned_response, recovered_claims, registry_with, test_key, unique_agent_id,
    unique_consumer_name,
};
use super::{CONSUMER_NAME, SharedRegistry, TriggerDispatcher};
use crate::agent::{Agent, AgentId};
use crate::bus::{EventBus, NATS_DEFAULT_MAX_ACK_PENDING, TRIGGER_STREAM_NAME};
use crate::config::{StateConfig, trigger_claim_retention_floor_days};
use crate::control_plane::retention::RetentionSweeper;
use crate::control_plane::{ControlPlaneStore, TriggerClaim, TriggerKey};
use crate::llm::LlmClient;
use crate::llm::fixture::FixtureClient;
use crate::test_support::nats::test_nats;
use crate::trigger::Trigger;
use crate::worker::{
    DrainRequest, DrainState, DurableStart, ExecutorError, InvocationOutcome, Worker,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

/// A run that makes its first durable write and finishes, counting how
/// many times it was started.
struct StartsAndFinishes {
    started: AtomicUsize,
}

#[async_trait::async_trait]
impl Worker for StartsAndFinishes {
    async fn run_invocation(
        &self,
        _agent: &Agent,
        _llm: &dyn LlmClient,
        _trigger: Trigger,
        _delivery_attempt: Option<u32>,
        mut durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        durable_start.fire();
        // The real runner awaits the store or the broker between its first
        // WAL write and its return, and `handle` learns of the durable
        // start only by polling for it: yield once so it does, and marks
        // the claim started rather than releasing it.
        tokio::task::yield_now().await;
        Ok(InvocationOutcome::Completed {
            invocation_id: Uuid::now_v7(),
            response: canned_response(),
            cost: 0.0,
            duration_ms: 0,
        })
    }

    async fn request_drain(&self, _req: DrainRequest) {}

    fn drain_status(&self) -> DrainState {
        DrainState::Running
    }
}

/// A dispatcher arbitrating through `claims` as `claimant`.
async fn dispatcher(
    bus: &EventBus,
    registry: SharedRegistry,
    worker: &Arc<StartsAndFinishes>,
    claims: &Arc<ControlPlaneStore>,
    claimant: &str,
) -> TriggerDispatcher {
    TriggerDispatcher::new(
        bus.clone(),
        registry,
        worker.clone() as Arc<dyn Worker>,
        Arc::new(FixtureClient::new()) as Arc<dyn LlmClient>,
        1,
    )
    .with_trigger_claims(recovered_claims(claims).await, claimant)
}

async fn next_delivery(consumer: &PullConsumer) -> Message {
    let mut messages = consumer.messages().await.expect("messages");
    tokio::time::timeout(Duration::from_secs(5), messages.next())
        .await
        .expect("a delivery within 5s")
        .expect("stream open")
        .expect("message ok")
}

async fn wait_until_acked(consumer: &mut PullConsumer, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let pending = consumer
            .info()
            .await
            .expect("consumer info")
            .num_ack_pending;
        if pending == 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} was not acked (num_ack_pending={pending})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **A trigger replayed after a sweep starts nothing while its started
/// claim is inside the window.**
///
/// One trigger runs and its claim is marked started. A sweep at the
/// shortest window the config accepts then runs, and deletes an old
/// started claim beside it, so the sweep is shown to have done its work.
/// The durable is then deleted and recreated, as a reset or a restore
/// would, and delivers the trigger again from the stream to a dispatcher
/// under a new claimant, as the next daemon would be. The started claim
/// is what refuses it: the copy is acked, counted as a duplicate, and
/// nothing starts. A sweep that took started claims inside the window
/// would let the replay win a fresh claim and run the trigger twice.
#[tokio::test]
async fn a_replayed_trigger_whose_started_claim_the_sweep_kept_starts_nothing() {
    let server = test_nats();
    let bus = EventBus::connect(server.url()).await.expect("connect NATS");
    let agent_id = unique_agent_id("claim-retention-replay");
    let (_agent_dir, registry) = registry_with(&agent_id);
    let claim_dir = tempfile::tempdir().unwrap();
    let claims = Arc::new(
        ControlPlaneStore::open(&claim_dir.path().join("control-plane.db"))
            .await
            .unwrap(),
    );
    let worker = Arc::new(StartsAndFinishes {
        started: AtomicUsize::new(0),
    });
    let durable = unique_consumer_name();
    let filter = crate::events::subjects::trigger(&agent_id);

    // The run.
    let first = dispatcher(&bus, registry.clone(), &worker, &claims, "worker-a").await;
    let mut consumer = bus
        .trigger_consumer_with_filter(&durable, &filter, NATS_DEFAULT_MAX_ACK_PENDING)
        .await
        .expect("consumer");
    bus.publish_trigger(&AgentId::new(&agent_id).unwrap(), &json!({"input": "hi"}))
        .await
        .expect("publish");
    let delivery = next_delivery(&consumer).await;
    let seq = delivery
        .info()
        .expect("a JetStream delivery")
        .stream_sequence;
    tokio::time::timeout(Duration::from_secs(10), first.handle(&delivery))
        .await
        .expect("handle returns");
    assert_eq!(worker.started.load(Ordering::SeqCst), 1, "the trigger ran");
    wait_until_acked(&mut consumer, "the trigger").await;
    assert!(
        matches!(
            claims
                .claim_trigger(test_key(&bus, seq), "probe", "probe", 0)
                .await
                .unwrap(),
            TriggerClaim::Started { .. }
        ),
        "the run's claim is marked started"
    );

    // The sweep, at the shortest window the config accepts, beside a
    // started claim from long ago whose message is long gone.
    let window_days = trigger_claim_retention_floor_days();
    let ancient = TriggerKey {
        stream: TRIGGER_STREAM_NAME,
        stream_epoch: bus.trigger_stream_epoch(),
        stream_seq: seq + 1_000_000,
    };
    let ancient_claimed_at = Utc::now().timestamp_millis() - (window_days + 1) * DAY_MS;
    claims
        .claim_trigger(ancient, "worker-gone", "inv-ancient", ancient_claimed_at)
        .await
        .unwrap();
    claims
        .mark_trigger_started(ancient, "inv-ancient")
        .await
        .unwrap();
    RetentionSweeper::new(
        claims.clone(),
        &StateConfig {
            retention_days: -1,
            stale_worker_retention_days: -1,
            trigger_claim_retention_days: window_days,
            ..StateConfig::default()
        },
    )
    .with_claimant("worker-a")
    .sweep_now()
    .await
    .unwrap();
    assert_eq!(
        claims
            .trigger_claim_for_invocation("inv-ancient")
            .await
            .unwrap(),
        None,
        "the sweep ran and took the claim past its window"
    );

    // The replay: the durable is reset, and the recreated one delivers
    // the trigger again from the stream to the next daemon.
    bus.jetstream()
        .get_stream(TRIGGER_STREAM_NAME)
        .await
        .expect("trigger stream")
        .delete_consumer(&durable)
        .await
        .expect("delete the durable");
    let mut recreated = bus
        .trigger_consumer_with_filter(&durable, &filter, NATS_DEFAULT_MAX_ACK_PENDING)
        .await
        .expect("recreate the durable");
    let replay = next_delivery(&recreated).await;
    let replay_info = replay.info().expect("a JetStream delivery");
    assert_eq!(
        (replay_info.stream_sequence, replay_info.delivered),
        (seq, 1),
        "the recreated durable delivers the same message afresh"
    );
    let next = dispatcher(&bus, registry, &worker, &claims, "worker-b").await;
    tokio::time::timeout(Duration::from_secs(10), next.handle(&replay))
        .await
        .expect("the replay is answered rather than held");

    assert_eq!(
        worker.started.load(Ordering::SeqCst),
        1,
        "the replayed trigger started a second run"
    );
    assert_eq!(
        bus.consumer_ledger()
            .record(CONSUMER_NAME)
            .duplicate_dropped,
        1,
        "the replay is dropped as a duplicate of a started trigger"
    );
    wait_until_acked(&mut recreated, "the replay").await;
}
