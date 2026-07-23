//! The edge identity survives a daemon restart: `save`/`load`
//! roundtrip the TLS material and the token root — the fingerprint
//! clients pin is stable, and tokens minted before the restart verify
//! after it. Private material lands owner-only on disk.

use fq_edge::EdgeIdentity;
use fq_edge::auth::verify_token;

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
