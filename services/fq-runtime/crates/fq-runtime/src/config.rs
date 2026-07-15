//! Runtime configuration.
//!
//! Configuration is loaded from a TOML file (typically `fq.toml` at the
//! project root), with sensible defaults for unspecified fields. Secrets
//! are never stored in the config file — the config names the environment
//! variable that holds each secret, and the runtime reads the variable at
//! use time.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Default name of the config file at the project root.
pub const DEFAULT_CONFIG_FILENAME: &str = "fq.toml";

/// Runtime configuration for the factor-q daemon.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub nats: NatsConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub state: StateConfig,
    /// Daemon default cap on LLM turns per invocation. A per-agent
    /// `max_iterations` in an agent definition overrides this; when
    /// neither is set the built-in fallback
    /// ([`crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS`])
    /// applies (Design Principle 8 — tunable parameters are
    /// configuration, not code).
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// How long `fq drain` (ADR-0027) waits for in-flight invocations to
    /// suspend at a step boundary before hard-stopping the stragglers and
    /// letting the next binary's recovery resume them. A bounded wait,
    /// never block-forever. Config, not code (Design Principle 8).
    #[serde(default = "default_drain_deadline_ms")]
    pub drain_deadline_ms: u64,
    #[serde(default)]
    pub read_service: ReadServiceConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub summary: SummaryConfig,
}

/// The in-daemon read-only operator service (#105 layer 2) — the tarpc
/// surface the CLI's remote reads and `fq-dashboard` poll. Off by
/// default; the daemon refuses a non-loopback bind because the service
/// is unauthenticated (same posture as NATS / `fq-cas serve`).
#[derive(Debug, Clone, Deserialize)]
pub struct ReadServiceConfig {
    /// Start the service with `fq run`.
    #[serde(default)]
    pub enabled: bool,
    /// Loopback bind address for the tarpc listener.
    #[serde(default = "default_read_service_bind")]
    pub bind: String,
    /// Upper bound on the JetStream health probe inside `health()`, so
    /// a wedged broker cannot wedge the health surface.
    #[serde(default = "default_read_service_probe_timeout_ms")]
    pub probe_timeout_ms: u64,
}

impl Default for ReadServiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_read_service_bind(),
            probe_timeout_ms: default_read_service_probe_timeout_ms(),
        }
    }
}

fn default_read_service_bind() -> String {
    "127.0.0.1:9471".to_string()
}

fn default_read_service_probe_timeout_ms() -> u64 {
    2_000
}

/// Built-in tool configuration — `[tools]` in `fq.toml`. Today only the
/// `exec` tool exposes knobs; future built-ins add their own subsections
/// here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolsConfig {
    #[serde(default)]
    pub exec: ExecToolConfig,
}

/// Timeouts for the built-in `exec` tool — `[tools.exec]` in `fq.toml`.
///
/// The `fq-tools` crate keeps its own conservative defaults (30s default
/// / 300s max) so the primitive is safe in isolation; the runtime raises
/// them here (120s / 600s) because a fleet agent running a full `just ci`
/// legitimately needs headroom the crate-level ceiling would clamp away.
/// Tunable parameters are configuration, not code (Design Principle 8).
#[derive(Debug, Clone, Deserialize)]
pub struct ExecToolConfig {
    /// Timeout applied when a caller does not request one, in seconds.
    #[serde(default = "default_exec_default_timeout_secs")]
    pub default_timeout_secs: u64,
    /// Hard ceiling on any single `exec` call, in seconds. A
    /// caller-supplied `timeout_secs` above this is clamped down, not
    /// rejected, to avoid trapping an agent in a retry loop.
    #[serde(default = "default_exec_max_timeout_secs")]
    pub max_timeout_secs: u64,
}

fn default_exec_default_timeout_secs() -> u64 {
    120
}

fn default_exec_max_timeout_secs() -> u64 {
    600
}

impl Default for ExecToolConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: default_exec_default_timeout_secs(),
            max_timeout_secs: default_exec_max_timeout_secs(),
        }
    }
}

impl ExecToolConfig {
    /// Convert to the `fq-tools` [`ExecConfig`](fq_tools::builtin::ExecConfig),
    /// mapping the two configured timeouts and preserving the crate's
    /// defaults for the fields this section does not expose
    /// (`max_output_bytes`, `default_path`).
    pub fn to_exec_config(&self) -> fq_tools::builtin::ExecConfig {
        fq_tools::builtin::ExecConfig {
            default_timeout: Duration::from_secs(self.default_timeout_secs),
            max_timeout: Duration::from_secs(self.max_timeout_secs),
            ..fq_tools::builtin::ExecConfig::default()
        }
    }
}

/// Control-plane state-retention knobs. Drives the
/// `invocation_archive` retention sweep (step 10 of
/// data-architecture-v1).
#[derive(Debug, Clone, Deserialize)]
pub struct StateConfig {
    /// How long to keep `invocation_archive` rows before the
    /// retention sweep deletes them. Default 30 days. Set to
    /// `-1` to disable the sweep entirely.
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,
    /// How often the retention sweep runs, in seconds.
    /// Default 1 hour. Production doesn't need faster; tests
    /// override via `[state]` in their config.
    #[serde(default = "default_sweep_interval_seconds")]
    pub sweep_interval_seconds: u64,
}

fn default_retention_days() -> i64 {
    30
}

fn default_sweep_interval_seconds() -> u64 {
    3_600
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
            sweep_interval_seconds: default_sweep_interval_seconds(),
        }
    }
}

/// Worker-side knobs. Today only the archive hand-off retry
/// cadence and the warn-after threshold are operator-tunable;
/// the heartbeat cadence is a const because changing it
/// independently of the control-plane's stale threshold would
/// change semantics.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerConfig {
    /// How often the archive retry sweeper republishes pending
    /// `invocation.archived` events, in milliseconds. Default
    /// 10_000 (10s). Lower values shorten time-to-recovery
    /// after a control-plane restart at the cost of more NATS
    /// traffic during sustained outages.
    #[serde(default = "default_archive_retry_interval_ms")]
    pub archive_retry_interval_ms: u64,
    /// How long after `terminal_at` (in ms) the sweeper logs a
    /// warning once per pending row. Default 60_000 (60s). The
    /// sweeper keeps republishing past this point — the warn is
    /// the operator-visible signal that the control-plane is
    /// not acknowledging in a reasonable time.
    #[serde(default = "default_archive_warn_after_ms")]
    pub archive_warn_after_ms: i64,
    /// Retry policy for transient LLM API errors (rate limits, transport
    /// failures). Retrying is safe — a model call is idempotent — and does
    /// not consume a reducer iteration. Tuning knobs, so configuration
    /// (design principle 8), overridable in `fq.toml`.
    #[serde(default)]
    pub llm_retry: crate::llm::RetryConfig,
    /// How many invocations one daemon runs concurrently (#70, the
    /// parallel-workers plan). Default 1 — the serial behavior — until
    /// the Phase-2 concurrent recovery/drain/shutdown gate is green.
    /// Bounds *dispatcher-run* invocations only: startup recovery has
    /// always resumed every recoverable invocation concurrently and is
    /// not gated by this. Until a fleet-level cost cap lands (#42),
    /// this is the only steady-state concurrency spend guardrail, so
    /// raise deliberately.
    #[serde(default = "default_max_concurrent_invocations")]
    pub max_concurrent_invocations: usize,
}

fn default_max_concurrent_invocations() -> usize {
    1
}

fn default_archive_retry_interval_ms() -> u64 {
    crate::worker::archive_retry::DEFAULT_RETRY_INTERVAL_MS
}

fn default_archive_warn_after_ms() -> i64 {
    crate::worker::archive_retry::DEFAULT_WARN_AFTER_MS
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            archive_retry_interval_ms: default_archive_retry_interval_ms(),
            archive_warn_after_ms: default_archive_warn_after_ms(),
            llm_retry: crate::llm::RetryConfig::default(),
            max_concurrent_invocations: default_max_concurrent_invocations(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NatsConfig {
    #[serde(default = "default_nats_url")]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentsConfig {
    #[serde(default = "default_agents_directory")]
    pub directory: PathBuf,
    /// Fallback model for definitions that omit `model:` in their
    /// frontmatter — the worker default (ADR-0003). When set, an agent
    /// with no explicit model inherits this; when `None`, every agent
    /// must name its own model or fail to load. Must itself be a
    /// declared, priced model (see [`validate_model_registry`]).
    #[serde(default)]
    pub default_model: Option<String>,
}

/// Per-invocation workspace binding (parallel-workers plan, Phase 0 —
/// #14/#70). Agents reference their working directory as `${workspace}`;
/// this section says what the token binds to. Deliberately mechanism-
/// free: the runtime provisions *directories* and never touches a VCS —
/// populating a workspace (cloning an upstream, branching, …) is the
/// agent's job through its granted tools.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceConfig {
    /// The workspace path. With `per_invocation = false` this directory
    /// itself is the `${workspace}` binding, shared by every invocation
    /// (today's behavior); with `per_invocation = true` it is the root
    /// under which each invocation gets a fresh empty directory named
    /// by its invocation id. When unset, `${workspace}` is unbound and
    /// any agent that uses the token fails loudly at invocation start.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Provision a fresh empty directory per invocation. Default off —
    /// the rollback switch back to the single-shared-directory behavior.
    #[serde(default)]
    pub per_invocation: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProvidersConfig {
    pub anthropic: Option<AnthropicConfig>,
    /// Additional named providers — `[providers.<name>]` for any name
    /// other than `anthropic`. Each declares an API shape, endpoint,
    /// auth env var, and the model ids it serves, so non-Anthropic
    /// models become available by configuration (ADR-0003).
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, ProviderConfig>,
}

impl ProvidersConfig {
    /// Every model id declared across all providers (anthropic + extra) —
    /// the registry. An agent may only name a model in this set.
    pub fn declared_models(&self) -> impl Iterator<Item = &str> {
        self.anthropic
            .iter()
            .flat_map(|a| a.models.iter())
            .chain(self.extra.values().flat_map(|p| p.models.iter()))
            .map(String::as_str)
    }

    /// Every per-model price override across all providers, as
    /// `(model_id, override)`.
    pub fn pricing_overrides(&self) -> impl Iterator<Item = (&str, &ModelPriceOverride)> {
        self.anthropic
            .iter()
            .flat_map(|a| a.pricing.iter())
            .chain(self.extra.values().flat_map(|p| p.pricing.iter()))
            .map(|(k, v)| (k.as_str(), v))
    }
}

/// Error listing every model-registry / pricing-coverage violation found
/// at startup. Fail-fast: the daemon refuses to run rather than let an
/// undeclared or unpriced model silently defeat budget enforcement
/// (ADR-0004) by tracking its cost as $0.
#[derive(Debug, thiserror::Error)]
#[error("model registry validation failed:\n  - {}", .problems.join("\n  - "))]
pub struct ModelRegistryError {
    problems: Vec<String>,
}

impl ModelRegistryError {
    /// The individual violation messages.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }
}

/// Validate the model registry and pricing coverage at startup — the
/// ADR-0004 invariant *"a model is available iff it is declared,
/// routable, and priced."*
///
/// 1. every agent's resolved model is **declared** (in some provider's
///    `models = [...]`);
/// 2. the `default_model`, if set, is declared;
/// 3. every declared model resolves to a **price** (the LiteLLM table or
///    a `[providers.<name>.pricing]` override merged into `pricing`).
///
/// All violations are collected so the operator sees the full list at
/// once. `agent_models` is `(agent_id, model)` for readable errors.
pub fn validate_model_registry(
    providers: &ProvidersConfig,
    default_model: Option<&str>,
    agent_models: &[(String, String)],
    pricing: &crate::pricing::PricingTable,
) -> Result<(), ModelRegistryError> {
    use std::collections::BTreeSet;
    let declared: BTreeSet<&str> = providers.declared_models().collect();
    let mut problems = Vec::new();

    if let Some(dm) = default_model
        && !declared.contains(dm)
    {
        problems.push(format!(
            "agents.default_model = \"{dm}\" is not declared under any [providers.<name>] models = [...]"
        ));
    }

    for (id, model) in agent_models {
        if !declared.contains(model.as_str()) {
            problems.push(format!(
                "agent \"{id}\" uses model \"{model}\", not declared under any [providers.<name>] models = [...]"
            ));
        }
    }

    for &model in &declared {
        if pricing.lookup(model).is_none() {
            problems.push(format!(
                "model \"{model}\" is declared but has no pricing — add [providers.<name>.pricing.\"{model}\"] or ensure the LiteLLM table lists it"
            ));
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(ModelRegistryError { problems })
    }
}

/// API wire shape for a provider — which genai adapter format it speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiShape {
    #[default]
    Anthropic,
    Openai,
    Gemini,
    Ollama,
    OpenaiCompatible,
}

/// A configurable LLM provider: an API shape, an optional endpoint
/// override, an auth env var, and the model ids routed to it.
/// `[providers.<name>]` in `fq.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_shape: ApiShape,
    /// Endpoint override; `None` uses genai's default for the shape.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var holding this provider's API key.
    pub api_key_env: String,
    /// Model ids routed to this provider's endpoint + auth. Also the
    /// provider's slice of the model **registry**: an agent may only
    /// name a model that some provider declares here.
    #[serde(default)]
    pub models: Vec<String>,
    /// Per-model price overrides — `[providers.<name>.pricing."<model>"]`.
    /// Merged over the LiteLLM table so models the table doesn't list
    /// (custom endpoints, OpenRouter-namespaced ids) are still priced,
    /// which the startup pricing guarantee requires (ADR-0004).
    #[serde(default)]
    pub pricing: std::collections::BTreeMap<String, ModelPriceOverride>,
}

/// A per-model price override in USD per **million** tokens. Merged into
/// the [`crate::pricing::PricingTable`] at startup so an operator can
/// guarantee coverage for a model the LiteLLM table doesn't list.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ModelPriceOverride {
    /// Input (prompt) price, USD per million tokens.
    pub input_per_mtok: f64,
    /// Output (completion) price, USD per million tokens.
    pub output_per_mtok: f64,
    /// Cache-read price; `None` charges cache reads at the input rate.
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
    /// Cache-write price; `None` charges cache writes at the input rate.
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
}

impl ModelPriceOverride {
    /// Convert to a [`crate::pricing::ModelPricing`] entry. The units
    /// already match — the pricing table is keyed in USD per million
    /// tokens — so this is a field copy.
    pub fn to_pricing(&self) -> crate::pricing::ModelPricing {
        crate::pricing::ModelPricing {
            input_per_million: self.input_per_mtok,
            output_per_million: self.output_per_mtok,
            cache_read_per_million: self.cache_read_per_mtok,
            cache_write_per_million: self.cache_write_per_mtok,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicConfig {
    #[serde(default = "default_anthropic_api_key_env")]
    pub api_key_env: String,
    /// Optional override for the Anthropic API base URL. When `None`
    /// the genai crate uses Anthropic's public endpoint. Set this to
    /// point at a test mock, an internal proxy, or a future
    /// Bedrock-compatible endpoint.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Anthropic's slice of the model **registry**. Routing for
    /// `claude-*` stays native (genai resolves it), so this list is
    /// purely the declaration that makes those models usable and
    /// subject to the pricing guarantee — list every `claude-*` id the
    /// fleet uses.
    #[serde(default)]
    pub models: Vec<String>,
    /// Per-model price overrides — `[providers.anthropic.pricing."<model>"]`.
    /// Rarely needed (LiteLLM lists Anthropic models), but available for
    /// parity with other providers.
    #[serde(default)]
    pub pricing: std::collections::BTreeMap<String, ModelPriceOverride>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    /// Directory where factor-q writes cache files (e.g. the LiteLLM
    /// pricing snapshot). Defaults to the system cache directory — see
    /// [`crate::pricing::default_cache_dir`] for the resolution order.
    #[serde(default = "default_cache_dir_for_config")]
    pub directory: PathBuf,
}

/// `[summary]` — the invocation summariser (#216): a cheap model that
/// keeps a one-line, operator-facing status per invocation on the
/// dashboard. Disabled unless `model` is set. The model resolves
/// through `[providers]` like any other, and the startup pricing
/// guarantee (ADR-0004) applies to it — the summariser's spend is
/// itself cost-accounted, under the reserved `summary` agent id.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SummaryConfig {
    /// Summariser model id (e.g. `claude-haiku-4-5`, or an
    /// `openrouter/...` flash-class model). `None` disables the
    /// summariser entirely.
    #[serde(default)]
    pub model: Option<String>,
    /// Hard cap on the summary line length, enforced host-side.
    #[serde(default = "default_summary_max_line_chars")]
    pub max_line_chars: usize,
}

fn default_summary_max_line_chars() -> usize {
    120
}

fn default_nats_url() -> String {
    "nats://localhost:4222".to_string()
}

fn default_cache_dir_for_config() -> PathBuf {
    crate::pricing::default_cache_dir()
}

fn default_agents_directory() -> PathBuf {
    PathBuf::from("agents")
}

fn default_max_iterations() -> u32 {
    crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS
}

/// Default graceful-drain deadline: 120s. Long enough for a typical
/// model/tool step to finish so the invocation suspends at the next
/// boundary; past it, `fq drain` hard-stops and recovery takes over.
fn default_drain_deadline_ms() -> u64 {
    120_000
}

fn default_anthropic_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            url: default_nats_url(),
        }
    }
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            directory: default_agents_directory(),
            default_model: None,
        }
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_anthropic_api_key_env(),
            base_url: None,
            models: Vec::new(),
            pricing: std::collections::BTreeMap::new(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            directory: default_cache_dir_for_config(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            nats: NatsConfig::default(),
            agents: AgentsConfig::default(),
            workspace: WorkspaceConfig::default(),
            providers: ProvidersConfig {
                anthropic: Some(AnthropicConfig::default()),
                extra: Default::default(),
            },
            cache: CacheConfig::default(),
            worker: WorkerConfig::default(),
            state: StateConfig::default(),
            summary: SummaryConfig::default(),
            max_iterations: default_max_iterations(),
            drain_deadline_ms: default_drain_deadline_ms(),
            read_service: ReadServiceConfig::default(),
            tools: ToolsConfig::default(),
        }
    }
}

impl Config {
    /// Parse configuration from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|err| ConfigError::InvalidToml(err.to_string()))
    }

    /// Load configuration from a file, returning an error if the file is
    /// missing or malformed.
    ///
    /// Relative paths in the config are resolved against the directory
    /// containing the config file, not the process's current working
    /// directory. This matches the conventional expectation for config
    /// files (`cargo`, `git`, etc).
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path).map_err(|err| ConfigError::ReadFile {
            path: path.to_path_buf(),
            source: err,
        })?;
        let mut config = Self::from_toml_str(&content)?;
        let base = path.parent().unwrap_or(Path::new(""));
        config.resolve_paths_relative_to(base);
        Ok(config)
    }

    /// Resolve any relative paths in the config against a given base
    /// directory. Absolute paths are left unchanged.
    fn resolve_paths_relative_to(&mut self, base: &Path) {
        if base.as_os_str().is_empty() {
            return;
        }
        if self.agents.directory.is_relative() {
            self.agents.directory = base.join(&self.agents.directory);
        }
        if self.cache.directory.is_relative() {
            self.cache.directory = base.join(&self.cache.directory);
        }
    }

    /// Load configuration from a file, or return the default config if the
    /// file does not exist. Other errors (malformed TOML, I/O errors) are
    /// still surfaced.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match Self::from_file(path) {
            Ok(config) => Ok(config),
            Err(ConfigError::ReadFile { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(Self::default())
            }
            Err(other) => Err(other),
        }
    }

    /// Resolve the Anthropic API key from the configured environment
    /// variable.
    ///
    /// Returns an error if no Anthropic provider is configured, or if the
    /// environment variable is unset or empty.
    pub fn resolve_anthropic_api_key(&self) -> Result<String, ConfigError> {
        let anthropic = self
            .providers
            .anthropic
            .as_ref()
            .ok_or(ConfigError::ProviderNotConfigured("anthropic"))?;
        let value =
            std::env::var(&anthropic.api_key_env).map_err(|_| ConfigError::SecretNotSet {
                env_var: anthropic.api_key_env.clone(),
            })?;
        if value.is_empty() {
            return Err(ConfigError::SecretNotSet {
                env_var: anthropic.api_key_env.clone(),
            });
        }
        Ok(value)
    }
}

/// Errors arising from configuration loading and secret resolution.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid TOML in config file: {0}")]
    InvalidToml(String),

    #[error("provider '{0}' is not configured")]
    ProviderNotConfigured(&'static str),

    #[error("required secret not set in environment variable: {env_var}")]
    SecretNotSet { env_var: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let config = Config::default();
        assert_eq!(config.nats.url, "nats://localhost:4222");
        assert_eq!(config.agents.directory, PathBuf::from("agents"));
        assert_eq!(
            config.providers.anthropic.unwrap().api_key_env,
            "ANTHROPIC_API_KEY"
        );
    }

    #[test]
    fn parses_full_toml() {
        let toml = r#"
[nats]
url = "nats://custom:4222"

[agents]
directory = "my-agents"

[providers.anthropic]
api_key_env = "MY_ANTHROPIC_KEY"
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.nats.url, "nats://custom:4222");
        assert_eq!(config.agents.directory, PathBuf::from("my-agents"));
        assert_eq!(
            config.providers.anthropic.unwrap().api_key_env,
            "MY_ANTHROPIC_KEY"
        );
    }

    #[test]
    fn parses_empty_toml_as_defaults() {
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.nats.url, "nats://localhost:4222");
        assert_eq!(config.agents.directory, PathBuf::from("agents"));
    }

    #[test]
    fn extra_providers_parse_as_a_flattened_map() {
        let toml = r#"
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"

[providers.openai]
api_shape = "openai"
api_key_env = "OPENAI_API_KEY"
models = ["gpt-4o-mini"]

[providers.groq]
api_shape = "openai-compatible"
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
models = ["llama-3.1-8b-instant"]
"#;
        let config = Config::from_toml_str(toml).unwrap();
        // anthropic stays on its own named field (back-compat)
        assert!(config.providers.anthropic.is_some());
        // the rest land in the flattened `extra` map, keyed by name
        let extra = &config.providers.extra;
        assert_eq!(
            extra.len(),
            2,
            "keys: {:?}",
            extra.keys().collect::<Vec<_>>()
        );
        let openai = extra.get("openai").expect("openai provider");
        assert_eq!(openai.api_shape, ApiShape::Openai);
        assert_eq!(openai.api_key_env, "OPENAI_API_KEY");
        assert_eq!(openai.models, vec!["gpt-4o-mini".to_string()]);
        let groq = extra.get("groq").expect("groq provider");
        assert_eq!(groq.api_shape, ApiShape::OpenaiCompatible);
        assert_eq!(
            groq.base_url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
    }

    fn priced(input: f64, output: f64) -> crate::pricing::ModelPricing {
        crate::pricing::ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cache_read_per_million: None,
            cache_write_per_million: None,
        }
    }

    #[test]
    fn validate_model_registry_flags_undeclared_and_unpriced() {
        let toml = r#"
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
models = ["claude-haiku-4-5"]

[providers.openrouter]
api_shape = "openai-compatible"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
models = ["openai/gpt-4o-mini"]
"#;
        let config = Config::from_toml_str(toml).unwrap();
        // claude priced; openai/gpt-4o-mini deliberately left unpriced.
        let mut pricing = crate::pricing::PricingTable::empty();
        pricing.insert("claude-haiku-4-5", priced(1.0, 2.0));

        let agents = vec![("triage".to_string(), "undeclared-model".to_string())];
        let err = validate_model_registry(
            &config.providers,
            Some("also-undeclared"),
            &agents,
            &pricing,
        )
        .expect_err("expected registry violations");
        let problems = err.problems();

        assert!(
            problems
                .iter()
                .any(|p| p.contains("default_model") && p.contains("also-undeclared")),
            "missing default_model violation: {problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("triage") && p.contains("undeclared-model")),
            "missing agent-model violation: {problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("openai/gpt-4o-mini") && p.contains("no pricing")),
            "missing unpriced violation: {problems:?}"
        );
    }

    #[test]
    fn validate_model_registry_passes_when_declared_and_priced() {
        let toml = r#"
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
models = ["claude-haiku-4-5"]
"#;
        let config = Config::from_toml_str(toml).unwrap();
        let mut pricing = crate::pricing::PricingTable::empty();
        pricing.insert("claude-haiku-4-5", priced(1.0, 2.0));
        let agents = vec![("triage".to_string(), "claude-haiku-4-5".to_string())];
        validate_model_registry(
            &config.providers,
            Some("claude-haiku-4-5"),
            &agents,
            &pricing,
        )
        .expect("declared + priced should validate");
    }

    #[test]
    fn pricing_override_from_toml_makes_a_model_priced() {
        // Exercises the `[providers.<name>.pricing."<model>"]` shape and
        // the override -> table merge, then validation over it.
        let toml = r#"
[providers.groq]
api_shape = "openai-compatible"
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
models = ["llama-3.1-8b-instant"]
[providers.groq.pricing."llama-3.1-8b-instant"]
input_per_mtok = 0.05
output_per_mtok = 0.08
"#;
        let config = Config::from_toml_str(toml).unwrap();
        let mut pricing = crate::pricing::PricingTable::empty();
        for (model, ov) in config.providers.pricing_overrides() {
            pricing.insert(model.to_string(), ov.to_pricing());
        }
        let entry = pricing
            .lookup("llama-3.1-8b-instant")
            .expect("override merged into the table");
        assert_eq!(entry.input_per_million, 0.05);
        assert_eq!(entry.output_per_million, 0.08);

        let agents = vec![("t".to_string(), "llama-3.1-8b-instant".to_string())];
        validate_model_registry(&config.providers, None, &agents, &pricing)
            .expect("override should satisfy the pricing guarantee");
    }

    #[test]
    fn anthropic_config_parses_base_url_from_toml() {
        let toml = r#"
[providers.anthropic]
base_url = "http://127.0.0.1:12345"
"#;
        let config = Config::from_toml_str(toml).unwrap();
        let anthropic = config.providers.anthropic.unwrap();
        assert_eq!(
            anthropic.base_url.as_deref(),
            Some("http://127.0.0.1:12345")
        );
        // api_key_env still defaults when only base_url is set.
        assert_eq!(anthropic.api_key_env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn anthropic_config_base_url_defaults_to_none() {
        let toml = r#"
[providers.anthropic]
api_key_env = "SOMETHING"
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert!(config.providers.anthropic.unwrap().base_url.is_none());
    }

    #[test]
    fn rejects_invalid_toml() {
        let err = Config::from_toml_str("not = valid = toml = at all").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidToml(_)));
    }

    #[test]
    fn state_config_defaults_when_absent() {
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.state.retention_days, 30);
        assert_eq!(config.state.sweep_interval_seconds, 3_600);
    }

    #[test]
    fn state_config_parses_overrides() {
        let toml = r#"
[state]
retention_days = 7
sweep_interval_seconds = 300
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.state.retention_days, 7);
        assert_eq!(config.state.sweep_interval_seconds, 300);
    }

    #[test]
    fn state_config_accepts_negative_retention_to_disable() {
        let toml = r#"
[state]
retention_days = -1
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.state.retention_days, -1);
        // sweep_interval_seconds still defaults.
        assert_eq!(config.state.sweep_interval_seconds, 3_600);
    }

    #[test]
    fn tools_exec_config_defaults_when_absent() {
        // Absent `[tools.exec]` → the runtime defaults (120s / 600s),
        // deliberately higher than the fq-tools crate defaults.
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.tools.exec.default_timeout_secs, 120);
        assert_eq!(config.tools.exec.max_timeout_secs, 600);
        let exec = config.tools.exec.to_exec_config();
        assert_eq!(exec.default_timeout, Duration::from_secs(120));
        assert_eq!(exec.max_timeout, Duration::from_secs(600));
    }

    #[test]
    fn tools_exec_config_parses_overrides() {
        let toml = r#"
[tools.exec]
default_timeout_secs = 200
max_timeout_secs = 900
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.tools.exec.default_timeout_secs, 200);
        assert_eq!(config.tools.exec.max_timeout_secs, 900);
        let exec = config.tools.exec.to_exec_config();
        assert_eq!(exec.default_timeout, Duration::from_secs(200));
        assert_eq!(exec.max_timeout, Duration::from_secs(900));
        // Fields this section does not expose keep the crate defaults.
        let crate_default = fq_tools::builtin::ExecConfig::default();
        assert_eq!(exec.max_output_bytes, crate_default.max_output_bytes);
        assert_eq!(exec.default_path, crate_default.default_path);
    }

    #[test]
    fn tools_exec_config_partial_override_keeps_other_default() {
        // Only one knob set → the other falls back to its serde default.
        let toml = r#"
[tools.exec]
default_timeout_secs = 45
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.tools.exec.default_timeout_secs, 45);
        assert_eq!(config.tools.exec.max_timeout_secs, 600);
    }

    #[test]
    fn max_iterations_defaults_when_absent() {
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(
            config.max_iterations,
            crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS
        );
    }

    #[test]
    fn max_iterations_parses_override() {
        let toml = r#"
max_iterations = 250
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.max_iterations, 250);
    }

    #[test]
    fn load_or_default_returns_default_when_missing() {
        let path = PathBuf::from("/tmp/definitely-does-not-exist-fq.toml");
        let config = Config::load_or_default(&path).unwrap();
        assert_eq!(config.nats.url, "nats://localhost:4222");
    }

    #[test]
    fn resolve_api_key_reads_env_var() {
        // Use a unique env var name to avoid test interaction.
        let env_var = "FQ_TEST_API_KEY_RESOLVE";
        // Safety: tests share a process, but this key is unique to this test.
        unsafe { std::env::set_var(env_var, "sk-test-value") };

        let toml = format!(
            r#"
[providers.anthropic]
api_key_env = "{env_var}"
"#
        );
        let config = Config::from_toml_str(&toml).unwrap();
        let key = config.resolve_anthropic_api_key().unwrap();
        assert_eq!(key, "sk-test-value");

        unsafe { std::env::remove_var(env_var) };
    }

    #[test]
    fn relative_paths_in_config_file_resolve_to_config_dir() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("fq.toml");
        std::fs::write(
            &config_path,
            r#"
[agents]
directory = "my-agents"
"#,
        )
        .unwrap();

        let config = Config::from_file(&config_path).unwrap();
        assert_eq!(config.agents.directory, dir.path().join("my-agents"));
    }

    #[test]
    fn nested_relative_paths_in_config_file_resolve_to_config_dir() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("fq.toml");
        std::fs::write(
            &config_path,
            r#"
[agents]
directory = "sub/agents"
"#,
        )
        .unwrap();

        let config = Config::from_file(&config_path).unwrap();
        assert_eq!(config.agents.directory, dir.path().join("sub/agents"));
    }

    #[test]
    fn absolute_paths_in_config_file_are_unchanged() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("fq.toml");
        std::fs::write(
            &config_path,
            r#"
[agents]
directory = "/var/lib/factor-q/agents"
"#,
        )
        .unwrap();

        let config = Config::from_file(&config_path).unwrap();
        assert_eq!(
            config.agents.directory,
            PathBuf::from("/var/lib/factor-q/agents")
        );
    }

    #[test]
    fn paths_from_toml_string_are_unchanged() {
        let toml = r#"
[agents]
directory = "relative-agents"
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.agents.directory, PathBuf::from("relative-agents"));
    }

    #[test]
    fn resolve_api_key_fails_when_env_var_missing() {
        let env_var = "FQ_TEST_API_KEY_MISSING";
        unsafe { std::env::remove_var(env_var) };

        let toml = format!(
            r#"
[providers.anthropic]
api_key_env = "{env_var}"
"#
        );
        let config = Config::from_toml_str(&toml).unwrap();
        let err = config.resolve_anthropic_api_key().unwrap_err();
        assert!(matches!(err, ConfigError::SecretNotSet { .. }));
    }
}
