//! The `triggers` table: a trigger's permanent record, and the three
//! reads the Trigger atom answers from it.
//!
//! Its own module beside `costs.rs`, on the same reasoning: the parent
//! is the projection's general store and this is one domain's slice of
//! it, with room to say why the queries are shaped as they are.
//!
//! **One store, three verbs.** Get, List and Stream all answer from
//! here. That is not the Event atom's arrangement (Get and Stream read
//! the log, List reads the index) and the difference is the payload:
//! an event's payload lives only on the log, a trigger's lives in the
//! row. With nothing to hop to there is no window where a trigger lists
//! and then cannot be fetched, and no second store for a filter to mean
//! something different in — the divergence #463's findings were about.
//!
//! What the three do differ in is **shape, not population**: List hands
//! back [`TriggerView`] rows with no payload, Get and Stream hand back
//! whole [`Trigger`]s. The atom declares that; see `trigger_command.rs`.

use sqlx::Row;

use super::{ProjectionStore, StoreError};
use crate::events::{Event, TriggerSource};
use crate::trigger::{Trigger, TriggerView};

/// The columns a whole [`Trigger`] is read from, in the order
/// [`trigger_at`] expects them.
const TRIGGER_COLUMNS: &str =
    "trigger_id, recorded_at, agent_id, source, subject, payload, requeued_from";

/// `TriggerSource` as the row stores it — its serde name, so the
/// column's vocabulary and the wire's are one vocabulary rather than
/// two kept in step by hand.
fn source_name(source: TriggerSource) -> String {
    serde_json::to_value(source)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "subject".to_string())
}

fn source_from_name(name: &str) -> Result<TriggerSource, StoreError> {
    serde_json::from_value(serde_json::Value::String(name.to_string()))
        .map_err(|e| StoreError::Backend(format!("stored trigger source `{name}`: {e}")))
}

/// Rebuild a trigger from [`TRIGGER_COLUMNS`] starting at column `at`.
///
/// The offset is a parameter rather than a second reader because the
/// stream's select prefixes `seq`, and re-indexing by hand at one of
/// two call sites is exactly how two readers come to disagree about
/// which column holds the payload.
///
/// A row this cannot parse is a corrupt database rather than a missing
/// trigger, so it errors instead of degrading to a partial value: a
/// trigger with a silently-nulled payload is a different task than the
/// one that was published, handed back as though it were that one.
///
/// `try_get` throughout rather than `get`, which panics: the promise
/// above is that a bad row is an *error*, and a decode that unwound the
/// handler task instead would break it in the one case it exists for.
fn trigger_at(row: &sqlx::sqlite::SqliteRow, at: usize) -> Result<Trigger, StoreError> {
    let column = |e: sqlx::Error| StoreError::Backend(format!("stored trigger column: {e}"));
    let raw_id: String = row.try_get(at).map_err(column)?;
    let corrupt = |what: &str, e: String| {
        StoreError::Backend(format!("stored trigger {what} for `{raw_id}`: {e}"))
    };
    let id = uuid::Uuid::parse_str(&raw_id).map_err(|e| corrupt("id", e.to_string()))?;
    let payload: String = row.try_get(at + 5).map_err(column)?;
    let requeued_from: Option<String> = row.try_get(at + 6).map_err(column)?;
    Ok(Trigger {
        id,
        source: source_from_name(&row.try_get::<String, _>(at + 3).map_err(column)?)?,
        subject: row.try_get::<Option<String>, _>(at + 4).map_err(column)?,
        payload: serde_json::from_str(&payload).map_err(|e| corrupt("payload", e.to_string()))?,
        requeued_from: requeued_from
            .map(|raw| uuid::Uuid::parse_str(&raw))
            .transpose()
            .map_err(|e| corrupt("requeued_from", e.to_string()))?,
    })
}

/// Append the `agent` / `since` narrowing to a query being built.
/// `seeded` says the query already has a `WHERE`. Bindings follow in
/// the same order the caller supplies them.
///
/// One narrowing, one function: List and Stream select the same
/// triggers for the same filter because they run the same clauses, not
/// because two hand-written `WHERE`s were kept in agreement.
fn narrow(sql: &mut String, agent: Option<&str>, since: Option<&str>, seeded: bool) {
    let mut clauses: Vec<&str> = Vec::new();
    if agent.is_some() {
        clauses.push("agent_id = ?");
    }
    if since.is_some() {
        clauses.push("recorded_at >= ?");
    }
    for (i, clause) in clauses.iter().enumerate() {
        sql.push_str(if seeded || i > 0 { " AND " } else { " WHERE " });
        sql.push_str(clause);
    }
}

impl ProjectionStore {
    /// Record the trigger this event names, if it names one.
    ///
    /// **First record wins**, the same idempotency `events` has on
    /// `event_id`. A trigger redelivered N times has N `triggered`
    /// records naming it, all carrying the same source, subject and
    /// payload; the earliest is the moment the runtime first took
    /// responsibility for it, which is what `recorded_at` means. It is
    /// also what makes re-delivery to the projection consumer a no-op
    /// here, as it already is for `events`.
    ///
    /// **`seq` is the one exception, and it is not one.** The conflict
    /// clause fills a NULL position in from a later record, because a
    /// log position is a fact about the *record*, not about the
    /// trigger — first-record-wins governs what the trigger IS, and
    /// nothing here can rewrite that. It exists for the row a requeue
    /// writes ([`ProjectionStore::reserve_requeue`]): that row is
    /// created at publish time, before any log record names the
    /// trigger, so without this the requeued trigger would list and get
    /// but never appear in `trigger.stream` — a permanent hole in an
    /// atom whose whole point is that it is kept.
    ///
    /// `requeued_from` is deliberately absent from the column list: an
    /// event never carries lineage (see [`Trigger::requeue_of`]), so
    /// the only writer of that column is the requeue itself.
    pub(super) async fn insert_trigger(
        &self,
        event: &Event,
        seq: Option<u64>,
    ) -> Result<(), StoreError> {
        let Some(trigger) = Trigger::from_event(event) else {
            return Ok(());
        };
        let payload = serde_json::to_string(&trigger.payload)
            .map_err(|e| StoreError::Backend(format!("serialising trigger payload: {e}")))?;
        sqlx::query(
            "INSERT INTO triggers
                 (trigger_id, recorded_at, agent_id, source, subject, payload, seq)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(trigger_id) DO UPDATE SET seq = excluded.seq
                 WHERE triggers.seq IS NULL",
        )
        .bind(trigger.id.to_string())
        .bind(event.envelope.timestamp.to_rfc3339())
        .bind(event.envelope.agent_id.as_str())
        .bind(source_name(trigger.source))
        .bind(trigger.subject)
        // THE CAS SEAM: the body goes in the row today and becomes a
        // content address when the object store lands. Nothing here or
        // below reads inside it, so the swap is this bind and the two
        // reads that hand it back.
        .bind(payload)
        .bind(seq.map(|s| s as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Claim the right to requeue one dead-lettered trigger, by
    /// writing the requeued trigger's row before it is published.
    ///
    /// **This write IS the idempotency check.** `requeued_from` carries
    /// a UNIQUE index, so at most one row can ever name a given
    /// original — and SQLite's unique indexes ignore NULLs, so every
    /// trigger that is not a requeue is unaffected. `Ok(false)` means
    /// the claim was lost: some earlier call already requeued this dead
    /// letter, and [`ProjectionStore::requeue_of`] names what it made.
    ///
    /// **Reserve first, publish second**, which is the order that makes
    /// the guarantee hold under a race. Checking and then publishing
    /// would let two concurrent requeues both read "not yet" and both
    /// publish, running the agent twice — which is precisely the harm
    /// the operator asked to be rid of. A failed publish is compensated
    /// by [`ProjectionStore::release_requeue`]; the residual window (a
    /// crash between a failed publish and its release) leaves a
    /// reservation that blocks a re-attempt, which is the safe
    /// direction to fail in.
    ///
    /// The row carries no `seq`: nothing on the log names this trigger
    /// yet. `insert_trigger` fills the position in when something does.
    pub async fn reserve_requeue(
        &self,
        trigger: &Trigger,
        agent_id: &str,
        recorded_at: &str,
    ) -> Result<bool, StoreError> {
        let payload = serde_json::to_string(&trigger.payload)
            .map_err(|e| StoreError::Backend(format!("serialising trigger payload: {e}")))?;
        let done = sqlx::query(
            "INSERT OR IGNORE INTO triggers
                 (trigger_id, recorded_at, agent_id, source, subject, payload, requeued_from)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(trigger.id.to_string())
        .bind(recorded_at)
        .bind(agent_id)
        .bind(source_name(trigger.source))
        .bind(trigger.subject.clone())
        .bind(payload)
        .bind(trigger.requeued_from.map(|id| id.to_string()))
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The trigger a previous requeue of `original` produced, if one
    /// did — the reference the second attempt's refusal carries.
    ///
    /// Answers the id alone rather than the whole trigger: this runs on
    /// a refusal path, and reading back a payload that may be half a
    /// megabyte to quote a name in an error message would make the
    /// refusal the most expensive thing the command does.
    pub async fn requeue_of(&self, original: &str) -> Result<Option<uuid::Uuid>, StoreError> {
        let row: Option<String> =
            sqlx::query_scalar("SELECT trigger_id FROM triggers WHERE requeued_from = ?")
                .bind(original)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|raw| {
            uuid::Uuid::parse_str(&raw)
                .map_err(|e| StoreError::Backend(format!("stored trigger id `{raw}`: {e}")))
        })
        .transpose()
    }

    /// Give back a reservation whose publish never landed.
    ///
    /// Scoped to rows that are requeues *of the original named* — the
    /// only rows this could ever be asked to remove. `triggers` is the
    /// one table the retention sweep never touches, so a general
    /// "delete a trigger" would be a hole in that promise; this cannot
    /// reach a trigger that was recorded rather than reserved.
    pub async fn release_requeue(
        &self,
        trigger_id: uuid::Uuid,
        requeued_from: uuid::Uuid,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM triggers WHERE trigger_id = ? AND requeued_from = ?")
            .bind(trigger_id.to_string())
            .bind(requeued_from.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One whole trigger by identity — a primary-key lookup, and the
    /// whole of `trigger.get`.
    ///
    /// `None` is "no durable record", which is one state with several
    /// causes; the caller names them rather than collapsing them here.
    pub async fn trigger(&self, trigger_id: &str) -> Result<Option<Trigger>, StoreError> {
        let sql = format!("SELECT {TRIGGER_COLUMNS} FROM triggers WHERE trigger_id = ?");
        let row = sqlx::query(&sql)
            .bind(trigger_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| trigger_at(&row, 0)).transpose()
    }

    /// The most recently recorded `limit` triggers matching the
    /// narrowing — index rows, no payloads.
    ///
    /// Newest first, like every other listing an operator reads. The
    /// `limit` has already been through the atom's declared cap, so the
    /// `Vec` is bounded before the query runs rather than after the
    /// rows are in hand.
    pub async fn query_triggers(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TriggerView>, StoreError> {
        let mut sql =
            String::from("SELECT trigger_id, agent_id, recorded_at, source FROM triggers");
        narrow(&mut sql, agent, since, false);
        // The identity breaks ties, so the order is total. Timestamps
        // collide — a batch projected together shares one, to the
        // second in some sources — and a page whose tail reshuffles
        // between two identical calls is a listing an operator cannot
        // trust twice. UUIDv7 makes the tiebreak time-ordered too, so
        // it agrees with the column it is breaking ties in.
        sql.push_str(" ORDER BY recorded_at DESC, trigger_id DESC LIMIT ?");
        let mut q = sqlx::query(&sql);
        for bound in [agent, since].into_iter().flatten() {
            q = q.bind(bound);
        }
        let rows = q.bind(limit).fetch_all(&self.pool).await?;
        let column = |e: sqlx::Error| StoreError::Backend(format!("stored trigger column: {e}"));
        rows.into_iter()
            .map(|row| {
                Ok(TriggerView {
                    trigger_id: row.try_get(0).map_err(column)?,
                    agent_id: row.try_get(1).map_err(column)?,
                    recorded_at: row.try_get(2).map_err(column)?,
                    source: source_from_name(&row.try_get::<String, _>(3).map_err(column)?)?,
                })
            })
            .collect()
    }

    /// Whole triggers at or after `from_seq`, in **sequence order** —
    /// one page of `trigger.stream`.
    ///
    /// Sequence order rather than the newest-first List renders, and
    /// for the reason the DeadLetter atom gives: List says what exists,
    /// Stream continues from a cursor, and the two compose only if the
    /// stream advances monotonically through the same population.
    ///
    /// A row with no `seq` is skipped rather than served at a made-up
    /// position — its record arrived without JetStream metadata, so
    /// there is no cursor it could honestly be handed back at. It still
    /// lists and it is still gettable; a cursor is the one thing it
    /// cannot have.
    ///
    /// **The cursor saturates rather than wrapping.** `from_seq` is a
    /// `u64` from the wire and the column is a SQLite `INTEGER`, so a
    /// plain `as i64` turns anything above `i64::MAX` *negative* — and
    /// `seq >= <negative>` matches every row, which would answer a
    /// cursor from beyond the end of the log with the *oldest* page and
    /// a `next_from_seq` far behind what was asked for: silent
    /// re-delivery, and a cursor that went backwards. Saturating is
    /// both safe and correct, because a cursor past every possible
    /// sequence means "nothing after this", which is what an empty page
    /// says. The guard is here and not at the caller because this
    /// method is `pub` through `Views` and the sentinel handling is a
    /// layer above.
    pub async fn triggers_from(
        &self,
        agent: Option<&str>,
        since: Option<&str>,
        from_seq: u64,
        limit: i64,
    ) -> Result<Vec<(u64, Trigger)>, StoreError> {
        let mut sql = format!(
            "SELECT seq, {TRIGGER_COLUMNS} FROM triggers WHERE seq IS NOT NULL AND seq >= ?"
        );
        narrow(&mut sql, agent, since, true);
        sql.push_str(" ORDER BY seq LIMIT ?");
        let mut q = sqlx::query(&sql).bind(i64::try_from(from_seq).unwrap_or(i64::MAX));
        for bound in [agent, since].into_iter().flatten() {
            q = q.bind(bound);
        }
        let rows = q.bind(limit).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| Ok((row.get::<i64, _>(0) as u64, trigger_at(&row, 1)?)))
            .collect()
    }

    /// The highest log position any recorded trigger carries, or 0 when
    /// none does — where a tail-seeking stream resumes from.
    pub async fn max_trigger_seq(&self) -> Result<u64, StoreError> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) FROM triggers")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>(0).max(0) as u64)
    }
}
