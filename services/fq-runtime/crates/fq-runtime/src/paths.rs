//! Where factor-q keeps data it must never regenerate.
//!
//! The cache directory ([`crate::pricing::default_cache_dir`], which
//! lives next to its first tenant) resolves through `XDG_CACHE_HOME` →
//! `$HOME/.cache` → the system temp dir, and both FHS §5.5 and the XDG
//! spec license a cleaner to empty it at any moment. That is fine for
//! the LiteLLM pricing snapshot. It is not fine for the edge identity
//! — a self-signed certificate plus the biscuit token root, whose loss
//! orphans every pinned client and every issued token (#362).
//!
//! Hence a separate *state* directory, resolved through the XDG spec's
//! state slot, whose contract is "durable, but not important enough to
//! back up" — exactly the edge identity's lifetime. The daemon's SQLite
//! stores are the obvious next tenant; they still live under the cache
//! directory and moving them is its own migration (schema files, the
//! legacy-split path, and every operator's mounted volume).

use std::path::PathBuf;

/// The last-resort state directory: the FHS home for variable state a
/// daemon must keep across restarts. Deliberately *not* temp-dir
/// shaped — an unwritable `/var/lib` surfaces as a startup error the
/// operator can fix, whereas a `/tmp` fallback silently mints a fresh
/// identity on every boot, which is the failure #362 exists to
/// prevent.
const SYSTEM_STATE_DIR: &str = "/var/lib/factor-q";

/// Return the default state directory for factor-q.
///
/// Resolution order:
/// 1. `$XDG_STATE_HOME/factor-q` if set
/// 2. `$HOME/.local/state/factor-q` if set
/// 3. `/var/lib/factor-q` as a last resort
///
/// Operators deploying factor-q should still prefer setting
/// `FQ_STATE_DIR` explicitly to a mounted volume — the default only
/// exists so a fresh binary runs without any configuration.
pub fn default_state_dir() -> PathBuf {
    resolve_state_dir(
        std::env::var("XDG_STATE_HOME").ok(),
        std::env::var("HOME").ok(),
    )
}

/// Pure resolution of the state directory, for testing.
fn resolve_state_dir(xdg: Option<String>, home: Option<String>) -> PathBuf {
    if let Some(xdg) = xdg.filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("factor-q");
    }
    if let Some(home) = home.filter(|s| !s.is_empty()) {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("factor-q");
    }
    PathBuf::from(SYSTEM_STATE_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_state_dir_prefers_xdg() {
        let dir = resolve_state_dir(
            Some("/xdg/state".to_string()),
            Some("/home/user".to_string()),
        );
        assert_eq!(dir, PathBuf::from("/xdg/state/factor-q"));
    }

    #[test]
    fn resolve_state_dir_falls_back_to_home_when_xdg_unset() {
        let dir = resolve_state_dir(None, Some("/home/user".to_string()));
        assert_eq!(dir, PathBuf::from("/home/user/.local/state/factor-q"));
    }

    /// The distroless/k8s shape: no `HOME`, no XDG. Durable by
    /// default — never the temp dir, which is what put the identity at
    /// risk in the first place.
    #[test]
    fn resolve_state_dir_falls_back_to_var_lib_when_both_unset() {
        let dir = resolve_state_dir(None, None);
        assert_eq!(dir, PathBuf::from("/var/lib/factor-q"));
        assert!(
            !dir.starts_with(std::env::temp_dir()),
            "the state fallback must never be temp-dir shaped"
        );
    }

    #[test]
    fn resolve_state_dir_treats_empty_env_vars_as_unset() {
        assert_eq!(
            resolve_state_dir(Some(String::new()), Some("/home/user".to_string())),
            PathBuf::from("/home/user/.local/state/factor-q")
        );
        assert_eq!(
            resolve_state_dir(Some(String::new()), Some(String::new())),
            PathBuf::from("/var/lib/factor-q")
        );
    }
}
