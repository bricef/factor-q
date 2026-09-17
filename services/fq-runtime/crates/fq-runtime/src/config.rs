//! Runtime configuration.
//!
//! Configuration is loaded from a TOML file (`fqd.toml` at the project
//! root, by the daemon's default), with sensible defaults for
//! unspecified fields. Secrets are never stored in the config file —
//! the config names the environment variable that holds each secret,
//! and the runtime reads the variable at use time.
//!
//! The file's *name* is not this module's to know. [`Config`] parses
//! whatever path it is handed, and the binary that reads it names it —
//! `fq-daemon`'s `DEFAULT_CONFIG_PATH`. A second constant here said
//! `fq.toml` long after that became the client's own, differently
//! shaped config; nothing read it, so nothing caught it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

mod bus;
mod edge;
mod error;
mod events;
mod maintenance;
mod mcp;
mod nats;
mod pricing;
mod providers;
mod stuck;
mod tools;
pub use bus::BusConfig;
pub use edge::EdgeConfig;
pub use error::ConfigError;
pub use events::EventsConfig;
pub use maintenance::MaintenanceConfig;
pub use mcp::McpConfig;
pub use nats::NatsConfig;
pub use pricing::PricingConfig;
pub use providers::{
    AnthropicConfig, ApiShape, ModelPriceOverride, ModelRegistryError, ProviderConfig,
    ProvidersConfig, validate_model_registry,
};
pub use tools::{ExecToolConfig, ToolsConfig};

/// Runtime configuration for the factor-q daemon.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub nats: NatsConfig,
    /// Retention of the payload-bearing JetStream event trail.
    #[serde(default)]
    pub events: EventsConfig,
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
    /// How long `fq down` (ADR-0027) waits for in-flight invocations to
    /// suspend at a step boundary before hard-stopping the stragglers and
    /// letting the next binary's recovery resume them. A bounded wait,
    /// never block-forever. Config, not code (Design Principle 8).
    #[serde(default = "default_drain_deadline_ms")]
    pub drain_deadline_ms: u64,
    #[serde(default)]
    pub edge: EdgeConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    /// What an MCP server is allowed to cost: the deadlines a start
    /// runs under, the caps discovery and the stdio transport refuse
    /// past, and how often an unavailable server is tried again (#548).
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub summary: SummaryConfig,
    /// Whether this daemon runs the maintenance tasks a scheduler
    /// publishes to `fq.maintenance.<task>`, and the ack window one
    /// gets (#257).
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
    /// How every durable consumer on the event bus paces redelivery of
    /// a message its handler keeps failing on, and when a consumer that
    /// is still retrying counts as stuck.
    #[serde(default)]
    pub bus: BusConfig,
    /// Where the price list comes from, and what the daemon accepts
    /// from it (#735).
    #[serde(default)]
    pub pricing: PricingConfig,
}

/// `[state]` — durable runtime state: where it lives, and how long
/// the sweepable parts of it are kept. Drives the scheduled sweeps for
/// the `invocation_archive` and rebuildable projection `events` tables,
/// and for stale rows in `coordination_worker`.
///
/// Two independent retention windows, because they hold different
/// things: `retention_days` bounds the record of work that finished,
/// `stale_worker_retention_days` bounds the roster of daemons that
/// died. Each disables on its own `-1`.
#[derive(Debug, Clone, Deserialize)]
pub struct StateConfig {
    /// Directory for data factor-q must never regenerate — today the
    /// edge identity (certificate + biscuit token root), whose loss
    /// orphans every pinned client and every issued token (#362).
    /// Defaults to the system state directory — see
    /// [`crate::paths::default_state_dir`] for the resolution order.
    /// The SQLite stores are the obvious next tenant; they still live
    /// under `[cache]`, and moving them is its own migration.
    #[serde(default = "default_state_dir_for_config")]
    pub directory: PathBuf,
    /// How long to keep archive and projected event rows before the
    /// retention sweep deletes them. Default 30 days. Set to
    /// `-1` to disable the sweep entirely. Cost-bearing event rows
    /// are exempt and kept indefinitely regardless of this setting.
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,
    /// How long a worker registration that has already gone **stale** is
    /// kept before the retention sweep deletes its row. Default 7 days.
    /// Set to `-1` to disable stale-worker collection entirely.
    ///
    /// **This is not the stale threshold, and must not be set as if it
    /// were.** A worker is *marked* stale after roughly 30s of missed
    /// heartbeats (`DEFAULT_STALE_THRESHOLD_MS`); that value is
    /// deliberately aggressive so orphan recovery reacts while the work
    /// is still fresh. Deletion is the opposite trade. The row is the
    /// operator's evidence that a worker died — it is what `fq workers
    /// list --stale-only` shows and what an owned invocation is
    /// recovered through — so it has to outlive the diagnosis, not race
    /// it. Days, not seconds: the default survives a weekend, so a
    /// Friday-night failure is still on the roster on Monday.
    ///
    /// The sweep exists because `worker_id` is the daemon's `runtime_id`,
    /// a fresh UUID per run, so `coordination_worker` gains a row on
    /// every restart. Reclaiming them was an operator verb (`fq workers
    /// prune`) until it wasn't: the system should not depend on operator
    /// remediations to work normally.
    #[serde(default = "default_stale_worker_retention_days")]
    pub stale_worker_retention_days: i64,
    /// How often the retention sweep runs, in seconds.
    /// Default 1 hour. Production doesn't need faster; tests
    /// override via `[state]` in their config. Shared by both
    /// windows above — an hourly tick is right for a sweep whose
    /// shortest window is measured in days.
    #[serde(default = "default_sweep_interval_seconds")]
    pub sweep_interval_seconds: u64,
}

fn default_retention_days() -> i64 {
    30
}

fn default_stale_worker_retention_days() -> i64 {
    7
}

fn default_sweep_interval_seconds() -> u64 {
    3_600
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            directory: default_state_dir_for_config(),
            retention_days: default_retention_days(),
            stale_worker_retention_days: default_stale_worker_retention_days(),
            sweep_interval_seconds: default_sweep_interval_seconds(),
        }
    }
}

/// Worker-side knobs — `[worker]` in `fqd.toml`: the archive hand-off
/// retry cadence, the LLM retry policy and call deadlines, and the
/// concurrency bound. The heartbeat cadence is a const because changing
/// it independently of the control-plane's stale threshold would change
/// semantics.
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
    /// (design principle 8), overridable in `fqd.toml`.
    #[serde(default)]
    pub llm_retry: crate::llm::RetryConfig,
    /// The per-model provider throttle (#278): the pause a 429 sets, the
    /// AIMD cap on calls in flight, and the dispatcher's hold on triggers
    /// for a paused model. Its ceiling is `max_concurrent_invocations`
    /// and its longest default pause is `llm_retry.max_retry_after_ms`
    /// — see [`Self::throttle_bounds`].
    #[serde(default)]
    pub throttle: crate::llm::ThrottleConfig,
    /// The deadline on every model call, in seconds (#546, review
    /// finding B1): the whole call, connect to last byte, applied on the
    /// HTTP client and again around the call. A call past it fails as a
    /// transient timeout that `llm_retry` retries under its own cap, so
    /// a provider that never answers holds a worker for at most
    /// `llm_retry.timeout_max_attempts` (default 2) times this. Default
    /// 600; [`crate::llm::LlmTimeouts`] says why so long.
    #[serde(default = "default_llm_timeout_secs")]
    pub llm_timeout_secs: u64,
    /// How long establishing the connection may take, in seconds, so an
    /// endpoint that never answers fails in seconds rather than after
    /// the whole budget. Default 10.
    #[serde(default = "default_llm_connect_timeout_secs")]
    pub llm_connect_timeout_secs: u64,
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
    /// Override the derived stuck threshold (see [`Config::stuck_after`]),
    /// in seconds. `None` by default, and nothing in this repository
    /// sets it.
    ///
    /// An emergency escape hatch, not a tuning knob: it exists so an
    /// operator staring at a live wedge can widen or narrow the report
    /// without a rebuild. A workload that needs it permanently is
    /// telling you the deadlines it is derived from are wrong, and
    /// those are what should move.
    #[serde(default)]
    pub stuck_threshold_override_secs: Option<u64>,
}

fn default_max_concurrent_invocations() -> usize {
    1
}

fn default_llm_timeout_secs() -> u64 {
    crate::llm::LlmTimeouts::default().request.as_secs()
}

fn default_llm_connect_timeout_secs() -> u64 {
    crate::llm::LlmTimeouts::default().connect.as_secs()
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
            throttle: crate::llm::ThrottleConfig::default(),
            llm_timeout_secs: default_llm_timeout_secs(),
            llm_connect_timeout_secs: default_llm_connect_timeout_secs(),
            max_concurrent_invocations: default_max_concurrent_invocations(),
            stuck_threshold_override_secs: None,
        }
    }
}

impl WorkerConfig {
    /// The two deadlines as the LLM client takes them.
    pub fn llm_timeouts(&self) -> crate::llm::LlmTimeouts {
        crate::llm::LlmTimeouts {
            connect: Duration::from_secs(self.llm_connect_timeout_secs),
            request: Duration::from_secs(self.llm_timeout_secs),
        }
    }

    /// The bounds the throttle must agree with, read off the keys that
    /// already own them rather than declared a second time: the permit
    /// ceiling is `max_concurrent_invocations`, the longest default pause
    /// is `llm_retry.max_retry_after_ms`.
    pub fn throttle_bounds(&self) -> crate::llm::ThrottleBounds {
        crate::llm::ThrottleBounds {
            ceiling: self.max_concurrent_invocations.max(1),
            max_pause: Duration::from_millis(self.llm_retry.max_retry_after_ms),
        }
    }
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

fn default_cache_dir_for_config() -> PathBuf {
    crate::pricing::default_cache_dir()
}

fn default_state_dir_for_config() -> PathBuf {
    crate::paths::default_state_dir()
}

fn default_agents_directory() -> PathBuf {
    PathBuf::from("agents")
}

fn default_max_iterations() -> u32 {
    crate::worker::reducer::harness::DEFAULT_MAX_ITERATIONS
}

/// Default graceful-drain deadline: 120s. Long enough for a typical
/// model/tool step to finish so the invocation suspends at the next
/// boundary; past it, `fq down` hard-stops and recovery takes over.
fn default_drain_deadline_ms() -> u64 {
    120_000
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            directory: default_agents_directory(),
            default_model: None,
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
            events: EventsConfig::default(),
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
            maintenance: MaintenanceConfig::default(),
            max_iterations: default_max_iterations(),
            drain_deadline_ms: default_drain_deadline_ms(),
            edge: EdgeConfig::default(),
            tools: ToolsConfig::default(),
            mcp: McpConfig::default(),
            bus: BusConfig::default(),
            pricing: PricingConfig::default(),
        }
    }
}

impl Config {
    /// Parse configuration from a TOML string.
    ///
    /// A key this struct does not know is an error, not a shrug. An
    /// operator who writes `max_iters` for `max_iterations`, or puts a
    /// `[worker]` setting under `[workspace]`, has made an edit that
    /// does not take effect — and silence there is indistinguishable
    /// from the setting working. The daemon reads its config once, at
    /// startup, so the alternative to failing here is running for weeks
    /// on a value nobody set.
    ///
    /// `#[serde(deny_unknown_fields)]` cannot express this: serde
    /// rejects it alongside the `#[serde(flatten)]` that
    /// `ProvidersConfig` needs to accept `[providers.<name>]` for any
    /// name. `serde_ignored` reports the same thing from the outside,
    /// and gives the full dotted path rather than just the leaf.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let mut ignored = Vec::new();
        // toml 1.x splits parsing from deserializing: `Deserializer::parse`
        // returns the syntax error up front where 0.8's `new` deferred it to
        // the `deserialize` call. Both still land in `InvalidToml`.
        let deserializer = toml::Deserializer::parse(s)
            .map_err(|err| ConfigError::InvalidToml(err.to_string()))?;
        let config: Self = serde_ignored::deserialize(deserializer, |path| {
            ignored.push(path.to_string());
        })
        .map_err(|err| ConfigError::InvalidToml(err.to_string()))?;
        if !ignored.is_empty() {
            return Err(ConfigError::UnknownKeys(ignored));
        }
        config.validate()?;
        Ok(config)
    }

    /// The checks that must hold on the *effective* config — the
    /// parsed file here, and again in the daemon after its flag and
    /// environment overrides are merged, since `FQ_NATS_URL` can carry
    /// the same mistake the file can.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_iterations > crate::MAX_SUPPORTED_ITERATIONS {
            return Err(ConfigError::MaxIterationsExceedsRuntime {
                value: self.max_iterations,
                maximum: crate::MAX_SUPPORTED_ITERATIONS,
                host_step_budget: fq_ops::agent::ITERATION_HOST_STEP_BUDGET,
            });
        }
        self.nats.validate()?;
        self.events.validate()?;
        self.tools.validate()?;
        self.mcp.validate()
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
    /// New path-typed config fields must be added here and to
    /// `all_path_fields_resolve_relative_to_config_dir`.
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
        if self.state.directory.is_relative() {
            self.state.directory = base.join(&self.state.directory);
        }
        if let Some(path) = &mut self.workspace.path
            && path.is_relative()
        {
            *path = base.join(&*path);
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
    fn event_retention_defaults_parses_and_rejects_nonsense() {
        let defaults = Config::from_toml_str("").unwrap();
        assert_eq!(
            defaults.events.max_age().unwrap(),
            crate::bus::DEFAULT_MAX_AGE
        );

        let configured = Config::from_toml_str("[events]\nmax_age = \"12h\"\n").unwrap();
        assert_eq!(
            configured.events.max_age().unwrap(),
            Duration::from_secs(12 * 60 * 60)
        );

        for value in ["0s", "-1d"] {
            let err = Config::from_toml_str(&format!("[events]\nmax_age = \"{value}\"\n"))
                .expect_err("zero and negative retention must fail at config load");
            let message = err.to_string();
            assert!(message.contains("[events] max_age"), "{message}");
            assert!(message.contains(value), "{message}");
        }
    }

    /// #548: every `[mcp]` bound is reachable from `fqd.toml`, and the
    /// defaults are the ones the template and the guide document. An
    /// operator reading "default 30" has to get 30 from an empty file.
    #[test]
    fn mcp_bounds_default_and_parse() {
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.mcp.startup_timeout_secs, 30);
        assert_eq!(config.mcp.discovery_timeout_secs, 30);
        assert_eq!(config.mcp.max_discovery_pages, 100);
        assert_eq!(config.mcp.max_tools, 1_000);
        assert_eq!(config.mcp.max_line_bytes, 1024 * 1024);
        assert_eq!(config.mcp.retry_initial_secs, 30);
        assert_eq!(config.mcp.retry_max_secs, 600);
        assert_eq!(config.mcp.to_limits(), crate::mcp::McpLimits::default());

        let config = Config::from_toml_str(
            "[mcp]\nstartup_timeout_secs = 5\ndiscovery_timeout_secs = 7\n\
             max_discovery_pages = 3\nmax_tools = 11\nmax_line_bytes = 4096\n\
             retry_initial_secs = 1\nretry_max_secs = 9\n",
        )
        .unwrap();
        assert_eq!(
            config.mcp.to_limits(),
            crate::mcp::McpLimits {
                startup_timeout: Duration::from_secs(5),
                discovery_timeout: Duration::from_secs(7),
                max_discovery_pages: 3,
                max_tools: 11,
                max_line_bytes: 4096,
                retry_initial: Duration::from_secs(1),
                retry_max: Duration::from_secs(9),
            },
            "every key must reach the value the manager applies"
        );
    }

    /// A zero bound is refused at load, naming the key. Every one of
    /// them would make *every* MCP server unavailable rather than
    /// bounding one that misbehaves, and the daemon would boot and run
    /// that way silently. `retry_initial_secs = 0` is the one
    /// meaningful zero and stays accepted.
    #[test]
    fn a_zero_mcp_bound_is_refused_by_name() {
        for key in [
            "startup_timeout_secs",
            "discovery_timeout_secs",
            "max_discovery_pages",
            "max_tools",
            "max_line_bytes",
        ] {
            let err = Config::from_toml_str(&format!("[mcp]\n{key} = 0\n"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(key), "{key}: {err}");
            assert!(err.contains("greater than zero"), "{key}: {err}");
        }
        let config = Config::from_toml_str("[mcp]\nretry_initial_secs = 0\n")
            .expect("zero disables retrying, which is a real choice");
        assert!(config.mcp.to_limits().retry_backoff(1).is_none());
    }

    /// A misspelled `[mcp]` key is refused rather than ignored: the
    /// daemon reads its config once, so silence here would mean running
    /// for weeks on a bound nobody set.
    #[test]
    fn a_misspelled_mcp_key_is_named() {
        let err = Config::from_toml_str("[mcp]\nstartup_timeout = 5\n").unwrap_err();
        assert!(err.to_string().contains("mcp.startup_timeout"), "{err}");
    }

    /// #546: the call deadlines and the `Retry-After` cap are `[worker]`
    /// keys with the documented defaults, and an operator's values reach
    /// the client's own type.
    #[test]
    fn worker_llm_deadlines_default_and_parse() {
        use crate::llm::LlmTimeouts;

        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.worker.llm_timeout_secs, 600);
        assert_eq!(config.worker.llm_connect_timeout_secs, 10);
        assert_eq!(config.worker.llm_timeouts(), LlmTimeouts::default());
        assert_eq!(config.worker.llm_retry.max_retry_after_ms, 120_000);
        assert_eq!(
            config.worker.llm_retry.timeout_max_attempts, 2,
            "a hung provider is asked twice at the defaults (#607)"
        );

        let config = Config::from_toml_str(
            "[worker]\nllm_timeout_secs = 45\nllm_connect_timeout_secs = 2\n\n\
             [worker.llm_retry]\nmax_retry_after_ms = 9000\ntimeout_max_attempts = 1\n",
        )
        .unwrap();
        assert_eq!(
            config.worker.llm_timeouts(),
            LlmTimeouts {
                connect: Duration::from_secs(2),
                request: Duration::from_secs(45),
            }
        );
        assert_eq!(config.worker.llm_retry.max_retry_after_ms, 9000);
        assert_eq!(
            config.worker.llm_retry.timeout_max_attempts, 1,
            "every retry number is reachable from fqd.toml"
        );
        assert_eq!(
            config.worker.llm_retry.max_attempts, 4,
            "the other retry knobs keep their defaults"
        );
    }

    /// #278: the throttle keys are `[worker.throttle]` with the
    /// documented defaults, and its bounds are read off the keys that
    /// already own them rather than declared twice.
    #[test]
    fn worker_throttle_defaults_and_parses() {
        use crate::llm::{ThrottleBounds, ThrottleConfig};

        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.worker.throttle, ThrottleConfig::default());
        assert!(config.worker.throttle.enabled, "on by default");
        assert_eq!(config.worker.throttle.default_pause_ms, 30_000);
        assert_eq!(config.worker.throttle.success_window, 10);
        assert_eq!(
            config.worker.throttle_bounds(),
            ThrottleBounds {
                ceiling: 1,
                max_pause: Duration::from_secs(120),
            },
            "the defaults: one permit, a two-minute longest pause"
        );

        let config = Config::from_toml_str(
            "[worker]\nmax_concurrent_invocations = 4\n\n\
             [worker.llm_retry]\nmax_retry_after_ms = 9000\n\n\
             [worker.throttle]\nenabled = false\ndefault_pause_ms = 500\nsuccess_window = 3\n",
        )
        .unwrap();
        assert_eq!(
            config.worker.throttle,
            ThrottleConfig {
                enabled: false,
                default_pause_ms: 500,
                success_window: 3,
            }
        );
        assert_eq!(
            config.worker.throttle_bounds(),
            ThrottleBounds {
                ceiling: 4,
                max_pause: Duration::from_millis(9000),
            },
            "the ceiling is max_concurrent_invocations and the cap is max_retry_after_ms"
        );
    }

    /// #549: every number in the redelivery policy is reachable from
    /// `fqd.toml`, and an unconfigured daemon runs the documented
    /// defaults — 1s doubling to a 60s cap, an explicit 30s `ack_wait`,
    /// a line a minute, stuck after five redeliveries.
    #[test]
    fn bus_redelivery_policy_defaults_and_parses() {
        use crate::bus::ConsumerRedeliveryPolicy;

        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.bus.policy(), ConsumerRedeliveryPolicy::default());
        assert_eq!(config.bus.nak_initial_ms, 1_000);
        assert_eq!(config.bus.nak_max_ms, 60_000);
        assert_eq!(config.bus.ack_wait_ms, 30_000);
        assert_eq!(config.bus.log_interval_ms, 60_000);
        assert_eq!(config.bus.stuck_after_redeliveries, 5);

        let config = Config::from_toml_str(
            "[bus]\nnak_initial_ms = 250\nnak_max_ms = 5000\nack_wait_ms = 45000\n\
             log_interval_ms = 30000\nstuck_after_redeliveries = 2\n",
        )
        .unwrap();
        assert_eq!(
            config.bus.policy(),
            ConsumerRedeliveryPolicy {
                nak_initial: Duration::from_millis(250),
                nak_max: Duration::from_millis(5_000),
                ack_wait: Duration::from_millis(45_000),
                log_interval: Duration::from_millis(30_000),
                stuck_after_redeliveries: 2,
            }
        );

        // Partial tables keep the other defaults, so an operator who
        // only wants a longer ack window does not silently reset the
        // escalation.
        let config = Config::from_toml_str("[bus]\nack_wait_ms = 90000\n").unwrap();
        assert_eq!(config.bus.ack_wait_ms, 90_000);
        assert_eq!(config.bus.nak_initial_ms, 1_000);
    }

    #[test]
    fn the_edge_cannot_be_switched_off() {
        // `[edge] enabled` was vestigial: nothing but this test's
        // ancestor ever set it, and a daemon without an edge cannot be
        // inspected, reloaded or stopped by anything but a signal. The
        // key is gone, so the strict parser refuses it — which is the
        // point. An operator who had it in a file learns that at
        // startup rather than by wondering where their daemon went.
        let refused = Config::from_toml_str("[edge]\nenabled = false\n")
            .expect_err("`[edge] enabled` must not parse");
        assert!(
            matches!(&refused, ConfigError::UnknownKeys(keys) if keys.iter().any(|k| k == "edge.enabled")),
            "expected the unknown-key refusal to name edge.enabled, got: {refused}"
        );
    }

    #[test]
    fn the_edge_carries_its_connection_budget_with_defaults() {
        // The caps exist so an anonymous peer cannot make the daemon
        // allocate without bound; the defaults have to hold for a file
        // that names none of them, which is every existing file.
        let defaults: Config = toml::from_str("[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
        assert_eq!(defaults.edge.max_connections, 256);
        assert_eq!(defaults.edge.max_pre_auth_connections, 64);
        assert_eq!(defaults.edge.max_concurrent_requests, 32);
        assert_eq!(defaults.edge.accept_error_backoff_ms, 100);

        let tuned: Config = toml::from_str(
            "[edge]\nmax_connections = 8\nmax_pre_auth_connections = 2\n\
             max_concurrent_requests = 4\naccept_error_backoff_ms = 25\n",
        )
        .unwrap();
        assert_eq!(tuned.edge.max_connections, 8);
        assert_eq!(tuned.edge.max_pre_auth_connections, 2);
        assert_eq!(tuned.edge.max_concurrent_requests, 4);
        assert_eq!(tuned.edge.accept_error_backoff_ms, 25);
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
    fn rejects_invalid_toml() {
        let err = Config::from_toml_str("not = valid = toml = at all").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidToml(_)));
    }

    #[test]
    fn state_config_defaults_when_absent() {
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.state.retention_days, 30);
        assert_eq!(config.state.stale_worker_retention_days, 7);
        assert_eq!(config.state.sweep_interval_seconds, 3_600);
    }

    /// The default worker-retention window must stay orders of
    /// magnitude above the 30s stale threshold. Conflating the two is
    /// the failure mode the knob's doc comment warns about, and a
    /// default in seconds is how it would arrive.
    #[test]
    fn stale_worker_retention_default_dwarfs_the_stale_threshold() {
        let config = Config::from_toml_str("").unwrap();
        let window_ms = config.state.stale_worker_retention_days * 24 * 60 * 60 * 1_000;
        let stale_threshold_ms =
            crate::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
        assert!(
            window_ms > stale_threshold_ms * 1_000,
            "deletion window ({window_ms}ms) must not be within \
             three orders of magnitude of the stale threshold \
             ({stale_threshold_ms}ms)"
        );
    }

    #[test]
    fn state_config_parses_overrides() {
        let toml = r#"
[state]
retention_days = 7
stale_worker_retention_days = 90
sweep_interval_seconds = 300
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.state.retention_days, 7);
        assert_eq!(config.state.stale_worker_retention_days, 90);
        assert_eq!(config.state.sweep_interval_seconds, 300);
    }

    /// Each window disables on its own `-1`, and neither default drags
    /// the other with it.
    #[test]
    fn state_config_accepts_negative_retention_to_disable() {
        let config = Config::from_toml_str("[state]\nretention_days = -1\n").unwrap();
        assert_eq!(config.state.retention_days, -1);
        assert_eq!(config.state.stale_worker_retention_days, 7);
        // sweep_interval_seconds still defaults.
        assert_eq!(config.state.sweep_interval_seconds, 3_600);

        let config = Config::from_toml_str("[state]\nstale_worker_retention_days = -1\n").unwrap();
        assert_eq!(config.state.stale_worker_retention_days, -1);
        assert_eq!(config.state.retention_days, 30);
    }

    #[test]
    fn tools_exec_config_defaults_when_absent() {
        // Absent `[tools.exec]` → the runtime defaults (120s / 600s),
        // deliberately higher than the fq-tools crate defaults.
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.tools.exec.default_timeout_secs, 120);
        assert_eq!(config.tools.exec.max_timeout_secs, 600);
        assert_eq!(config.tools.exec.kill_grace_secs, 2);
        assert_eq!(config.tools.exec.drain_grace_secs, 2);
        let exec = config.tools.exec.to_exec_config();
        assert_eq!(exec.default_timeout, Duration::from_secs(120));
        assert_eq!(exec.max_timeout, Duration::from_secs(600));
        assert_eq!(exec.kill_grace, Duration::from_secs(2));
        assert_eq!(exec.drain_grace, Duration::from_secs(2));
    }

    /// The two teardown graces are configuration like every other
    /// number, and both reach the tool (#552).
    #[test]
    fn tools_exec_teardown_graces_parse_and_reach_the_tool() {
        let toml = r#"
[tools.exec]
kill_grace_secs = 3
drain_grace_secs = 1
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.tools.exec.kill_grace_secs, 3);
        assert_eq!(config.tools.exec.drain_grace_secs, 1);
        let exec = config.tools.exec.to_exec_config();
        assert_eq!(exec.kill_grace, Duration::from_secs(3));
        assert_eq!(exec.drain_grace, Duration::from_secs(1));
        // The timeouts this section also owns keep their defaults.
        assert_eq!(exec.max_timeout, Duration::from_secs(600));
    }

    /// Both graces run *after* an exec call's deadline, and the host
    /// cancels the call 5s after that same deadline. A sum that reaches
    /// the backstop would have the host drop the call mid-teardown,
    /// leaving alive the process group the kill existed to end — so it
    /// is refused at load, naming all three numbers (#552).
    #[test]
    fn exec_teardown_at_or_past_the_backstop_is_refused() {
        let toml = r#"
[tools.exec]
kill_grace_secs = 4
drain_grace_secs = 1
"#;
        let err = Config::from_toml_str(toml).unwrap_err();
        let msg = err.to_string();
        for fragment in [
            "kill_grace_secs = 4",
            "drain_grace_secs = 1",
            "5s backstop",
            "[tools.exec]",
        ] {
            assert!(
                msg.contains(fragment),
                "message must name {fragment}: {msg}"
            );
        }

        // One second under the backstop is fine — the check is on the
        // sum reaching it, not on either grace alone.
        let ok = Config::from_toml_str("[tools.exec]\nkill_grace_secs = 3\ndrain_grace_secs = 1\n")
            .unwrap();
        assert_eq!(ok.tools.exec.kill_grace_secs, 3);
    }

    /// The shipped defaults must satisfy the invariant, or a daemon
    /// with no `[tools.exec]` section would refuse to start.
    #[test]
    fn shipped_exec_teardown_defaults_fit_the_backstop() {
        let config = Config::from_toml_str("").unwrap();
        let teardown = config.tools.exec.kill_grace_secs + config.tools.exec.drain_grace_secs;
        assert!(
            teardown < crate::tools::ToolCallLimits::BACKSTOP_GRACE.as_secs(),
            "default teardown {teardown}s must stay under the backstop"
        );
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
    fn tool_call_limits_default_when_absent() {
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.tools.default_timeout_secs, 120);
        assert_eq!(config.tools.max_timeout_secs, 900);
        assert_eq!(config.tools.max_consecutive_timeouts, 3);
        assert_eq!(
            config.tools.call_limits(),
            crate::tools::ToolCallLimits::default()
        );
        // The shipped defaults must satisfy their own rule, or every
        // daemon with no `[tools]` section would refuse to start.
        assert!(config.tools.max_timeout_secs >= config.tools.exec.max_timeout_secs);
    }

    #[test]
    fn tool_call_limits_parse_from_toml() {
        let toml = r#"
[tools]
default_timeout_secs = 30
max_timeout_secs = 300
max_consecutive_timeouts = 5

[tools.exec]
max_timeout_secs = 300
"#;
        let config = Config::from_toml_str(toml).unwrap();
        let limits = config.tools.call_limits();
        assert_eq!(limits.default_timeout, Duration::from_secs(30));
        assert_eq!(limits.max_timeout, Duration::from_secs(300));
        assert_eq!(limits.max_consecutive_timeouts, 5);
    }

    /// The dogfood host's shape: `[tools.exec] max_timeout_secs = 900`
    /// under a lower general ceiling. Refused at load, naming both keys
    /// and both values (#547).
    #[test]
    fn tool_ceiling_below_exec_ceiling_is_refused() {
        let toml = r#"
[tools]
max_timeout_secs = 60

[tools.exec]
max_timeout_secs = 900
"#;
        let err = Config::from_toml_str(toml).unwrap_err();
        let msg = err.to_string();
        for fragment in [
            "[tools] max_timeout_secs",
            "[tools.exec] max_timeout_secs",
            "60",
            "900",
        ] {
            assert!(
                msg.contains(fragment),
                "message must name {fragment}: {msg}"
            );
        }
    }

    /// A default above its own ceiling states a deadline nothing
    /// honours — and in `[tools.exec]` it would let the child outlive
    /// the host's backstop. Refused in both sections, each naming
    /// itself (#547 review).
    #[test]
    fn a_default_above_its_own_ceiling_is_refused_in_either_section() {
        for (section, toml) in [
            (
                "[tools]",
                "[tools]\ndefault_timeout_secs = 900\nmax_timeout_secs = 600\n\n\
                 [tools.exec]\nmax_timeout_secs = 600\n",
            ),
            (
                "[tools.exec]",
                "[tools.exec]\ndefault_timeout_secs = 700\nmax_timeout_secs = 600\n",
            ),
        ] {
            let msg = Config::from_toml_str(toml).unwrap_err().to_string();
            assert!(
                msg.contains(section) && msg.contains("default_timeout_secs"),
                "the error must name the offending section and key: {msg}"
            );
        }
    }

    /// Equal ceilings are fine — the rule is "not below".
    #[test]
    fn tool_ceiling_equal_to_exec_ceiling_is_accepted() {
        let toml = r#"
[tools]
max_timeout_secs = 900

[tools.exec]
max_timeout_secs = 900
"#;
        assert!(Config::from_toml_str(toml).is_ok());
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
    fn max_iterations_above_runtime_limit_is_rejected() {
        let err = Config::from_toml_str("max_iterations = 501\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "`max_iterations` = 501 exceeds the runtime's supported maximum of 500 (host step budget 1000, two steps per turn)"
        );
    }

    #[test]
    fn max_iterations_at_runtime_limit_is_accepted() {
        assert!(Config::from_toml_str("max_iterations = 500\n").is_ok());
    }

    #[test]
    fn an_unknown_top_level_key_is_rejected() {
        let err = Config::from_toml_str("max_iters = 5\n").unwrap_err();
        assert!(
            matches!(&err, ConfigError::UnknownKeys(keys) if keys == &["max_iters"]),
            "expected the misspelling to be named, got: {err}"
        );
    }

    #[test]
    fn an_unknown_key_inside_a_table_is_named_by_its_full_path() {
        // The leaf alone would be ambiguous: several tables have a
        // `directory`, and knowing which one did nothing is the point.
        let err = Config::from_toml_str("[workspace]\nper_invokation = true\n").unwrap_err();
        let ConfigError::UnknownKeys(keys) = &err else {
            panic!("expected UnknownKeys, got: {err}");
        };
        assert_eq!(keys, &["workspace.per_invokation"]);
    }

    #[test]
    fn load_or_default_returns_default_when_missing() {
        let path = PathBuf::from("/tmp/definitely-does-not-exist-fqd.toml");
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
    fn all_path_fields_resolve_relative_to_config_dir() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("fqd.toml");
        std::fs::write(
            &config_path,
            r#"
[agents]
directory = "agents"
[cache]
directory = "cache"
[state]
directory = "state"
[workspace]
path = "work"
"#,
        )
        .unwrap();

        let config = Config::from_file(&config_path).unwrap();
        assert_eq!(config.agents.directory, dir.path().join("agents"));
        assert_eq!(config.cache.directory, dir.path().join("cache"));
        assert_eq!(config.state.directory, dir.path().join("state"));
        assert_eq!(config.workspace.path, Some(dir.path().join("work")));
    }

    /// `[state] directory` follows `[cache] directory` in every
    /// respect: same key shape, and a relative path resolves against
    /// the config file rather than the process cwd (#362).
    #[test]
    fn state_directory_is_configurable_and_resolves_like_the_cache_directory() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("fqd.toml");
        std::fs::write(
            &config_path,
            r#"
[cache]
directory = "./cache"

[state]
directory = "./state"
retention_days = 7
"#,
        )
        .unwrap();

        let config = Config::from_file(&config_path).unwrap();
        assert_eq!(config.cache.directory, dir.path().join("./cache"));
        assert_eq!(config.state.directory, dir.path().join("./state"));
        // The directory key coexists with the retention knobs already
        // under `[state]` rather than displacing them.
        assert_eq!(config.state.retention_days, 7);
    }

    /// An `fqd.toml` with no `[state] directory` — every deployment
    /// that predates #362 — must not fall back to the cache directory
    /// or to anything temp-dir shaped.
    #[test]
    fn state_directory_defaults_to_the_system_state_dir() {
        let config = Config::from_toml_str("[state]\nretention_days = 1\n").unwrap();
        assert_eq!(config.state.directory, crate::paths::default_state_dir());
        assert_ne!(config.state.directory, config.cache.directory);
    }

    #[test]
    fn nested_relative_paths_in_config_file_resolve_to_config_dir() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("fqd.toml");
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
        let config_path = dir.path().join("fqd.toml");
        std::fs::write(
            &config_path,
            r#"
[agents]
directory = "/var/lib/factor-q/agents"
[workspace]
path = "/var/lib/factor-q/workspace"
"#,
        )
        .unwrap();

        let config = Config::from_file(&config_path).unwrap();
        assert_eq!(
            config.agents.directory,
            PathBuf::from("/var/lib/factor-q/agents")
        );
        assert_eq!(
            config.workspace.path,
            Some(PathBuf::from("/var/lib/factor-q/workspace"))
        );
    }

    #[test]
    fn paths_from_toml_string_are_unchanged() {
        let toml = r#"
[agents]
directory = "relative-agents"
[workspace]
path = "relative-workspace"
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(config.agents.directory, PathBuf::from("relative-agents"));
        assert_eq!(
            config.workspace.path,
            Some(PathBuf::from("relative-workspace"))
        );
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
