//! The edge identity survives a daemon restart: `save`/`load`
//! roundtrip the TLS material and the token root — the fingerprint
//! clients pin is stable, and tokens minted before the restart verify
//! after it. Private material lands owner-only on disk.
//!
//! It also survives the identity *moving* (#362, cache dir → state
//! dir): an identity found only at the legacy location is adopted, not
//! re-minted.

use fq_edge::auth::verify_token;
use fq_edge::{EdgeIdentity, IdentityOrigin};

#[test]
fn save_load_roundtrip_preserves_identity() {
    let dir = tempfile::tempdir().unwrap();
    let original = EdgeIdentity::provision().unwrap();
    original.save(dir.path()).unwrap();
    let loaded = EdgeIdentity::load(dir.path()).unwrap();

    assert_eq!(original.fingerprint(), loaded.fingerprint());
    assert_eq!(original.cert_der, loaded.cert_der);
    assert_eq!(original.key_der, loaded.key_der);

    // The token root survived: a token minted before the reload
    // verifies under the reloaded root, and vice versa.
    let before = original.mint_admin_token().unwrap();
    verify_token(&before, loaded.public_key())
        .expect("pre-reload token verifies under the reloaded root");
    let after = loaded.mint_admin_token().unwrap();
    verify_token(&after, original.public_key())
        .expect("post-reload token verifies under the original root");
}

#[test]
fn load_or_provision_provisions_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    // A nested path exercises directory creation.
    let identity_dir = dir.path().join("edge");
    let (first, fresh) = EdgeIdentity::load_or_provision(&identity_dir).unwrap();
    assert!(fresh, "first call provisions");
    let (second, fresh) = EdgeIdentity::load_or_provision(&identity_dir).unwrap();
    assert!(!fresh, "second call loads");
    assert_eq!(first.fingerprint(), second.fingerprint());
}

#[cfg(unix)]
#[test]
fn private_material_and_directory_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // A nested path so `save` creates the directory itself.
    let identity_dir = dir.path().join("edge");
    EdgeIdentity::provision()
        .unwrap()
        .save(&identity_dir)
        .unwrap();
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(&identity_dir),
        0o700,
        "the identity directory must be owner-only"
    );
    for name in ["key.der", "root.key"] {
        let m = mode(&identity_dir.join(name));
        assert_eq!(m, 0o600, "{name} must be owner-only, got {m:o}");
    }
}

#[test]
fn partial_identity_fails_closed_instead_of_rotating() {
    let dir = tempfile::tempdir().unwrap();
    // Private material present but no certificate: the shape left by a
    // partial restore. Re-provisioning here would silently rotate the
    // root and orphan every pinned client and issued token.
    std::fs::write(dir.path().join("key.der"), b"stale").unwrap();
    let err = match EdgeIdentity::load_or_provision(dir.path()) {
        Ok(_) => panic!("a partial identity must be refused, not silently rotated"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("partial"),
        "expected the partial-identity refusal, got: {err}"
    );
}

/// #362: the identity moved from the cache directory to the state
/// directory. A daemon whose new location is empty must adopt the
/// identity already on disk at the old one — minting here would rotate
/// the root and orphan every pinned client, which is the failure the
/// move exists to prevent.
#[test]
fn load_or_adopt_adopts_the_legacy_identity_without_rotating() {
    let root = tempfile::tempdir().unwrap();
    let new_dir = root.path().join("state/edge");
    let legacy = root.path().join("cache/edge");
    let original = EdgeIdentity::provision().unwrap();
    original.save(&legacy).unwrap();
    let issued = original.mint_admin_token().unwrap();

    let (adopted, origin) = EdgeIdentity::load_or_adopt(&new_dir, &legacy).unwrap();
    assert_eq!(origin, IdentityOrigin::Adopted);
    assert_eq!(adopted.fingerprint(), original.fingerprint());
    assert_eq!(adopted.cert_der, original.cert_der);
    verify_token(&issued, adopted.public_key())
        .expect("a token issued before the move verifies after it");

    // The copy is authoritative from here: a second call reads the new
    // location, and the legacy copy is left untouched for rollback.
    let (again, origin) = EdgeIdentity::load_or_adopt(&new_dir, &legacy).unwrap();
    assert_eq!(origin, IdentityOrigin::Loaded);
    assert_eq!(again.fingerprint(), original.fingerprint());
    assert!(legacy.join("cert.der").exists(), "legacy copy is preserved");
}

#[test]
fn load_or_adopt_mints_only_when_neither_location_holds_one() {
    let root = tempfile::tempdir().unwrap();
    let new_dir = root.path().join("state/edge");
    let legacy = root.path().join("cache/edge");
    let (minted, origin) = EdgeIdentity::load_or_adopt(&new_dir, &legacy).unwrap();
    assert_eq!(origin, IdentityOrigin::Minted);
    assert!(new_dir.join("cert.der").exists());
    assert!(!legacy.exists(), "minting must not touch the legacy path");

    // An identity at the new location wins outright — a stale legacy
    // copy never overrides it.
    let stale = EdgeIdentity::provision().unwrap();
    stale.save(&legacy).unwrap();
    let (loaded, origin) = EdgeIdentity::load_or_adopt(&new_dir, &legacy).unwrap();
    assert_eq!(origin, IdentityOrigin::Loaded);
    assert_eq!(loaded.fingerprint(), minted.fingerprint());
}

/// Fail-closed applies to *both* locations. A partial legacy identity
/// is the dangerous one: skipping past it lands on "neither location
/// has one" and mints a fresh root.
#[test]
fn load_or_adopt_fails_closed_on_partial_state_in_either_location() {
    for partial_in_legacy in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let new_dir = root.path().join("state/edge");
        let legacy = root.path().join("cache/edge");
        let target = if partial_in_legacy { &legacy } else { &new_dir };
        std::fs::create_dir_all(target).unwrap();
        std::fs::write(target.join("root.key"), b"stale").unwrap();

        let err = match EdgeIdentity::load_or_adopt(&new_dir, &legacy) {
            Ok(_) => panic!("partial identity (legacy={partial_in_legacy}) must be refused"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("partial"), "got: {err}");
        assert!(
            err.contains(&target.display().to_string()),
            "the refusal must name the offending directory; got: {err}"
        );
    }
}

/// The adopted copy is created with the same hardening as a minted one
/// — 0700 directory, 0600 private material — rather than inheriting
/// whatever bits the legacy directory happened to carry.
#[cfg(unix)]
#[test]
fn the_adopted_copy_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let new_dir = root.path().join("state/edge");
    let legacy = root.path().join("cache/edge");
    EdgeIdentity::provision().unwrap().save(&legacy).unwrap();
    // Loosen the legacy directory: adoption must not carry this over.
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o755)).unwrap();

    EdgeIdentity::load_or_adopt(&new_dir, &legacy).unwrap();
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&new_dir), 0o700, "the adopted directory is owner-only");
    for name in ["key.der", "root.key"] {
        assert_eq!(mode(&new_dir.join(name)), 0o600, "{name} is owner-only");
    }
}

#[test]
fn save_refuses_to_overwrite_private_material() {
    let dir = tempfile::tempdir().unwrap();
    let identity = EdgeIdentity::provision().unwrap();
    identity.save(dir.path()).unwrap();
    // A second save must not truncate-in-place: `mode` is only
    // honoured at creation, so overwriting could inherit looser bits.
    let err = identity.save(dir.path()).unwrap_err();
    assert!(
        err.to_string().contains("refusing to write"),
        "expected the overwrite refusal, got: {err}"
    );
}
