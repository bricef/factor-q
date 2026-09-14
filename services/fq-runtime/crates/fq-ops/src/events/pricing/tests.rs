use super::*;

fn provenance(source: &str, digest: &str) -> PricingProvenance {
    PricingProvenance {
        source: source.to_string(),
        commit: Some("f00dcafe".to_string()),
        etag: Some("0badc0de".to_string()),
        digest: digest.to_string(),
        accepted_at: Utc::now(),
    }
}

#[test]
fn the_version_is_the_source_and_twelve_digest_characters() {
    let p = provenance(
        "litellm-main",
        "3f9a1c0b2d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8",
    );
    assert_eq!(p.version(), "litellm-main@3f9a1c0b2d4e");
}

/// A hand-edited or truncated digest must not panic the version — the
/// sidecar is a file on disk an operator can open.
#[test]
fn a_short_digest_yields_a_short_version_rather_than_a_panic() {
    let p = provenance("pinned:abc1234", "beef");
    assert_eq!(p.version(), "pinned:abc1234@beef");
}

#[test]
fn two_runs_on_the_same_table_cite_the_same_version() {
    let digest = "aa".repeat(32);
    let first = provenance("litellm-main", &digest);
    let mut second = provenance("litellm-main", &digest);
    second.accepted_at = first.accepted_at + chrono::Duration::days(1);
    second.commit = Some("another-commit".to_string());
    assert_eq!(
        first.version(),
        second.version(),
        "the version is content-addressed: the same prices are the same version"
    );
}

#[test]
fn an_absent_commit_is_omitted_from_the_wire() {
    let mut p = provenance("litellm-main", "abcdef0123456789");
    p.commit = None;
    let json = serde_json::to_value(&p).unwrap();
    assert!(json.get("commit").is_none());
    assert_eq!(json["etag"], serde_json::json!("0badc0de"));
    let back: PricingProvenance = serde_json::from_value(json).unwrap();
    assert_eq!(back, p);
}

/// The commit and the `ETag` are separate fields because they are
/// separate facts: one names the file's history, the other the blob that
/// arrived. A provenance that has only an `ETag` says so rather than
/// offering it as a commit, which is what a reader would otherwise look
/// up upstream and not find.
#[test]
fn an_etag_never_stands_in_for_a_commit() {
    let mut p = provenance("litellm-main", "abcdef0123456789");
    p.commit = None;
    p.etag = Some("f00dcafe".to_string());
    let json = serde_json::to_value(&p).unwrap();
    assert!(json.get("commit").is_none(), "{json}");
    assert_eq!(json["etag"], serde_json::json!("f00dcafe"));
}

/// Both are optional and both drop out of the wire when unknown — a
/// table the daemon fetched with neither is still identified by its
/// digest.
#[test]
fn a_provenance_with_neither_still_names_its_table() {
    let mut p = provenance("litellm-main", "aa".repeat(32).as_str());
    p.commit = None;
    p.etag = None;
    let json = serde_json::to_value(&p).unwrap();
    assert!(json.get("commit").is_none());
    assert!(json.get("etag").is_none());
    assert_eq!(p.version(), "litellm-main@aaaaaaaaaaaa");
    let back: PricingProvenance = serde_json::from_value(json).unwrap();
    assert_eq!(back, p);
}
