//! Gate: the dashboard reads through the edge, and links no store.
//!
//! This crate is the first consumer to hold the shape ADR-0031 Phase 5
//! wants for `fq` itself: a reader that talks to a daemon over the
//! edge, renders the typed shapes it answers with, and links neither
//! the daemon crate nor the storage under it. Every page dials
//! `fq-edge`, deserialises into `fq-ops` types, and renders.
//!
//! It used to depend on `fq-runtime` for those types, which meant
//! linking sqlx, NATS, reqwest and rmcp to render a table — a
//! dependency on a database driver held by a process that never opens
//! a database. The types moved to `fq-ops`; this gate is what stops
//! them drifting back.
//!
//! Two gates compose to make the claim hold. This one refuses a direct
//! edge from the dashboard's own manifest. `fq-ops`' own
//! `forbidden_dependency_gate` keeps the leaf a leaf, which is what
//! closes the transitive route — the way this dependency arrived the
//! first time, through a crate that looked like it only carried types.
//!
//! The manifest is read at runtime rather than embedded, for the reason
//! `just lint-sources` gives: a compile-time embed of source-as-data is
//! the same splice one step removed, and it goes stale silently.
//!
//! Tokio is deliberately absent from the list. This is an axum server
//! and needs a runtime; what it does not need is a way to reach a
//! store.

use std::path::PathBuf;

/// What a reader must not link. `fq-runtime` heads the list because it
/// transitively carries every other entry — which is how the dependency
/// would return, as one line that looks harmless.
const FORBIDDEN: &[&str] = &["fq-runtime", "sqlx", "async-nats", "reqwest", "rmcp"];

/// Every dependency table, `[dev-dependencies]` included. A test-only
/// edge still compiles the crate in, and a fixture built from a store
/// row is exactly how a reader reacquires a reason to open one.
#[test]
fn the_dashboard_links_no_path_to_a_store() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let body = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));

    let offenders: Vec<(usize, &str)> = body
        .lines()
        .enumerate()
        // Comments explain the boundary; they must be free to name it.
        .filter(|(_, line)| !line.trim_start().starts_with('#'))
        .filter(|(_, line)| {
            FORBIDDEN
                .iter()
                .any(|dep| line.trim_start().starts_with(dep))
        })
        .map(|(i, line)| (i + 1, line.trim()))
        .collect();

    assert!(
        offenders.is_empty(),
        "the dashboard reads through the edge and must link no store \
         ({}):\n{}",
        manifest.display(),
        offenders
            .iter()
            .map(|(n, line)| format!("  {n}: {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
