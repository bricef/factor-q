//! Source-level gate: fq-ops stays a leaf.
//!
//! ADR-0031's thin `fq` client links this crate alone, so any runtime
//! dependency added here ships in the client binary. This gate fails
//! the moment a forbidden crate appears in `[dependencies]` — the
//! same tripwire idea as fq-cli's `store_open_gate.rs`. It reads the
//! manifest textually (no cargo metadata dependency), which catches
//! direct additions; transitive leakage is caught by the Phase 5
//! build-fact gate on `fq`'s own manifest.

/// Crates that must never be direct dependencies of fq-ops. sqlx and
/// async-nats are the ADR-0031 headline exclusions; tokio/tarpc/axum
/// keep the contract crate runtime-free so every surface (including
/// wasm-adjacent futures) can link it.
const FORBIDDEN: &[&str] = &["sqlx", "async-nats", "tokio", "tarpc", "axum", "reqwest"];

#[test]
fn dependencies_stay_leaf() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read fq-ops Cargo.toml");

    // Scan only the `[dependencies]` table: dev-dependencies never
    // reach the client binary.
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let dep = line.split(['=', ' ', '.']).next().unwrap_or_default();
        assert!(
            !FORBIDDEN.contains(&dep),
            "`{dep}` must not be a dependency of fq-ops — this crate is the thin \
             client's entire dependency tree (ADR-0031); put runtime machinery in \
             fq-runtime instead"
        );
    }
}
