//! Invocation summary consumer (#216).
//!
//! Keeps a one-line, operator-facing status per invocation, generated
//! by a cheap configured model: what work was expected (from the
//! trigger payload), what it is doing now (rolling, from each model
//! turn), and how it ended. The line lands on the dashboard's
//! invocation surfaces via the projection's `invocation_summary`
//! table.
//!
//! Design constraints (issue #216 — binding):
//!
//! - **Never in the execution path.** This is an async event consumer,
//!   the same family as the projection/heartbeat consumers. It reads
//!   lifecycle events off the bus and writes nothing but its own
//!   `invocation.summary` events; the runner, reducer, WAL, and agent
//!   conversation are untouched, so resume/drain equivalence cannot
//!   be affected.
//! - **Incremental, never the conversation.** A rolling update's input
//!   is the prior summary plus the *latest* response content — full
//!   re-reads were ruled out analytically (a median invocation carries
//!   2.24M input tokens; re-reading them per turn costs more than the
//!   invocation itself).
//! - **Operator-costed.** Every summary event carries the summariser's
//!   own usage/cost on `envelope.cost` under the reserved `summary`
//!   agent id, exactly as `llm_response` events do for real agents —
//!   `fq costs` and the dashboard's per-model split report it with no
//!   changes to the costs path, and no invocation budget is touched.
//! - **Best-effort.** A summariser LLM failure is logged and the
//!   triggering event acked: a missing summary line is cosmetic, and a
//!   retry storm against a broken cheap model would cost more than it
//!   is worth. Only a failed *publish* is NAK'd (the summary was
//!   already paid for; redelivery re-tries the emit).
//!
//! Delivery semantics: at-least-once from JetStream. A redelivered
//! lifecycle event re-generates one summary line — idempotent at the
//! projection (last write per invocation wins) at the price of a
//! duplicate penny-call, which the ack-early posture below makes rare.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::agent::AgentId;
use crate::bus::{BusError, EventBus};
use crate::events::{
    CostMetadata, Event, EventPayload, InvocationSummaryPayload, Message, MessageRole,
    RequestParams, SummaryKind, subjects,
};
use crate::llm::{ChatRequest, LlmClient};
use crate::pricing::PricingTable;

/// Name of the durable JetStream consumer.
pub const CONSUMER_NAME: &str = "fq-summary";

/// The lifecycle moments the summariser reacts to. Multi-subject so
/// the consumer never churns through the tool-event firehose.
pub const FILTER_SUBJECTS: [&str; 4] = [
    "fq.agent.*.triggered",
    "fq.agent.*.llm_response",
    "fq.agent.*.completed",
    "fq.agent.*.failed",
];

/// Cap on how much of a trigger payload reaches the summariser —
/// payloads are operator/task-sized in practice; this only guards
/// against a pathological blob.
const MAX_TRIGGER_CHARS: usize = 4_000;

/// Cap on how much of a model turn reaches the summariser. Median
/// assistant output is ~220 tokens; this only guards the tail.
const MAX_CONTENT_CHARS: usize = 4_000;

/// Invocation summary consumer. Owns the bus, the cheap model's
/// client, and the pricing table; spawn via [`Self::run`].
pub struct SummaryConsumer {
    bus: EventBus,
    llm: Arc<dyn LlmClient>,
    pricing: Arc<PricingTable>,
    model: String,
    max_line_chars: usize,
    /// Current line per in-flight invocation — the "prior summary" a
    /// rolling update refines. In-memory only: after a daemon restart
    /// the next model turn simply starts a fresh line (a briefly
    /// stale line is acceptable; rebuilding from our own emitted
    /// events is not worth the machinery).
    summaries: Mutex<HashMap<Uuid, String>>,
    /// Test-only durable-name override; also switches the consumer to
    /// deliver-new-only (the coordination consumer's isolation
    /// pattern — each test gets a fresh cursor on the shared stream).
    test_consumer_name: Option<String>,
    /// Test-only agent scope: narrows the filter subjects to one
    /// agent id so parallel tests never summarise each other's events.
    test_agent_scope: Option<String>,
}

impl SummaryConsumer {
    pub fn new(
        bus: EventBus,
        llm: Arc<dyn LlmClient>,
        pricing: Arc<PricingTable>,
        model: String,
        max_line_chars: usize,
    ) -> Self {
        Self {
            bus,
            llm,
            pricing,
            model,
            max_line_chars,
            summaries: Mutex::new(HashMap::new()),
            test_consumer_name: None,
            test_agent_scope: None,
        }
    }

    /// Test-only isolation: a unique durable name (deliver-new-only)
    /// scoped to a single agent's events. Mirrors
    /// `CoordinationConsumer::with_test_consumer_name`.
    pub fn with_test_scope(mut self, consumer_name: String, agent_id: String) -> Self {
        self.test_consumer_name = Some(consumer_name);
        self.test_agent_scope = Some(agent_id);
        self
    }

    /// Run the consumer loop until `shutdown` fires.
    pub async fn run(
        self,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), SummaryConsumerError> {
        info!(
            model = %self.model,
            filters = ?FILTER_SUBJECTS,
            "invocation summary consumer starting"
        );
        let filters: Vec<String> = match &self.test_agent_scope {
            Some(agent) => vec![
                subjects::agent_triggered(agent),
                subjects::agent_llm_response(agent),
                subjects::agent_completed(agent),
                subjects::agent_failed(agent),
            ],
            None => FILTER_SUBJECTS.iter().map(|s| s.to_string()).collect(),
        };
        let consumer = match &self.test_consumer_name {
            Some(name) => {
                self.bus
                    .durable_consumer_with_filters_from_new(name, &filters)
                    .await?
            }
            None => {
                let refs: Vec<&str> = filters.iter().map(|s| s.as_str()).collect();
                self.bus
                    .durable_consumer_with_filters(CONSUMER_NAME, &refs)
                    .await?
            }
        };
        let mut messages = consumer
            .messages()
            .await
            .map_err(|err| SummaryConsumerError::Stream(err.to_string()))?;

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    info!("invocation summary consumer received shutdown signal");
                    break;
                }
                msg = messages.next() => {
                    match msg {
                        Some(Ok(msg)) => {
                            self.handle_message(&msg).await;
                        }
                        Some(Err(err)) => {
                            warn!(error = %err, "error reading summary-consumer message");
                        }
                        None => {
                            warn!("summary-consumer message stream ended unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        info!("invocation summary consumer stopped");
        Ok(())
    }

    async fn handle_message(&self, msg: &async_nats::jetstream::Message) {
        let event = match serde_json::from_slice::<Event>(&msg.payload) {
            Ok(e) => e,
            Err(err) => {
                warn!(error = %err, "failed to deserialise summary-consumer message; acking");
                let _ = msg.ack().await;
                return;
            }
        };

        // Never react to our own (or any sentinel-agent) events —
        // belt-and-braces against a summarisation loop; the filters
        // are already narrower than this.
        if event.envelope.agent_id.as_str() == AgentId::SUMMARY_STR
            || event.envelope.agent_id.as_str() == AgentId::SYSTEM_STR
        {
            let _ = msg.ack().await;
            return;
        }

        let invocation_id = event.envelope.invocation_id;
        let agent_id = event.envelope.agent_id.as_str();
        let (kind, instruction) = match &event.payload {
            EventPayload::Triggered(p) => {
                let task = truncate_chars(&p.trigger_payload.to_string(), MAX_TRIGGER_CHARS);
                (
                    SummaryKind::Start,
                    format!(
                        "Agent `{agent_id}` was just triggered with this payload:\n{task}\n\
                         Summarise what work is expected."
                    ),
                )
            }
            EventPayload::LlmResponse(p) => {
                let mut latest = String::new();
                if let Some(content) = &p.content {
                    latest.push_str(&truncate_chars(content, MAX_CONTENT_CHARS));
                }
                if !p.tool_calls.is_empty() {
                    let names: Vec<&str> =
                        p.tool_calls.iter().map(|t| t.tool_name.as_str()).collect();
                    latest.push_str(&format!("\n[calling tools: {}]", names.join(", ")));
                }
                if latest.trim().is_empty() {
                    // Nothing new to fold in.
                    let _ = msg.ack().await;
                    return;
                }
                let prior = self
                    .summaries
                    .lock()
                    .expect("summaries lock")
                    .get(&invocation_id)
                    .cloned()
                    .unwrap_or_else(|| "(no prior summary)".to_string());
                (
                    SummaryKind::Progress,
                    format!(
                        "Prior status line: {prior}\n\
                         The agent's latest turn:\n{latest}\n\
                         Update the status line to reflect where the work is now."
                    ),
                )
            }
            EventPayload::Completed(_) => {
                let prior = self.take_summary(&invocation_id);
                (
                    SummaryKind::Outcome,
                    format!(
                        "Prior status line: {prior}\n\
                         The invocation just COMPLETED successfully. Write the final \
                         one-line outcome (what was delivered)."
                    ),
                )
            }
            EventPayload::Failed(p) => {
                let prior = self.take_summary(&invocation_id);
                (
                    SummaryKind::Outcome,
                    format!(
                        "Prior status line: {prior}\n\
                         The invocation just FAILED ({:?}). Write the final one-line \
                         outcome naming the failure.",
                        p.error_kind
                    ),
                )
            }
            other => {
                debug!(event_type = ?std::mem::discriminant(other), "unexpected event on summary filter; ignoring");
                let _ = msg.ack().await;
                return;
            }
        };

        // The summariser call itself. A failure is logged and the
        // event acked — a missing line is cosmetic (see module doc).
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                Message {
                    role: MessageRole::System,
                    content: Some(format!(
                        "You maintain a one-line status summary of an autonomous agent's \
                         work for a human operator's dashboard. Reply with EXACTLY one \
                         line, at most {} characters: present tense, status first, no \
                         preamble, no quotes. Example: `Fixing #83: tests green, opening \
                         the PR`.",
                        self.max_line_chars
                    )),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
                Message {
                    role: MessageRole::User,
                    content: Some(instruction),
                    tool_calls: vec![],
                    tool_call_id: None,
                },
            ],
            tools: vec![],
            params: RequestParams {
                // Minimal, explicitly: gpt-5-family models otherwise
                // scale reasoning to fill max_tokens and return EMPTY
                // content — every summary silently skipped (found live
                // on gpt-5-nano; reasoning consumed all of 100 and,
                // raised to 400, all of 400).
                effort: Some(crate::events::Effort::Minimal),
                temperature: None,
                max_tokens: Some(150),
            },
        };
        let response = match self.llm.chat(request).await {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    invocation_id = %invocation_id,
                    error = %err,
                    "summariser LLM call failed; skipping this summary"
                );
                let _ = msg.ack().await;
                return;
            }
        };

        let line = one_line(
            response.content.as_deref().unwrap_or(""),
            self.max_line_chars,
        );
        if line.is_empty() {
            // stop_reason in the warning is the diagnosis: MaxTokens
            // with empty content means reasoning ate the budget.
            warn!(
                invocation_id = %invocation_id,
                stop_reason = ?response.stop_reason,
                output_tokens = response.usage.output_tokens,
                "summariser returned no content; skipping"
            );
            let _ = msg.ack().await;
            return;
        }

        // Cost the call against the pricing table (mirrors the runner:
        // an unpriced model reports $0 with a warning rather than
        // failing — but note the startup guarantee normally prevents
        // an unpriced summariser from running at all).
        let pricing = self.pricing.lookup(&self.model);
        if pricing.is_none() {
            warn!(model = %self.model, "no pricing known for summary model; cost will be reported as $0");
        }
        let (input_cost, output_cost, total_cost) = pricing
            .map(|p| p.calculate(&response.usage))
            .unwrap_or((0.0, 0.0, 0.0));

        let summary_event = Event::new(
            AgentId::summary(),
            invocation_id,
            EventPayload::InvocationSummary(InvocationSummaryPayload {
                kind,
                summary: line.clone(),
            }),
        )
        .with_cost(CostMetadata {
            call_id: Uuid::now_v7(),
            model: self.model.clone(),
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_read_tokens: response.usage.cache_read_tokens,
            cache_write_tokens: response.usage.cache_write_tokens,
            input_cost,
            output_cost,
            total_cost,
            // The summariser has no invocation/agent budget of its
            // own; cumulative tracking is the cost view's job
            // (SUM over the `summary` agent's rows).
            cumulative_invocation_cost: total_cost,
            cumulative_agent_cost: total_cost,
            origin: Default::default(),
        });

        match self.bus.publish(&summary_event).await {
            Ok(()) => {
                if kind != SummaryKind::Outcome {
                    self.summaries
                        .lock()
                        .expect("summaries lock")
                        .insert(invocation_id, line);
                }
                debug!(invocation_id = %invocation_id, ?kind, "published invocation summary");
                if let Err(err) = msg.ack().await {
                    error!(error = %err, "failed to ack summarised event");
                }
            }
            Err(err) => {
                error!(
                    invocation_id = %invocation_id,
                    error = %err,
                    "failed to publish invocation summary; NAK for redelivery"
                );
                let _ = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await;
            }
        }
    }

    /// Remove and return the invocation's current line (terminal
    /// events bound the map's size).
    fn take_summary(&self, invocation_id: &Uuid) -> String {
        self.summaries
            .lock()
            .expect("summaries lock")
            .remove(invocation_id)
            .unwrap_or_else(|| "(no prior summary)".to_string())
    }
}

/// First line of `text`, truncated to `max_chars` characters.
fn one_line(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    truncate_chars(line, max_chars)
}

/// Truncate to at most `max_chars` characters (not bytes — the line
/// may carry multi-byte punctuation from the model).
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Errors from the summary consumer's run loop.
#[derive(Debug, thiserror::Error)]
pub enum SummaryConsumerError {
    #[error("bus error: {0}")]
    Bus(#[from] BusError),

    #[error("summary consumer stream error: {0}")]
    Stream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        ConfigSnapshot, FailedPayload, FailureKind, FailurePhase, InvocationTotals,
        LlmResponsePayload, SandboxSnapshot, StopReason, TokenUsage, TriggerSource,
        TriggeredPayload,
    };
    use crate::llm::ChatResponse;
    use crate::llm::fixture::FixtureClient;
    use crate::pricing::ModelPricing;
    use crate::test_support::events::require_nats;
    use std::collections::HashMap as StdHashMap;
    use std::time::Duration;

    fn canned(line: &str) -> ChatResponse {
        ChatResponse {
            content: Some(line.to_string()),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 400,
                output_tokens: 20,
                ..Default::default()
            },
        }
    }

    fn priced_table() -> Arc<PricingTable> {
        let mut entries = StdHashMap::new();
        entries.insert(
            "cheap-model".to_string(),
            ModelPricing {
                input_per_million: 1.0,
                output_per_million: 5.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
            },
        );
        Arc::new(PricingTable::from_map(entries))
    }

    /// A per-test world over the shared dev broker: unique agent id,
    /// unique durable consumer, deliver-new-only — so parallel tests
    /// neither steal each other's lifecycle events nor cross-read
    /// each other's summaries (assertions filter by invocation id).
    struct World {
        bus: EventBus,
        agent: AgentId,
        fixture: FixtureClient,
        shutdown: Option<oneshot::Sender<()>>,
        handle: tokio::task::JoinHandle<Result<(), SummaryConsumerError>>,
    }

    impl World {
        async fn start(url: &str) -> Self {
            let bus = EventBus::connect(url).await.expect("connect NATS");
            let tag = Uuid::now_v7().simple().to_string();
            let agent = AgentId::new(format!("sumtest-{tag}")).unwrap();
            let fixture = FixtureClient::new();
            let consumer = SummaryConsumer::new(
                bus.clone(),
                Arc::new(fixture.clone()),
                priced_table(),
                "cheap-model".to_string(),
                120,
            )
            .with_test_scope(format!("fq-sum-test-{tag}"), agent.as_str().to_string());
            let (tx, rx) = oneshot::channel();
            let handle = tokio::spawn(consumer.run(rx));
            tokio::time::sleep(Duration::from_millis(300)).await;
            Self {
                bus,
                agent,
                fixture,
                shutdown: Some(tx),
                handle,
            }
        }

        fn triggered(&self, inv: Uuid) -> Event {
            Event::new(
                self.agent.clone(),
                inv,
                EventPayload::Triggered(TriggeredPayload {
                    trigger_source: TriggerSource::Manual,
                    trigger_subject: None,
                    trigger_payload: serde_json::json!({
                        "task": "Fix issue #7: the widget frobs backwards"
                    }),
                    config_snapshot: serde_json::from_value::<ConfigSnapshot>(serde_json::json!({
                        "name": self.agent.as_str(),
                        "model": "big-model",
                        "system_prompt": "",
                        "tools": [],
                        "sandbox": SandboxSnapshot::default(),
                        "budget": null,
                    }))
                    .expect("valid snapshot"),
                }),
            )
        }

        async fn stop(mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            let _ = self.handle.await;
        }
    }

    /// Await the next summary event for `inv`, skipping other tests'
    /// summaries on the shared `fq.agent.summary.*` subject.
    async fn await_summary(
        sub: &mut (impl futures::Stream<Item = Result<Event, BusError>> + Unpin),
        inv: Uuid,
    ) -> (SummaryKind, String, Event) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .expect("summary for the invocation within deadline");
            let event = tokio::time::timeout(remaining, sub.next())
                .await
                .expect("summary event within deadline")
                .expect("subscription open")
                .expect("decoded event");
            if event.envelope.invocation_id != inv {
                continue;
            }
            let EventPayload::InvocationSummary(p) = &event.payload else {
                panic!("expected InvocationSummary, got {:?}", event.payload);
            };
            return (p.kind, p.summary.clone(), event.clone());
        }
    }

    /// One-line/truncation helpers are pure — pin their edges.
    #[test]
    fn one_line_takes_first_line_and_truncates_by_chars() {
        assert_eq!(one_line("status here\nsecond line", 120), "status here");
        assert_eq!(one_line("", 120), "");
        let long = "x".repeat(200);
        let cut = one_line(&long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
        // Multi-byte safety: no panic, counts characters not bytes.
        assert_eq!(
            truncate_chars("é".repeat(10).as_str(), 5).chars().count(),
            5
        );
    }

    /// End-to-end over a real broker: a triggered event produces a
    /// start summary event under the reserved `summary` agent, with
    /// the summariser's own cost on the envelope (the
    /// operator-overhead accounting #216 requires).
    #[tokio::test]
    async fn triggered_event_produces_costed_start_summary() {
        let Some(url) = require_nats() else {
            return;
        };
        let world = World::start(&url).await;
        world
            .fixture
            .push_response(canned("Fixing #7: work not yet started"));

        let mut sub = world
            .bus
            .subscribe(subjects::agent_invocation_summary(AgentId::SUMMARY_STR))
            .await
            .expect("subscribe");

        let inv = Uuid::now_v7();
        world
            .bus
            .publish(&world.triggered(inv))
            .await
            .expect("publish");

        let (kind, line, event) = await_summary(&mut sub, inv).await;
        assert_eq!(event.envelope.agent_id.as_str(), AgentId::SUMMARY_STR);
        assert_eq!(kind, SummaryKind::Start);
        assert_eq!(line, "Fixing #7: work not yet started");
        // The cost is baked into the envelope: 400 in @ $1/M + 20 out
        // @ $5/M.
        let cost = event.envelope.cost.as_ref().expect("cost metadata");
        assert_eq!(cost.model, "cheap-model");
        assert!((cost.total_cost - (400.0 * 1e-6 + 20.0 * 5e-6)).abs() < 1e-12);

        world.stop().await;
    }

    /// A summariser LLM failure is logged and skipped — the consumer
    /// stays alive and the next event still summarises.
    #[tokio::test]
    async fn llm_failure_is_skipped_and_consumer_survives() {
        let Some(url) = require_nats() else {
            return;
        };
        let world = World::start(&url).await;
        world
            .fixture
            .push_error(crate::llm::LlmError::RequestFailed(
                "summariser down".to_string(),
            ));
        world
            .fixture
            .push_response(canned("Recovered: summarising again"));

        let mut sub = world
            .bus
            .subscribe(subjects::agent_invocation_summary(AgentId::SUMMARY_STR))
            .await
            .expect("subscribe");

        // First event: LLM errors — no summary. Second: succeeds.
        let inv1 = Uuid::now_v7();
        let inv2 = Uuid::now_v7();
        world
            .bus
            .publish(&world.triggered(inv1))
            .await
            .expect("publish 1");
        world
            .bus
            .publish(&world.triggered(inv2))
            .await
            .expect("publish 2");

        let (_, line, _) = await_summary(&mut sub, inv2).await;
        assert_eq!(line, "Recovered: summarising again");

        world.stop().await;
    }

    /// A rolling update folds the prior line and the latest turn into
    /// the summariser's input — never the conversation.
    #[tokio::test]
    async fn rolling_update_feeds_prior_summary_plus_latest_turn_only() {
        let Some(url) = require_nats() else {
            return;
        };
        let world = World::start(&url).await;
        world
            .fixture
            .push_response(canned("Fixing #7: reading the issue"));
        world
            .fixture
            .push_response(canned("Fixing #7: editing widget.rs"));

        let mut sub = world
            .bus
            .subscribe(subjects::agent_invocation_summary(AgentId::SUMMARY_STR))
            .await
            .expect("subscribe");

        let inv = Uuid::now_v7();
        world
            .bus
            .publish(&world.triggered(inv))
            .await
            .expect("publish");
        let _ = await_summary(&mut sub, inv).await;

        world
            .bus
            .publish(&Event::new(
                world.agent.clone(),
                inv,
                EventPayload::LlmResponse(LlmResponsePayload {
                    call_id: Uuid::now_v7(),
                    content: Some("I'll edit widget.rs to reverse the frob.".to_string()),
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    origin: Default::default(),
                }),
            ))
            .await
            .expect("publish llm_response");

        let (kind, _, _) = await_summary(&mut sub, inv).await;
        assert_eq!(kind, SummaryKind::Progress);

        // The summariser's second request must contain the prior line
        // and the latest content — and must NOT be conversation-sized.
        let requests = world.fixture.requests();
        let second = &requests[1];
        // Reasoning must be pinned to Minimal: gpt-5-family models
        // otherwise burn the whole max_tokens budget thinking and
        // return no content at all.
        assert_eq!(second.params.effort, Some(crate::events::Effort::Minimal));
        let user = second.messages[1].content.as_deref().unwrap();
        assert!(
            user.contains("Fixing #7: reading the issue"),
            "prior line fed in: {user}"
        );
        assert!(
            user.contains("reverse the frob"),
            "latest turn fed in: {user}"
        );

        world.stop().await;
    }

    /// A failed invocation gets an Outcome summary.
    #[tokio::test]
    async fn failed_event_produces_outcome_summary() {
        let Some(url) = require_nats() else {
            return;
        };
        let world = World::start(&url).await;
        world
            .fixture
            .push_response(canned("Failed: budget exceeded before the PR"));

        let mut sub = world
            .bus
            .subscribe(subjects::agent_invocation_summary(AgentId::SUMMARY_STR))
            .await
            .expect("subscribe");

        let inv = Uuid::now_v7();
        world
            .bus
            .publish(&Event::new(
                world.agent.clone(),
                inv,
                EventPayload::Failed(FailedPayload {
                    error_kind: FailureKind::BudgetExceeded,
                    error_message: "over budget".to_string(),
                    phase: FailurePhase::LlmResponse,
                    partial_totals: InvocationTotals::default(),
                }),
            ))
            .await
            .expect("publish failed event");

        let (kind, _, _) = await_summary(&mut sub, inv).await;
        assert_eq!(kind, SummaryKind::Outcome);

        world.stop().await;
    }
}
