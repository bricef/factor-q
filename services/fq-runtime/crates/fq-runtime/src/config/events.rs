//! `[events]` — retention for the JetStream event trail.

use std::time::Duration;

use serde::Deserialize;

use super::ConfigError;
use crate::bus::DEFAULT_MAX_AGE;

/// Event-stream settings from `[events]` in `fqd.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventsConfig {
    /// How long JetStream retains events. Written as days, hours, minutes,
    /// or seconds (for example `30d` or `12h`).
    #[serde(default = "default_max_age")]
    pub max_age: String,
}

impl EventsConfig {
    /// Parse the configured retention after configuration validation.
    pub fn max_age(&self) -> Result<Duration, ConfigError> {
        parse_duration(&self.max_age)
    }

    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        self.max_age().map(|_| ())
    }
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            max_age: default_max_age(),
        }
    }
}

fn default_max_age() -> String {
    format!("{}d", DEFAULT_MAX_AGE.as_secs() / (24 * 60 * 60))
}

fn parse_duration(setting: &str) -> Result<Duration, ConfigError> {
    let setting = setting.trim();
    let invalid = || ConfigError::InvalidEventMaxAge(setting.to_string());
    let (count, unit) = setting.split_at(setting.len().checked_sub(1).ok_or_else(invalid)?);
    let count: u64 = count.parse().map_err(|_| invalid())?;
    let unit_seconds: u64 = match unit {
        "d" => 24 * 60 * 60,
        "h" => 60 * 60,
        "m" => 60,
        "s" => 1,
        _ => return Err(invalid()),
    };
    let seconds = count.checked_mul(unit_seconds).ok_or_else(invalid)?;
    if seconds == 0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_existing_thirty_day_window() {
        let config = EventsConfig::default();
        assert_eq!(config.max_age().unwrap(), DEFAULT_MAX_AGE);
    }

    #[test]
    fn parses_a_non_default_window() {
        let config = EventsConfig {
            max_age: "36h".to_string(),
        };
        assert_eq!(config.max_age().unwrap(), Duration::from_secs(36 * 60 * 60));
    }
}
