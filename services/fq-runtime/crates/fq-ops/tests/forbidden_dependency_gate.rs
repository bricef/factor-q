//! Source-level gate: forbidden dependencies stay out of fq-ops.
//!
//! ADR-0031's thin `fq` client links this crate alone, so any runtime
//! dependency added here ships in the client binary. This gate parses every
//! normal and build dependency table, including target-specific tables and
//! renamed packages. Transitive leakage is caught by the Phase 5 build-fact
//! gate on `fq`'s own manifest.

use fq_test_support::manifest_dependencies::manifest_dependency_names;

/// Crates that must never be direct dependencies of fq-ops. sqlx and
/// async-nats are the ADR-0031 headline exclusions; tokio/tarpc/axum
/// keep the contract crate runtime-free so every surface (including
/// wasm-adjacent futures) can link it.
const FORBIDDEN: &[&str] = &["sqlx", "async-nats", "tokio", "tarpc", "axum", "reqwest"];

#[test]
fn forbidden_dependencies_stay_out() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read fq-ops Cargo.toml");
    let dependencies = manifest_dependency_names(&manifest).expect("parse fq-ops Cargo.toml");

    for dep in FORBIDDEN {
        assert!(
            !dependencies.contains(*dep),
            "`{dep}` must not be a dependency of fq-ops — this crate is the thin \
             client's entire dependency tree (ADR-0031); put runtime machinery in \
             fq-runtime instead"
        );
    }
}
