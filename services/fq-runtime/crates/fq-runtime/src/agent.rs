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

use crate::events::{ConfigSnapshot, SandboxSnapshot};

/// An MCP server declared in an agent definition.
#[derive(Debug, Clone)]
pub struct McpServerDeclaration {
    pub server: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
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
