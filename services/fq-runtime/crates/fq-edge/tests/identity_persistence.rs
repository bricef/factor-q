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
fn private_material_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    EdgeIdentity::provision().unwrap().save(dir.path()).unwrap();
    for name in ["key.der", "root.key"] {
        let mode = std::fs::metadata(dir.path().join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name} must be owner-only, got {mode:o}");
    }
}
