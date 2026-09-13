//! Unit tests for [`super`] — the rendering, without an edge.

use super::*;
use fq_ops::events::SignalSeverity;
use fq_ops::views::SignalReferencesView;

fn signal() -> OperatorSignalDetailView {
    OperatorSignalDetailView {
        event_id: "01990000-0000-7000-8000-000000000028".into(),
        timestamp: "2026-09-07T12:00:28+00:00".into(),
        agent_id: "system".into(),
        invocation_id: "01990000-0000-7000-8000-000000002000".into(),
        seq: Some(42),
        severity: SignalSeverity::Alert,
        source: "pricing".into(),
        kind: "pricing.stale".into(),
        summary: "the pricing table has not refreshed in 31 hours".into(),
        detail: serde_json::json!({ "window_hours": 24 }),
        references: SignalReferencesView {
            url: Some("https://example.invalid/run/1".into()),
            ..SignalReferencesView::default()
        },
        newer_from_source: None,
        older_from_source: Some("01990000-0000-7000-8000-000000000027".into()),
    }
}

/// The rendering carries the four things an operator triages on — when,
/// how loudly, what it was, and the line — plus the identity that reads
/// it back and the walk to its neighbour.
#[test]
fn a_signal_renders_its_severity_kind_summary_and_walk() {
    let out = render_signal(&signal());
    assert!(out.contains("alert"), "got: {out}");
    assert!(out.contains("pricing.stale"), "got: {out}");
    assert!(
        out.contains("the pricing table has not refreshed in 31 hours"),
        "got: {out}"
    );
    assert!(
        out.contains("01990000-0000-7000-8000-000000000028"),
        "the identity must be printed in full so the walk is copyable: {out}"
    );
    assert!(out.contains("https://example.invalid/run/1"), "got: {out}");
    assert!(out.contains("window_hours"), "got: {out}");
    assert!(
        out.contains("older") && out.contains("01990000-0000-7000-8000-000000000027"),
        "got: {out}"
    );
}

/// **A kind with no particulars says so.** An empty block and a block
/// that failed to render read the same to an operator, and one of them
/// is a bug — so the absent case is a sentence rather than a gap.
#[test]
fn a_signal_with_no_detail_says_so_rather_than_showing_a_gap() {
    let mut signal = signal();
    signal.detail = serde_json::Value::Null;
    let out = render_signal(&signal);
    assert!(
        out.contains("(none — this kind carries no particulars)"),
        "got: {out}"
    );
}

/// The summary is a producer-written sentence with no bound on the
/// wire, so the table cell caps it on a char boundary rather than
/// splitting a multi-byte character or spilling the column.
#[test]
fn a_long_summary_is_capped_on_a_char_boundary() {
    let long = "é".repeat(200);
    let capped = display_cap(&long, 52);
    assert_eq!(capped.chars().count(), 52);
    assert!(capped.ends_with('…'));
    // Short lines are untouched — no ellipsis on a summary that fits.
    assert_eq!(display_cap("short", 52), "short");
}
