//! Source-level gate: forbidden dependencies stay out of fq-agent.
//!
//! This crate exists so that both ends can read an agent definition —
//! the daemon loading a directory at startup, and `fq agent validate`
//! linting one file offline. The second of those is only worth having
//! if the crate stays light: `fq` links this, so anything heavy added
//! here ships in the operator's binary and quietly re-opens the
//! dependency `fq-cli`'s `thin_client_gate.rs` says is closed.
//!
//! That gate reads `fq-cli`'s own manifest and so sees direct edges
//! only. This is the other half — a crate that looks like it only
//! carries a domain model is exactly the route by which a store driver
//! travelled into the dashboard once already. Same tripwire as
//! `fq-ops`' gate of the same name.

/// Crates that must never be direct dependencies of fq-agent. The
/// store, the broker, and the two clients the runtime uses to reach a
/// model — parsing Markdown needs none of them.
const FORBIDDEN: &[&str] = &[
    "fq-runtime",
    "sqlx",
    "async-nats",
    "reqwest",
    "rmcp",
    "genai",
];

#[test]
fn forbidden_dependencies_stay_out() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read fq-agent Cargo.toml");

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
            "`{dep}` must not be a dependency of fq-agent — the operator client links \
             this crate to validate a definition offline (#264), so a store, a broker \
             or a model client added here ships in `fq`. It belongs in fq-runtime."
        );
    }
}
