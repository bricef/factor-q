//! Agent type and fluent builder.
//!
//! An [`Agent`] is the validated, runtime representation of an agent that
//! the executor consumes. Agents are constructed via [`AgentBuilder`] with
//! a fluent API. Validation runs at [`AgentBuilder::build`] time and
//! returns a [`BuildError`] if required fields are missing or invalid.
//!
//! The Markdown frontmatter parser in the `definition` submodule produces
//! `Agent` values by calling the builder internally. Programmatic
//! construction is equally supported:
//!
//! ```
//! use fq_runtime::agent::{Agent, Sandbox};
//!
//! let agent = Agent::builder()
//!     .id("researcher")
//!     .model("claude-haiku")
//!     .system_prompt("You are a research agent.")
//!     .tools(["read", "web_search"])
//!     .sandbox(Sandbox::new().fs_read("/project/docs"))
//!     .budget(0.50)
//!     .build()
//!     .unwrap();
//!
//! assert_eq!(agent.id().as_str(), "researcher");
//! ```

pub mod definition;
pub mod registry;

pub use registry::{AgentRegistry, LoadError, LoadedAgent, RegistryError};

use serde::{Deserialize, Serialize};

use crate::events::{ConfigSnapshot, SandboxSnapshot};

/// An MCP server declared in an agent definition.
#[derive(Debug, Clone)]
pub struct McpServerDeclaration {
    pub server: String,
    /// Executable for the stdio transport. `None` when the server is
    /// reached over Streamable HTTP (`url`); exactly one of `command`
    /// or `url` is set.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Streamable HTTP endpoint (the 2025-11-25 remote transport). When
    /// set, `command` / `args` / `env` are unused.
    pub url: Option<String>,
}

/// A concrete MCP resource statically pinned for guaranteed inclusion
/// (the `static_resources:` frontmatter field). Parsed from a
/// `mcp://<server>/<native-uri>` URL: `server` names a server in the
/// `mcp:` block; `uri` is that server's native resource URI. Concrete
/// only — templated resources are model-driven via the read tools,
/// not statically pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticResourcePin {
    pub server: String,
    pub uri: String,
}

impl StaticResourcePin {
    /// Parse a `mcp://<server>/<native-uri>` pin.
    pub fn parse(s: &str) -> Result<Self, BuildError> {
        let rest = s.strip_prefix("mcp://").ok_or_else(|| {
            BuildError::InvalidStaticResource(format!("{s:?}: must start with mcp://"))
        })?;
        let (server, uri) = rest.split_once('/').ok_or_else(|| {
            BuildError::InvalidStaticResource(format!(
                "{s:?}: missing /<resource-uri> after the server name"
            ))
        })?;
        if server.is_empty() || uri.is_empty() {
            return Err(BuildError::InvalidStaticResource(format!(
                "{s:?}: server and resource uri must both be non-empty"
            )));
        }
        Ok(Self {
            server: server.to_string(),
            uri: uri.to_string(),
        })
    }
}

/// Declarative grant for MCP **sampling** (`sampling/createMessage`),
/// the one server-initiated primitive that spends the agent's model
/// budget on a server's behalf (ADR-0017 / ADR-0018). Nothing by
/// default: an agent with no grant declines every sampling request,
/// no model call.
///
/// In v1 this is set programmatically (tests, and any caller that
/// constructs an [`Agent`] directly); Step 8 parses it from agent
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
/// synchronous [`crate::policy`] validators (redactor / request gate),
/// while the `*_validation` lists drive the async evaluator sequence in
/// the runner.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
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

/// A validated agent ready to be executed.
#[derive(Debug, Clone)]
pub struct Agent {
    id: AgentId,
    model: String,
    system_prompt: String,
    tools: Vec<String>,
    sandbox: Sandbox,
    budget: Option<f64>,
    trigger: Option<String>,
    mcp_servers: Vec<McpServerDeclaration>,
    static_resources: Vec<StaticResourcePin>,
    sampling: Option<SamplingGrant>,
    roots: Option<RootsGrant>,
    elicitation: Option<ElicitationGrant>,
    sampling_validation: CapabilityValidation,
    elicitation_validation: CapabilityValidation,
}

impl Agent {
    /// Start building a new agent with a fluent API.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    pub fn id(&self) -> &AgentId {
        &self.id
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    pub fn budget(&self) -> Option<f64> {
        self.budget
    }

    pub fn trigger(&self) -> Option<&str> {
        self.trigger.as_deref()
    }

    pub fn mcp_servers(&self) -> &[McpServerDeclaration] {
        &self.mcp_servers
    }

    /// Concrete MCP resources to always read and inject at invocation
    /// start (the `static_resources:` frontmatter field).
    pub fn static_resources(&self) -> &[StaticResourcePin] {
        &self.static_resources
    }

    /// The agent's MCP sampling grant, if any. `None` means no server
    /// may request sampling (the default — nothing by default).
    pub fn sampling_grant(&self) -> Option<&SamplingGrant> {
        self.sampling.as_ref()
    }

    /// The agent's MCP roots grant, if any. `None` means roots are
    /// advertised to no server (the default — nothing by default).
    pub fn roots_grant(&self) -> Option<&RootsGrant> {
        self.roots.as_ref()
    }

    /// The agent's MCP elicitation grant, if any. `None` means no
    /// server may request elicitation (the default).
    pub fn elicitation_grant(&self) -> Option<&ElicitationGrant> {
        self.elicitation.as_ref()
    }

    /// The agent's MCP **sampling** validation policy (redaction +
    /// evaluator gates). Default-empty = the allow-everything seam.
    pub fn sampling_validation(&self) -> &CapabilityValidation {
        &self.sampling_validation
    }

    /// The agent's MCP **elicitation** validation policy. Default-empty
    /// = the allow-everything seam.
    pub fn elicitation_validation(&self) -> &CapabilityValidation {
        &self.elicitation_validation
    }

    /// Whether this agent grants `server` any inbound MCP capability
    /// (sampling / elicitation / roots). Such servers run as
    /// per-invocation instances with a wired request channel
    /// (ADR-0018), rather than shared at daemon boot.
    pub fn grants_inbound_capability(&self, server: &str) -> bool {
        self.sampling.as_ref().is_some_and(|g| g.permits(server))
            || self.elicitation.as_ref().is_some_and(|g| g.permits(server))
            || self.roots.as_ref().is_some_and(|g| g.permits(server))
    }

    /// Produce a [`ConfigSnapshot`] for inclusion in a `Triggered` event.
    ///
    /// Snapshots are how replay is made meaningful: even if the underlying
    /// agent definition is later modified, the event log still shows
    /// exactly what was running at trigger time.
    pub fn to_snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            name: self.id.as_str().to_string(),
            model: self.model.clone(),
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            sandbox: self.sandbox.to_snapshot(),
            budget: self.budget,
            sampling: self.sampling.clone(),
            roots: self.roots.clone(),
            elicitation: self.elicitation.clone(),
            sampling_validation: self.sampling_validation.clone(),
            elicitation_validation: self.elicitation_validation.clone(),
        }
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

    /// Construct an agent id from a string, validating its shape.
    pub fn new(s: impl Into<String>) -> Result<Self, BuildError> {
        let s = s.into();
        validate(&s)?;
        Ok(Self(s))
    }

    /// The system sentinel as an [`AgentId`]. Equivalent to
    /// `AgentId::new(Self::SYSTEM_STR).unwrap()` but infallible.
    pub fn system() -> Self {
        // "system" passes `validate`; this never panics. The
        // expect-message documents the invariant.
        Self::new(Self::SYSTEM_STR).expect("`system` is a valid agent id")
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

/// Local wrapper around [`crate::events::subjects::validate_token`].
/// Used by both [`AgentId::new`] and the [`serde::Deserialize`] impl
/// so the wire-boundary check is identical to the construction-time
/// check, and so the same predicate applies as for any other NATS-
/// subject-token newtype (e.g. `WorkerId`).
fn validate(s: &str) -> Result<(), BuildError> {
    crate::events::subjects::validate_token(s)
        .map_err(|err| BuildError::InvalidId(format!("agent id {err}")))
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
    type Err = BuildError;
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
        validate(&s).map_err(serde::de::Error::custom)?;
        Ok(Self(s))
    }
}

/// Sandbox configuration declaring what an agent is allowed to access.
///
/// Nothing is permitted by default. Callers explicitly grant access by
/// chaining the fluent setters.
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    fs_read: Vec<String>,
    fs_write: Vec<String>,
    network: Vec<String>,
    env: Vec<String>,
    exec_cwd: Vec<String>,
}

impl Sandbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fs_read(mut self, path: impl Into<String>) -> Self {
        self.fs_read.push(path.into());
        self
    }

    pub fn fs_write(mut self, path: impl Into<String>) -> Self {
        self.fs_write.push(path.into());
        self
    }

    pub fn network(mut self, pattern: impl Into<String>) -> Self {
        self.network.push(pattern.into());
        self
    }

    pub fn env(mut self, var: impl Into<String>) -> Self {
        self.env.push(var.into());
        self
    }

    /// Grant permission to run commands with this path as their
    /// working directory. Distinct from read/write access.
    pub fn exec_cwd(mut self, path: impl Into<String>) -> Self {
        self.exec_cwd.push(path.into());
        self
    }

    pub fn fs_read_paths(&self) -> &[String] {
        &self.fs_read
    }

    pub fn fs_write_paths(&self) -> &[String] {
        &self.fs_write
    }

    pub fn network_patterns(&self) -> &[String] {
        &self.network
    }

    pub fn env_vars(&self) -> &[String] {
        &self.env
    }

    pub fn exec_cwd_paths(&self) -> &[String] {
        &self.exec_cwd
    }

    pub fn to_snapshot(&self) -> SandboxSnapshot {
        SandboxSnapshot {
            fs_read: self.fs_read.clone(),
            fs_write: self.fs_write.clone(),
            network: self.network.clone(),
            env: self.env.clone(),
            exec_cwd: self.exec_cwd.clone(),
        }
    }

    /// Materialise this declarative sandbox into a runtime
    /// [`fq_tools::ToolSandbox`] that tools can check against. Each
    /// string path is converted to a `PathBuf` as-is; canonicalisation
    /// happens at tool-check time.
    pub fn to_tool_sandbox(&self) -> fq_tools::ToolSandbox {
        let mut sb = fq_tools::ToolSandbox::new();
        for path in &self.fs_read {
            sb = sb.allow_read(std::path::PathBuf::from(path));
        }
        for path in &self.fs_write {
            sb = sb.allow_write(std::path::PathBuf::from(path));
        }
        for path in &self.exec_cwd {
            sb = sb.allow_exec_cwd(std::path::PathBuf::from(path));
        }
        sb
    }
}

/// Fluent builder for constructing an [`Agent`].
///
/// Validation is deferred to [`AgentBuilder::build`], which returns a
/// [`BuildError`] if required fields are missing or invalid. This keeps the
/// fluent chain unbroken — no `?` or intermediate `Result` handling on
/// individual setters.
#[derive(Debug, Default)]
pub struct AgentBuilder {
    id: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    tools: Vec<String>,
    sandbox: Sandbox,
    budget: Option<f64>,
    trigger: Option<String>,
    mcp_servers: Vec<McpServerDeclaration>,
    static_resources: Vec<StaticResourcePin>,
    sampling: Option<SamplingGrant>,
    roots: Option<RootsGrant>,
    elicitation: Option<ElicitationGrant>,
    sampling_validation: CapabilityValidation,
    elicitation_validation: CapabilityValidation,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn tool(mut self, tool: impl Into<String>) -> Self {
        self.tools.push(tool.into());
        self
    }

    pub fn sandbox(mut self, sandbox: Sandbox) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn budget(mut self, budget: f64) -> Self {
        self.budget = Some(budget);
        self
    }

    pub fn trigger(mut self, trigger: impl Into<String>) -> Self {
        self.trigger = Some(trigger.into());
        self
    }

    pub fn mcp_servers(mut self, servers: Vec<McpServerDeclaration>) -> Self {
        self.mcp_servers = servers;
        self
    }

    pub fn static_resources(mut self, pins: Vec<StaticResourcePin>) -> Self {
        self.static_resources = pins;
        self
    }

    /// Grant MCP sampling to the named servers (see [`SamplingGrant`]).
    /// Absent by default — nothing by default.
    pub fn sampling_grant(mut self, grant: SamplingGrant) -> Self {
        self.sampling = Some(grant);
        self
    }

    /// Advertise the agent's workspace roots to the named servers
    /// (see [`RootsGrant`]). Absent by default — nothing by default.
    pub fn roots_grant(mut self, grant: RootsGrant) -> Self {
        self.roots = Some(grant);
        self
    }

    /// Grant MCP elicitation to the named servers (see
    /// [`ElicitationGrant`]). Absent by default — nothing by default.
    pub fn elicitation_grant(mut self, grant: ElicitationGrant) -> Self {
        self.elicitation = Some(grant);
        self
    }

    /// Set the MCP **sampling** validation policy (redaction + evaluator
    /// gates). Empty by default — the allow-everything seam.
    pub fn sampling_validation(mut self, validation: CapabilityValidation) -> Self {
        self.sampling_validation = validation;
        self
    }

    /// Set the MCP **elicitation** validation policy. Empty by default.
    pub fn elicitation_validation(mut self, validation: CapabilityValidation) -> Self {
        self.elicitation_validation = validation;
        self
    }

    /// Finalise construction, validating required fields.
    pub fn build(self) -> Result<Agent, BuildError> {
        let id_str = self.id.ok_or(BuildError::MissingField("id"))?;
        let id = AgentId::new(id_str)?;
        let model = self.model.ok_or(BuildError::MissingField("model"))?;
        if model.is_empty() {
            return Err(BuildError::EmptyField("model"));
        }
        let system_prompt = self
            .system_prompt
            .ok_or(BuildError::MissingField("system_prompt"))?;
        if system_prompt.trim().is_empty() {
            return Err(BuildError::EmptyField("system_prompt"));
        }
        if let Some(budget) = self.budget
            && (!budget.is_finite() || budget < 0.0)
        {
            return Err(BuildError::InvalidBudget(budget));
        }

        Ok(Agent {
            id,
            model,
            system_prompt,
            tools: self.tools,
            sandbox: self.sandbox,
            budget: self.budget,
            trigger: self.trigger,
            mcp_servers: self.mcp_servers,
            static_resources: self.static_resources,
            sampling: self.sampling,
            roots: self.roots,
            elicitation: self.elicitation,
            sampling_validation: self.sampling_validation,
            elicitation_validation: self.elicitation_validation,
        })
    }
}

/// Errors from [`AgentBuilder::build`] and related validation.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),

    #[error("required field is empty: {0}")]
    EmptyField(&'static str),

    #[error("invalid agent id: {0}")]
    InvalidId(String),

    #[error("invalid budget: must be finite and non-negative, got {0}")]
    InvalidBudget(f64),

    #[error("invalid static_resources entry: {0}")]
    InvalidStaticResource(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_builder() -> AgentBuilder {
        Agent::builder()
            .id("researcher")
            .model("claude-haiku")
            .system_prompt("You are a research agent.")
    }

    #[test]
    fn builds_minimal_agent() {
        let agent = valid_builder().build().unwrap();
        assert_eq!(agent.id().as_str(), "researcher");
        assert_eq!(agent.model(), "claude-haiku");
        assert_eq!(agent.system_prompt(), "You are a research agent.");
        assert!(agent.tools().is_empty());
        assert!(agent.budget().is_none());
        assert!(agent.trigger().is_none());
    }

    #[test]
    fn builds_full_agent() {
        let agent = Agent::builder()
            .id("researcher")
            .model("claude-haiku")
            .system_prompt("You are a research agent.")
            .tools(["read", "web_search"])
            .tool("grep")
            .sandbox(
                Sandbox::new()
                    .fs_read("/project/docs")
                    .fs_write("/project/out")
                    .network("*.api.internal")
                    .env("RESEARCH_API_KEY"),
            )
            .budget(0.50)
            .trigger("tasks.research.*")
            .build()
            .unwrap();

        assert_eq!(agent.tools(), &["read", "web_search", "grep"]);
        assert_eq!(agent.budget(), Some(0.50));
        assert_eq!(agent.trigger(), Some("tasks.research.*"));
        assert_eq!(
            agent.sandbox().fs_read_paths(),
            &["/project/docs".to_string()]
        );
        assert_eq!(
            agent.sandbox().network_patterns(),
            &["*.api.internal".to_string()]
        );
    }

    #[test]
    fn missing_id_is_error() {
        let err = Agent::builder()
            .model("claude-haiku")
            .system_prompt("...")
            .build()
            .unwrap_err();
        assert!(matches!(err, BuildError::MissingField("id")));
    }

    #[test]
    fn missing_model_is_error() {
        let err = Agent::builder()
            .id("researcher")
            .system_prompt("...")
            .build()
            .unwrap_err();
        assert!(matches!(err, BuildError::MissingField("model")));
    }

    #[test]
    fn missing_prompt_is_error() {
        let err = Agent::builder()
            .id("researcher")
            .model("claude-haiku")
            .build()
            .unwrap_err();
        assert!(matches!(err, BuildError::MissingField("system_prompt")));
    }

    #[test]
    fn empty_prompt_is_error() {
        let err = Agent::builder()
            .id("researcher")
            .model("claude-haiku")
            .system_prompt("   ")
            .build()
            .unwrap_err();
        assert!(matches!(err, BuildError::EmptyField("system_prompt")));
    }

    #[test]
    fn agent_id_with_dot_is_rejected() {
        let err = Agent::builder()
            .id("re.searcher")
            .model("claude-haiku")
            .system_prompt("...")
            .build()
            .unwrap_err();
        assert!(matches!(err, BuildError::InvalidId(_)));
    }

    #[test]
    fn agent_id_with_wildcard_is_rejected() {
        assert!(matches!(
            AgentId::new("agent*").unwrap_err(),
            BuildError::InvalidId(_)
        ));
        assert!(matches!(
            AgentId::new("agent>").unwrap_err(),
            BuildError::InvalidId(_)
        ));
    }

    #[test]
    fn empty_agent_id_is_rejected() {
        assert!(matches!(
            AgentId::new("").unwrap_err(),
            BuildError::InvalidId(_)
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

    #[test]
    fn negative_budget_is_rejected() {
        let err = valid_builder().budget(-0.50).build().unwrap_err();
        assert!(matches!(err, BuildError::InvalidBudget(_)));
    }

    #[test]
    fn nan_budget_is_rejected() {
        let err = valid_builder().budget(f64::NAN).build().unwrap_err();
        assert!(matches!(err, BuildError::InvalidBudget(_)));
    }

    #[test]
    fn to_snapshot_captures_all_fields() {
        let agent = Agent::builder()
            .id("researcher")
            .model("claude-haiku")
            .system_prompt("prompt")
            .tools(["read"])
            .sandbox(Sandbox::new().fs_read("/docs"))
            .budget(0.25)
            .build()
            .unwrap();

        let snapshot = agent.to_snapshot();
        assert_eq!(snapshot.name, "researcher");
        assert_eq!(snapshot.model, "claude-haiku");
        assert_eq!(snapshot.system_prompt, "prompt");
        assert_eq!(snapshot.tools, vec!["read"]);
        assert_eq!(snapshot.sandbox.fs_read, vec!["/docs"]);
        assert_eq!(snapshot.budget, Some(0.25));
    }
}
