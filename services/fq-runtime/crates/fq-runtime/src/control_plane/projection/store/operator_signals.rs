//! The `operator_signals` table: what a component of the daemon said
//! an operator should look at, and the three reads the pane makes of
//! it.
//!
//! Its own module beside `triggers.rs` and `costs.rs`, on the same
//! reasoning: the parent is the projection's general store and this is
//! one domain's slice of it, with room to say why the queries are
//! shaped as they are.
//!
//! **One store, and the whole signal is in the row.** Get, List and
//! the counts all answer from here, and the row carries `detail` and
//! `references` verbatim rather than an identity to hop with. That is
//! the `triggers` arrangement rather than the Event atom's, and for a
//! sharper version of the same reason: an alert outlives the log it
//! was folded from *by design*, so a detail page that read the payload
//! back out of the log would go blank on exactly the signals the table
//! exists to keep.
//!
//! **The sweep's rule lives here.** Notifications age out with the
//! log; alerts are never swept. See [`ProjectionStore::sweep_operator_signals`].

use sqlx::{QueryBuilder, Row, Sqlite};

use super::{ProjectionStore, StoreError, push_filter};
use crate::events::{Event, EventPayload, SignalSeverity};
use crate::views::{OperatorSignalDetailView, OperatorSignalView, SignalReferencesView};

/// The columns a whole signal is read from, in the order
/// [`signal_at`] expects them.
const SIGNAL_COLUMNS: &str = "event_id, seq, timestamp, agent_id, invocation_id, \
                              severity, source, kind, summary, detail, refs";

/// The columns one index row is read from — no payload, because the
/// pane's list shows a line and a severity and nothing else.
const SIGNAL_INDEX_COLUMNS: &str = "event_id, timestamp, severity, source, kind, summary";

/// The severity as the row stores it — the wire spelling, so the
/// column's vocabulary and the payload's are one vocabulary rather
/// than two kept in step by hand.
///
/// The alert spelling is also what [`ProjectionStore::sweep_operator_signals`]
/// binds, so the exemption and the writer cannot disagree about what an
/// alert is called.
fn severity_name(severity: SignalSeverity) -> &'static str {
    severity.as_str()
}

/// A stored severity back as the value. A spelling this build does not
/// know is a corrupt row rather than a third severity: there are two,
/// the enum is closed, and rounding an unknown down to `notification`
/// would silently demote an alert.
fn severity_from_name(name: &str) -> Result<SignalSeverity, StoreError> {
    match name {
        "notification" => Ok(SignalSeverity::Notification),
        "alert" => Ok(SignalSeverity::Alert),
        other => Err(StoreError::Backend(format!(
            "stored operator-signal severity `{other}` is neither `notification` nor `alert`"
        ))),
    }
}

/// A stored JSON column back as a value; `NULL` is `Value::Null`,
/// which is what the payload writes when a producer sent none.
fn json_column(raw: Option<String>, what: &str, id: &str) -> Result<serde_json::Value, StoreError> {
    let Some(raw) = raw else {
        return Ok(serde_json::Value::Null);
    };
    serde_json::from_str(&raw)
        .map_err(|e| StoreError::Backend(format!("stored operator-signal {what} for `{id}`: {e}")))
}

/// Append the `severity` / `source` / `since` narrowing to a query
/// being built, each value bound where its clause is pushed. `seeded`
/// says the query already has a `WHERE`.
///
/// One narrowing, one function: the list and the neighbour walk run
/// the same clauses rather than two hand-written `WHERE`s kept in
/// agreement.
fn narrow(
    qb: &mut QueryBuilder<Sqlite>,
    severity: Option<&str>,
    source: Option<&str>,
    since: Option<&str>,
    seeded: bool,
) -> bool {
    let seeded = push_filter(qb, seeded, "severity = ", severity);
    let seeded = push_filter(qb, seeded, "source = ", source);
    push_filter(qb, seeded, "timestamp >= ", since)
}

impl ProjectionStore {
    /// Record the operator signal this event carries, if it carries
    /// one.
    ///
    /// An upsert on `event_id`, for the same reason `insert_event` is
    /// one: a rebuild carries the rows across and the replay that
    /// follows refreshes whichever of them the stream still holds. The
    /// row is derived from the payload alone, so a refresh writes the
    /// same values — except `seq`, which is kept where a redelivery
    /// carries no position, so a redelivery cannot unlocate a row.
    pub(super) async fn insert_operator_signal(
        &self,
        event: &Event,
        seq: Option<u64>,
    ) -> Result<(), StoreError> {
        let EventPayload::OperatorSignal(signal) = &event.payload else {
            return Ok(());
        };
        let encode = |value: &serde_json::Value, what: &str| {
            if value.is_null() {
                return Ok(None);
            }
            serde_json::to_string(value).map(Some).map_err(|e| {
                StoreError::Backend(format!("serialising operator-signal {what}: {e}"))
            })
        };
        let refs = if signal.references.is_empty() {
            None
        } else {
            encode(
                &serde_json::to_value(&signal.references).map_err(|e| {
                    StoreError::Backend(format!("serialising operator-signal references: {e}"))
                })?,
                "references",
            )?
        };
        sqlx::query(
            "INSERT INTO operator_signals
                 (event_id, seq, timestamp, agent_id, invocation_id,
                  severity, source, kind, summary, detail, refs)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(event_id) DO UPDATE SET
                 seq = COALESCE(excluded.seq, operator_signals.seq),
                 timestamp = excluded.timestamp,
                 agent_id = excluded.agent_id,
                 invocation_id = excluded.invocation_id,
                 severity = excluded.severity,
                 source = excluded.source,
                 kind = excluded.kind,
                 summary = excluded.summary,
                 detail = excluded.detail,
                 refs = excluded.refs",
        )
        .bind(event.envelope.event_id.to_string())
        .bind(seq.map(|s| s as i64))
        .bind(event.envelope.timestamp.to_rfc3339())
        .bind(event.envelope.agent_id.as_str())
        .bind(event.envelope.invocation_id.to_string())
        .bind(severity_name(signal.severity))
        .bind(signal.kind.source())
        .bind(signal.kind.as_str())
        .bind(signal.summary.as_str())
        .bind(encode(&signal.detail, "detail")?)
        .bind(refs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete swept-out operator signals older than `cutoff_ms`,
    /// **except alerts**. Returns the number of rows deleted.
    ///
    /// **Alerts are never swept.** A notification is a thing an
    /// operator reads during normal hours, and once the event it was
    /// folded from has aged out of the log there is nothing to read it
    /// against; it goes with the log. An alert is the record that the
    /// system could not recover on its own and a person had to
    /// intervene, and that record is worth more than the log it
    /// arrived on — so it stays, with no expiry, and the pane can still
    /// show what needed a human last quarter.
    ///
    /// This is the **one predicate exemption** in the sweep, and the
    /// deliberate exception to the rule stated on
    /// [`ProjectionStore::sweep_events`]: cost rows, triggers and
    /// summaries are exempt structurally, by living in a table this
    /// never names. Two severities share one table here because the
    /// pane orders them against each other, and splitting them to win
    /// a structural exemption would cost a UNION on every read of the
    /// pane. The clause is written once, here, and
    /// `an_alert_survives_the_sweep_and_a_notification_does_not` is
    /// what stops it drifting.
    ///
    /// Batched like the event sweep, and for the same reason.
    pub async fn sweep_operator_signals(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        const SWEEP_BATCH_ROWS: i64 = 10_000;
        self.sweep_operator_signals_batched(cutoff_ms, SWEEP_BATCH_ROWS)
            .await
    }

    async fn sweep_operator_signals_batched(
        &self,
        cutoff_ms: i64,
        batch: i64,
    ) -> Result<u64, StoreError> {
        let cutoff = chrono::DateTime::from_timestamp_millis(cutoff_ms)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
            .to_rfc3339();
        let mut total = 0u64;
        loop {
            let result = sqlx::query(
                "DELETE FROM operator_signals WHERE rowid IN \
                 (SELECT rowid FROM operator_signals \
                  WHERE timestamp < ? AND severity <> ? LIMIT ?)",
            )
            .bind(&cutoff)
            .bind(severity_name(SignalSeverity::Alert))
            .bind(batch)
            .execute(&self.pool)
            .await?;
            total += result.rows_affected();
            if result.rows_affected() < batch as u64 {
                return Ok(total);
            }
        }
    }

    /// One whole signal by identity — a primary-key lookup, and the
    /// whole of the view's Get, bar the neighbour walk the caller adds.
    pub async fn operator_signal(
        &self,
        event_id: &str,
    ) -> Result<Option<OperatorSignalDetailView>, StoreError> {
        let mut qb = QueryBuilder::new("SELECT ");
        qb.push(SIGNAL_COLUMNS)
            .push(" FROM operator_signals WHERE event_id = ")
            .push_bind(event_id);
        let row = qb.build().fetch_optional(&self.pool).await?;
        row.map(|row| signal_at(&row)).transpose()
    }

    /// The most recent `limit` signals matching the narrowing — index
    /// rows, no payload.
    ///
    /// Newest first, like every other listing an operator reads, and
    /// the identity breaks ties so the order is total: timestamps
    /// collide, and a page whose tail reshuffles between two identical
    /// calls is a listing an operator cannot trust twice.
    ///
    /// The `limit` has already been through the view's declared cap, so
    /// the `Vec` is bounded before the query runs rather than after the
    /// rows are in hand.
    pub async fn query_operator_signals(
        &self,
        severity: Option<&str>,
        source: Option<&str>,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OperatorSignalView>, StoreError> {
        let mut qb = QueryBuilder::new("SELECT ");
        qb.push(SIGNAL_INDEX_COLUMNS).push(" FROM operator_signals");
        narrow(&mut qb, severity, source, since, false);
        qb.push(" ORDER BY timestamp DESC, event_id DESC LIMIT ")
            .push_bind(limit);
        let rows = qb.build().fetch_all(&self.pool).await?;
        let column =
            |e: sqlx::Error| StoreError::Backend(format!("stored operator-signal column: {e}"));
        rows.into_iter()
            .map(|row| {
                Ok(OperatorSignalView {
                    event_id: row.try_get(0).map_err(column)?,
                    timestamp: row.try_get(1).map_err(column)?,
                    severity: severity_from_name(&row.try_get::<String, _>(2).map_err(column)?)?,
                    source: row.try_get(3).map_err(column)?,
                    kind: row.try_get(4).map_err(column)?,
                    summary: row.try_get(5).map_err(column)?,
                })
            })
            .collect()
    }

    /// The signal immediately before and after `event_id` **from the
    /// same source**, in the pane's own newest-first order — the detail
    /// page's walk through one component's history.
    ///
    /// Ordered by `(timestamp, event_id)` so it agrees with the listing
    /// exactly: a "next" that skipped a row the list shows would make
    /// the walk and the pane two different orders over one population.
    /// Answers ids rather than rows, because a walk needs a link and
    /// the page it walks to reads the rest.
    pub async fn operator_signal_neighbours(
        &self,
        source: &str,
        timestamp: &str,
        event_id: &str,
    ) -> Result<(Option<String>, Option<String>), StoreError> {
        // `newer` is the row above this one in the pane (later, or the
        // same instant with a higher identity); `older` is the row
        // below. The two-clause compare is the lexicographic
        // `(timestamp, event_id)` tuple SQLite has no operator for.
        let one = |direction: &'static str, order: &'static str| {
            let mut qb = QueryBuilder::<Sqlite>::new(
                "SELECT event_id FROM operator_signals WHERE source = ",
            );
            qb.push_bind(source)
                .push(" AND (timestamp ")
                .push(direction)
                .push(" ")
                .push_bind(timestamp)
                .push(" OR (timestamp = ")
                .push_bind(timestamp)
                .push(" AND event_id ")
                .push(direction)
                .push(" ")
                .push_bind(event_id)
                .push(")) ORDER BY timestamp ")
                .push(order)
                .push(", event_id ")
                .push(order)
                .push(" LIMIT 1");
            qb
        };
        let newer: Option<String> = one(">", "ASC")
            .build_query_scalar()
            .fetch_optional(&self.pool)
            .await?;
        let older: Option<String> = one("<", "DESC")
            .build_query_scalar()
            .fetch_optional(&self.pool)
            .await?;
        Ok((newer, older))
    }

    /// How many notifications landed at or after `since`, and how many
    /// alerts are on the record at all.
    ///
    /// The asymmetry is the retention rule showing through: a
    /// notification is only interesting inside a window, and alerts are
    /// never swept, so counting them inside one would answer a
    /// different question from the one the home line asks.
    pub async fn operator_signal_counts(
        &self,
        notifications_since: Option<&str>,
    ) -> Result<(i64, i64), StoreError> {
        let mut qb = QueryBuilder::new("SELECT COUNT(*) FROM operator_signals WHERE severity = ");
        qb.push_bind(severity_name(SignalSeverity::Notification));
        push_filter(&mut qb, true, "timestamp >= ", notifications_since);
        let notifications: i64 = qb.build_query_scalar().fetch_one(&self.pool).await?;
        let alerts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM operator_signals WHERE severity = ?")
                .bind(severity_name(SignalSeverity::Alert))
                .fetch_one(&self.pool)
                .await?;
        Ok((notifications, alerts))
    }
}

/// Rebuild a whole signal from [`SIGNAL_COLUMNS`].
///
/// `try_get` throughout rather than `get`, which panics: a row this
/// cannot parse is a corrupt database rather than a missing signal, and
/// a decode that unwound the handler task instead would turn one bad
/// row into a dead page.
fn signal_at(row: &sqlx::sqlite::SqliteRow) -> Result<OperatorSignalDetailView, StoreError> {
    let column =
        |e: sqlx::Error| StoreError::Backend(format!("stored operator-signal column: {e}"));
    let event_id: String = row.try_get(0).map_err(column)?;
    let detail = json_column(row.try_get(9).map_err(column)?, "detail", &event_id)?;
    let refs: serde_json::Value = json_column(row.try_get(10).map_err(column)?, "refs", &event_id)?;
    let reference = |name: &str| {
        refs.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Ok(OperatorSignalDetailView {
        seq: row
            .try_get::<Option<i64>, _>(1)
            .map_err(column)?
            .map(|s| s as u64),
        timestamp: row.try_get(2).map_err(column)?,
        agent_id: row.try_get(3).map_err(column)?,
        invocation_id: row.try_get(4).map_err(column)?,
        severity: severity_from_name(&row.try_get::<String, _>(5).map_err(column)?)?,
        source: row.try_get(6).map_err(column)?,
        kind: row.try_get(7).map_err(column)?,
        summary: row.try_get(8).map_err(column)?,
        detail,
        references: SignalReferencesView {
            agent: reference("agent"),
            invocation: reference("invocation"),
            url: reference("url"),
        },
        event_id,
        // Filled in by the read that composes the walk; the row itself
        // knows nothing about its neighbours.
        newer_from_source: None,
        older_from_source: None,
    })
}

#[cfg(test)]
mod tests;
