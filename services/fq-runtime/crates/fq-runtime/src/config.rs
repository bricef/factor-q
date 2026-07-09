//! Runtime configuration.
//!
//! Configuration is loaded from a TOML file (typically `fq.toml` at the
//! project root), with sensible defaults for unspecified fields. Secrets
//! are never stored in the config file — the config names the environment
//! variable that holds each secret, and the runtime reads the variable at
//! use time.

use std::fs;
use std::path::{Path, PathBuf};

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
    /// Model ids routed to this provider's endpoint + auth.
    #[serde(default)]
    pub models: Vec<String>,
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    /// Directory where factor-q writes cache files (e.g. the LiteLLM
    /// pricing snapshot). Defaults to the system cache directory — see
    /// [`crate::pricing::default_cache_dir`] for the resolution order.
    #[serde(default = "default_cache_dir_for_config")]
    pub directory: PathBuf,
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
        }
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_anthropic_api_key_env(),
            base_url: None,
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
            providers: ProvidersConfig {
                anthropic: Some(AnthropicConfig::default()),
                extra: Default::default(),
            },
            cache: CacheConfig::default(),
            worker: WorkerConfig::default(),
            state: StateConfig::default(),
            max_iterations: default_max_iterations(),
            drain_deadline_ms: default_drain_deadline_ms(),
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
