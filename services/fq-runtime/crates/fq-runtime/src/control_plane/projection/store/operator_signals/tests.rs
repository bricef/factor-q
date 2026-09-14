//! Unit tests for [`super`] — the index, the retention rule that makes
//! it worth having, and the walk the detail page reads.

use super::*;
use crate::events::operator_signal::kinds;
use crate::events::{OperatorSignalPayload, SignalKind};
use serde_json::json;
use tempfile::tempdir;
use uuid::Uuid;

async fn store() -> (tempfile::TempDir, ProjectionStore) {
    let dir = tempdir().unwrap();
    let store = ProjectionStore::open(&dir.path().join("projection.db"))
        .await
        .unwrap();
    (dir, store)
}

fn signal_event(payload: OperatorSignalPayload) -> Event {
    Event::system(Uuid::now_v7(), EventPayload::OperatorSignal(payload))
}

fn notification() -> Event {
    signal_event(
        OperatorSignalPayload::notification(
            SignalKind::registered(kinds::PRICING_CHANGE_REFUSED),
            "kimi-k3 input price moved 6.2x; kept the prior price",
        )
        .with_detail(json!({ "rule": "drift_bound" })),
    )
}

fn alert() -> Event {
    signal_event(OperatorSignalPayload::alert(
        SignalKind::registered(kinds::PRICING_STALE),
        "the pricing table has not refreshed in 31h",
    ))
}

/// The recovery of `alert`: the same kind, a notification, naming the
/// alert it closes. That shape is the producer's contract — the topic
/// has not changed, only its state.
fn recovery_of(alert: &Event) -> Event {
    signal_event(
        OperatorSignalPayload::notification(
            SignalKind::registered(kinds::PRICING_STALE),
            "the pricing table refreshed",
        )
        .resolving(alert.envelope.event_id),
    )
}

/// Backdate a row so a sweep at a later cutoff reaches it.
async fn backdate(store: &ProjectionStore, event: &Event) {
    for table in ["events", "operator_signals"] {
        let sql = format!("UPDATE {table} SET timestamp = ? WHERE event_id = ?");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind("2020-01-01T00:00:00+00:00")
            .bind(event.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }
}

fn cutoff() -> i64 {
    chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .timestamp_millis()
}

/// **The projector indexes an operator signal**, and indexes the whole
/// of it: the two things the pane filters on, the line it shows, and
/// the particulars the detail page renders — with no hop back into the
/// log for any of them.
#[tokio::test]
async fn an_operator_signal_is_indexed_whole() {
    let (_dir, store) = store().await;
    let event = notification();
    store.insert_event(&event, Some(7)).await.unwrap();

    let rows = store
        .query_operator_signals(None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].severity, SignalSeverity::Notification);
    // The source is the kind's first segment, read off the kind rather
    // than carried twice — so it cannot disagree with it.
    assert_eq!(rows[0].source, "pricing");
    assert_eq!(rows[0].kind, "pricing.change_refused");

    let whole = store
        .operator_signal(&event.envelope.event_id.to_string())
        .await
        .unwrap()
        .expect("the row Get reads");
    assert_eq!(whole.detail, json!({ "rule": "drift_bound" }));
    assert_eq!(whole.seq, Some(7));
    assert_eq!(whole.agent_id, "system");

    // An event that is not a signal writes no row here.
    let other = Event::system(
        Uuid::now_v7(),
        EventPayload::InvocationSummary(crate::events::InvocationSummaryPayload {
            kind: crate::events::SummaryKind::Progress,
            summary: "not a signal".into(),
        }),
    );
    store.insert_event(&other, None).await.unwrap();
    assert_eq!(
        store
            .query_operator_signals(None, None, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// **An alert survives the retention sweep and a notification does
/// not.**
///
/// This is the rule the pane is built on: an alert is the record that
/// the system could not recover on its own, and it has to outlive the
/// thirty-day log it arrived on, while a notification goes with it. It
/// is the sweep's one predicate exemption, so it is the one that can be
/// broken by an edit to a `WHERE` clause without anything else
/// noticing — which is why the assertion is both halves at once, over
/// two rows of identical age.
#[tokio::test]
async fn an_alert_survives_the_sweep_and_a_notification_does_not() {
    let (_dir, store) = store().await;
    let kept = alert();
    let swept = notification();
    store.insert_event(&kept, None).await.unwrap();
    store.insert_event(&swept, None).await.unwrap();
    backdate(&store, &kept).await;
    backdate(&store, &swept).await;

    assert_eq!(store.sweep_operator_signals(cutoff()).await.unwrap(), 1);
    let rows = store
        .query_operator_signals(None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "only the notification is swept");
    assert_eq!(rows[0].severity, SignalSeverity::Alert);
    assert_eq!(rows[0].event_id, kept.envelope.event_id.to_string());

    // …and it is still whole, not a husk: the pane can still open a
    // signal whose event the log let go years ago.
    let whole = store
        .operator_signal(&kept.envelope.event_id.to_string())
        .await
        .unwrap()
        .expect("an alert is readable after the sweep");
    assert_eq!(whole.summary, "the pricing table has not refreshed in 31h");

    // A second sweep is a no-op — the exemption is not a one-shot.
    assert_eq!(store.sweep_operator_signals(cutoff()).await.unwrap(), 0);
}

/// **A recovery outlives the window, so the alert it closed stays
/// closed.**
///
/// The half of the exemption that is easy to miss: the row that closes
/// an alert is a *notification*, so a sweep that reads severity alone
/// deletes it on the thirty-first day. The alert it closed is exempt
/// and stays, the `NOT EXISTS` that made it closed finds nothing, and
/// it re-opens — a month late, on a page nobody was watching, and for
/// ever. An alert's record is the pair, so the sweep keeps both halves
/// of it.
///
/// The ordinary notification of identical age is here to keep the
/// exemption from widening into "nothing is swept": it closes nothing,
/// so it goes.
#[tokio::test]
async fn a_recovery_outlives_the_window_so_a_closed_alert_stays_closed() {
    let (_dir, store) = store().await;
    let raised = alert();
    let recovery = recovery_of(&raised);
    let ordinary = notification();
    for event in [&raised, &recovery, &ordinary] {
        store.insert_event(event, None).await.unwrap();
        backdate(&store, event).await;
    }

    let (_, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 0, "the recovery closed it before the sweep ran");

    assert_eq!(
        store.sweep_operator_signals(cutoff()).await.unwrap(),
        1,
        "the notification that resolves nothing is the only row swept"
    );

    let (_, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 0, "and it is still closed once the sweep has run");

    // Both ends of the relation are still readable, so the detail page
    // can still say what closed this alert years after the log let the
    // recovery's event go.
    let raised_id = raised.envelope.event_id.to_string();
    let recovery_id = recovery.envelope.event_id.to_string();
    let closed = store.operator_signal(&raised_id).await.unwrap().unwrap();
    assert_eq!(closed.resolved_by.as_deref(), Some(recovery_id.as_str()));
    let closer = store
        .operator_signal(&recovery_id)
        .await
        .unwrap()
        .expect("a resolving notification is kept as part of the alert's record");
    assert_eq!(closer.resolves.as_deref(), Some(raised_id.as_str()));

    // The exemption is not a one-shot, here either.
    assert_eq!(store.sweep_operator_signals(cutoff()).await.unwrap(), 0);
}

/// A notification inside the window is left alone: the sweep bounds by
/// time as well as by severity, so "alerts are kept" must not have
/// become "nothing is swept".
#[tokio::test]
async fn a_notification_inside_the_window_is_kept() {
    let (_dir, store) = store().await;
    let fresh = notification();
    store.insert_event(&fresh, None).await.unwrap();
    assert_eq!(store.sweep_operator_signals(cutoff()).await.unwrap(), 0);
    assert_eq!(
        store
            .query_operator_signals(None, None, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// The sweep clears a whole backlog in batches, the way the event
/// sweep does — the first pass after an upgrade can face months of it.
#[tokio::test]
async fn the_signal_sweep_batches_until_the_backlog_is_clear() {
    let (_dir, store) = store().await;
    for _ in 0..3 {
        let event = notification();
        store.insert_event(&event, None).await.unwrap();
        backdate(&store, &event).await;
    }
    let kept = alert();
    store.insert_event(&kept, None).await.unwrap();
    backdate(&store, &kept).await;
    assert_eq!(
        store
            .sweep_operator_signals_batched(cutoff(), 1)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .query_operator_signals(None, None, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// The narrowing the pane offers, applied where it belongs: the daemon
/// answers only the rows asked for.
#[tokio::test]
async fn the_listing_narrows_by_severity_and_source() {
    let (_dir, store) = store().await;
    for event in [notification(), alert()] {
        store.insert_event(&event, None).await.unwrap();
    }
    let deploy = signal_event(OperatorSignalPayload::notification(
        SignalKind::registered(kinds::DEPLOY_SUCCEEDED),
        "build abc came up",
    ));
    store.insert_event(&deploy, None).await.unwrap();

    let alerts = store
        .query_operator_signals(Some("alert"), None, None, 10)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    let pricing = store
        .query_operator_signals(None, Some("pricing"), None, 10)
        .await
        .unwrap();
    assert_eq!(pricing.len(), 2);
    let deploys = store
        .query_operator_signals(None, Some("deploy"), None, 10)
        .await
        .unwrap();
    assert_eq!(deploys.len(), 1);
    assert_eq!(deploys[0].kind, "deploy.succeeded");
}

/// **The counts answer the home line's two questions, and only one of
/// them takes a window.** Alerts are never swept, so counting them
/// inside one would answer something else.
#[tokio::test]
async fn the_counts_window_notifications_and_never_the_alerts() {
    let (_dir, store) = store().await;
    let old = notification();
    let fresh = notification();
    let standing = alert();
    for event in [&old, &fresh, &standing] {
        store.insert_event(event, None).await.unwrap();
    }
    backdate(&store, &old).await;
    backdate(&store, &standing).await;

    let (notifications, alerts) = store
        .operator_signal_counts(Some("2021-01-01T00:00:00+00:00"))
        .await
        .unwrap();
    assert_eq!(notifications, 1, "the backdated notification is outside");
    assert_eq!(alerts, 1, "the backdated alert is counted regardless");

    let (all, _) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(all, 2, "no window counts every notification indexed");
}

/// The walk the detail page renders is the pane's own order, so a
/// "next" cannot skip a row the listing shows.
#[tokio::test]
async fn the_neighbour_walk_follows_the_listing_order() {
    let (_dir, store) = store().await;
    let mut ids = Vec::new();
    for n in 0..3 {
        let event = notification();
        store.insert_event(&event, None).await.unwrap();
        // Distinct, ascending timestamps so the order is unambiguous.
        sqlx::query("UPDATE operator_signals SET timestamp = ? WHERE event_id = ?")
            .bind(format!("2026-01-0{}T00:00:00+00:00", n + 1))
            .bind(event.envelope.event_id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        ids.push(event.envelope.event_id.to_string());
    }
    let middle = store.operator_signal(&ids[1]).await.unwrap().unwrap();
    let (newer, older) = store
        .operator_signal_neighbours(&middle.source, &middle.timestamp, &middle.event_id)
        .await
        .unwrap();
    assert_eq!(newer.as_deref(), Some(ids[2].as_str()));
    assert_eq!(older.as_deref(), Some(ids[0].as_str()));

    // The ends of a source's history have one neighbour each.
    let newest = store.operator_signal(&ids[2]).await.unwrap().unwrap();
    let (newer, older) = store
        .operator_signal_neighbours(&newest.source, &newest.timestamp, &newest.event_id)
        .await
        .unwrap();
    assert_eq!(newer, None);
    assert_eq!(older.as_deref(), Some(ids[1].as_str()));
}

/// A redelivery of the same event upserts rather than duplicating, and
/// keeps a position it already knows — the property that makes a
/// rebuild's replay refresh the index instead of doubling it.
#[tokio::test]
async fn a_redelivery_refreshes_the_row_and_keeps_its_position() {
    let (_dir, store) = store().await;
    let event = notification();
    store.insert_event(&event, Some(11)).await.unwrap();
    store.insert_event(&event, None).await.unwrap();
    let rows = store
        .query_operator_signals(None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let whole = store
        .operator_signal(&event.envelope.event_id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(whole.seq, Some(11), "a redelivery must not unlocate a row");
}

/// **An alert is open until a later signal resolves it, and the count
/// says so.**
///
/// The rule the home page's number rests on. Both directions over rows
/// of identical severity: an alert nothing has answered is counted, and
/// the same alert once a recovery names it is not. Without this the
/// count can only grow — a week of a broken upstream is twenty-eight
/// things to act on, none of which can ever close.
#[tokio::test]
async fn a_resolved_alert_is_not_an_open_one() {
    let (_dir, store) = store().await;
    let standing = alert();
    let recovered = alert();
    store.insert_event(&standing, None).await.unwrap();
    store.insert_event(&recovered, None).await.unwrap();

    let (_, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 2, "nothing has resolved either of them yet");

    let recovery = recovery_of(&recovered);
    store.insert_event(&recovery, None).await.unwrap();
    let (notifications, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 1, "the resolved alert is no longer open");
    assert_eq!(
        notifications, 1,
        "…and the recovery is itself a notification, counted as one"
    );

    // A resolution is not a deletion: the alert is still on the record,
    // still listed, and still whole. "Closed" is a state, not a sweep.
    let rows = store
        .query_operator_signals(Some("alert"), None, None, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "both alerts are still listed");
}

/// The relation is readable from both ends, which is what the detail
/// page renders: the alert names what closed it, and the recovery names
/// what it closed.
#[tokio::test]
async fn the_resolution_is_readable_from_both_ends() {
    let (_dir, store) = store().await;
    let raised = alert();
    let recovery = recovery_of(&raised);
    store.insert_event(&raised, None).await.unwrap();
    store.insert_event(&recovery, None).await.unwrap();

    let raised_id = raised.envelope.event_id.to_string();
    let recovery_id = recovery.envelope.event_id.to_string();

    let closed = store.operator_signal(&raised_id).await.unwrap().unwrap();
    assert_eq!(closed.resolved_by.as_deref(), Some(recovery_id.as_str()));
    assert_eq!(closed.resolves, None);

    let closer = store.operator_signal(&recovery_id).await.unwrap().unwrap();
    assert_eq!(closer.resolves.as_deref(), Some(raised_id.as_str()));
    assert_eq!(closer.resolved_by, None);

    // And on the index row the pane lists, so a reader sees the state
    // without opening the signal.
    let rows = store
        .query_operator_signals(None, None, None, 10)
        .await
        .unwrap();
    let row = |id: &str| {
        rows.iter()
            .find(|r| r.event_id == id)
            .expect("the row is listed")
    };
    assert_eq!(
        row(&raised_id).resolved_by.as_deref(),
        Some(recovery_id.as_str())
    );
    assert_eq!(
        row(&recovery_id).resolves.as_deref(),
        Some(raised_id.as_str())
    );
    assert_eq!(row(&recovery_id).resolved_by, None);
}

/// A resolution that arrives before the signal it names — a redelivery
/// out of order, or a projector that folded the recovery first — is not
/// refused and is not lost: the edge is a value on the row, so the
/// count is right as soon as both rows exist, whichever order they
/// landed in.
#[tokio::test]
async fn a_resolution_may_be_indexed_before_the_signal_it_closes() {
    let (_dir, store) = store().await;
    let raised = alert();
    let recovery = recovery_of(&raised);
    store.insert_event(&recovery, None).await.unwrap();
    let (_, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 0, "nothing is open: the alert is not indexed yet");

    store.insert_event(&raised, None).await.unwrap();
    let (_, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 0, "and it arrives already resolved");
}

/// **End to end across the seam: the producer raises a standing
/// condition, the producer closes it, and the pane's count falls.**
///
/// Every other test here hand-builds the pair, which proves the index
/// but assumes the shape. This one takes the signals from
/// [`PricingEpisodes`] — the code that actually mints them on a
/// refresh — so the two halves are only ever right together. If a
/// producer stopped naming the raising id, or closed an episode under a
/// different kind, the count would silently stop falling and nothing
/// else in this file would notice.
#[tokio::test]
async fn a_pricing_alert_and_the_recovery_that_closes_it_leave_nothing_open() {
    use crate::pricing::PricingTable;
    use crate::pricing::episodes::PricingEpisodes;
    use crate::pricing::live::{AcceptedLoad, Staleness};

    let load = |stale: bool| AcceptedLoad {
        table: PricingTable::empty(),
        refusals: Vec::new(),
        staleness: stale.then(|| Staleness {
            accepted_at: chrono::Utc::now() - chrono::Duration::days(9),
            age: std::time::Duration::from_secs(9 * 24 * 3_600),
            max_age: std::time::Duration::from_secs(7 * 24 * 3_600),
        }),
        fetch_error: None,
    };

    let (_dir, store) = store().await;
    let episodes = PricingEpisodes::new();
    let runtime = Uuid::now_v7();

    // The refresh that finds the table past its window.
    let raised = episodes.edges(&load(true), load(true).signals());
    let raised_id = {
        let mut ids = Vec::new();
        for signal in raised {
            let id = signal.event_id.to_string();
            let kind = signal.kind().as_str().to_string();
            store
                .insert_event(&signal.into_event(runtime), None)
                .await
                .unwrap();
            ids.push((kind, id));
        }
        let (_, id) = ids
            .iter()
            .find(|(kind, _)| kind == kinds::PRICING_STALE)
            .expect("a table past its window raises pricing.stale");
        id.clone()
    };
    let (_, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 1, "the stale table is something to act on");

    // The refresh that lands a document, which ends the episode.
    let recovered = episodes.edges(&load(false), load(false).signals());
    assert_eq!(recovered.len(), 1, "one recovery, for the one open episode");
    let recovery_id = recovered[0].event_id.to_string();
    store
        .insert_event(
            &recovered.into_iter().next().unwrap().into_event(runtime),
            None,
        )
        .await
        .unwrap();

    let (_, open) = store.operator_signal_counts(None).await.unwrap();
    assert_eq!(open, 0, "the recovery closed it; nothing is open");

    // And the detail page reads the pair from both ends, without the
    // producer's in-process memory, which is gone the moment it restarts.
    let alert = store.operator_signal(&raised_id).await.unwrap().unwrap();
    assert_eq!(alert.severity, SignalSeverity::Alert);
    assert_eq!(alert.resolved_by.as_deref(), Some(recovery_id.as_str()));
    let recovery = store.operator_signal(&recovery_id).await.unwrap().unwrap();
    assert_eq!(recovery.severity, SignalSeverity::Notification);
    assert_eq!(
        recovery.kind.as_str(),
        kinds::PRICING_STALE,
        "the topic is the same"
    );
    assert_eq!(recovery.resolves.as_deref(), Some(raised_id.as_str()));
}
