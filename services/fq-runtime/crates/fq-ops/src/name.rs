//! The P8 name grammar: `<domain>.<imperative>` is a command,
//! `<domain>.<noun>` is a query, `<domain>.tail` (or
//! `<domain>.<noun>.tail`) is a stream. An op whose name misparses is
//! misclassified — so the registry refuses it at registration.
//!
//! Imperative-vs-noun cannot be parsed from English; it is parsed from
//! a **curated vocabulary** (P11). Extending a vocabulary below is a
//! deliberate, reviewable act — exactly the curation gate the ADR
//! wants — and an unknown leaf segment is an error, never a guess.

use crate::meta::OpKind;

/// Leaf segments that name a state change. `worker.prune`,
/// `trigger.publish`, `control.reload` … (`run` is here for the
/// traversal ops ADR-0006 names as born-derived: `traversal.run`.)
pub const COMMAND_VERBS: &[&str] = &[
    "down", "drop", "prune", "publish", "reload", "requeue", "rotate", "run",
];

/// Leaf segments that name a projection read. `invocation.list`,
/// `cost.summary`, `registry.describe` … (`status` reads as a query
/// leaf — `traversal.status` — while `runtime.status` stays a probe
/// via the allowlist, which takes precedence.)
pub const QUERY_NOUNS: &[&str] = &[
    "by_agent",
    "describe",
    "doctor",
    "list",
    "query",
    "show",
    "status",
    "summary",
    "transcript",
    "version",
    "watermark",
];

/// The one stream suffix (D5). Streams are the exception, not the
/// default: an endpoint is a stream only when its subject matter is
/// genuinely unbounded and live.
pub const STREAM_LEAF: &str = "tail";

/// D2 keeps `Probe` deliberately small: the two live-infrastructure
/// reads, allowlisted by full name rather than grammar.
pub const PROBE_NAMES: &[&str] = &["runtime.health", "runtime.status"];

/// Why a name failed to parse under the grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("operation name must be `<domain>.<leaf>` (2–3 dot-separated segments), got `{0}`")]
    SegmentCount(String),
    #[error(
        "segment `{segment}` in `{name}` is invalid: segments are lowercase \
         `[a-z][a-z0-9_]*`"
    )]
    BadSegment { name: String, segment: String },
    #[error(
        "leaf segment `{leaf}` of `{name}` is in no vocabulary — if this operation is \
         genuinely new vocabulary, extend `COMMAND_VERBS`/`QUERY_NOUNS` deliberately \
         (P11: curate the registry)"
    )]
    UnknownLeaf { name: String, leaf: String },
}

fn valid_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

/// Parse a name and return the kind the grammar says it must be.
/// Registration compares this against the declared kind and refuses a
/// mismatch — misclassification is a reviewable defect (ADR-0006).
pub fn expected_kind(name: &str) -> Result<OpKind, NameError> {
    let segments: Vec<&str> = name.split('.').collect();
    if !(2..=3).contains(&segments.len()) {
        return Err(NameError::SegmentCount(name.to_string()));
    }
    for segment in &segments {
        if !valid_segment(segment) {
            return Err(NameError::BadSegment {
                name: name.to_string(),
                segment: (*segment).to_string(),
            });
        }
    }
    if PROBE_NAMES.contains(&name) {
        return Ok(OpKind::Probe);
    }
    let leaf = segments[segments.len() - 1];
    if leaf == STREAM_LEAF {
        return Ok(OpKind::Stream);
    }
    if COMMAND_VERBS.contains(&leaf) {
        return Ok(OpKind::Command);
    }
    if QUERY_NOUNS.contains(&leaf) {
        return Ok(OpKind::Query);
    }
    Err(NameError::UnknownLeaf {
        name: name.to_string(),
        leaf: leaf.to_string(),
    })
}
