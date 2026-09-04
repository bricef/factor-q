//! The configuration error type. Its own module so the sections that
//! produce these errors ([`super`], [`super::nats`]) share one
//! definition without the parent file carrying it.

use std::path::PathBuf;

/// Errors arising from configuration loading and secret resolution.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Keys the config does not know. Named in full, because the
    /// point of the error is telling the operator which line of theirs
    /// did nothing.
    #[error("unknown setting(s) in config: {} — check the spelling and the table they are under", .0.join(", "))]
    UnknownKeys(Vec<String>),

    #[error("invalid TOML in config file: {0}")]
    InvalidToml(String),

    #[error("provider '{0}' is not configured")]
    ProviderNotConfigured(&'static str),

    #[error("required secret not set in environment variable: {env_var}")]
    SecretNotSet { env_var: String },

    /// `[nats] token_env` names a variable that is unset or empty at
    /// startup. Its own variant rather than [`Self::SecretNotSet`] so
    /// the message says which setting named the variable and what the
    /// two ways out are.
    #[error(
        "[nats] token_env names environment variable `{env_var}`, which is unset or empty: \
         export the broker token in it before starting the daemon, or remove token_env for a \
         broker that requires none"
    )]
    NatsTokenNotSet { env_var: String },

    /// `[nats] url` carries a credential in its userinfo. Named without
    /// the URL on purpose: this error exists so the token never reaches
    /// a log line, and echoing the offending value would leak it.
    #[error(
        "[nats] url must not carry a credential (nats://TOKEN@host or nats://USER:PASS@host): \
         export the token in an environment variable and name that variable in [nats] token_env"
    )]
    NatsUrlCarriesCredential,
}
