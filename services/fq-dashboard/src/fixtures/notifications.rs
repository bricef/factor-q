//! The notifications pane's fixtures: both severities in one listing,
//! one signal in full, and the empty pane.
//!
//! Its own module beside `health.rs` and `costs.rs`, on the same
//! reasoning the ratchet gives: `fixtures.rs` is near its budget and a
//! page's canned data belongs with itself.
//!
//! The timestamps are fixed against [`super::NOW_MS`], so the
//! screenshots are byte-stable and a visual diff is a rendering change
//! rather than the clock. Both severities appear because the whole
//! point of the pane's form language is the contrast between them: a
//! gallery with only notifications in it would never show the alert
//! stripe.

use fq_ops::events::SignalSeverity;
use fq_ops::surface::OperatorSignalCounts;
use fq_ops::views::{OperatorSignalDetailView, OperatorSignalView, SignalReferencesView};

use super::NOW_MS;

/// The identity of the alert the detail fixture expands. Shared so the
/// listing links to the page the gallery also renders.
const STALE_ID: &str = "019f6a01-0000-7000-8000-0000000000a1";
const REFUSED_ID: &str = "019f6a00-0000-7000-8000-0000000000a2";
const DEPLOYED_ID: &str = "019f69ff-0000-7000-8000-0000000000a3";
/// A yesterday's alert and the notification that closed it — the pair
/// that makes *open* a visible state rather than a word in a tooltip.
/// The gallery needs both an open alert and a resolved one or the two
/// renderings are never compared.
const OLD_STALE_ID: &str = "019f69f0-0000-7000-8000-0000000000a4";
const RECOVERED_ID: &str = "019f69f1-0000-7000-8000-0000000000a5";

/// A fixed RFC3339 instant `ms_ago` before the fixtures' frozen now.
fn at(ms_ago: i64) -> String {
    chrono::DateTime::from_timestamp_millis(NOW_MS - ms_ago)
        .expect("the frozen clock is a valid instant")
        .to_rfc3339()
}

/// The pane's rows: an alert at the top, two notifications under it,
/// two sources between them — so the severity filter, the source
/// filter and both badges all have something to render.
pub(crate) fn signals() -> Vec<OperatorSignalView> {
    vec![
        OperatorSignalView {
            event_id: STALE_ID.into(),
            timestamp: at(11 * 60 * 1000),
            severity: SignalSeverity::Alert,
            source: "pricing".into(),
            kind: "pricing.stale".into(),
            summary: "the live pricing table has not refreshed in 31h — prices are older \
                      than the models they price"
                .into(),
            resolves: None,
            resolved_by: None,
        },
        OperatorSignalView {
            event_id: REFUSED_ID.into(),
            timestamp: at(96 * 60 * 1000),
            severity: SignalSeverity::Notification,
            source: "pricing".into(),
            kind: "pricing.change_refused".into(),
            summary: "moonshotai/kimi-k3 input price moved 6.2x; kept the prior price".into(),
            resolves: None,
            resolved_by: None,
        },
        OperatorSignalView {
            event_id: DEPLOYED_ID.into(),
            timestamp: at(6 * 3600 * 1000),
            severity: SignalSeverity::Notification,
            source: "deploy".into(),
            kind: "deploy.succeeded".into(),
            summary: "build ff7db0ee91c5 came up on fq-dogfood".into(),
            resolves: None,
            resolved_by: None,
        },
        OperatorSignalView {
            event_id: RECOVERED_ID.into(),
            timestamp: at(20 * 3600 * 1000),
            severity: SignalSeverity::Notification,
            source: "pricing".into(),
            kind: "pricing.stale".into(),
            summary: "the live pricing table refreshed — 1,412 entries, 3m old".into(),
            resolves: Some(OLD_STALE_ID.into()),
            resolved_by: None,
        },
        OperatorSignalView {
            event_id: OLD_STALE_ID.into(),
            timestamp: at(26 * 3600 * 1000),
            severity: SignalSeverity::Alert,
            source: "pricing".into(),
            kind: "pricing.stale".into(),
            summary: "the live pricing table has not refreshed in 25h".into(),
            resolves: None,
            resolved_by: Some(RECOVERED_ID.into()),
        },
    ]
}

/// One whole signal — the alert, because it is the one with a walk to
/// render and the one whose retention the page explains.
pub(crate) fn signal_detail() -> OperatorSignalDetailView {
    OperatorSignalDetailView {
        event_id: STALE_ID.into(),
        timestamp: at(11 * 60 * 1000),
        agent_id: "system".into(),
        invocation_id: "019f6a01-0000-7000-8000-0000000000b0".into(),
        seq: Some(148_112),
        severity: SignalSeverity::Alert,
        source: "pricing".into(),
        kind: "pricing.stale".into(),
        summary: "the live pricing table has not refreshed in 31h — prices are older than \
                  the models they price"
            .into(),
        detail: serde_json::json!({
            "last_refresh_ms": 1_783_692_000_000i64,
            "window_hours": 24,
            "entries": 1_412,
        }),
        references: SignalReferencesView {
            agent_id: None,
            invocation_id: None,
            url: Some("https://github.com/BerriAI/litellm/commits/main".into()),
        },
        resolves: None,
        resolved_by: None,
        newer_from_source: None,
        older_from_source: Some(REFUSED_ID.into()),
    }
}

/// The home page's counts, consistent with the rows above: three
/// notifications inside the day, and one of the two alerts still open —
/// the older one was resolved, which is the whole reason the number is
/// a fold and not a row count.
pub(crate) fn signal_counts() -> OperatorSignalCounts {
    OperatorSignalCounts {
        notifications: 3,
        open_alerts: 1,
    }
}

/// The whole signal behind one listed identity, for a fake edge that
/// has to answer a Get. Only the alert is expanded: the fixtures are
/// the gallery's, and the gallery renders one detail page.
#[cfg(test)]
pub(crate) fn signal_detail_for(event_id: &str) -> Option<OperatorSignalDetailView> {
    (event_id == STALE_ID).then(signal_detail)
}
