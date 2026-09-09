//! The cost a provider reports on its own response.
//!
//! OpenRouter returns the billed figure on every chat completion as
//! `usage.cost` (USD, its fee and any cache discount included), with a
//! `cost_details` breakdown beside it. The dependency's normalised
//! `Usage` has no slot for it and drops the key on deserialisation, so
//! the adapter asks for the raw body back and reads the figure from
//! there. The native Anthropic, OpenAI and Gemini wires carry no such
//! field, and read as "not reported".

use serde_json::Value;

/// The `usage.cost` figure from a raw response body, when the body was
/// captured and carries one. A value that is not a finite, non-negative
/// number is a provider bug rather than a cost, and reads as unreported
/// — the same rule the token counts follow.
pub(super) fn from_raw_body(body: Option<&Value>) -> Option<f64> {
    body?
        .get("usage")?
        .get("cost")?
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_openrouter_usage_cost() {
        let body = json!({
            "id": "gen-1",
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cost": 0.000123,
                "cost_details": { "upstream_inference_cost": 0.000117 }
            }
        });
        assert_eq!(from_raw_body(Some(&body)), Some(0.000123));
    }

    #[test]
    fn a_body_without_a_cost_is_unreported() {
        let native = json!({ "usage": { "prompt_tokens": 100, "completion_tokens": 20 } });
        assert_eq!(from_raw_body(Some(&native)), None);
        assert_eq!(from_raw_body(Some(&json!({}))), None);
        assert_eq!(from_raw_body(None), None);
    }

    #[test]
    fn a_cost_that_is_not_a_number_is_unreported() {
        assert_eq!(
            from_raw_body(Some(&json!({ "usage": { "cost": "0.01" } }))),
            None
        );
        assert_eq!(
            from_raw_body(Some(&json!({ "usage": { "cost": -1.0 } }))),
            None
        );
        assert_eq!(
            from_raw_body(Some(&json!({ "usage": { "cost": null } }))),
            None
        );
    }

    #[test]
    fn a_free_call_reports_zero() {
        // Zero is a reported figure — a free model — and stays apart
        // from "not reported".
        assert_eq!(
            from_raw_body(Some(&json!({ "usage": { "cost": 0 } }))),
            Some(0.0)
        );
    }
}
