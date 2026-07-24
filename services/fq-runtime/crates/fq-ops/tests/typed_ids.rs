//! The typed op-identifier vocabulary: every verb and report the
//! catalogue declares is an enum variant, named at exactly one site
//! (the declaration constructor), with `Unknown` reserved for
//! version-skew grace. These tests pin the three properties the
//! typing owes us: typed variants roundtrip the wire without decay,
//! unknown vocabulary parses (never a wire error) and resolves to
//! nothing, and the wire bytes are unchanged from the pre-typed
//! encoding.

use std::str::FromStr;

use fq_ops::{Control, Cost, Domain, Invocation, OpId, ReportId, Trigger, VerbId};
use strum::IntoEnumIterator;

/// Every typed verb, across every per-domain enum. Grows with the
/// catalogue; the tests below iterate it so new variants are covered
/// automatically.
fn all_verbs() -> Vec<VerbId> {
    let mut verbs: Vec<VerbId> = Vec::new();
    verbs.extend(Invocation::iter().map(VerbId::Invocation));
    verbs.extend(Control::iter().map(VerbId::Control));
    verbs.extend(Trigger::iter().map(VerbId::Trigger));
    verbs
}

fn all_reports() -> Vec<ReportId> {
    Cost::iter().map(ReportId::Cost).collect()
}

/// Serde roundtrip never decays a typed variant to `Unknown`: the
/// parse table and the render table are the same strum derives, so
/// they cannot disagree.
#[test]
fn typed_variants_roundtrip_without_decay() {
    for verb in all_verbs() {
        let json = serde_json::to_string(&verb).unwrap();
        let back: VerbId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, verb, "roundtrip decayed: {json}");
        assert!(!matches!(back, VerbId::Unknown { .. }));
    }
    for report in all_reports() {
        let json = serde_json::to_string(&report).unwrap();
        let back: ReportId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report, "roundtrip decayed: {json}");
    }
}

/// The typed segments and the wire encoding agree — the analogue of
/// `verb_encoding.rs` for op vocabulary. The domain segment must
/// also parse back to the same domain (`EnumString` and
/// `IntoStaticStr` are inverse).
#[test]
fn segments_agree_with_the_wire_encoding() {
    for verb in all_verbs() {
        let value = serde_json::to_value(&verb).unwrap();
        assert_eq!(value["domain"], verb.domain_segment());
        assert_eq!(value["verb"], verb.verb_segment());
        assert_eq!(
            Domain::from_str(verb.domain_segment()).unwrap(),
            verb.domain().unwrap()
        );
    }
    for report in all_reports() {
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["domain"], report.domain_segment());
        assert_eq!(value["name"], report.name_segment());
    }
}

/// Version skew: vocabulary this build doesn't know — an unknown
/// verb on a known domain, or a wholly unknown domain — parses to
/// `Unknown` (never a wire error) and renders faithfully. A domain
/// verb pair that exists as words but was never declared behaves the
/// same way: representable, refusable.
#[test]
fn unknown_vocabulary_parses_to_unknown() {
    let cases = [
        (
            r#"{"domain":"invocation","verb":"frobnicate"}"#,
            "invocation.frobnicate",
        ),
        (
            r#"{"domain":"warp_core","verb":"eject"}"#,
            "warp_core.eject",
        ),
        // A real domain crossed with another domain's real verb —
        // the old stringly nonsense case, now only reachable via the
        // wire, never from code.
        (r#"{"domain":"cost","verb":"drop"}"#, "cost.drop"),
    ];
    for (json, rendered) in cases {
        let verb: VerbId = serde_json::from_str(json).unwrap();
        assert!(matches!(verb, VerbId::Unknown { .. }), "{json}");
        assert_eq!(OpId::Verb(verb).to_string(), rendered);
    }
}

/// An `Unknown` op resolves to nothing — the registry refuses it as
/// not-registered, which is the skew-grace contract end to end.
#[test]
fn unknown_ops_resolve_to_nothing() {
    let mut registry = fq_ops::Registry::new();
    registry.register(fq_ops::fixtures::invocation()).unwrap();
    registry
        .register(fq_ops::fixtures::invocation_drop())
        .unwrap();

    let skewed: VerbId =
        serde_json::from_str(r#"{"domain":"invocation","verb":"frobnicate"}"#).unwrap();
    assert!(registry.resolve(&OpId::Verb(skewed)).is_none());

    // And the typed declaration still resolves — Unknown is a
    // parallel track, not a replacement.
    assert!(
        registry
            .resolve(&OpId::Verb(VerbId::Invocation(Invocation::Drop)))
            .is_some()
    );
}
