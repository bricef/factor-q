//! The agent definition domain: the [`Agent`] model, the frontmatter
//! parser, and the directory registry.
//!
//! Both ends of the system read a definition. The daemon loads a whole
//! directory of them at startup, rejects the ones that will not run,
//! and hot-swaps the registry on `fq reload`; the operator wants to
//! check one file before it is ever deployed (`fq agent validate`).
//! Same model, same parser, two callers — which is why this is a crate
//! rather than a module of the runtime, and why it carries no store,
//! no broker and no HTTP client. `fq-runtime` re-exports it under
//! `agent`, so the daemon's call sites read exactly as they did.
//!
//! An [`Agent`] is the validated representation the executor consumes.
//! Agents are constructed via [`AgentBuilder`] with a fluent API.
//! Validation runs at [`AgentBuilder::build`] time and returns a
//! [`BuildError`] if required fields are missing or invalid.
//!
//! The Markdown frontmatter parser in the [`definition`] submodule
//! produces `Agent` values by calling the builder internally.
//! Programmatic construction is equally supported:
//!
//! ```
//! use fq_agent::{Agent, Sandbox};
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
pub mod view;

pub use registry::{AgentRegistry, LoadError, LoadedAgent, RegistryError};

// The identifier and the capability grants ride the wire — an
// [`AgentId`] on every envelope, the grants inside a `triggered`
// event's [`ConfigSnapshot`] — so they live in the contract crate and
// are re-exported here, where the rest of the agent is. What stays is
// everything that *reads* a definition: the parser, the registry, the
// builder, and the validation below.
pub use fq_ops::agent::{
    AgentId, CapabilityValidation, ElicitationGrant, EvaluatorSpec, RootsGrant, SamplingGrant,
};

use fq_ops::events::subjects::SubjectTokenError;
use fq_ops::events::{ConfigSnapshot, Effort, SandboxSnapshot};

/// The token agents write in sandbox paths and tool parameters; the
/// runtime substitutes the invocation's workspace path for it.
///
/// It lives here because it is part of the definition vocabulary — the
/// thing an author types into `sandbox.exec_cwd` — rather than part of
/// the provisioning that later binds it. The runtime's workspace module
/// re-exports it for the substitution sites.
pub const WORKSPACE_TOKEN: &str = "${workspace}";

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

/// A validated agent ready to be executed.
#[derive(Debug, Clone)]
pub struct Agent {
    id: AgentId,
    model: String,
    system_prompt: String,
    tools: Vec<String>,
    sandbox: Sandbox,
    budget: Option<f64>,
    /// Optional per-agent override for the per-invocation LLM-turn cap.
    /// When `None`, the daemon config default (else the built-in
    /// fallback) applies. Overriding here means `fq reload` picks up a
    /// change with no restart (Design Principle 8 / backlog §1.5.1.1).
    max_iterations: Option<u32>,
    effort: Option<Effort>,
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

    /// The agent's per-invocation `max_iterations` override, if the
    /// definition sets one. `None` means "use the daemon config
    /// default" — `max_iterations` in the runtime's `Config`.
    pub fn max_iterations(&self) -> Option<u32> {
        self.max_iterations
    }

    /// The optional per-agent reasoning effort.
    pub fn effort(&self) -> Option<Effort> {
        self.effort
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

    /// The declared `network` allowlist, when non-empty — a declaration
    /// the runtime does **not** enforce (#35).
    ///
    /// `network` is parsed and carried, but no tool consults it: an
    /// agent's `exec` children reach any host regardless of what the
    /// definition declares. The load path calls this to warn loudly
    /// rather than silently honour nothing — an unenforced declared
    /// boundary is a silent trust hazard (design principle 3).
    ///
    /// Deliberately a warning and **not** a load error: definitions
    /// declare `sandbox.network` ahead of enforcement, so rejecting them
    /// would break every such agent at load. Enforcement is tracked by
    /// #208 (filtering proxy) and #209 (ADR-0010 container boundary);
    /// this goes away once `network` is enforced.
    pub fn unenforced_network(&self) -> Option<&[String]> {
        if self.network.is_empty() {
            None
        } else {
            Some(&self.network)
        }
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
    /// [`fq_tools::ToolSandbox`] that tools can check against, binding
    /// `${workspace}` to the invocation's workspace path. Each string
    /// path is converted to a `PathBuf` as-is beyond the substitution;
    /// canonicalisation happens at tool-check time.
    ///
    /// Fails loud when a path uses the token and no workspace is bound
    /// (design principle 7 — an unresolvable grant must not silently
    /// narrow or widen).
    pub fn to_tool_sandbox(
        &self,
        workspace: Option<&std::path::Path>,
    ) -> Result<fq_tools::ToolSandbox, UnboundWorkspace> {
        let mut sb = fq_tools::ToolSandbox::new();
        for path in &self.fs_read {
            sb = sb.allow_read(bind_workspace_path(path, workspace)?);
        }
        for path in &self.fs_write {
            sb = sb.allow_write(bind_workspace_path(path, workspace)?);
        }
        for path in &self.exec_cwd {
            sb = sb.allow_exec_cwd(bind_workspace_path(path, workspace)?);
        }
        // Env-var grants pass through by name (issue #34); no workspace
        // binding — these are variable names, not paths.
        for var in &self.env {
            sb = sb.allow_env(var.clone());
        }
        Ok(sb)
    }
}

/// A sandbox path used `${workspace}` but the invocation has no
/// workspace binding — the daemon has no `[workspace]` configured.
#[derive(Debug, thiserror::Error)]
#[error(
    "sandbox path {path:?} uses ${{workspace}} but no workspace is bound \
     (set [workspace] repo in fq.toml)"
)]
pub struct UnboundWorkspace {
    pub path: String,
}

fn bind_workspace_path(
    raw: &str,
    workspace: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, UnboundWorkspace> {
    if !raw.contains(WORKSPACE_TOKEN) {
        return Ok(std::path::PathBuf::from(raw));
    }
    match workspace {
        Some(ws) => Ok(std::path::PathBuf::from(
            raw.replace(WORKSPACE_TOKEN, &ws.to_string_lossy()),
        )),
        None => Err(UnboundWorkspace {
            path: raw.to_string(),
        }),
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
    max_iterations: Option<u32>,
    effort: Option<Effort>,
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

    /// Override the per-invocation `max_iterations` cap for this agent.
    /// Absent by default — the daemon config default (else the built-in
    /// fallback) applies.
    pub fn max_iterations(mut self, max_iterations: u32) -> Self {
        self.max_iterations = Some(max_iterations);
        self
    }

    /// Set the per-agent reasoning effort.
    pub fn effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
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
            max_iterations: self.max_iterations,
            effort: self.effort,
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

/// An id that is not a legal subject token is, to the builder, an
/// invalid required field. [`AgentId::new`] reports the token
/// predicate's own verdict; this restates it in the builder's terms so
/// `build()` keeps answering with one error type.
impl From<SubjectTokenError> for BuildError {
    fn from(err: SubjectTokenError) -> Self {
        BuildError::InvalidId(format!("agent id {err}"))
    }
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
    fn max_iterations_defaults_to_none() {
        let agent = valid_builder().build().unwrap();
        assert!(agent.max_iterations().is_none());
    }

    #[test]
    fn max_iterations_override_is_stored() {
        let agent = valid_builder().max_iterations(250).build().unwrap();
        assert_eq!(agent.max_iterations(), Some(250));
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
