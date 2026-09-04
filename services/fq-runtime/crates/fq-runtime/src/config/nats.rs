//! `[nats]` — the broker section of the daemon config, and the two
//! guarantees that keep the broker credential out of every string the
//! daemon prints (#540): the URL is refused if it carries userinfo, and
//! the token is read from a named environment variable instead.

use serde::Deserialize;

use super::ConfigError;

/// `[nats]` — the broker the daemon publishes to and consumes from.
///
/// The credential never travels in `url`. A broker that requires token
/// auth gets it through `token_env`: the *name* of an environment
/// variable the daemon reads once at startup. That is what keeps the
/// URL safe to print — the banner, the daemon log and the
/// `system.startup` event all carry `url` verbatim, and none of them can
/// leak what the string never contained (#540).
#[derive(Debug, Clone, Deserialize)]
pub struct NatsConfig {
    /// Broker URL, host and port only — `nats://host:4222`. Userinfo
    /// (`nats://TOKEN@host`, `nats://USER:PASS@host`) is refused by
    /// [`NatsConfig::validate`].
    #[serde(default = "default_nats_url")]
    pub url: String,
    /// Name of the environment variable holding the broker token, e.g.
    /// `"FQ_NATS_TOKEN"`. When set, the variable must be present and
    /// non-empty at startup or the daemon refuses to start, naming it.
    /// When unset the daemon connects without a credential — right for
    /// a private or test broker, and loudly wrong (the broker refuses
    /// the connection) for an authenticated one.
    #[serde(default)]
    pub token_env: Option<String>,
}

impl NatsConfig {
    /// Refuse a `url` that smuggles a credential in its userinfo. The
    /// message names `token_env` and deliberately does not echo the
    /// URL: this check exists so the string is never printed.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if url_has_userinfo(&self.url) {
            return Err(ConfigError::NatsUrlCarriesCredential);
        }
        Ok(())
    }

    /// The broker token, read from the variable `token_env` names.
    /// `Ok(None)` when no variable is configured; an error naming the
    /// variable when one is configured but unset or empty.
    pub fn resolve_token(&self) -> Result<Option<String>, ConfigError> {
        let Some(name) = &self.token_env else {
            return Ok(None);
        };
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => Ok(Some(value)),
            _ => Err(ConfigError::SecretNotSet {
                env_var: name.clone(),
            }),
        }
    }
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            url: default_nats_url(),
            token_env: None,
        }
    }
}

fn default_nats_url() -> String {
    "nats://localhost:4222".to_string()
}

/// Does any entry of a (possibly comma-separated) NATS server list carry
/// userinfo? Judged on the authority — the text between `://` and the
/// next `/` — containing `@`, which covers `TOKEN@host` and
/// `USER:PASS@host` alike without parsing a URL the broker may accept
/// in a shape the `url` crate does not.
fn url_has_userinfo(url: &str) -> bool {
    url.split(',').any(|entry| {
        let authority = entry.split_once("://").map_or(entry, |(_, rest)| rest);
        authority.split('/').next().unwrap_or("").contains('@')
    })
}

#[cfg(test)]
mod tests;
