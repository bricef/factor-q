//! Concrete validation policies for MCP server-initiated calls.
//!
//! [`crate::validation`] defines the generic, provider-neutral seam
//! ([`Validator`](crate::validation::Validator),
//! [`ValidatorChain`](crate::validation::ValidatorChain)); this module
//! holds the concrete factor-q policies that plug into it. They are
//! *synchronous* — pure inspection / transformation of a value — and
//! are declared per agent-def (see the MCP guide) and installed on the
//! runner per invocation (ADR-0017 / ADR-0018).
//!
//! The LLM-backed *evaluators* (approve/deny gates that may run a model
//! judge) are a separate, async mechanism and live with the runner, not
//! here — the sync `Validator` trait cannot host an `await`.

use std::collections::HashMap;

use rmcp::model::{
    CreateElicitationRequestParams, CreateMessageResult, SamplingContent, SamplingMessageContent,
};
use serde_json::Value;

use crate::validation::{Validator, ValidatorResult};

/// Replacement inserted where a high-entropy (secret-looking) token is
/// redacted.
const REDACTION: &str = "[REDACTED]";

/// Minimum token length considered for high-entropy redaction. Short
/// tokens (ordinary words, small numbers) are never flagged.
const MIN_SECRET_LEN: usize = 20;

/// Shannon-entropy threshold (bits/char) above which a long,
/// token-shaped string is treated as a secret. Natural-language words
/// sit below this; random base64 / hex API keys sit above it.
const ENTROPY_THRESHOLD: f64 = 3.5;

/// Field-name substrings that mark an elicitation request as asking for
/// a credential. Matched against separator-stripped, lowercased names,
/// so `api_key` / `apiKey` / `API-KEY` all hit `apikey`. Curated to
/// avoid short ambiguous fragments (`pin`, `cvv`).
const SENSITIVE_FIELD_PATTERNS: &[&str] = &[
    "password",
    "passwd",
    "apikey",
    "secret",
    "token",
    "credential",
    "privatekey",
    "socialsecurity",
    "creditcard",
];

/// Redacts high-entropy, secret-looking tokens from text that crosses
/// the trust boundary — sampled completions returned to a server and
/// structured values elicited from the model. Yields
/// [`ValidatorResult::Modify`] when anything was redacted,
/// [`ValidatorResult::Allow`] otherwise; it never denies (redaction is
/// best-effort hygiene, not a hard gate).
pub struct HighEntropyRedactor;

impl HighEntropyRedactor {
    /// Redact secret-looking tokens in `text`, preserving the
    /// surrounding words and whitespace. Returns `Some(redacted)` if
    /// anything changed, `None` if the text was left untouched.
    fn redact_text(text: &str) -> Option<String> {
        let mut changed = false;
        let out: String = text
            .split_inclusive(char::is_whitespace)
            .map(|chunk| {
                // Separate any single trailing whitespace char so the
                // bare token is tested but the spacing is preserved.
                let token = chunk.trim_end();
                let ws = &chunk[token.len()..];
                if is_secret_like(token) {
                    changed = true;
                    format!("{REDACTION}{ws}")
                } else {
                    chunk.to_string()
                }
            })
            .collect();
        changed.then_some(out)
    }

    /// Recursively redact every string in a JSON value (object values
    /// and array elements; keys are left untouched). Returns `Some(new)`
    /// if anything changed.
    fn redact_value(value: &Value) -> Option<Value> {
        match value {
            Value::String(s) => Self::redact_text(s).map(Value::String),
            Value::Array(items) => {
                let mut changed = false;
                let next: Vec<Value> = items
                    .iter()
                    .map(|item| match Self::redact_value(item) {
                        Some(v) => {
                            changed = true;
                            v
                        }
                        None => item.clone(),
                    })
                    .collect();
                changed.then_some(Value::Array(next))
            }
            Value::Object(map) => {
                let mut changed = false;
                let next: serde_json::Map<String, Value> = map
                    .iter()
                    .map(|(k, v)| match Self::redact_value(v) {
                        Some(nv) => {
                            changed = true;
                            (k.clone(), nv)
                        }
                        None => (k.clone(), v.clone()),
                    })
                    .collect();
                changed.then_some(Value::Object(next))
            }
            // numbers / bools / null carry no redactable text.
            _ => None,
        }
    }
}

impl Validator<Value> for HighEntropyRedactor {
    fn validate(&self, value: &Value) -> ValidatorResult<Value> {
        match Self::redact_value(value) {
            Some(redacted) => ValidatorResult::Modify(redacted),
            None => ValidatorResult::Allow,
        }
    }
}

impl Validator<CreateMessageResult> for HighEntropyRedactor {
    fn validate(&self, value: &CreateMessageResult) -> ValidatorResult<CreateMessageResult> {
        // A factor-q sampling result is always single text content
        // (`model_response_to_create_message`); redact that case and
        // pass anything else through untouched in v1.
        let SamplingContent::Single(SamplingMessageContent::Text(text)) = &value.message.content
        else {
            return ValidatorResult::Allow;
        };
        let Some(redacted) = Self::redact_text(&text.text) else {
            return ValidatorResult::Allow;
        };
        let mut next = value.clone();
        if let SamplingContent::Single(SamplingMessageContent::Text(t)) = &mut next.message.content
        {
            t.text = redacted;
        }
        ValidatorResult::Modify(next)
    }
}

/// Rejects an inbound elicitation request that tries to coax the model
/// into surrendering a credential — either a schema property whose name
/// looks sensitive (e.g. `api_key`), or a message that names one.
/// Yields [`ValidatorResult::Deny`] in that case, [`ValidatorResult::Allow`]
/// otherwise. URL-mode elicitation is out of scope here and passes
/// through (it is declined upstream).
pub struct ValidateRequestPolicy;

impl Validator<CreateElicitationRequestParams> for ValidateRequestPolicy {
    fn validate(
        &self,
        value: &CreateElicitationRequestParams,
    ) -> ValidatorResult<CreateElicitationRequestParams> {
        let CreateElicitationRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = value
        else {
            return ValidatorResult::Allow;
        };

        for field in requested_schema.properties.keys() {
            if is_sensitive_name(field) {
                return ValidatorResult::Deny(format!(
                    "elicitation requests a sensitive field '{field}'"
                ));
            }
        }
        if let Some(hit) = matched_sensitive_pattern(message) {
            return ValidatorResult::Deny(format!(
                "elicitation message references sensitive credentials ('{hit}')"
            ));
        }
        ValidatorResult::Allow
    }
}

/// Whether a whitespace-delimited token looks like a secret: long
/// enough, made only of token characters (base64 / hex / url-safe), and
/// high-entropy.
fn is_secret_like(token: &str) -> bool {
    if token.len() < MIN_SECRET_LEN {
        return false;
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+/=-_.".contains(c))
    {
        return false;
    }
    shannon_entropy(token) >= ENTROPY_THRESHOLD
}

/// Shannon entropy of a string in bits per character.
fn shannon_entropy(s: &str) -> f64 {
    let len = s.chars().count() as f64;
    if len == 0.0 {
        return 0.0;
    }
    let mut counts: HashMap<char, u32> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    counts
        .values()
        .map(|&count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Normalise a name to lowercase with separators stripped, so
/// `api_key` / `apiKey` / `API-KEY` collapse to `apikey`.
fn normalise_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a field name matches any sensitive pattern.
fn is_sensitive_name(name: &str) -> bool {
    let norm = normalise_name(name);
    SENSITIVE_FIELD_PATTERNS.iter().any(|p| norm.contains(p))
}

/// The first sensitive pattern named in free text, if any (normalised
/// the same way as field names).
fn matched_sensitive_pattern(text: &str) -> Option<&'static str> {
    let norm = normalise_name(text);
    SENSITIVE_FIELD_PATTERNS
        .iter()
        .copied()
        .find(|p| norm.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::SamplingMessage;
    use serde_json::json;

    const SECRET: &str = "dGhpc2lzYXZlcnlsb25nc2VjcmV0a2V5MTIzNDU2Nzg5MDEK";

    #[test]
    fn redacts_a_high_entropy_token_and_preserves_surrounding_text() {
        let input = format!("your key is {SECRET} ok");
        let out = HighEntropyRedactor::redact_text(&input).expect("should redact");
        assert_eq!(out, "your key is [REDACTED] ok");
    }

    #[test]
    fn leaves_ordinary_prose_untouched() {
        // Long words stay below the entropy threshold.
        assert!(HighEntropyRedactor::redact_text("the internationalization effort").is_none());
        assert!(HighEntropyRedactor::redact_text("the quick brown fox jumps over").is_none());
    }

    #[test]
    fn redactor_modifies_secret_value_and_allows_clean_value() {
        let redactor = HighEntropyRedactor;
        let dirty = json!({ "note": format!("token {SECRET}"), "n": 3 });
        match redactor.validate(&dirty) {
            ValidatorResult::Modify(v) => {
                assert_eq!(v["note"], json!("token [REDACTED]"));
                assert_eq!(v["n"], json!(3));
            }
            other => panic!("expected Modify, got {other:?}"),
        }

        let clean = json!({ "note": "nothing secret here", "n": 3 });
        assert_eq!(redactor.validate(&clean), ValidatorResult::Allow);
    }

    #[test]
    fn redactor_modifies_secret_in_a_sampling_result() {
        let redactor = HighEntropyRedactor;
        let result = CreateMessageResult::new(
            SamplingMessage::assistant_text(format!("here: {SECRET}")),
            "test-model".to_string(),
        );
        match redactor.validate(&result) {
            ValidatorResult::Modify(v) => {
                let SamplingContent::Single(SamplingMessageContent::Text(t)) = &v.message.content
                else {
                    panic!("expected single text content");
                };
                assert_eq!(t.text, "here: [REDACTED]");
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    fn form_request(message: &str, schema: Value) -> CreateElicitationRequestParams {
        CreateElicitationRequestParams::FormElicitationParams {
            meta: None,
            message: message.to_string(),
            requested_schema: serde_json::from_value(schema).expect("valid elicitation schema"),
        }
    }

    #[test]
    fn request_policy_denies_a_sensitive_schema_field() {
        let policy = ValidateRequestPolicy;
        let req = form_request(
            "Please confirm your details",
            json!({
                "type": "object",
                "properties": { "api_key": { "type": "string" } },
                "required": ["api_key"]
            }),
        );
        match policy.validate(&req) {
            ValidatorResult::Deny(reason) => assert!(reason.contains("api_key")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn request_policy_denies_a_sensitive_message() {
        let policy = ValidateRequestPolicy;
        let req = form_request(
            "What is your account password?",
            json!({
                "type": "object",
                "properties": { "answer": { "type": "string" } }
            }),
        );
        assert!(matches!(policy.validate(&req), ValidatorResult::Deny(_)));
    }

    #[test]
    fn request_policy_allows_a_benign_request() {
        let policy = ValidateRequestPolicy;
        let req = form_request(
            "Which city should I book the flight to?",
            json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
        );
        assert_eq!(policy.validate(&req), ValidatorResult::Allow);
    }
}
