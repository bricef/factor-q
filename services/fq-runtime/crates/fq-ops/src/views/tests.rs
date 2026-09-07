//! Unit tests for [`super`]: the reasoning-total fold and the JSON
//! shape it reaches the operator in (#536).

use super::*;

/// The fold, all three cases — and the one that looks like a fourth:
/// a reported zero is a report, so it survives the fold as `Some(0)`.
#[test]
fn sum_reported_keeps_an_unreported_split_from_becoming_zero() {
    assert_eq!(
        sum_reported(None, None),
        None,
        "no reports is no total, not zero"
    );
    assert_eq!(sum_reported(Some(45), None), Some(45));
    assert_eq!(sum_reported(None, Some(45)), Some(45));
    assert_eq!(sum_reported(Some(45), Some(3)), Some(48));
    assert_eq!(
        sum_reported(Some(0), None),
        Some(0),
        "a reported zero is a report"
    );
}

/// `fq costs --json`, `fq invocation show --json` and the dashboard's
/// reads all see the same two values: `null` for an unreported split,
/// `0` for a reported zero. The key is always present, so a reader
/// sees `null` rather than nothing.
#[test]
fn reasoning_totals_serialise_null_against_zero() {
    let view = |reasoning: Option<i64>| CostView {
        agent_id: "a".into(),
        event_count: 1,
        total_cost: 0.1,
        total_input_tokens: 1,
        total_output_tokens: 1,
        total_cache_read_tokens: 0,
        total_cache_write_tokens: 0,
        total_reasoning_tokens: reasoning,
        invocation_count: 1,
        framework_cost: 0.0,
    };

    let json = serde_json::to_value(view(None)).unwrap();
    assert!(
        json.get("total_reasoning_tokens")
            .is_some_and(|v| v.is_null()),
        "an unreported split is an explicit null: {json}"
    );
    let json = serde_json::to_value(view(Some(0))).unwrap();
    assert_eq!(json["total_reasoning_tokens"], serde_json::json!(0));

    let back: CostView = serde_json::from_value(serde_json::to_value(view(None)).unwrap()).unwrap();
    assert_eq!(back.total_reasoning_tokens, None);
    let back: CostView =
        serde_json::from_value(serde_json::to_value(view(Some(0))).unwrap()).unwrap();
    assert_eq!(back.total_reasoning_tokens, Some(0));

    // The per-invocation shape carries the same field the same way.
    let inv = InvocationCostView {
        invocation_id: "inv".into(),
        started_at_ms: 0,
        event_count: 1,
        total_cost: 0.1,
        total_input_tokens: 1,
        total_output_tokens: 1,
        total_cache_read_tokens: 0,
        total_cache_write_tokens: 0,
        total_reasoning_tokens: None,
    };
    let json = serde_json::to_value(&inv).unwrap();
    assert!(json["total_reasoning_tokens"].is_null(), "{json}");
}

/// One model's row, for the per-model split.
fn model(name: &str, cache: (i64, i64), reasoning: Option<i64>) -> ModelCostView {
    ModelCostView {
        model: name.into(),
        event_count: 1,
        total_cost: 0.1,
        total_input_tokens: 1,
        total_output_tokens: 1,
        total_cache_read_tokens: cache.0,
        total_cache_write_tokens: cache.1,
        total_reasoning_tokens: reasoning,
    }
}

/// The per-model shape carries the same field the same way — `null`
/// against `0`, the key always present, and each reads back as itself.
#[test]
fn model_reasoning_totals_serialise_null_against_zero() {
    let json = serde_json::to_value(model("claude-opus", (40, 20), None)).unwrap();
    assert!(
        json.get("total_reasoning_tokens")
            .is_some_and(|v| v.is_null()),
        "an unreported split is an explicit null: {json}"
    );
    assert_eq!(json["total_cache_read_tokens"], serde_json::json!(40));
    assert_eq!(json["total_cache_write_tokens"], serde_json::json!(20));
    let json = serde_json::to_value(model("openai/gpt", (0, 0), Some(0))).unwrap();
    assert_eq!(json["total_reasoning_tokens"], serde_json::json!(0));

    let back: ModelCostView =
        serde_json::from_value(serde_json::to_value(model("claude-opus", (0, 0), None)).unwrap())
            .unwrap();
    assert_eq!(back.total_reasoning_tokens, None);
    let back: ModelCostView =
        serde_json::from_value(serde_json::to_value(model("openai/gpt", (0, 0), Some(0))).unwrap())
            .unwrap();
    assert_eq!(back.total_reasoning_tokens, Some(0));
}

/// A subtotal over model rows folds the way the fleet total does: an
/// unreported split contributes nothing and never becomes a zero, a
/// reported zero stays a report, and the cache figures add plainly.
#[test]
fn a_subtotal_over_models_folds_an_unreported_split_away() {
    let fold = |models: &[ModelCostView]| {
        models
            .iter()
            .fold(None, |acc, m| sum_reported(acc, m.total_reasoning_tokens))
    };

    let models = [
        model("claude-opus", (40, 20), None),
        model("openai/gpt", (10, 5), Some(0)),
        model("kimi-k3", (0, 0), Some(1_234)),
    ];
    assert_eq!(
        fold(&models),
        Some(1_234),
        "None + Some(0) + Some(1234): the unreported model contributes nothing"
    );
    let cache_read: i64 = models.iter().map(|m| m.total_cache_read_tokens).sum();
    let cache_write: i64 = models.iter().map(|m| m.total_cache_write_tokens).sum();
    assert_eq!((cache_read, cache_write), (50, 25));

    assert_eq!(
        fold(&[
            model("claude-opus", (0, 0), None),
            model("claude-haiku", (0, 0), None)
        ]),
        None,
        "models none of which reported a split have no subtotal, not zero"
    );
    assert_eq!(
        fold(&[
            model("claude-opus", (0, 0), None),
            model("openai/gpt", (0, 0), Some(0))
        ]),
        Some(0),
        "a reported zero beside an unreported split is a reported zero"
    );
}
