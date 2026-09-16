//! Source-level gate: forbidden dependencies stay out of fq-tools.
//!
//! The fq-agent manifest promises that "fq-tools is a leaf battery — no store,
//! no broker, no HTTP" because the operator client links this crate. Parse all
//! dependency forms so that promise remains true as the manifest evolves.

use fq_test_support::manifest_dependencies::manifest_dependency_names;

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
    .expect("read fq-tools Cargo.toml");
    let dependencies = manifest_dependency_names(&manifest).expect("parse fq-tools Cargo.toml");

    for dep in FORBIDDEN {
        assert!(
            !dependencies.contains(*dep),
            "`{dep}` must not be a dependency of fq-tools — fq-tools is a leaf battery \
             linked into the operator client; store, broker, HTTP, and model machinery \
             belong in fq-runtime"
        );
    }
}
