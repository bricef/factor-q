//! `[tools] max_result_bytes` (#827): the default, an override, and a
//! zero refused at load. The timeout keys' tests predate this file and
//! live with the rest of the config tests in `config.rs`.

use crate::config::{Config, ConfigError};

#[test]
fn the_result_cap_defaults_to_256_kib() {
    let config = Config::from_toml_str("").unwrap();
    assert_eq!(config.tools.max_result_bytes, 262_144);
    assert_eq!(config.tools.call_limits().max_result_bytes, 262_144);
    // The runner's own default is the section's default, so a runner
    // built without `[tools]` caps where a daemon would.
    assert_eq!(
        crate::tools::ToolCallLimits::default().max_result_bytes,
        config.tools.max_result_bytes
    );
}

/// The default must leave `exec`'s two capped streams whole, or every
/// long `exec` result would be cut twice with two different notes.
#[test]
fn the_default_result_cap_sits_above_both_exec_streams() {
    let config = Config::from_toml_str("").unwrap();
    let exec = config.tools.exec.to_exec_config();
    assert!(config.tools.max_result_bytes > 2 * exec.max_output_bytes);
}

#[test]
fn the_result_cap_parses_an_override() {
    let config = Config::from_toml_str("[tools]\nmax_result_bytes = 65536\n").unwrap();
    assert_eq!(config.tools.max_result_bytes, 65_536);
    assert_eq!(config.tools.call_limits().max_result_bytes, 65_536);
}

/// A zero cap would cut every tool result to nothing: refused at load,
/// naming the key, rather than read as a working daemon whose tools all
/// answer with an empty string.
#[test]
fn a_zero_result_cap_is_refused_at_load() {
    let err = Config::from_toml_str("[tools]\nmax_result_bytes = 0\n").unwrap_err();
    assert!(
        matches!(err, ConfigError::ToolResultCapZero),
        "expected ToolResultCapZero, got: {err}"
    );
    assert!(
        err.to_string().contains("[tools] max_result_bytes"),
        "the message must name the key: {err}"
    );
}
