//! Resolving the edge identity at daemon startup.
//!
//! The identity — self-signed certificate plus the biscuit token root
//! — is durable secret material: losing it orphans every pinned client
//! and invalidates every issued token. It therefore lives under the
//! **state** directory, not the cache directory, whose contract (FHS
//! §5.5, XDG) says a cleaner may empty it at any time (#362).
//!
//! It used to live under the cache directory, so startup adopts an
//! identity found there before ever minting a new one — an upgrade
//! that rotated the root would deliver exactly the failure the move
//! exists to prevent.

use std::path::{Path, PathBuf};

use anyhow::Context;
use fq_edge::{EdgeIdentity, IdentityOrigin};
use fq_runtime::Config;

/// Where the edge identity lives under a given root.
fn identity_dir(root: &Path) -> PathBuf {
    root.join("edge")
}

/// Load, adopt, or mint the daemon's edge identity, reporting each
/// outcome on stdout the way the startup banner does. Returns the
/// identity and the directory it is now authoritative at.
pub(crate) fn resolve(config: &Config) -> anyhow::Result<(EdgeIdentity, PathBuf)> {
    let dir = identity_dir(&config.state.directory);
    let legacy = identity_dir(&config.cache.directory);
    let (identity, origin) = EdgeIdentity::load_or_adopt(&dir, &legacy)
        .context("edge: failed to load or provision identity (check [state] in fq.toml)")?;
    match origin {
        IdentityOrigin::Loaded => {}
        IdentityOrigin::Adopted => {
            tracing::warn!(
                from = %legacy.display(),
                to = %dir.display(),
                "edge identity adopted from the legacy cache location"
            );
            println!();
            println!(
                "edge: identity adopted from {} into {} (#362 — the identity is durable \
                 state, not cache)",
                legacy.display(),
                dir.display()
            );
            println!(
                "edge: fingerprints and issued tokens are unchanged; the legacy copy is \
                 left in place and can be deleted once you are satisfied"
            );
        }
        IdentityOrigin::Minted => {
            let admin = identity
                .mint_admin_token()
                .context("edge: failed to mint the admin token")?;
            println!();
            println!(
                "edge: first run — identity provisioned under {}",
                dir.display()
            );
            println!(
                "edge: certificate fingerprint (clients pin this): {}",
                hex(&identity.fingerprint())
            );
            println!("edge: admin token (printed once; store it securely):");
            println!("  {admin}");
        }
    }
    Ok((identity, dir))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(state: &Path, cache: &Path) -> Config {
        let mut config = Config::default();
        config.state.directory = state.to_path_buf();
        config.cache.directory = cache.to_path_buf();
        config
    }

    /// Nothing on disk anywhere: the identity is minted, and it lands
    /// under the *state* directory — the cache directory stays empty.
    #[test]
    fn fresh_install_mints_under_the_state_directory() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let cache = root.path().join("cache");
        let (_identity, dir) = resolve(&config_with(&state, &cache)).unwrap();

        assert_eq!(dir, state.join("edge"));
        assert!(dir.join("cert.der").exists(), "identity minted under state");
        assert!(
            !cache.join("edge").exists(),
            "nothing may be written to the cache directory"
        );
    }

    /// The upgrade path, and the reason this module exists: an
    /// identity that only exists at the old cache location is adopted,
    /// with the fingerprint every pinned client verifies unchanged.
    #[test]
    fn an_identity_at_the_legacy_cache_location_is_adopted_intact() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let cache = root.path().join("cache");
        let original = EdgeIdentity::provision().unwrap();
        original.save(&cache.join("edge")).unwrap();
        let pinned = original.fingerprint();
        let token = original.mint_admin_token().unwrap();

        let (adopted, dir) = resolve(&config_with(&state, &cache)).unwrap();
        assert_eq!(dir, state.join("edge"));
        assert_eq!(
            adopted.fingerprint(),
            pinned,
            "adoption must not rotate the certificate"
        );
        fq_edge::auth::verify_token(&token, adopted.public_key())
            .expect("a token issued before the move still verifies after it");

        // The legacy copy is left in place — rollback stays possible —
        // and the next start reads the new location without touching it.
        assert!(cache.join("edge").join("cert.der").exists());
        let (again, _) = resolve(&config_with(&state, &cache)).unwrap();
        assert_eq!(again.fingerprint(), pinned);
    }

    /// A cert-less directory holding private material fails closed at
    /// *either* location — including the legacy one, where skipping
    /// past it would silently mint a new root.
    #[test]
    fn partial_state_fails_closed_at_both_locations() {
        for (name, partial_at_legacy) in [("state", false), ("cache", true)] {
            let root = tempfile::tempdir().unwrap();
            let state = root.path().join("state");
            let cache = root.path().join("cache");
            let target = if partial_at_legacy { &cache } else { &state };
            std::fs::create_dir_all(target.join("edge")).unwrap();
            std::fs::write(target.join("edge").join("key.der"), b"stale").unwrap();

            let err = match resolve(&config_with(&state, &cache)) {
                Ok(_) => panic!("a partial identity under {name} must be refused"),
                Err(err) => format!("{err:#}"),
            };
            assert!(
                err.contains("partial"),
                "expected the partial-identity refusal for {name}, got: {err}"
            );
        }
    }
}
