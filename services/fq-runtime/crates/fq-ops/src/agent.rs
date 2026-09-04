//! The agent value types the event vocabulary carries: the validated
//! identifier every envelope is addressed by, and the declarative
//! capability grants a `triggered` event's config snapshot records.
//!
//! Data only. Loading an agent definition, building one, and enforcing
//! a grant at an MCP boundary all stay in `fq-runtime` — this is the
//! part that travels on the wire, so a reader that only renders events
//! links it without the runtime behind it.

use serde::{Deserialize, Serialize};

use crate::events::subjects::{SubjectTokenError, validate_token};

/// Declarative grant for MCP **sampling** (`sampling/createMessage`),
/// the one server-initiated primitive that spends the agent's model
/// budget on a server's behalf (ADR-0017 / ADR-0018). Nothing by
/// default: an agent with no grant declines every sampling request,
/// no model call.
///
/// In v1 this is set programmatically (tests, and any caller that
/// constructs an agent directly); Step 8 parses it from agent
/// frontmatter into this same shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SamplingGrant {
    /// Names of MCP servers (from the `mcp:` block) permitted to
    /// request sampling. A server not listed is declined.
    pub servers: Vec<String>,
    /// Optional aggregate sampling sub-budget (USD) within the
    /// invocation. The runtime declines once cumulative sampling
    /// spend reaches it, *before* the model call. `None` = bounded
    /// only by the invocation budget. `Some(0.0)` = granted in
    /// principle but no spend allowed (useful for tests / dry policy).
    pub max_cost: Option<f64>,
}

impl SamplingGrant {
    /// Whether `server` is permitted to request sampling.
    pub fn permits(&self, server: &str) -> bool {
        self.servers.iter().any(|s| s == server)
    }
}

/// Declarative grant for advertising workspace **roots** to an MCP
/// server (ADR-0017 / ADR-0018). Roots are advisory — they tell a
/// cooperative server its intended filesystem scope; the sandbox /
/// ADR-0010 proxy is the actual wall. A boolean per-server grant,
/// nothing by default. The advertised set is *derived* from the
/// agent's sandbox fs grant (advertised roots ⊆ sandbox boundary —
/// narrowable, never wideable), not configured here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RootsGrant {
    /// Names of MCP servers (from the `mcp:` block) to which the
    /// agent's workspace roots are advertised.
    pub servers: Vec<String>,
}

impl RootsGrant {
    /// Whether `server` is advertised the agent's roots.
    pub fn permits(&self, server: &str) -> bool {
        self.servers.iter().any(|s| s == server)
    }
}

/// Declarative grant for MCP **elicitation** (`elicitation/create`),
/// the server-initiated request for structured user input
/// (ADR-0017 / ADR-0018). factor-q answers it autonomously on the
/// agent's model rather than prompting a human; the schema is a named
/// extraction channel, so this is gated like sampling. Nothing by
/// default. Set programmatically in v1; Step 8 parses it from
/// frontmatter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationGrant {
    /// Names of MCP servers permitted to request elicitation.
    pub servers: Vec<String>,
    /// Optional aggregate elicitation sub-budget (USD) within the
    /// invocation, enforced *before* each model call. `None` = bounded
    /// only by the invocation budget; `Some(0.0)` = no spend allowed.
    pub max_cost: Option<f64>,
}

impl ElicitationGrant {
    /// Whether `server` is permitted to request elicitation.
    pub fn permits(&self, server: &str) -> bool {
        self.servers.iter().any(|s| s == server)
    }
}

/// One stage in a capability's `input_validation` / `output_validation`
/// list. `ApproveAll` / `DenyAll` are deterministic (useful for tests
/// and a hard allow/deny); `Llm` runs a model judge in the runner
/// (reusing the structured-completion primitive), optionally on a
/// cheaper model than the agent's own. Parsed from a frontmatter list,
/// e.g. `[approve_all, { llm: claude-haiku-4-5 }]`.
#[derive(Debug, Clone, PartialEq)]
pub enum EvaluatorSpec {
    /// Always approves — a no-op gate.
    ApproveAll,
    /// Always denies — short-circuits the chain.
    DenyAll,
    /// An LLM judge; `model` overrides the agent's model when set.
    Llm { model: Option<String> },
}

impl serde::Serialize for EvaluatorSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            EvaluatorSpec::ApproveAll => serializer.serialize_str("approve_all"),
            EvaluatorSpec::DenyAll => serializer.serialize_str("deny_all"),
            EvaluatorSpec::Llm { model: None } => serializer.serialize_str("llm"),
            EvaluatorSpec::Llm { model: Some(model) } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("llm", model)?;
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for EvaluatorSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        // A list entry is either a bare token (`approve_all` / `deny_all`
        // / `llm`) or a single-key map (`{ llm: <model> }`).
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Token(String),
            Llm { llm: Option<String> },
        }
        match Repr::deserialize(deserializer)? {
            Repr::Token(token) => match token.as_str() {
                "approve_all" => Ok(EvaluatorSpec::ApproveAll),
                "deny_all" => Ok(EvaluatorSpec::DenyAll),
                "llm" => Ok(EvaluatorSpec::Llm { model: None }),
                other => Err(D::Error::custom(format!(
                    "unknown evaluator '{other}' (expected approve_all, deny_all, llm, or {{ llm: <model> }})"
                ))),
            },
            Repr::Llm { llm } => Ok(EvaluatorSpec::Llm { model: llm }),
        }
    }
}

/// The validation policy for one capability (sampling or elicitation)
/// on one agent. Declared per server in frontmatter (`sampling:` /
/// `elicitation:` as a table) and aggregated here; installed on the
/// runner per invocation. All-default means "no validation" (the
/// nothing-by-default seam stays allow-everything).
///
/// Two layers feed two mechanisms: the boolean flags drive the
/// synchronous validators in the runtime's `policy` module (redactor /
/// request gate), while the `*_validation` lists drive the async
/// evaluator sequence in the runner.
///
/// `deny_unknown_fields` because every field here defaults to *off*, so
/// a key serde does not recognise is not dropped so much as inverted:
/// `redact_secretz: true` parsed as a table with redaction disabled, the
/// opposite of what the author wrote, and nothing said so. A security
/// default is the wrong place for silence (#514 set the rule, #515 and
/// #520 applied it to the frontmatter levels above, #526 to this one).
/// Legal because this struct flattens nothing. This is a wire type
/// (`config_snapshot`, `describe`), so strictness here is wire-visible.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct CapabilityValidation {
    /// Install `HighEntropyRedactor` on the outbound value / result.
    pub redact_secrets: bool,
    /// Install `ValidateRequestPolicy` on the inbound request
    /// (elicitation only; ignored for sampling, which has no schema).
    pub reject_sensitive_fields: bool,
    /// Evaluator gates run on the inbound request, in order.
    pub input_validation: Vec<EvaluatorSpec>,
    /// Evaluator gates run on the outbound value / result, in order.
    pub output_validation: Vec<EvaluatorSpec>,
}

impl CapabilityValidation {
    /// Whether nothing is configured (the default allow-everything seam).
    pub fn is_empty(&self) -> bool {
        !self.redact_secrets
            && !self.reject_sensitive_fields
            && self.input_validation.is_empty()
            && self.output_validation.is_empty()
    }
}

#[cfg(test)]
mod validation_config_tests {
    use super::{CapabilityValidation, EvaluatorSpec};

    #[test]
    fn evaluator_spec_round_trips_every_form() {
        let cases = [
            ("\"approve_all\"", EvaluatorSpec::ApproveAll),
            ("\"deny_all\"", EvaluatorSpec::DenyAll),
            ("\"llm\"", EvaluatorSpec::Llm { model: None }),
            (
                "{\"llm\":\"claude-haiku-4-5\"}",
                EvaluatorSpec::Llm {
                    model: Some("claude-haiku-4-5".to_string()),
                },
            ),
        ];
        for (json, expected) in cases {
            let parsed: EvaluatorSpec = serde_json::from_str(json).expect("parse");
            assert_eq!(parsed, expected, "parsing {json}");
            let reserialised = serde_json::to_string(&parsed).expect("serialise");
            assert_eq!(reserialised, json, "round-trip {json}");
        }
    }

    #[test]
    fn unknown_evaluator_token_is_rejected() {
        assert!(serde_json::from_str::<EvaluatorSpec>("\"sometimes\"").is_err());
    }

    #[test]
    fn capability_validation_parses_a_mixed_list_and_defaults_empty() {
        let cv: CapabilityValidation = serde_json::from_str(
            r#"{ "redact_secrets": true, "output_validation": ["approve_all", { "llm": "claude-haiku-4-5" }, "deny_all"] }"#,
        )
        .expect("parse");
        assert!(cv.redact_secrets);
        assert!(!cv.reject_sensitive_fields);
        assert!(cv.input_validation.is_empty());
        assert_eq!(cv.output_validation.len(), 3);
        assert_eq!(cv.output_validation[2], EvaluatorSpec::DenyAll);
        assert!(!cv.is_empty());

        assert!(CapabilityValidation::default().is_empty());
    }
}

/// A validated agent identifier.
///
/// Enforces that agent IDs are non-empty and compatible with NATS subject
/// tokens (no dots, wildcards, or whitespace). The serde Deserialize impl
/// applies the same validation — events arriving over the wire with a
/// bogus `agent_id` fail to parse rather than landing in the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentId(String);

impl AgentId {
    /// The sentinel agent id used for runtime/system events. System events
    /// share this id so they group together while staying disjoint from
    /// any real agent.
    pub const SYSTEM_STR: &'static str = "system";

    /// The reserved summariser sentinel — see [`AgentId::summary`].
    pub const SUMMARY_STR: &'static str = "summary";

    /// The reserved operator sentinel — see [`AgentId::operator`].
    pub const OPERATOR_STR: &'static str = "operator";

    /// Construct an agent id from a string, validating its shape.
    ///
    /// The error is [`SubjectTokenError`] — the shared predicate's own
    /// verdict, exactly as [`WorkerId`](crate::worker::WorkerId)
    /// reports it. The agent *builder* restates it as its own
    /// `InvalidId`, which is where that framing belongs: an id is a
    /// token here and a required field there.
    pub fn new(s: impl Into<String>) -> Result<Self, SubjectTokenError> {
        let s = s.into();
        validate_token(&s)?;
        Ok(Self(s))
    }

    /// The system sentinel as an [`AgentId`]. Equivalent to
    /// `AgentId::new(Self::SYSTEM_STR).unwrap()` but infallible.
    pub fn system() -> Self {
        // "system" passes `validate_token`; this never panics. The
        // expect-message documents the invariant.
        Self::new(Self::SYSTEM_STR).expect("`system` is a valid agent id")
    }

    /// The invocation-summariser sentinel (#216): the reserved agent
    /// id `invocation.summary` events are emitted under, so the
    /// summariser's own LLM spend appears in `fq costs` as its own row
    /// and is never confused with (or charged to) a real agent.
    pub fn summary() -> Self {
        Self::new(Self::SUMMARY_STR).expect("`summary` is a valid agent id")
    }

    /// The operator sentinel used for operator-authored recovery events.
    pub fn operator() -> Self {
        Self::new(Self::OPERATOR_STR).expect("`operator` is a valid agent id")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the newtype and return the inner `String`. Used at
    /// boundaries that need owned strings (CLI args, etc.).
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for AgentId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for AgentId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<AgentId> for str {
    fn eq(&self, other: &AgentId) -> bool {
        self == other.0.as_str()
    }
}

impl std::str::FromStr for AgentId {
    type Err = SubjectTokenError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl serde::Serialize for AgentId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for AgentId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        validate_token(&s).map_err(serde::de::Error::custom)?;
        Ok(Self(s))
    }
}

#[cfg(test)]
mod agent_id_tests {
    use super::*;

    #[test]
    fn agent_id_with_wildcard_is_rejected() {
        assert!(matches!(
            AgentId::new("agent*").unwrap_err(),
            SubjectTokenError::InvalidChar(_)
        ));
        assert!(matches!(
            AgentId::new("agent>").unwrap_err(),
            SubjectTokenError::InvalidChar(_)
        ));
    }

    #[test]
    fn empty_agent_id_is_rejected() {
        assert!(matches!(
            AgentId::new("").unwrap_err(),
            SubjectTokenError::Empty
        ));
    }

    #[test]
    fn agent_id_serialises_as_a_bare_string() {
        // Newtype must serialise transparently — no `{"AgentId": ...}`
        // wrapper. The wire format is unchanged versus what
        // `String` would produce.
        let id = AgentId::new("researcher").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"researcher\"");
    }

    #[test]
    fn agent_id_round_trips_through_serde() {
        let id = AgentId::new("researcher").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn agent_id_deserialise_rejects_invalid_input() {
        // Wire-boundary protection. An event arriving from NATS
        // with a malformed agent_id must fail to deserialise
        // rather than landing in the runtime as a bypass.
        let cases = [
            "\"\"",        // empty
            "\"foo.bar\"", // contains dot — would break NATS subjects
            "\"agent*\"",  // contains wildcard
            "\"agent>\"",  // contains wildcard
            "\"with space\"",
        ];
        for raw in cases {
            let result: Result<AgentId, _> = serde_json::from_str(raw);
            assert!(
                result.is_err(),
                "AgentId deserialise should have rejected {raw}"
            );
        }
    }

    #[test]
    fn agent_id_system_sentinel_is_valid() {
        // `AgentId::system()` must never panic — the "system"
        // string is statically known to be NATS-subject-safe.
        let id = AgentId::system();
        assert_eq!(id.as_str(), "system");
    }
}
