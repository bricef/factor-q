//! Unit tests for [`super`] — the edge's half of the contract: the
//! page cap, the severity vocabulary, and the declaration that
//! publishes both.

use super::*;

fn filter_with_limit(limit: Option<u32>) -> OperatorSignalFilter {
    OperatorSignalFilter {
        limit,
        ..OperatorSignalFilter::default()
    }
}

/// A page over the cap is refused, not silently shortened — the
/// discipline the Event atom states and every list here follows. The
/// refusal names the cap and the way out, so the caller's next request
/// is one edit away rather than a guess.
#[test]
fn a_page_over_the_cap_is_refused_rather_than_shortened() {
    for asked in [
        OPERATOR_SIGNAL_LIST_MAX_LIMIT + 1,
        OPERATOR_SIGNAL_LIST_MAX_LIMIT * 10,
        u32::MAX,
    ] {
        let err = list_limit(&filter_with_limit(Some(asked)))
            .expect_err("a page over the cap must be refused, not served short");
        assert!(
            matches!(&err, WireError::InvalidInput { op, message }
                if op == "operator_signal.list"
                    && message.contains(&asked.to_string())
                    && message.contains(&OPERATOR_SIGNAL_LIST_MAX_LIMIT.to_string())
                    && message.contains("severity")),
            "expected an InvalidInput naming {asked}, the cap and the way out; got {err:?}"
        );
    }
}

/// Under the cap the page size is the caller's own number, which is
/// what makes a row count readable; asking for nothing in particular
/// is the documented default rather than the largest page served.
#[test]
fn under_the_cap_the_page_is_the_callers_own_number() {
    for asked in [
        0,
        1,
        OPERATOR_SIGNAL_LIST_DEFAULT_LIMIT,
        OPERATOR_SIGNAL_LIST_MAX_LIMIT,
    ] {
        assert_eq!(
            list_limit(&filter_with_limit(Some(asked))).ok(),
            Some(asked)
        );
    }
    assert_eq!(
        list_limit(&filter_with_limit(None)).ok(),
        Some(OPERATOR_SIGNAL_LIST_DEFAULT_LIMIT)
    );
}

fn filter_with_severity(severity: Option<&str>) -> OperatorSignalFilter {
    OperatorSignalFilter {
        severity: severity.map(str::to_string),
        ..OperatorSignalFilter::default()
    }
}

/// **A severity that is not one of the two is a verdict on the
/// request.**
///
/// The failure this prevents is quiet: an operator who types `alerts`
/// and is answered with an empty list reads it as "nothing to see",
/// which is the one wrong conclusion a pane about alerts must never
/// invite. So the refusal names the accepted set.
#[test]
fn an_unknown_severity_is_refused_rather_than_answered_with_nothing() {
    for typo in ["alerts", "Alert", "warning", ""] {
        let err = severity_filter(&filter_with_severity(Some(typo)))
            .expect_err("an unknown severity must be refused");
        assert!(
            matches!(&err, WireError::InvalidInput { op, message }
                if op == "operator_signal.list"
                    && message.contains("notification")
                    && message.contains("alert")),
            "expected an InvalidInput naming both severities; got {err:?}"
        );
    }
    for known in OPERATOR_SIGNAL_SEVERITIES {
        assert_eq!(
            severity_filter(&filter_with_severity(Some(known))).ok(),
            Some(Some(known))
        );
    }
    // No severity selects both, which is what it always meant.
    assert_eq!(
        severity_filter(&filter_with_severity(None)).ok(),
        Some(None)
    );
}

/// The severity vocabulary the filter declares is the vocabulary the
/// payload writes.
///
/// Two spellings of one fact drift. `OPERATOR_SIGNAL_SEVERITIES` is
/// what the edge validates against and what the refusal quotes;
/// `SignalSeverity::as_str` is what the projection stores and what the
/// wire carries. This is the assertion that stops a rename of one from
/// silently making every filtered listing empty.
#[test]
fn the_declared_severities_are_the_ones_the_payload_writes() {
    use fq_ops::events::SignalSeverity;
    let written: Vec<&str> = [SignalSeverity::Notification, SignalSeverity::Alert]
        .into_iter()
        .map(SignalSeverity::as_str)
        .collect();
    assert_eq!(written, OPERATOR_SIGNAL_SEVERITIES.to_vec());
}

/// **The cap is on the declared surface, not only in the code.** A
/// consumer has to read the bound off the declaration rather than
/// discover it by being refused, so it ships twice on the filter's
/// `limit` property — as the schema's `maximum` and in the prose — and
/// both are asserted against the constant the daemon enforces.
#[test]
fn the_surface_declares_the_cap_it_enforces() {
    let schema = serde_json::to_value(schemars::schema_for!(OperatorSignalFilter))
        .expect("the filter schema serialises");
    let limit = &schema["properties"]["limit"];
    assert_eq!(
        limit["maximum"].as_u64(),
        Some(u64::from(OPERATOR_SIGNAL_LIST_MAX_LIMIT)),
        "the schema's maximum must be the cap the daemon enforces; got {limit}"
    );
    let described = limit["description"].as_str().expect("a described property");
    assert!(
        described.contains(&OPERATOR_SIGNAL_LIST_MAX_LIMIT.to_string()),
        "the declared description must name the cap; got {described:?}"
    );
}

/// An unparseable `since` is a verdict on the request, in the same
/// grammar every other time-narrowed read on this surface takes — a
/// bare date still names the day's first moment, so an argument copied
/// from `fq costs` or `fq events query` means the same thing here.
#[test]
fn since_takes_the_grammar_the_other_reads_take() {
    assert_eq!(
        since_as_stored(Some("2026-04-25"), "operator_signal.list").unwrap(),
        Some("2026-04-25T00:00:00+00:00".to_string())
    );
    let err = since_as_stored(Some("yesterday"), "operator_signal.list").expect_err("must refuse");
    assert!(
        matches!(&err, WireError::InvalidInput { op, message }
            if op == "operator_signal.list" && message.contains("yesterday")),
        "expected an InvalidInput quoting what was written; got {err:?}"
    );
    assert_eq!(since_as_stored(None, "operator_signal.list").unwrap(), None);
}
