use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::events::subjects;

/// The registry is the pane's filter list and a producer's only way to
/// mint a kind, so an entry that does not parse would be a producer
/// panicking in production. Checked as a set: valid, distinct, and each
/// naming a source.
#[test]
fn every_registered_kind_parses_and_is_distinct() {
    let mut seen = std::collections::BTreeSet::new();
    for entry in kinds::REGISTERED {
        let kind = SignalKind::new(*entry).unwrap_or_else(|e| {
            panic!("`{entry}` is in the registry but is not a valid kind: {e}")
        });
        assert_eq!(kind.as_str(), *entry);
        assert!(!kind.source().is_empty(), "`{entry}` names no source");
        assert!(seen.insert(*entry), "`{entry}` is in the registry twice");
    }
    // The two #735 emits and the one the deploy message is reserved for.
    assert!(seen.contains(&kinds::PRICING_CHANGE_REFUSED));
    assert!(seen.contains(&kinds::PRICING_STALE));
    assert!(seen.contains(&kinds::DEPLOY_SUCCEEDED));
}

/// A kind is `<source>.<name>`, and the source is read off the kind
/// rather than carried beside it — so the two cannot disagree.
#[test]
fn a_kind_splits_into_its_source_and_its_name() {
    let kind = SignalKind::new("pricing.change_refused").unwrap();
    assert_eq!(kind.source(), "pricing");
    assert_eq!(kind.name(), "change_refused");
    assert_eq!(kind.to_string(), "pricing.change_refused");

    // More than two segments: the source is still the first, and the
    // name is everything after it.
    let deep = SignalKind::new("deploy.rollback.failed").unwrap();
    assert_eq!(deep.source(), "deploy");
    assert_eq!(deep.name(), "rollback.failed");
}

/// The validation is what stops one thing being two kinds in a pane
/// that groups by name.
#[test]
fn a_kind_refuses_what_would_read_as_a_second_spelling() {
    assert_eq!(SignalKind::new(""), Err(SignalKindError::Empty));
    assert!(matches!(
        SignalKind::new("pricing"),
        Err(SignalKindError::MissingSource(_))
    ));
    for bad in [
        "Pricing.change_refused",
        "pricing.changeRefused",
        "pricing.",
        ".change_refused",
        "pricing..refused",
        "pricing.change refused",
        "pricing.change-refused",
        "pricing.*",
    ] {
        assert!(
            matches!(
                SignalKind::new(bad),
                Err(SignalKindError::InvalidSegment(_) | SignalKindError::MissingSource(_))
            ),
            "`{bad}` was accepted as a kind"
        );
    }
}

/// The kind rule is stricter than a subject token needs, and this is
/// where that claim is checked rather than assumed. It is what keeps the
/// door open to a per-source subject later
/// (<https://github.com/bricef/factor-q/issues/736>) without
/// re-validating every kind on the log.
#[test]
fn every_kind_segment_is_a_legal_subject_token() {
    for entry in kinds::REGISTERED {
        for segment in entry.split('.') {
            assert!(
                subjects::validate_token(segment).is_ok(),
                "`{segment}` of `{entry}` is not a legal subject token"
            );
        }
    }
}

/// The wire spellings of the two severities are the contract the
/// dashboard's stripe and chip are keyed on, and `as_str` is a second
/// spelling of what serde writes — so the two are asserted equal rather
/// than trusted.
#[test]
fn the_severities_spell_themselves_the_same_way_twice() {
    for (severity, spelling) in [
        (SignalSeverity::Notification, "notification"),
        (SignalSeverity::Alert, "alert"),
    ] {
        assert_eq!(severity.as_str(), spelling);
        assert_eq!(serde_json::to_value(severity).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<SignalSeverity>(json!(spelling)).unwrap(),
            severity
        );
    }
    assert!(SignalSeverity::Alert.is_alert());
    assert!(!SignalSeverity::Notification.is_alert());
}

/// A signal with nothing to point at and no particulars carries neither
/// on the wire — an empty `detail: {}` and an empty `references: {}`
/// would say the producer sent something empty rather than nothing.
#[test]
fn a_bare_signal_omits_its_detail_and_its_references() {
    let payload = OperatorSignalPayload::notification(
        SignalKind::registered(kinds::DEPLOY_SUCCEEDED),
        "build ff7db0e is live",
    );
    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(
        json,
        json!({
            "severity": "notification",
            "kind": "deploy.succeeded",
            "summary": "build ff7db0e is live",
        })
    );
    let read: OperatorSignalPayload = serde_json::from_value(json).unwrap();
    assert!(read.detail.is_null());
    assert!(read.references.is_empty());
}

/// The full shape, round-tripped: severity, kind, summary, the
/// kind-shaped `detail`, and the three references.
#[test]
fn a_signal_round_trips_with_its_detail_and_references() {
    let invocation = Uuid::now_v7();
    let payload = OperatorSignalPayload::alert(
        SignalKind::registered(kinds::PRICING_STALE),
        "the pricing table has not refreshed for 26 hours",
    )
    .with_detail(json!({"last_refresh_ms": 1_788_000_000_000i64, "window_hours": 24}))
    .about_invocation(
        crate::agent::AgentId::new("researcher").unwrap(),
        invocation,
    )
    .with_url("https://github.com/bricef/factor-q/actions/runs/1");

    let json = serde_json::to_value(&payload).unwrap();
    assert_eq!(json["severity"], "alert");
    assert_eq!(json["kind"], "pricing.stale");
    assert_eq!(json["detail"]["window_hours"], 24);
    assert_eq!(json["references"]["agent_id"], "researcher");
    assert_eq!(json["references"]["invocation_id"], invocation.to_string());
    assert_eq!(
        json["references"]["url"],
        "https://github.com/bricef/factor-q/actions/runs/1"
    );

    let read: OperatorSignalPayload = serde_json::from_value(json).unwrap();
    assert_eq!(read.severity, SignalSeverity::Alert);
    assert_eq!(read.kind, payload.kind);
    assert_eq!(read.source(), "pricing");
    assert_eq!(read.references.invocation_id, Some(invocation));
}

/// A kind that reached the wire malformed is refused at the payload
/// boundary rather than reaching a pane that groups by source. (An
/// `event_type` this build does not know degrades to
/// [`EventPayload::Unknown`](crate::events::EventPayload::Unknown); a
/// known type with a malformed body does not, by design.)
#[test]
fn a_malformed_kind_is_refused_on_the_way_in() {
    let err = serde_json::from_value::<OperatorSignalPayload>(json!({
        "severity": "alert",
        "kind": "PRICING",
        "summary": "no source, wrong case",
    }))
    .unwrap_err();
    assert!(err.to_string().contains("names no source"), "{err}");
}

/// `registered` takes registry entries and nothing else — an ad-hoc kind
/// invented at a producer would be a value the pane's filter list never
/// learns about.
#[test]
#[should_panic(expected = "is not in the operator-signal kind registry")]
fn registered_refuses_a_kind_that_is_not_in_the_registry() {
    let _ = SignalKind::registered("pricing.invented_here");
}
