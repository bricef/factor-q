//! Synthesis of `self_inspect` results.
//!
//! The `self_inspect` built-in is a **host-fulfilled tool**:
//! the runtime knows the data (current cost, iteration count,
//! configured model, available tools) but a normal
//! [`fq_tools::ToolContext`] cannot reach any of it — the
//! context only carries the sandbox. So
//! [`crate::worker::reducer::ReducerRunner`] intercepts tool
//! calls whose name is
//! [`fq_tools::builtin::SELF_INSPECT_TOOL_NAME`] and calls
//! [`synthesize_self_inspect`] to produce the result.
//!
//! The output is JSON, not free-form text. LLMs parse JSON
//! reliably, and structured output makes the data easier to
//! quote back to the user without re-asserting it (see design
//! principle #1 in `docs/design/committed/design-principles.md`).

use fq_tools::builtin::SELF_INSPECT_SECTIONS;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::events::InvocationTotals;

/// Snapshot of host-side invocation state at the moment the
/// `self_inspect` call is dispatched. Both executor paths
/// build this from their own tracked state and pass it in.
#[derive(Debug, Clone)]
pub struct HostInvocationStats<'a> {
    pub invocation_id: &'a str,
    pub agent_id: &'a str,
    pub model: &'a str,
    pub allowed_tool_names: &'a [String],
    pub budget: Option<f64>,
    pub max_iterations: u32,
    pub totals: InvocationTotals,
    /// Wall-clock duration of the invocation so far, in
    /// milliseconds. The runner already tracks this as part of
    /// the failure path; we surface it here for the agent too.
    pub elapsed_ms: u64,
    /// Prompt tokens the model consumed on the most recent LLM
    /// turn — the agent's current context occupancy. `None`
    /// before the first turn has landed (the runner has no usage
    /// figure to report yet).
    pub tokens_in_use: Option<u32>,
    /// The model's context-window size (max input tokens), from
    /// the pricing/context-window table. `None` when the table
    /// lists no window for the model — reported as unknown rather
    /// than guessed.
    pub context_window_size: Option<u32>,
    /// Number of messages in the conversation history at dispatch
    /// time (system + user + assistant + tool). `None` before the
    /// first turn, when the runner has not yet built a request.
    pub messages_in_history: Option<u32>,
    /// Unix-ms timestamp of the oldest turn in history — the
    /// invocation start, when the first (system/user) messages are
    /// seeded. `None` if unknown.
    pub oldest_turn_at_ms: Option<i64>,
}

/// Parameters accepted by the `self_inspect` tool.
#[derive(Debug, Default, Deserialize)]
struct SelfInspectParams {
    /// Optional subset of sections to return. If omitted,
    /// every section is returned.
    #[serde(default)]
    include: Option<Vec<String>>,
}

/// Build the JSON output for a `self_inspect` call.
///
/// The result is `(output, is_error)`. Today this is always
/// non-error — the synthesis itself can't fail. The tuple shape
/// matches what the executor expects from its synthesis path so
/// adding error branches later is a one-line change.
pub fn synthesize_self_inspect(stats: &HostInvocationStats<'_>, parameters: Value) -> String {
    let params: SelfInspectParams = serde_json::from_value(parameters).unwrap_or_default();
    let want = section_filter(params.include.as_deref());

    let mut out = serde_json::Map::new();

    // Mirrors the FQ_INVOCATION_ID, FQ_AGENT_ID, and FQ_MODEL ambient env;
    // the runner feeds both surfaces from the same values, so they must never drift.
    if want.contains(&"identity") {
        out.insert(
            "identity".to_string(),
            json!({
                "invocation_id": stats.invocation_id,
                "agent_id": stats.agent_id,
                "model": stats.model,
            }),
        );
    }

    if want.contains(&"model") {
        out.insert("model".to_string(), json!(stats.model));
        out.insert("agent_id".to_string(), json!(stats.agent_id));
    }

    if want.contains(&"iterations") {
        out.insert(
            "iterations".to_string(),
            json!({
                "llm_calls_made":   stats.totals.total_llm_calls,
                "tool_calls_made":  stats.totals.total_tool_calls,
                "max_iterations":   stats.max_iterations,
                "elapsed_ms":       stats.elapsed_ms,
            }),
        );
    }

    if want.contains(&"budget") {
        let mut budget_obj = serde_json::Map::new();
        budget_obj.insert("cost_used".to_string(), json!(stats.totals.total_cost));
        if let Some(budget) = stats.budget {
            budget_obj.insert("budget".to_string(), json!(budget));
            let remaining = (budget - stats.totals.total_cost).max(0.0);
            budget_obj.insert("cost_remaining".to_string(), json!(remaining));
        }
        out.insert("budget".to_string(), Value::Object(budget_obj));
    }

    if want.contains(&"context") {
        let mut context_obj = serde_json::Map::new();
        context_obj.insert("tokens_in_use".to_string(), json!(stats.tokens_in_use));
        context_obj.insert(
            "context_window_size".to_string(),
            json!(stats.context_window_size),
        );
        context_obj.insert(
            "messages_in_history".to_string(),
            json!(stats.messages_in_history),
        );
        context_obj.insert("oldest_turn_at".to_string(), json!(stats.oldest_turn_at_ms));
        // Surface the soft-pressure signal in the section itself when
        // both figures are known and usage has crossed the threshold.
        // The runner also emits it once to the event trail; here it is
        // an at-a-glance flag on the read path the agent already reads.
        if context_pressure(stats.tokens_in_use, stats.context_window_size).is_some() {
            context_obj.insert("warning".to_string(), json!(CONTEXT_PRESSURE_WARNING));
        }
        out.insert("context".to_string(), Value::Object(context_obj));
    }

    if want.contains(&"tools") {
        out.insert("tools".to_string(), json!(stats.allowed_tool_names));
    }

    serde_json::to_string(&Value::Object(out))
        .unwrap_or_else(|_| "{\"error\":\"failed to serialise self_inspect output\"}".to_string())
}

/// Fraction of the context window at which the soft warning fires
/// (~80%, per the 2026-07-09 review §8 and the ergonomics doc's
/// self-governance note). Below the threshold there is no signal; at
/// or above it the runner injects the one-shot warning and
/// `self_inspect` flags it.
pub const CONTEXT_PRESSURE_THRESHOLD: f64 = 0.80;

/// The one-shot soft-warning message injected once context occupancy
/// crosses [`CONTEXT_PRESSURE_THRESHOLD`]. Kept as a constant so the
/// runner's event-trail injection and the `self_inspect` context flag
/// carry identical text.
pub const CONTEXT_PRESSURE_WARNING: &str = "context nearly full — wrap up or summarise.";

/// Whether the current occupancy is at or past the soft threshold.
/// Returns the occupancy fraction when both figures are known and the
/// threshold is crossed, `None` otherwise (unknown window, unknown
/// usage, zero window, or below threshold). The runner uses this to
/// decide whether to inject the one-shot warning; `synthesize_self_inspect`
/// uses it to flag the `context` section.
pub fn context_pressure(tokens_in_use: Option<u32>, window: Option<u32>) -> Option<f64> {
    let (used, window) = (tokens_in_use?, window?);
    if window == 0 {
        return None;
    }
    let fraction = used as f64 / window as f64;
    (fraction >= CONTEXT_PRESSURE_THRESHOLD).then_some(fraction)
}

/// Resolve the `include` parameter to a static-string filter.
/// Unknown values are silently ignored — the model passing
/// `"capabilities"` or some made-up key gets back the empty
/// set for that name rather than an error, which is friendlier
/// than failing the whole call.
fn section_filter(include: Option<&[String]>) -> Vec<&'static str> {
    match include {
        None => SELF_INSPECT_SECTIONS.to_vec(),
        Some(items) => SELF_INSPECT_SECTIONS
            .iter()
            .copied()
            .filter(|s| items.iter().any(|i| i == s))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> HostInvocationStats<'static> {
        HostInvocationStats {
            invocation_id: "invocation-123",
            agent_id: "self-aware",
            model: "claude-haiku-4-5",
            allowed_tool_names: &[],
            budget: Some(0.10),
            max_iterations: 20,
            totals: InvocationTotals {
                total_llm_calls: 1,
                total_tool_calls: 0,
                total_cost: 0.000091,
                total_duration_ms: 0,
                sampling_cost: 0.0,
                elicitation_cost: 0.0,
            },
            elapsed_ms: 1234,
            tokens_in_use: Some(1_000),
            context_window_size: Some(200_000),
            messages_in_history: Some(4),
            oldest_turn_at_ms: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn full_output_has_every_section() {
        let names = vec!["self_inspect".to_string()];
        let s = HostInvocationStats {
            allowed_tool_names: &names,
            ..stats()
        };
        let raw = synthesize_self_inspect(&s, Value::Null);
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert!(v.get("identity").is_some());
        assert!(v.get("model").is_some());
        assert!(v.get("iterations").is_some());
        assert!(v.get("budget").is_some());
        assert!(v.get("tools").is_some());
        assert_eq!(
            v["identity"],
            json!({"invocation_id": "invocation-123", "agent_id": "self-aware", "model": "claude-haiku-4-5"})
        );
        assert_eq!(v["model"], "claude-haiku-4-5");
        assert_eq!(v["agent_id"], "self-aware");
    }

    #[test]
    fn include_identity_returns_only_identity() {
        let raw = synthesize_self_inspect(&stats(), json!({"include": ["identity"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v,
            json!({
                "identity": {
                    "invocation_id": "invocation-123",
                    "agent_id": "self-aware",
                    "model": "claude-haiku-4-5"
                }
            })
        );
    }

    #[test]
    fn budget_section_includes_remaining_when_budget_set() {
        let raw = synthesize_self_inspect(&stats(), json!({"include": ["budget"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert!(v.get("budget").is_some());
        assert!(v.get("model").is_none(), "include filter should hide model");
        let budget = &v["budget"];
        assert_eq!(budget["cost_used"], 0.000091);
        assert_eq!(budget["budget"], 0.10);
        // 0.10 - 0.000091 ≈ 0.099909
        let rem = budget["cost_remaining"].as_f64().unwrap();
        assert!((rem - 0.099909).abs() < 1e-9, "remaining was {rem}");
    }

    #[test]
    fn budget_section_omits_remaining_when_no_budget() {
        let s = HostInvocationStats {
            budget: None,
            ..stats()
        };
        let raw = synthesize_self_inspect(&s, json!({"include": ["budget"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        let budget = &v["budget"];
        assert_eq!(budget["cost_used"], 0.000091);
        assert!(budget.get("budget").is_none());
        assert!(budget.get("cost_remaining").is_none());
    }

    #[test]
    fn iterations_section_reports_call_counts() {
        let raw = synthesize_self_inspect(&stats(), json!({"include": ["iterations"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        let iters = &v["iterations"];
        assert_eq!(iters["llm_calls_made"], 1);
        assert_eq!(iters["tool_calls_made"], 0);
        assert_eq!(iters["max_iterations"], 20);
        assert_eq!(iters["elapsed_ms"], 1234);
    }

    #[test]
    fn unknown_include_value_is_silently_ignored() {
        // A model that asks for "capabilities" gets nothing,
        // not an error.
        let raw = synthesize_self_inspect(&stats(), json!({"include": ["capabilities"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 0);
    }

    #[test]
    fn empty_include_returns_empty_object() {
        let raw = synthesize_self_inspect(&stats(), json!({"include": []}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 0);
    }

    #[test]
    fn context_section_reports_window_usage_and_history() {
        let raw = synthesize_self_inspect(&stats(), json!({"include": ["context"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        let context = &v["context"];
        assert_eq!(context["tokens_in_use"], 1_000);
        assert_eq!(context["context_window_size"], 200_000);
        assert_eq!(context["messages_in_history"], 4);
        assert_eq!(context["oldest_turn_at"], 1_700_000_000_000i64);
        // Well under 80% — no warning flag.
        assert!(context.get("warning").is_none());
    }

    #[test]
    fn context_section_is_part_of_the_default_output() {
        let raw = synthesize_self_inspect(&stats(), Value::Null);
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert!(v.get("context").is_some(), "context is a default section");
    }

    #[test]
    fn context_section_reports_unknowns_as_null() {
        let s = HostInvocationStats {
            tokens_in_use: None,
            context_window_size: None,
            messages_in_history: None,
            oldest_turn_at_ms: None,
            ..stats()
        };
        let raw = synthesize_self_inspect(&s, json!({"include": ["context"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        let context = &v["context"];
        assert!(context["tokens_in_use"].is_null());
        assert!(context["context_window_size"].is_null());
        assert!(context["messages_in_history"].is_null());
        assert!(context["oldest_turn_at"].is_null());
        assert!(context.get("warning").is_none());
    }

    #[test]
    fn context_section_flags_warning_past_threshold() {
        let s = HostInvocationStats {
            tokens_in_use: Some(180_000),
            context_window_size: Some(200_000),
            ..stats()
        };
        let raw = synthesize_self_inspect(&s, json!({"include": ["context"]}));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["context"]["warning"], json!(CONTEXT_PRESSURE_WARNING));
    }

    #[test]
    fn context_pressure_thresholds() {
        // Below 80%: no pressure.
        assert!(context_pressure(Some(159_000), Some(200_000)).is_none());
        // At/above 80%: pressure, returning the occupancy fraction.
        assert!(context_pressure(Some(160_000), Some(200_000)).is_some());
        assert!(context_pressure(Some(200_000), Some(200_000)).is_some());
        // Unknown usage or window: no pressure signal.
        assert!(context_pressure(None, Some(200_000)).is_none());
        assert!(context_pressure(Some(160_000), None).is_none());
        // Zero window: guarded, no divide-by-zero.
        assert!(context_pressure(Some(1), Some(0)).is_none());
    }
}
