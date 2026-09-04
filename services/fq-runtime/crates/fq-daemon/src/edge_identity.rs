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
//!
//! Two files sit beside the identity for the operator's benefit
//! (<https://github.com/bricef/factor-q/issues/545>): `admin.token`,
//! written once at first run, owner-only, and **never printed** — a
//! token in a log is a token in journald, `docker logs` and every run
//! log for the life of the file — and `fingerprint`, the public pin,
//! so a script can pair without scraping stdout.

use std::path::{Path, PathBuf};

use anyhow::Context;
use fq_edge::{EdgeIdentity, FINGERPRINT_FILE, IdentityOrigin};
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
        .context("edge: failed to load or provision identity (check [state] in fqd.toml)")?;
    match origin {
        IdentityOrigin::Loaded => {
            // An identity saved before the fingerprint file existed has
            // none; give it one so the path the docs name is always
            // readable. Created only when absent — a loaded identity
            // never rewrites what sits beside it, and never mints a
            // second admin token.
            if !dir.join(FINGERPRINT_FILE).exists() {
                identity
                    .write_fingerprint(&dir)
                    .context("edge: failed to write the fingerprint file")?;
            }
        }
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
            let token_path = identity
                .write_admin_token(&dir)
                .context("edge: failed to write the admin token")?;
            println!();
            println!(
                "edge: first run — identity provisioned under {}",
                dir.display()
            );
            println!(
                "edge: certificate fingerprint (clients pin this): {}",
                hex(&identity.fingerprint())
            );
            println!(
                "edge: admin token written to {} (owner-only; it is not printed — read it \
                 from there)",
                token_path.display()
            );
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
    use fq_edge::ADMIN_TOKEN_FILE;

    fn config_with(state: &Path, cache: &Path) -> Config {
        let mut config = Config::default();
        config.state.directory = state.to_path_buf();
        config.cache.directory = cache.to_path_buf();
        config
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Nothing on disk anywhere: the identity is minted, and it lands
    /// under the *state* directory — the cache directory stays empty.
    /// Beside it: the admin token, owner-only, and the fingerprint
    /// (#545). A second start loads the identity and leaves both files
    /// exactly as they were.
    #[test]
    fn fresh_install_mints_under_the_state_directory() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let cache = root.path().join("cache");
        let (identity, dir) = resolve(&config_with(&state, &cache)).unwrap();

        assert_eq!(dir, state.join("edge"));
        assert!(dir.join("cert.der").exists(), "identity minted under state");
        assert!(
            !cache.join("edge").exists(),
            "nothing may be written to the cache directory"
        );

        let token_path = dir.join(ADMIN_TOKEN_FILE);
        let token = std::fs::read_to_string(&token_path).expect("admin.token written");
        fq_edge::auth::verify_token(token.trim(), identity.public_key())
            .expect("the file holds a token the identity's root signed");
        #[cfg(unix)]
        assert_eq!(mode_of(&token_path), 0o600, "admin.token must be owner-only");

        let fingerprint_path = dir.join(FINGERPRINT_FILE);
        let fingerprint = std::fs::read_to_string(&fingerprint_path).expect("fingerprint written");
        assert_eq!(
            fingerprint.trim(),
            hex(&identity.fingerprint()),
            "the fingerprint file is the pin the daemon prints"
        );

        // A loaded identity never rewrites either file — and never
        // mints a second admin token.
        let (_again, _) = resolve(&config_with(&state, &cache)).unwrap();
        assert_eq!(std::fs::read_to_string(&token_path).unwrap(), token);
        assert_eq!(
            std::fs::read_to_string(&fingerprint_path).unwrap(),
            fingerprint
        );
    }

    /// An identity saved before the fingerprint file existed gets one
    /// on load; an identity with no admin token stays without one —
    /// the token is minted at provisioning and never again.
    #[test]
    fn a_loaded_identity_gains_a_fingerprint_file_but_never_a_token() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let cache = root.path().join("cache");
        let dir = state.join("edge");
        EdgeIdentity::provision().unwrap().save(&dir).unwrap();
        std::fs::remove_file(dir.join(FINGERPRINT_FILE)).unwrap();
        assert!(!dir.join(ADMIN_TOKEN_FILE).exists(), "save() mints no token");

        let (identity, _) = resolve(&config_with(&state, &cache)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(FINGERPRINT_FILE))
                .unwrap()
                .trim(),
            hex(&identity.fingerprint())
        );
        assert!(
            !dir.join(ADMIN_TOKEN_FILE).exists(),
            "a loaded identity must not mint an admin token"
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

    /// A cert-less directory holding private material — the key, or a
    /// stale admin token — fails closed at *either* location, including
    /// the legacy one, where skipping past it would silently mint a new
    /// root.
    #[test]
    fn partial_state_fails_closed_at_both_locations() {
        for (name, partial_at_legacy) in [("state", false), ("cache", true)] {
            for leftover in ["key.der", ADMIN_TOKEN_FILE] {
                let root = tempfile::tempdir().unwrap();
                let state = root.path().join("state");
                let cache = root.path().join("cache");
                let target = if partial_at_legacy { &cache } else { &state };
                std::fs::create_dir_all(target.join("edge")).unwrap();
                std::fs::write(target.join("edge").join(leftover), b"stale").unwrap();

                let err = match resolve(&config_with(&state, &cache)) {
                    Ok(_) => panic!("a partial identity ({leftover} under {name}) must be refused"),
                    Err(err) => format!("{err:#}"),
                };
                assert!(
                    err.contains("partial") && err.contains(leftover),
                    "expected the partial-identity refusal naming {leftover} for {name}, got: {err}"
                );
            }
        }
    }
}
