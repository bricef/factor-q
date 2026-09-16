//! Source-level gate: fq-tools remains a leaf battery.
//!
//! fq-agent's manifest records the contract this gate enforces: "fq-tools is
//! a leaf battery — no store, no broker, no HTTP — so carrying it costs the
//! client nothing it does not already link." The operator client reaches this
//! crate directly and through fq-agent, so heavy dependencies here ship in it.

use fq_test_support::forbidden_dependencies::dependency_names;

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
    let dependencies = dependency_names(&manifest).expect("parse fq-tools Cargo.toml");

    for forbidden in FORBIDDEN {
        assert!(
            !dependencies.contains(*forbidden),
            "`{forbidden}` must not be a dependency of fq-tools — fq-tools is a leaf \
             battery linked by the operator client; store, broker, HTTP, and model \
             machinery belongs in fq-runtime"
        );
    }
}
