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
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicConfig {
    #[serde(default = "default_anthropic_api_key_env")]
    pub api_key_env: String,
}

fn default_nats_url() -> String {
    "nats://localhost:4222".to_string()
}

fn default_agents_directory() -> PathBuf {
    PathBuf::from("agents")
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
            },
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
        if self.agents.directory.is_relative() && !base.as_os_str().is_empty() {
            self.agents.directory = base.join(&self.agents.directory);
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
        let value = std::env::var(&anthropic.api_key_env).map_err(|_| ConfigError::SecretNotSet {
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
    fn rejects_invalid_toml() {
        let err = Config::from_toml_str("not = valid = toml = at all").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidToml(_)));
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
