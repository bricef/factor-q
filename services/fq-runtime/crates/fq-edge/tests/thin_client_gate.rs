//! Gate: this crate is what a *thin* client links, and it stays thin.
//!
//! `fq-edge` carries the transport and the envelope — `invoke` and
//! `next_batch` over `(OpId, serde_json::Value)`. Because dispatch is
//! generic and the surface describes itself, that layer does not grow
//! when an operation is added, and it has no reason to know a payload
//! type. Its consumers do: `fq-cli` and `fq-dashboard` deserialise the
//! output into view types and render them, and those types live in
//! `fq-ops` — the transport-free contract half, which is itself a leaf.
//!
//! The property worth keeping is that the *dependency* lives in the
//! consumers rather than here. `fq-runtime` pulls `sqlx`, `async-nats`,
//! `reqwest` and `rmcp`; ADR-0031 Phase 5 reduced `fq` to a client plus
//! renderers by dropping exactly those — `fq-cli` links `fq-edge` and
//! `fq-ops` and no longer links `fq-runtime` at all. This crate was
//! already free of them, and the gate is what keeps it that way as the
//! surface grows. Nothing about that is enforced by the
//! type system — an `EventView` in a signature here would compile
//! perfectly well — so it is a convention until something checks it,
//! and conventions about dependencies are lost in review.
//!
//! The manifest is read at runtime rather than embedded, for the reason
//! `just lint-sources` gives: a compile-time embed of source-as-data is
//! the same splice one step removed, and it goes stale silently.
//!
//! Adding one of these deliberately is still possible — it means
//! editing this list, which is a reviewable act in the diff, and the
//! reviewer's question is "which layer does this belong in?".

use std::path::PathBuf;

/// What a thin client must not link. Each of these is a Phase-5 target
/// named in ADR-0031, and `fq-runtime` transitively carries all of
/// them — which is why it heads the list.
const FORBIDDEN: &[&str] = &["fq-runtime", "async-nats", "sqlx", "reqwest", "rmcp"];

/// Every dependency table — including `[dev-dependencies]`. A test-only
/// edge is still an edge: it would let a payload type reach a signature
/// here through a fixture, which is exactly how the boundary erodes.
#[test]
fn the_envelope_layer_links_nothing_a_thin_client_could_not() {
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
        "fq-edge is the layer a thin client links; these belong in a consumer \
         ({}):\n{}",
        manifest.display(),
        offenders
            .iter()
            .map(|(n, line)| format!("  {n}: {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
