use super::*;

/// A 64-byte string with a multi-byte codepoint passes the length
/// gate; slicing must not panic mid-codepoint (ultrareview
/// bug_001) — it errors like any other junk.
#[test]
fn fingerprint_hex_rejects_non_ascii_without_panicking() {
    let hostile = format!("a\u{e9}{}", "a".repeat(61));
    assert_eq!(hostile.len(), 64, "64 bytes, 63 chars — the trap input");
    let err = parse_fingerprint_hex(&hostile).unwrap_err();
    assert!(err.to_string().contains("not valid hex"), "{err}");
}

/// Concurrent `fq connect` runs must merge, not lose each other's
/// entries (ultrareview bug_004): every writer re-reads under the
/// lock, so a slow writer cannot clobber a fast one.
#[test]
fn concurrent_connects_merge_rather_than_lose_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("factor-q").join("connections.toml");
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                store_connection(
                    &path,
                    &format!("127.0.0.1:{i}"),
                    ConnectionEntry {
                        fingerprint: "f".repeat(64),
                        token: format!("token-{i}"),
                    },
                )
                .unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let store = load_connections(&path).unwrap();
    assert_eq!(
        store.connections.len(),
        8,
        "every concurrent insert survives"
    );
}
