//! The rebuild itself, and the record the file keeps of it.
//!
//! A rebuild is one transaction: carry the sweep-exempt rows aside,
//! drop the projection tables, recreate them at
//! [`PROJECTION_SCHEMA_VERSION`], put the carried rows back, stamp the
//! version, and note in `projection_meta` that the durable consumer
//! must be reset before it next reads. SQLite runs DDL inside a
//! transaction, so a crash anywhere in the middle leaves the old file
//! whole and the next open does it again from the start.
//!
//! The consumer-reset note is durable on purpose. The reset is the
//! consumer's to perform — it holds the bus — and it happens after this
//! transaction commits, in another task. An in-memory flag would lose
//! the reset to a crash between the two, and the file would then look
//! rebuilt while its durable resumed from its old acked floor: every
//! event before that floor gone from the projection for good, silently.
//! `projection_meta` outlives the rebuild (it is not among the tables
//! dropped), so the note survives whatever happens next.
//!
//! `projection_meta` also keeps the last rebuild's record — when, why,
//! from which version, and the stream position the replay has to reach
//! — which is what `fq status` reports.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::schema::{
    PROJECTION_SCHEMA_VERSION, PROJECTION_TABLES, SCHEMA_SQL, TRIGGER_REQUEUE_INDEX_SQL,
};
use super::{ProjectionStore, StoreError};
use crate::db::schema::{split_sql, write_user_version};

/// The key-value table that records the rebuild. Not a projection
/// table — a rebuild recreates those and must not lose this.
const PROJECTION_META_SQL: &str = "CREATE TABLE IF NOT EXISTS projection_meta (\
     key TEXT PRIMARY KEY, value TEXT NOT NULL)";

/// Present while the durable consumer has yet to be reset; its value is
/// the reason.
const META_CONSUMER_RESET_PENDING: &str = "consumer_reset_pending";
/// The last rebuild, as [`RebuildRecord`] JSON.
const META_REBUILD: &str = "rebuild";

/// The consumer-reset reason a fresh file records.
pub const FRESH_FILE_REASON: &str = "the projection file was created";

/// Why the tables were rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildReason {
    /// The file recorded an older schema version than this binary's.
    SchemaUpgrade { from: u32 },
    /// An operator asked (`fq projection rebuild`), with their reason
    /// if they gave one.
    Operator(Option<String>),
}

impl RebuildReason {
    fn describe(&self) -> String {
        match self {
            RebuildReason::SchemaUpgrade { from } => format!(
                "schema version {from} -> {PROJECTION_SCHEMA_VERSION}: the projection schema \
                 changed and history is re-derived from the event stream"
            ),
            RebuildReason::Operator(None) => "operator request".to_string(),
            RebuildReason::Operator(Some(reason)) => format!("operator request: {reason}"),
        }
    }

    fn source_version(&self) -> Option<u32> {
        match self {
            RebuildReason::SchemaUpgrade { from } => Some(*from),
            RebuildReason::Operator(_) => None,
        }
    }
}

/// What the file remembers about its last rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildRecord {
    /// When the tables were recreated (RFC3339).
    pub started_at: String,
    pub reason: String,
    /// The version the file recorded before, when the rebuild was a
    /// schema upgrade.
    pub from_version: Option<u32>,
    /// The version the tables were recreated at.
    pub schema_version: u32,
    /// The stream's last sequence when the durable was reset — the
    /// position the replay has to reach before every event the stream
    /// still holds is back in the file. Absent until the reset.
    pub target_seq: Option<u64>,
    /// When the durable was reset (RFC3339). Absent until then.
    pub consumer_reset_at: Option<String>,
}

impl RebuildRecord {
    /// The wire shape `control.status` reports, judged against where
    /// the durable's acked floor is now.
    ///
    /// The rebuild is in progress while the reset is still pending and
    /// then while the replay has not reached `target_seq`. A floor that
    /// cannot be read (the durable is between its deletion and its
    /// recreation) is "not there yet" rather than "done".
    pub fn status(
        &self,
        consumer_reset_pending: bool,
        projector_ack_floor: Option<u64>,
    ) -> fq_ops::surface::ProjectionRebuild {
        let in_progress = consumer_reset_pending
            || match (self.target_seq, projector_ack_floor) {
                (Some(target), Some(floor)) => floor < target,
                _ => true,
            };
        fq_ops::surface::ProjectionRebuild {
            started_at: self.started_at.clone(),
            reason: self.reason.clone(),
            from_version: self.from_version,
            schema_version: self.schema_version,
            target_seq: self.target_seq,
            consumer_reset_pending,
            in_progress,
        }
    }
}

/// A plain `[A-Za-z_][A-Za-z0-9_]*` identifier, or a refusal. Column
/// names read back from `pragma_table_info` are interpolated into the
/// carry-across SQL; every one of them comes from this module's own
/// `CREATE TABLE` (the old set is intersected with the new), so the
/// check never fires on a file fq wrote — it is what makes the
/// interpolation safe to reason about rather than something to trust.
fn plain_identifier(name: &str) -> Result<&str, StoreError> {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        Ok(name)
    } else {
        Err(StoreError::Backend(format!(
            "projection rebuild: refusing to carry a column named {name:?}"
        )))
    }
}

/// The column names of `table` on `conn`, in declaration order; empty
/// when the table does not exist.
async fn table_columns(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Result<Vec<String>, StoreError> {
    Ok(sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
        .bind(table)
        .fetch_all(&mut *conn)
        .await?)
}

impl ProjectionStore {
    pub(super) async fn ensure_meta_table(&self) -> Result<(), StoreError> {
        sqlx::query(PROJECTION_META_SQL).execute(&self.pool).await?;
        Ok(())
    }

    async fn meta_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT value FROM projection_meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>(0)))
    }

    /// Note that the durable consumer must be reset before it next
    /// reads. Durable, for the reason in the module docs.
    pub(super) async fn request_consumer_reset(&self, reason: &str) -> Result<(), StoreError> {
        sqlx::query("INSERT OR REPLACE INTO projection_meta (key, value) VALUES (?, ?)")
            .bind(META_CONSUMER_RESET_PENDING)
            .bind(reason)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether the durable consumer still has to be reset, and why.
    pub async fn consumer_reset_pending(&self) -> Result<Option<String>, StoreError> {
        self.meta_get(META_CONSUMER_RESET_PENDING).await
    }

    /// The last rebuild this file remembers, if any.
    pub async fn rebuild_record(&self) -> Result<Option<RebuildRecord>, StoreError> {
        let Some(json) = self.meta_get(META_REBUILD).await? else {
            return Ok(None);
        };
        serde_json::from_str(&json).map(Some).map_err(|err| {
            StoreError::Backend(format!(
                "projection_meta holds an unreadable rebuild record: {err}"
            ))
        })
    }

    async fn write_rebuild_record(&self, record: &RebuildRecord) -> Result<(), StoreError> {
        let json = serde_json::to_string(record)
            .map_err(|err| StoreError::Backend(format!("encode rebuild record: {err}")))?;
        sqlx::query("INSERT OR REPLACE INTO projection_meta (key, value) VALUES (?, ?)")
            .bind(META_REBUILD)
            .bind(json)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The durable has been reset: clear the note and complete the
    /// rebuild's record with the replay target.
    ///
    /// `deleted_durable` says whether there was a durable to delete. A
    /// fresh file that found one is a rebuild in every way that matters
    /// — the durable's old floor would have left the file short — so it
    /// is recorded as one, with the file's own reason. A fresh file
    /// that found none (a first start) records nothing: there was no
    /// history to replay and nothing for `fq status` to report.
    pub async fn consumer_reset_done(
        &self,
        target_seq: u64,
        deleted_durable: bool,
    ) -> Result<(), StoreError> {
        let now = chrono::Utc::now().to_rfc3339();
        let pending_reason = self.consumer_reset_pending().await?;
        let record = match self.rebuild_record().await? {
            Some(record) if record.target_seq.is_none() => Some(RebuildRecord {
                target_seq: Some(target_seq),
                consumer_reset_at: Some(now.clone()),
                ..record
            }),
            Some(_) => None,
            None if deleted_durable => Some(RebuildRecord {
                started_at: now.clone(),
                reason: pending_reason
                    .clone()
                    .unwrap_or_else(|| FRESH_FILE_REASON.to_string()),
                from_version: None,
                schema_version: PROJECTION_SCHEMA_VERSION,
                target_seq: Some(target_seq),
                consumer_reset_at: Some(now.clone()),
            }),
            None => None,
        };
        if let Some(record) = record {
            self.write_rebuild_record(&record).await?;
        }
        sqlx::query("DELETE FROM projection_meta WHERE key = ?")
            .bind(META_CONSUMER_RESET_PENDING)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Rebuild on demand — the operator path. Same transaction as the
    /// schema-upgrade rebuild; the caller resets the durable next.
    pub async fn rebuild(&self, reason: Option<&str>) -> Result<RebuildRecord, StoreError> {
        self.rebuild_tables(RebuildReason::Operator(reason.map(str::to_string)))
            .await
    }

    /// Drop and recreate the projection tables at
    /// [`PROJECTION_SCHEMA_VERSION`], carrying the sweep-exempt rows
    /// across, and note that the durable must be reset. One
    /// transaction; see the module docs.
    pub(super) async fn rebuild_tables(
        &self,
        reason: RebuildReason,
    ) -> Result<RebuildRecord, StoreError> {
        let from_version = self.recorded_version().await?;
        let mut tx = self.pool.begin().await?;

        // The rows that outlive the log, set aside on this connection
        // (a TEMP table is private to it) before their tables go.
        // A table an older file never had is simply not carried.
        let mut carried: Vec<(&'static str, Vec<String>)> = Vec::new();
        for table in PROJECTION_TABLES {
            let columns = table_columns(&mut tx, table).await?;
            if columns.is_empty() {
                continue;
            }
            let keep = match table {
                "events" if columns.iter().any(|c| c == "total_cost") => {
                    " WHERE total_cost IS NOT NULL"
                }
                "events" => continue,
                _ => "",
            };
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE TEMP TABLE carry_{table} AS SELECT * FROM {table}{keep}"
            )))
            .execute(&mut *tx)
            .await?;
            carried.push((table, columns));
        }

        for table in PROJECTION_TABLES {
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
                .execute(&mut *tx)
                .await?;
        }
        for statement in split_sql(SCHEMA_SQL) {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        sqlx::query(TRIGGER_REQUEUE_INDEX_SQL)
            .execute(&mut *tx)
            .await?;

        // Back into the new shape, by the columns the two shapes
        // share: a column the old file lacked reads NULL until the
        // replay reaches the row, and a column the new schema dropped
        // is left behind.
        for (table, old_columns) in carried {
            let new_columns = table_columns(&mut tx, table).await?;
            let common: Vec<&str> = new_columns
                .iter()
                .filter(|column| old_columns.contains(column))
                .map(|column| plain_identifier(column))
                .collect::<Result<_, _>>()?;
            let list = common.join(", ");
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {table} ({list}) SELECT {list} FROM carry_{table}"
            )))
            .execute(&mut *tx)
            .await?;
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE carry_{table}")))
                .execute(&mut *tx)
                .await?;
        }

        write_user_version(&mut *tx, PROJECTION_SCHEMA_VERSION).await?;

        let record = RebuildRecord {
            started_at: chrono::Utc::now().to_rfc3339(),
            reason: reason.describe(),
            from_version: reason.source_version(),
            schema_version: PROJECTION_SCHEMA_VERSION,
            target_seq: None,
            consumer_reset_at: None,
        };
        let json = serde_json::to_string(&record)
            .map_err(|err| StoreError::Backend(format!("encode rebuild record: {err}")))?;
        sqlx::query("INSERT OR REPLACE INTO projection_meta (key, value) VALUES (?, ?)")
            .bind(META_REBUILD)
            .bind(json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT OR REPLACE INTO projection_meta (key, value) VALUES (?, ?)")
            .bind(META_CONSUMER_RESET_PENDING)
            .bind(&record.reason)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        tracing::info!(
            from_version = ?from_version,
            to_version = PROJECTION_SCHEMA_VERSION,
            reason = %record.reason,
            "projection tables rebuilt; the durable consumer will be reset before it next reads"
        );
        Ok(record)
    }
}
