//! `[pricing]` — where the price list comes from, and what the daemon
//! will accept from it (#735).

use std::time::Duration;

use serde::Deserialize;

use crate::pricing::PricingError;
use crate::pricing::accept::{AcceptanceRules, DEFAULT_MAX_DRIFT_RATIO};
use crate::pricing::live::{DEFAULT_MAX_AGE, LoadSettings, TableSource};

/// The live LiteLLM document, with the discipline on acceptance rather
/// than on which commit is fetched.
///
/// **Every default here points at the live table**, and that is the
/// decision in this file worth arguing. LiteLLM adds models weekly and
/// ADR-0004 refuses to start on a model with no price, so pinning by
/// default would turn "prices drift" into "the daemon will not run a new
/// model until a human bumps a SHA" — an automated process made manual,
/// and manual processes go stale. The knobs are here for the deployment
/// that genuinely needs a pin (air-gapped, regulated) and for tuning the
/// bound; a deployment that sets none of them gets the intended
/// behaviour.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PricingConfig {
    /// `litellm-main` (default) or `pinned:<sha>`.
    #[serde(default = "default_source")]
    pub source: String,
    /// How far a price may move, in either direction, and still be
    /// accepted. Default 5.0.
    #[serde(default = "default_max_drift_ratio")]
    pub max_drift_ratio: f64,
    /// How old the accepted table may be before a load raises
    /// `pricing.stale`. A duration string — `7d` (the default), `36h`,
    /// `90m`.
    #[serde(default = "default_max_age")]
    pub max_age: String,
}

impl PricingConfig {
    /// Resolve the section into what the load path takes, refusing a
    /// setting that does not parse.
    ///
    /// Refusing rather than warning-and-defaulting: an operator who
    /// wrote `pinned:deadbeef` and got the live document has the
    /// opposite of what they configured, and a typo'd bound that silently
    /// becomes 5.0 is a guarantee nobody is enforcing.
    pub fn load_settings(&self) -> Result<LoadSettings, PricingError> {
        if !(self.max_drift_ratio.is_finite() && self.max_drift_ratio > 1.0) {
            return Err(PricingError::Source(format!(
                "[pricing] max_drift_ratio must be a number greater than 1, not {}",
                self.max_drift_ratio
            )));
        }
        Ok(LoadSettings {
            source: TableSource::parse(&self.source)?,
            rules: AcceptanceRules {
                max_drift_ratio: self.max_drift_ratio,
            },
            max_age: parse_duration(&self.max_age)?,
        })
    }
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            source: default_source(),
            max_drift_ratio: default_max_drift_ratio(),
            max_age: default_max_age(),
        }
    }
}

fn default_source() -> String {
    TableSource::LitellmMain.to_string()
}

fn default_max_drift_ratio() -> f64 {
    DEFAULT_MAX_DRIFT_RATIO
}

/// Spelled from the load path's constant rather than beside it: the
/// documented default and the one the code applies are one fact.
fn default_max_age() -> String {
    format!("{}d", DEFAULT_MAX_AGE.as_secs() / (24 * 60 * 60))
}

/// `7d`, `36h`, `90m`, `30s`. Deliberately small: a staleness window is
/// written in days and read by a person, and a general duration grammar
/// would be more surface than the one key needs.
fn parse_duration(setting: &str) -> Result<Duration, PricingError> {
    let setting = setting.trim();
    let invalid = || {
        PricingError::Source(format!(
            "[pricing] max_age must be a duration such as `7d`, `36h` or `90m`, not `{setting}`"
        ))
    };
    let (count, unit) = setting.split_at(setting.len().checked_sub(1).ok_or_else(invalid)?);
    let count: u64 = count.parse().map_err(|_| invalid())?;
    let seconds = match unit {
        "d" => 24 * 60 * 60,
        "h" => 60 * 60,
        "m" => 60,
        "s" => 1,
        _ => return Err(invalid()),
    };
    let seconds = count.checked_mul(seconds).ok_or_else(invalid)?;
    if seconds == 0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests;
