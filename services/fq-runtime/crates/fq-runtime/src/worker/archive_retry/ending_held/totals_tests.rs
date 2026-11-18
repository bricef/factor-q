//! The held ending's totals, recomputed from the worker's per-call rows,
//! against the totals the run's own ending carries, for every ending this
//! build stores and can read.
//!
//! The run's own ending is built from the totals the runner kept in
//! memory as it went; the held ending's are rebuilt from the WAL. They
//! must be the same figure, field by field and to the bit, or the stream
//! would carry two figures for one run once a held `completed` goes out
//! under a raised limit. The sweep runs the real runner over seeded
//! scripts: tool calls, parallel batches, empty and failed model calls,
//! priced calls with costs that do not sum exactly, the iteration cap, a
//! budget, sampling, and crashes resumed from the WAL.

use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use super::{recorded_totals, stored_totals};
use crate::events::{MessageToolCall, StopReason, TokenUsage, ToolCallId};
use crate::llm::fixture::FixtureClient;
use crate::llm::{ChatResponse, LlmError};
use crate::pricing::{ModelPricing, PricingTable};
use crate::test_support::sim::{SIM_TOOL, SimWorld};
use crate::worker::store::WorkerStore;

/// A seeded xorshift stream, so a failing seed replays.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A price that makes call costs sum inexactly in binary.
fn pricing() -> Arc<PricingTable> {
    let mut entries = std::collections::HashMap::new();
    entries.insert(
        "claude-sim".to_string(),
        ModelPricing {
            input_per_million: 1_234.567,
            output_per_million: 98.7,
            cache_read_per_million: None,
            cache_write_per_million: None,
            cache_write_1h_per_million: None,
        },
    );
    Arc::new(PricingTable::from_map(entries))
}

fn usage(rng: &mut Rng) -> TokenUsage {
    TokenUsage {
        input_tokens: 1 + rng.below(900) as u32,
        output_tokens: rng.below(90) as u32,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        reasoning_tokens: None,
    }
}

fn calls(rng: &mut Rng, names: &[&str]) -> ChatResponse {
    let calls = names
        .iter()
        .map(|name| MessageToolCall {
            tool_call_id: ToolCallId::new(format!("call-{}", rng.next())).unwrap(),
            tool_name: name.to_string(),
            parameters: if *name == SIM_TOOL {
                json!({"step": "sweep"})
            } else {
                json!({"status": "success", "summary": "swept"})
            },
        })
        .collect();
    ChatResponse {
        parts: crate::events::assistant_parts(None, calls),
        stop_reason: StopReason::ToolUse,
        usage: usage(rng),
        reported_cost_usd: None,
    }
}

/// One scripted model turn, or the end of the script.
fn push_turns(rng: &mut Rng, llm: &FixtureClient, turns: u64) {
    for _ in 0..turns {
        match rng.below(10) {
            0..=3 => llm.push_response(calls(rng, &[SIM_TOOL])),
            4 | 5 => {
                let n = 2 + rng.below(2) as usize;
                llm.push_response(calls(rng, &vec![SIM_TOOL; n]));
            }
            // A 200 with nothing in it: an errored row that still costs.
            6 => llm.push_response(ChatResponse {
                parts: vec![],
                stop_reason: StopReason::EndTurn,
                usage: usage(rng),
                reported_cost_usd: None,
            }),
            7 => llm.push_error(LlmError::InvalidResponse("swept".to_string())),
            // Text alone, which the harness answers with a nudge.
            8 => llm.push_response(ChatResponse {
                parts: crate::events::assistant_parts(Some("thinking aloud".to_string()), vec![]),
                stop_reason: StopReason::EndTurn,
                usage: usage(rng),
                reported_cost_usd: None,
            }),
            _ => llm.push_response(calls(rng, &[SIM_TOOL, SIM_TOOL])),
        }
    }
    // Most scripts end by reporting an outcome, some alongside a tool
    // call; the rest run dry, which fails the call.
    match rng.below(4) {
        0 | 1 => llm.push_response(calls(rng, &[crate::tools::REPORT_OUTCOME_CANONICAL_NAME])),
        2 => llm.push_response(calls(
            rng,
            &[SIM_TOOL, crate::tools::REPORT_OUTCOME_CANONICAL_NAME],
        )),
        _ => {}
    }
}

fn totals_of(payload: &crate::events::EventPayload) -> crate::events::RecordedTotals {
    match payload {
        crate::events::EventPayload::Completed(c) => crate::events::RecordedTotals {
            total_llm_calls: c.total_llm_calls,
            total_tool_calls: c.total_tool_calls,
            total_cost: c.total_cost,
        },
        crate::events::EventPayload::Failed(f) => crate::events::RecordedTotals {
            total_llm_calls: f.partial_totals.total_llm_calls,
            total_tool_calls: f.partial_totals.total_tool_calls,
            total_cost: f.partial_totals.total_cost,
        },
        other => panic!("not an ending: {other:?}"),
    }
}

#[derive(Default, Debug)]
struct Seen {
    compared: usize,
    completed: usize,
    failed: usize,
    resumed: usize,
    with_tools: usize,
    with_cost: usize,
    sampled: usize,
}

#[tokio::test]
async fn recorded_totals_equal_the_stored_ending_s_for_every_ending_this_build_stores() {
    let mut seen = Seen::default();
    for seed in 0..120u64 {
        let mut rng = Rng::new(seed);
        let budget = [1_000.0, 2.5, 6.0][rng.below(3) as usize];
        let kind = rng.below(4);
        let world = match kind {
            0 => SimWorld::with_max_iterations(seed, budget, 1 + rng.below(4) as u32).await,
            1 => SimWorld::with_sampling(seed, budget, pricing()).await,
            _ => SimWorld::with_pricing(seed, budget, pricing()).await,
        };
        let llm = FixtureClient::new();
        let turns = rng.below(6);
        push_turns(&mut rng, &llm, turns);
        let crash = rng.below(3) == 0;
        if crash {
            world
                .sink
                .fail_publish_at_invocation_count(1 + rng.below(14) as usize);
        }
        // A sampling world's tool asks the model on its first dispatches,
        // priced like any call and nested inside the tool call.
        if kind == 1 {
            world.tool.sample_on(&[0, 1]);
            let _ = world
                .run_with_sampling(&llm, world.tool.sampling_channel())
                .await;
        } else {
            let _ = world.run(&llm).await;
        }
        let store = WorkerStore::open(&world.worker_db_path()).await.unwrap();
        // A crash that left the run in flight is resumed from the WAL, as
        // the next start would.
        let resumed = crash && !store.find_in_flight_invocations().await.unwrap().is_empty();
        if resumed {
            let more = FixtureClient::new();
            let turns = rng.below(4);
            push_turns(&mut rng, &more, turns);
            let _ = world.resume(&more).await;
        }

        let invocation_id: Uuid = world.invocation_id();
        let Some(row) = store
            .get_invocation_state(&invocation_id.to_string())
            .await
            .unwrap()
        else {
            continue;
        };
        if row.terminal_at.is_none() {
            continue;
        }
        // The sweeper reads the stored ending while it is unpublished.
        let mut unpublished = row.clone();
        unpublished.terminal_published_at = None;
        let Some(stored) = stored_totals(&unpublished) else {
            continue;
        };
        let recorded = recorded_totals(&store, &row.invocation_id).await.unwrap();
        // The figure the run's own ending carried when the runner built it,
        // before any encoding: the event object the sink took or refused.
        let own_id =
            crate::events::Event::from_wire(row.terminal_outcome.as_deref().unwrap().as_bytes())
                .unwrap()
                .envelope
                .event_id;
        let own = world
            .sink
            .events()
            .into_iter()
            .chain(world.sink.refused())
            .find(|event| event.envelope.event_id == own_id)
            .map(|event| totals_of(&event.payload))
            .expect("the run's own ending was offered to the sink");
        assert_eq!(
            (recorded, recorded.total_cost.to_bits()),
            (own, own.total_cost.to_bits()),
            "seed {seed}: the recorded totals are the run's own ending's, field by field and \
             to the bit"
        );
        assert_eq!(
            (recorded, recorded.total_cost.to_bits()),
            (stored, stored.total_cost.to_bits()),
            "seed {seed}: and the stored ending's, read back"
        );
        seen.compared += 1;
        match row.phase.as_str() {
            "completed" => seen.completed += 1,
            _ => seen.failed += 1,
        }
        seen.resumed += usize::from(resumed);
        seen.with_tools += usize::from(stored.total_tool_calls > 0);
        seen.with_cost += usize::from(stored.total_cost > 0.0);
        seen.sampled += world
            .tool
            .sampled()
            .iter()
            .filter(|outcome| **outcome == crate::test_support::sim::SampledOutcome::Answered)
            .count();
    }
    // The sweep reached the shapes it is for.
    assert!(
        seen.compared >= 40
            && seen.completed >= 10
            && seen.failed >= 5
            && seen.resumed >= 3
            && seen.with_tools >= 10
            && seen.with_cost >= 10
            && seen.sampled >= 3,
        "the sweep compared too few endings of some shape: {seen:?}"
    );
}
