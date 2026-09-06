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

    /// The general tool ceiling sits below the `exec` ceiling, so it
    /// would silently override the one per-tool timeout that already
    /// worked. Both keys and both values are named: the fix is to raise
    /// one or lower the other, and the operator has to know which is
    /// which.
    #[error(
        "[tools] max_timeout_secs = {tools_max} is below [tools.exec] max_timeout_secs = \
         {exec_max}: the general ceiling bounds every tool call, so this would cap exec at \
         {tools_max}s while its own section still said {exec_max}s — raise [tools] \
         max_timeout_secs to at least {exec_max}, or lower [tools.exec] max_timeout_secs"
    )]
    ToolCeilingBelowExec { tools_max: u64, exec_max: u64 },

    /// A section's `default_timeout_secs` sits above its own
    /// `max_timeout_secs`, so the default is clamped everywhere it is
    /// used and the file states a deadline nothing honours. Named with
    /// its section because both `[tools]` and `[tools.exec]` carry the
    /// pair.
    #[error(
        "{section} default_timeout_secs = {default} is above {section} max_timeout_secs = \
         {max}: the default is clamped to the ceiling wherever it is applied, so this file \
         states a deadline nothing honours — lower default_timeout_secs to at most {max}, or \
         raise max_timeout_secs"
    )]
    ToolDefaultAboveMax {
        section: &'static str,
        default: u64,
        max: u64,
    },

    /// `exec`'s teardown budget reaches or passes the host's backstop,
    /// so the host would cancel the call part-way through the kill it
    /// exists to perform. All three numbers are named: the operator
    /// chose two of them and cannot see the third.
    #[error(
        "[tools.exec] kill_grace_secs = {kill_grace} + drain_grace_secs = {drain_grace} is \
         {teardown}s, which is not below the host's {backstop}s backstop: both graces run \
         after an exec call's deadline, and the host cancels the call {backstop}s after that \
         same deadline — so a {teardown}s teardown is cut off part-way and the process group \
         it was killing can survive. Lower kill_grace_secs or drain_grace_secs so their sum \
         is under {backstop}"
    )]
    ExecTeardownExceedsBackstop {
        kill_grace: u64,
        drain_grace: u64,
        teardown: u64,
        backstop: u64,
    },
}
