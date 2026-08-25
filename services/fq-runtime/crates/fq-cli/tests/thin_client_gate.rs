//! Gate: `fq` is a client, and links no daemon.
//!
//! ADR-0031 Phase 5 splits the binary — `fqd` hosts the daemon, `fq`
//! talks to it over the authenticated edge — and the acceptance
//! criterion for #264 is a build fact rather than a convention: this
//! crate depends on no `fq-runtime`, and therefore on no store, no
//! broker, no HTTP client and no MCP transport. Every verb dials
//! `fq-edge`, deserialises into `fq-ops` types, and renders.
//!
//! The last edge to fall was `fq agent validate`, which parsed a
//! definition by calling into the runtime — one function, carrying
//! sqlx and NATS and rmcp behind it, to lint a Markdown file. The
//! answer was not to push the parser into the wire crate (the agent
//! domain is not a wire contract) but to notice that reading a
//! definition is legitimately something both ends do: the daemon loads
//! a directory of them at startup, the operator checks one before it is
//! deployed. So the domain became `fq-agent`, and both sides depend on
//! it.
//!
//! Gates compose to make the claim hold. This one refuses a direct edge
//! from `fq`'s own manifest; the `forbidden_dependency_gate` in each of
//! `fq-ops` and `fq-agent` keeps those crates light. Together they close
//! the transitive route — which is how this dependency arrived the
//! first time, through a crate that looked like it only carried types.
//!
//! The manifest is read at runtime rather than embedded, for the reason
//! `just lint-sources` gives: a compile-time embed of source-as-data is
//! the same splice one step removed, and it goes stale silently.
//!
//! Tokio is deliberately absent from the list. This is an async binary
//! and needs a runtime; what it does not need is a way to reach a
//! store, a broker, or a model.

use std::path::PathBuf;

/// What the client must not link. `fq-runtime` heads the list because
/// it transitively carries every other entry — which is how the
/// dependency would return, as one line that looks harmless.
///
/// `fq-daemon` is on the list for the same reason and was missing from
/// it until 2026-08-25: this file is titled for keeping the daemon out
/// of the client, and the crate that *is* the daemon was the one name
/// it did not check. One line adding it re-links `fq-runtime` and
/// everything under it, with this gate still green — the precise
/// regression the binary split exists to prevent.
const FORBIDDEN: &[&str] = &[
    "fq-runtime",
    "fq-daemon",
    "sqlx",
    "async-nats",
    "reqwest",
    "rmcp",
];

/// Every dependency table, `[dev-dependencies]` included. A test-only
/// edge still compiles the crate in, and a fixture built by spawning a
/// daemon in-process is exactly how a client reacquires a reason to
/// link one.
#[test]
fn the_client_links_no_daemon() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let body = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));

    let offenders: Vec<(usize, &str)> = body
        .lines()
        .enumerate()
        // Comments explain the boundary; they must be free to name it.
        .filter(|(_, line)| !line.trim_start().starts_with('#'))
        .filter(|(_, line)| {
            let line = line.trim_start();
            FORBIDDEN.iter().any(|dep| {
                // `fq-daemon = { workspace = true }` — the ordinary form.
                line.starts_with(dep)
                    // `[dependencies.fq-daemon]` — the same edge written
                    // as a table header, which does not begin with the
                    // crate's name and so slips past the check above.
                    || (line.starts_with('[')
                        && line.contains(&format!("dependencies.{dep}")))
            })
        })
        .map(|(i, line)| (i + 1, line.trim()))
        .collect();

    assert!(
        offenders.is_empty(),
        "`fq` is a client: it invokes declared ops over the edge and must link no \
         daemon ({}):\n{}",
        manifest.display(),
        offenders
            .iter()
            .map(|(n, line)| format!("  {n}: {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
