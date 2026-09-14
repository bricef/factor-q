//! Unit tests for [`super`] — the pane's rendering, from the same
//! fixtures the screenshot gallery renders, so a test and a screenshot
//! cannot disagree about what the page looks like.

use super::*;
use crate::fixtures;

fn rows() -> Vec<OperatorSignalView> {
    fixtures::notification_signals()
}

/// **Both severities, and the difference is carried in form.**
///
/// This is the assertion the whole pane exists for. Colour is not a
/// distinction to bet an out-of-hours page on — a greyscale
/// screenshot, a printed row, a reader who does not see red — so the
/// alert wears a stripe (`tr class="alert"`) and the word behind a
/// glyph, and the notification wears a chip. A change that dropped
/// either and kept the hue would pass a colour-only test and fail
/// here.
#[test]
fn both_severities_are_marked_in_form_and_not_only_in_colour() {
    let html = notifications(&rows(), &SignalFilters::default(), 1_783_803_600_000);
    assert!(
        html.contains(r#"<tr class="alert">"#),
        "the alert row must carry the stripe class: {html}"
    );
    assert!(html.contains("▲ alert"), "got: {html}");
    assert!(
        html.contains(r#"<span class="chip">notification</span>"#),
        "a notification must wear the chip: {html}"
    );
    // The words themselves, not just the classes: the class draws the
    // stripe and the word is what a screen reader gets.
    assert!(html.contains("alert") && html.contains("notification"));
}

/// Every row carries its source, its time and the walk into its own
/// page — a listing an operator cannot open from is a listing that has
/// quietly lost the particulars rather than deferred them.
#[test]
fn every_row_carries_its_source_time_and_a_way_in() {
    let rows = rows();
    let html = notifications(&rows, &SignalFilters::default(), 1_783_803_600_000);
    for row in &rows {
        assert!(
            html.contains(&format!(r#"<a href="/notifications/{}">"#, row.event_id)),
            "row {} must link to its detail page: {html}",
            row.event_id
        );
        assert!(html.contains(&row.source), "got: {html}");
        assert!(html.contains(&row.kind), "got: {html}");
    }
    // Ages, not raw timestamps — the same reading every other listing
    // here gives.
    assert!(
        html.contains("m ago") || html.contains("h ago"),
        "got: {html}"
    );
    // And the pane says why a row can be older than the log, because a
    // listing that reaches back further than `/events` invites exactly
    // that question.
    assert!(html.contains("alerts are never swept"), "got: {html}");
}

/// The severity and source selectors render the current state as text
/// and the rest as links, the way the costs page's window row does —
/// so the filter in force is legible without reading the URL.
#[test]
fn the_filters_show_which_one_is_in_force() {
    let html = notifications(&rows(), &SignalFilters::default(), 1_783_803_600_000);
    assert!(html.contains("<b>all</b>"), "got: {html}");
    assert!(
        html.contains(r#"<a href="/notifications?severity=alert">alerts</a>"#),
        "got: {html}"
    );
    // Two sources in the fixture, so the source row appears.
    assert!(
        html.contains(r#"<a href="/notifications?source=pricing">pricing</a>"#),
        "got: {html}"
    );

    let alerts_only = SignalFilters {
        severity: Some("alert".into()),
        source: None,
    };
    let rows: Vec<OperatorSignalView> = rows()
        .into_iter()
        .filter(|r| r.severity.is_alert())
        .collect();
    let html = notifications(&rows, &alerts_only, 1_783_803_600_000);
    assert!(html.contains("<b>alerts</b>"), "got: {html}");
    assert!(
        html.contains(r#"<a href="/notifications">all</a>"#),
        "the way back to everything must stay one click away: {html}"
    );
}

/// **The empty pane says what empty means.** "Nothing has asked for a
/// person's attention" is the state an operator sees most days, and a
/// bare heading reads as a page that failed to load — which is the one
/// wrong conclusion a pane about alerts must never invite.
#[test]
fn the_empty_pane_says_what_empty_means() {
    let html = notifications(&[], &SignalFilters::default(), 1_783_803_600_000);
    assert!(
        html.contains("Nothing has asked for a person's attention"),
        "got: {html}"
    );
    assert!(!html.contains("<table>"), "no table over no rows: {html}");

    // With a filter in force it is a different fact, and says so:
    // "none match" is not "none exist".
    let filtered = SignalFilters {
        severity: Some("alert".into()),
        source: None,
    };
    let html = notifications(&[], &filtered, 1_783_803_600_000);
    assert!(html.contains("match the filters"), "got: {html}");
}

/// The detail page shows the structured particulars, the envelope the
/// signal rode in on, the links out to what it concerns, and the walk
/// through its own source's history — the four things a listing
/// deliberately leaves out.
#[test]
fn the_detail_page_carries_the_particulars_envelope_and_walk() {
    let signal = fixtures::notification_signal_detail();
    let html = notification_detail(&signal, 1_783_803_600_000);
    assert!(html.contains("▲ alert"), "got: {html}");
    // The structured detail, pretty-printed and escaped.
    assert!(html.contains("window_hours"), "got: {html}");
    assert!(html.contains("<pre>"), "got: {html}");
    // The envelope, as its own block: an operator signal is
    // daemon-scoped, so where it came from and what it is about are
    // deliberately two different questions.
    assert!(html.contains("factor-q/operator_signal@1"), "got: {html}");
    assert!(html.contains(&signal.event_id), "got: {html}");
    assert!(html.contains("148112"), "the log position: {html}");
    // Somewhere to look.
    assert!(
        html.contains("https://github.com/BerriAI/litellm/commits/main"),
        "got: {html}"
    );
    // The walk: one neighbour present, one absent — the absent one is
    // muted rather than missing, so the control does not move between
    // pages.
    assert!(
        html.contains(
            r#"<a href="/notifications/019f6a00-0000-7000-8000-0000000000a2">older →</a>"#
        ),
        "got: {html}"
    );
    assert!(
        html.contains(r#"<span class="muted">← newer</span>"#),
        "got: {html}"
    );
}

/// A kind with no particulars says so rather than showing an empty
/// block: an absent `detail` and a `detail` that failed to render must
/// not look the same.
#[test]
fn a_signal_with_no_detail_says_so_rather_than_showing_an_empty_block() {
    let mut signal = fixtures::notification_signal_detail();
    signal.detail = serde_json::Value::Null;
    let html = notification_detail(&signal, 1_783_803_600_000);
    assert!(html.contains("carries no particulars"), "got: {html}");
    assert!(!html.contains("<pre>"), "got: {html}");
}

/// A summary is producer-written text with no bound and no escaping on
/// the wire, and it is the first attacker-influenced string this pane
/// renders. Every dynamic value goes through `esc`.
#[test]
fn a_hostile_summary_is_escaped() {
    let rows = [OperatorSignalView {
        event_id: "019f6a01-0000-7000-8000-0000000000ff".into(),
        timestamp: "2026-07-11T20:49:00+00:00".into(),
        severity: SignalSeverity::Notification,
        source: "pricing".into(),
        kind: "pricing.change_refused".into(),
        summary: "<script>alert(1)</script>".into(),
        resolves: None,
        resolved_by: None,
    }];
    let html = notifications(&rows, &SignalFilters::default(), 1_783_803_600_000);
    assert!(!html.contains("<script>"), "got: {html}");
    assert!(html.contains("&lt;script&gt;"), "got: {html}");
}

/// **A resolved alert reads as resolved, on the row.**
///
/// The count on the home page is a fold over `resolves`, so the pane
/// has to show the same state or the two disagree: an operator who
/// sees "1 open alert" and then a list of two alerts with nothing to
/// tell them apart learns that the number is wrong. The stripe is the
/// carrier — it means *this is open* — and the row says the word as
/// well, because the stripe's absence is not a signal on its own.
#[test]
fn a_resolved_alert_is_marked_resolved_and_loses_the_stripe() {
    let rows = rows();
    let html = notifications(&rows, &SignalFilters::default(), 1_783_803_600_000);

    let open = rows
        .iter()
        .find(|r| r.severity.is_alert() && r.resolved_by.is_none())
        .expect("the fixture has an open alert");
    let closed = rows
        .iter()
        .find(|r| r.severity.is_alert() && r.resolved_by.is_some())
        .expect("the fixture has a resolved alert");
    // Rows are found by their summary rather than by their identity:
    // a resolved row also *links* to the signal that closed it, so
    // matching on the id alone picks up the wrong row.
    let row_of = |summary: &str| {
        html.split("<tr")
            .find(|row| row.contains(summary))
            .map(|row| format!("<tr{row}"))
            .expect("the row is rendered")
    };

    let open_row = row_of(&open.summary);
    assert!(
        open_row.starts_with(r#"<tr class="alert">"#),
        "an open alert keeps the stripe: {open_row}"
    );
    assert!(open_row.contains("▲ alert"), "got: {open_row}");
    assert!(
        !open_row.contains("resolved"),
        "and claims nothing about being closed: {open_row}"
    );

    let closed_row = row_of(&closed.summary);
    assert!(
        !closed_row.starts_with(r#"<tr class="alert">"#),
        "a resolved alert drops the stripe: {closed_row}"
    );
    assert!(
        closed_row.contains("▲ alert · resolved"),
        "…and says so in words: {closed_row}"
    );
    assert!(
        closed_row.contains(&format!(
            r#"resolved by <a href="/notifications/{}">"#,
            closed.resolved_by.as_deref().unwrap()
        )),
        "with the link to what closed it: {closed_row}"
    );

    // And the other end of the relation: the recovery names what it
    // resolved, so the walk works from either row.
    let recovery = rows
        .iter()
        .find(|r| r.resolves.is_some())
        .expect("the fixture has a resolving signal");
    assert!(
        row_of(&recovery.summary).contains(&format!(
            r#"resolves <a href="/notifications/{}">"#,
            recovery.resolves.as_deref().unwrap()
        )),
        "got: {html}"
    );
}

/// The detail page carries the same relation, in both directions, and
/// names an unresolved alert's state rather than leaving it implied by
/// an absent row.
#[test]
fn the_detail_page_says_whether_an_alert_is_open() {
    let mut signal = fixtures::notification_signal_detail();
    assert!(signal.severity.is_alert(), "the fixture is the alert");

    let html = notification_detail(&signal, 1_783_803_600_000);
    assert!(
        html.contains(r#"<th>state</th><td><span class="bad">open</span>"#),
        "an alert nothing has resolved says it is open: {html}"
    );

    signal.resolved_by = Some("019f69f1-0000-7000-8000-0000000000a5".into());
    let html = notification_detail(&signal, 1_783_803_600_000);
    assert!(
        html.contains(
            r#"<th>resolved by</th><td><a href="/notifications/019f69f1-0000-7000-8000-0000000000a5">"#
        ),
        "got: {html}"
    );
    assert!(
        !html.contains("<th>state</th>"),
        "a resolved alert is not also open: {html}"
    );
    assert!(html.contains("▲ alert · resolved"), "got: {html}");

    // The other direction, on the signal that did the resolving.
    signal.resolved_by = None;
    signal.resolves = Some("019f69f0-0000-7000-8000-0000000000a4".into());
    let html = notification_detail(&signal, 1_783_803_600_000);
    assert!(
        html.contains(
            r#"<th>resolves</th><td><a href="/notifications/019f69f0-0000-7000-8000-0000000000a4">"#
        ),
        "got: {html}"
    );
}

/// **A full page says it is full.** A listing that ended and a listing
/// the cap cut short are the same bytes, and this pane's own design
/// argues that point at length to justify the counts report — so it
/// must not then truncate in silence. Under the cap it says nothing:
/// a "there may be more" on every page is a line nobody reads.
#[test]
fn a_page_at_the_cap_says_there_are_probably_more() {
    let one = rows().into_iter().next().expect("a fixture row");
    let short = notifications(&rows(), &SignalFilters::default(), 1_783_803_600_000);
    assert!(
        !short.contains("this page is full"),
        "a short page claims nothing: {short}"
    );

    let full: Vec<OperatorSignalView> = (0..super::super::SIGNAL_PAGE_LIMIT)
        .map(|n| OperatorSignalView {
            event_id: format!("019f6a01-0000-7000-8000-{n:012}"),
            ..one.clone()
        })
        .collect();
    let html = notifications(&full, &SignalFilters::default(), 1_783_803_600_000);
    assert!(html.contains("this page is full"), "got: {html}");
    assert!(
        html.contains(&format!(
            "showing the most recent {}",
            super::super::SIGNAL_PAGE_LIMIT
        )),
        "and names the number: {html}"
    );
}

/// **`references.url` is a link only for http(s).** `esc` escapes
/// HTML, not a scheme: `javascript:` survives it whole and an `href`
/// runs it. The row is copied verbatim from an event payload, so the
/// check belongs at the render rather than in a promise about
/// producers.
#[test]
fn a_reference_url_is_a_link_only_for_a_web_address() {
    let mut signal = fixtures::notification_signal_detail();

    signal.references.url = Some("https://example.invalid/run/1".into());
    let html = notification_detail(&signal, 1_783_803_600_000);
    assert!(
        html.contains(r#"<a href="https://example.invalid/run/1" rel="noreferrer noopener">"#),
        "got: {html}"
    );

    for hostile in [
        "javascript:alert(1)",
        "data:text/html;base64,PHNjcmlwdD4=",
        "JavaScript:alert(1)",
    ] {
        signal.references.url = Some(hostile.into());
        let html = notification_detail(&signal, 1_783_803_600_000);
        assert!(
            !html.contains(&format!(r#"href="{hostile}"#)),
            "{hostile} must not become an href: {html}"
        );
        assert!(
            html.contains("not a link"),
            "…and the page says why: {html}"
        );
    }
}

/// The filter links are URLs, so their values are percent-encoded
/// before they are HTML-escaped. Escaping alone turns `&` into `&amp;`,
/// which a browser reads back as a parameter separator.
#[test]
fn a_filter_value_is_percent_encoded_in_the_link() {
    let filters = SignalFilters {
        severity: None,
        source: Some("a&b c".into()),
    };
    let html = notifications(&rows(), &filters.with_source(None), 1_783_803_600_000);
    let target = notifications(&[], &filters, 1_783_803_600_000);
    assert!(
        target.contains("<b>a&amp;b c</b>"),
        "the active filter is shown as text, escaped: {target}"
    );
    // And the link that selects it carries the encoded form.
    let html = format!(
        "{html}{}",
        selector("x", &filters, &SignalFilters::default())
    );
    assert!(
        html.contains("source=a%26b%20c"),
        "the query value is percent-encoded: {html}"
    );
}

/// The detail page escapes producer-written text too, not only the
/// listing — the summary, the kind and the structured `detail` all come
/// off the wire unescaped.
#[test]
fn a_hostile_signal_is_escaped_on_the_detail_page_too() {
    let mut signal = fixtures::notification_signal_detail();
    signal.summary = "<script>alert('summary')</script>".into();
    signal.kind = "pricing.<img src=x onerror=1>".into();
    signal.detail = serde_json::json!({ "<script>": "</script>" });
    let html = notification_detail(&signal, 1_783_803_600_000);
    assert!(!html.contains("<script>"), "got: {html}");
    assert!(!html.contains("<img src=x"), "got: {html}");
    assert!(html.contains("&lt;script&gt;"), "got: {html}");
}
