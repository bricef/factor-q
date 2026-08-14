use super::*;

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
