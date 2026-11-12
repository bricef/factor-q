//! `[tools] max_result_bytes` (#827): the default, an override, and a
//! value below the floor refused at load. The timeout keys' tests
//! predate this file and live with the rest of the config tests in
//! `config.rs`.

use super::MIN_RESULT_BYTES;
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

/// The default sits above `exec`'s two capped streams, so ordinary
/// `exec` output is cut once, by `exec`. Output with many invalid bytes
/// or control characters renders larger than `exec`'s raw-byte cap and
/// can still be cut again here (see #985); this pins only the ordinary
/// case.
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

/// The floor itself loads; anything under it is refused, naming the
/// value and the minimum — `0`, which would cut every result to nothing,
/// and `256`, which reads like someone meant KiB.
#[test]
fn a_result_cap_below_the_floor_is_refused_at_load() {
    let at_floor = format!("[tools]\nmax_result_bytes = {MIN_RESULT_BYTES}\n");
    let config = Config::from_toml_str(&at_floor).unwrap();
    assert_eq!(config.tools.max_result_bytes, 1024);

    for value in [0, 256, MIN_RESULT_BYTES - 1] {
        let err =
            Config::from_toml_str(&format!("[tools]\nmax_result_bytes = {value}\n")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::ToolResultCapTooSmall { value: v, minimum: 1024 } if v == value
            ),
            "expected ToolResultCapTooSmall for {value}, got: {err}"
        );
        let message = err.to_string();
        for fragment in [
            format!("[tools] max_result_bytes = {value}"),
            "minimum of 1024".to_string(),
        ] {
            assert!(
                message.contains(&fragment),
                "the message must name {fragment}: {message}"
            );
        }
    }
}
