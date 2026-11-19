//! The ownership row's storage: the row, its readers, the archive's state
//! the row keeps beside its status, the termination record beside both,
//! and the one write every move of the row goes through, a compare-and-set
//! on the state the move was decided from. What the moves are is
//! `control_plane::ownership`'s.

use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus};
use crate::control_plane::ownership::RowState;

/// The rung that adds `archive_state` beside `status` (issue 1024). Every
/// row a control plane holds when it runs records a run that has not
/// ended (`not_due`) or one whose archive record was written: before the
/// column, only the archive hand-off and an operator's drop wrote a
/// finished status, and each wrote the archive record first.
///
/// Its number is frozen, as every merged rung's is (`MIGRATIONS`).
///
/// The `CHECK` covers the column's values, not which pairs of status and
/// archive state a row may hold (`RowState::is_valid`): a check across
/// both columns needs the table rebuilt, since SQLite adds no table
/// constraint to an existing table. [`TERMINATION_RECORD_SQL`] rebuilds
/// it, and adds that check.
pub(super) const ARCHIVE_STATE_SQL: &str = r#"
ALTER TABLE coordination_invocation_owner
    ADD COLUMN archive_state TEXT NOT NULL DEFAULT 'not_due'
    CHECK (archive_state IN ('not_due', 'pending', 'archived'));
UPDATE coordination_invocation_owner SET archive_state = 'archived'
    WHERE status IN ('completed', 'failed');
"#;

/// The rung that adds the termination record (issue 1070): who ended the
/// run and when, and the first termination an operator requested, beside
/// the row's status. Five nullable columns:
///
/// - `ended_by`, `run` or `drop`, and `ended_at`, the ending event's own
///   time, set together on a row that has ended;
/// - `termination_requested_at`, `termination_requested_by` and
///   `termination_reason`, from the first drop only, on a row that has
///   ended, since a drop of a run in progress ends it; the caller and the
///   reason come only with the time.
///
/// The coordination consumer's fold of a run's ending and of a drop
/// writes them, through the compare-and-set every move takes
/// ([`ControlPlaneStore::compare_and_set_ownership`]). A row from before
/// this rung keeps all five empty, which reads "not recorded"; nothing is
/// migrated into them. The archive state's values admit `none`, the
/// archive a dropped run that never wrote a worker row will never have, so
/// the table is rebuilt once for the whole record.
///
/// SQLite adds no table constraint to a table that exists, so the rung
/// rebuilds the table, copies every row, and puts the index back. The
/// rebuilt table's checks, all that a row must pass:
///
/// | Check | Accepts |
/// |---|---|
/// | `archive_state`'s values | `not_due`, `pending`, `archived`, `none` |
/// | `ended_by`'s values | none, `run`, `drop` |
/// | the status and archive state | `in_flight` or `ambiguous` with `not_due`; `completed` with `pending` or `archived`; `failed` with `pending`, `archived` or `none`; any other status with any archive state |
/// | an ender and its time | both, or neither |
/// | an ender | on any status but `in_flight` and `ambiguous` |
/// | a drop's ending | on `failed`, or a status this build does not know |
/// | a request's caller and reason | only with the request's time |
/// | a request | on any status but `in_flight` and `ambiguous` |
///
/// None of them refuses a row for a status this build does not know,
/// which only a hand edit or a newer build writes (issue 1082): such a row
/// is copied as it is and left to its readers, and a later write that
/// records an ending or a request on it is not refused for good, which
/// would hold the strict-order coordination consumer on it for ever.
///
/// **A row with a known status and an archive state that does not go with
/// it is carried through, not refused.** No writer ever wrote one, but a
/// hand edit of the status alone can (the guide's advice for a row this
/// build cannot read used to invite it), and one such row must not keep
/// the daemon from starting (issue 1024's rule). The copy gives that row
/// the archive state its status implies, by the rule the archive-state
/// rung used for rows from before its column
/// ([`ArchiveState::before_the_column`]), and changes nothing else in it;
/// the store's open reads those rows before the ladder runs and logs each
/// at error once the rung has landed ([`pairs_the_record_rung_corrects`]).
///
/// Like every rung it runs once, inside the ladder's one transaction, so
/// a rebuild cut short leaves the old table as it was. A second run would
/// erase every record, since the copy takes only the columns from before
/// it, so the rung begins by adding one of its columns to the old table,
/// which fails with `duplicate column name` on a table that has it: a
/// second run stops the store's open, loudly, with nothing applied.
pub(super) const TERMINATION_RECORD_SQL: &str = r#"
ALTER TABLE coordination_invocation_owner ADD COLUMN ended_by TEXT;
CREATE TABLE coordination_invocation_owner_rebuilt (
    invocation_id   TEXT PRIMARY KEY,
    worker_id       TEXT NOT NULL,
    assigned_at     INTEGER NOT NULL,
    status          TEXT NOT NULL,
    archive_state   TEXT NOT NULL DEFAULT 'not_due'
        CHECK (archive_state IN ('not_due', 'pending', 'archived', 'none')),
    ended_by        TEXT CHECK (ended_by IN ('run', 'drop')),
    ended_at        INTEGER,
    termination_requested_at INTEGER,
    termination_requested_by TEXT,
    termination_reason       TEXT,
    CHECK (CASE status
        WHEN 'in_flight' THEN archive_state = 'not_due'
        WHEN 'ambiguous' THEN archive_state = 'not_due'
        WHEN 'completed' THEN archive_state IN ('pending', 'archived')
        WHEN 'failed' THEN archive_state IN ('pending', 'archived', 'none')
        ELSE 1
    END),
    CHECK ((ended_by IS NULL) = (ended_at IS NULL)),
    CHECK (ended_by IS NULL OR status NOT IN ('in_flight', 'ambiguous')),
    CHECK (ended_by IS NOT 'drop' OR status NOT IN ('in_flight', 'ambiguous', 'completed')),
    CHECK (termination_requested_at IS NOT NULL
        OR (termination_requested_by IS NULL AND termination_reason IS NULL)),
    CHECK (termination_requested_at IS NULL OR status NOT IN ('in_flight', 'ambiguous'))
);
INSERT INTO coordination_invocation_owner_rebuilt
    (invocation_id, worker_id, assigned_at, status, archive_state)
    SELECT invocation_id, worker_id, assigned_at, status,
        CASE
            WHEN status IN ('in_flight', 'ambiguous') AND archive_state <> 'not_due'
                THEN 'not_due'
            WHEN status IN ('completed', 'failed') AND archive_state = 'not_due'
                THEN 'archived'
            ELSE archive_state
        END
    FROM coordination_invocation_owner;
DROP TABLE coordination_invocation_owner;
ALTER TABLE coordination_invocation_owner_rebuilt RENAME TO coordination_invocation_owner;
CREATE INDEX idx_owner_worker_status ON coordination_invocation_owner(worker_id, status);
"#;

/// The rows of the ownership table whose archive state does not go with
/// their status, read the way an operator checks a copy of a live file
/// before a deploy: what the termination record's rung corrects.
///
/// The columns are read as text whatever their storage class, so a value
/// no writer stores (an id stored as a blob, say) is reported rather than
/// stopping the open: the copy's SQL corrects such a row all the same.
const PAIRS_THE_RECORD_RUNG_CORRECTS_SQL: &str = "SELECT \
     CAST(invocation_id AS TEXT) AS invocation_id, CAST(status AS TEXT) AS status, \
     CAST(archive_state AS TEXT) AS archive_state \
     FROM coordination_invocation_owner \
     WHERE (status IN ('in_flight', 'ambiguous') AND archive_state <> 'not_due') \
        OR (status IN ('completed', 'failed') AND archive_state = 'not_due') \
     ORDER BY invocation_id";

/// An ownership row whose archive state did not go with its status, and
/// the one the termination record's rung gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PairCorrected {
    pub invocation_id: String,
    pub status: String,
    pub before: String,
    pub after: String,
}

/// The rows the termination record's rung will correct
/// ([`TERMINATION_RECORD_SQL`]), read before the ladder runs. None on a
/// fresh store, on one past the rung, and on one from before the
/// archive-state column, whose rung gives every row the archive state its
/// status implies.
pub(super) async fn pairs_the_record_rung_corrects(
    pool: &sqlx::Pool<sqlx::Sqlite>,
) -> Result<Vec<PairCorrected>, ControlPlaneStoreError> {
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('coordination_invocation_owner')")
            .fetch_all(pool)
            .await?;
    let has = |column: &str| columns.iter().any(|c| c == column);
    if !has("archive_state") || has("ended_by") {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(PAIRS_THE_RECORD_RUNG_CORRECTS_SQL)
        .fetch_all(pool)
        .await?;
    let text = |row: &sqlx::sqlite::SqliteRow, column: &str| {
        row.try_get::<Option<String>, _>(column)
            .ok()
            .flatten()
            .unwrap_or_else(|| "(unreadable)".to_string())
    };
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let status = text(&row, "status");
            let after = ArchiveState::before_the_column(OwnerStatus::parse(&status)?);
            Some(PairCorrected {
                invocation_id: text(&row, "invocation_id"),
                before: text(&row, "archive_state"),
                after: after.as_str().to_string(),
                status,
            })
        })
        .collect())
}

/// Say, once each, what the termination record's rung corrected.
pub(super) fn report_corrected(corrected: &[PairCorrected]) {
    for row in corrected {
        tracing::error!(
            invocation_id = %row.invocation_id,
            status = %row.status,
            archive_state_before = %row.before,
            archive_state_after = %row.after,
            "an ownership row's archive state did not go with its status, as no writer leaves \
             it; the termination record's rung gave it the archive state its status implies"
        );
    }
}

/// One run's ownership row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRow {
    pub invocation_id: String,
    pub worker_id: String,
    pub assigned_at: i64,
    pub status: OwnerStatus,
    pub archive_state: ArchiveState,
    /// Who ended the run, `None` while it has not, or when the row ended
    /// before the record existed: not recorded.
    pub ended_by: Option<EndedBy>,
    /// The ending event's own time, beside `ended_by`.
    pub ended_at: Option<i64>,
    /// The first termination an operator requested, `None` when none was
    /// or the row is from before the record.
    pub termination: Option<TerminationRequest>,
}

/// Who ended a run, as its ownership row records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndedBy {
    /// The run reached its own ending: its finishing write landed.
    Run,
    /// An operator's drop ended it.
    Drop,
}

impl EndedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            EndedBy::Run => "run",
            EndedBy::Drop => "drop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "run" => Some(EndedBy::Run),
            "drop" => Some(EndedBy::Drop),
            _ => None,
        }
    }
}

/// The first termination an operator requested for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminationRequest {
    /// The request's own time: its event's.
    pub requested_at: i64,
    /// Who asked, when the request said.
    pub requested_by: Option<String>,
    /// Why, when the request said.
    pub reason: Option<String>,
}

/// Every column of the ownership row, in the order its readers select
/// them.
const OWNER_COLUMNS: &str = "invocation_id, worker_id, assigned_at, status, archive_state, \
    ended_by, ended_at, termination_requested_at, termination_requested_by, termination_reason";

/// `SELECT` every column of the ownership row, then `clause`: `'static`,
/// so only a literal reaches asserted-safe SQL.
pub(super) fn select_owner(clause: &'static str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT {OWNER_COLUMNS} FROM coordination_invocation_owner {clause}"
    ))
}

/// One ownership row as [`select_owner`] reads it. A status, an archive
/// state or an ender this build cannot read is
/// [`ControlPlaneStoreError::Malformed`].
pub(super) fn row_to_owner(
    row: sqlx::sqlite::SqliteRow,
) -> Result<OwnerRow, ControlPlaneStoreError> {
    let status_str: String = row.get("status");
    let status = OwnerStatus::parse(&status_str).ok_or_else(|| {
        ControlPlaneStoreError::Malformed(format!("unknown owner status `{status_str}`"))
    })?;
    let archive_str: String = row.get("archive_state");
    let archive_state = ArchiveState::parse(&archive_str).ok_or_else(|| {
        ControlPlaneStoreError::Malformed(format!("unknown archive state `{archive_str}`"))
    })?;
    let ended_by = match row.get::<Option<String>, _>("ended_by") {
        None => None,
        Some(ender) => Some(EndedBy::parse(&ender).ok_or_else(|| {
            ControlPlaneStoreError::Malformed(format!("unknown ender `{ender}`"))
        })?),
    };
    let termination = row
        .get::<Option<i64>, _>("termination_requested_at")
        .map(|requested_at| TerminationRequest {
            requested_at,
            requested_by: row.get("termination_requested_by"),
            reason: row.get("termination_reason"),
        });
    Ok(OwnerRow {
        invocation_id: row.get("invocation_id"),
        worker_id: row.get("worker_id"),
        assigned_at: row.get("assigned_at"),
        status,
        archive_state,
        ended_by,
        ended_at: row.get("ended_at"),
        termination,
    })
}

/// Whether the control plane holds a run's archive record: the state an
/// ownership row keeps beside its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveState {
    /// The run has not ended, so no archive is due.
    NotDue,
    /// The run has ended, and its archive record is not written yet: the
    /// worker's `invocation.archived` is on its way, refused, held by the
    /// archive sweeper, or its worker is gone.
    Pending,
    /// The archive record was written, from the worker's archive (an
    /// operator's drop wrote one too, with no state, before issue 1070). A
    /// record retention later sweeps leaves the row `archived`.
    Archived,
    /// No archive record will come: an operator's drop said so, of a run
    /// with no row on its worker and none to come there, and wrote none
    /// (issue 1070). Only a `failed` row holds it, and not for good: an
    /// archive that arrives is recorded, and the run's own ending makes it
    /// `pending`. Stored as `none`.
    WillNotCome,
}

impl ArchiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveState::NotDue => "not_due",
            ArchiveState::Pending => "pending",
            ArchiveState::Archived => "archived",
            ArchiveState::WillNotCome => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "not_due" => Some(ArchiveState::NotDue),
            "pending" => Some(ArchiveState::Pending),
            "archived" => Some(ArchiveState::Archived),
            "none" => Some(ArchiveState::WillNotCome),
            _ => None,
        }
    }

    /// The archive's state of a row with `status` written before the
    /// column existed: what the rung that adds it (`ARCHIVE_STATE_SQL`)
    /// gives such a row.
    pub fn before_the_column(status: OwnerStatus) -> Self {
        match status {
            OwnerStatus::InFlight | OwnerStatus::Ambiguous => ArchiveState::NotDue,
            OwnerStatus::Completed | OwnerStatus::Failed => ArchiveState::Archived,
        }
    }
}

/// The stored state as the invocation views show it. `held` is not
/// stored: the daemon reads it from the archive sweeper where it builds a
/// report.
impl From<ArchiveState> for fq_ops::views::InvocationArchiveState {
    fn from(state: ArchiveState) -> Self {
        match state {
            ArchiveState::NotDue => Self::NotDue,
            ArchiveState::Pending => Self::Pending,
            ArchiveState::Archived => Self::Archived,
            ArchiveState::WillNotCome => Self::WillNotCome,
        }
    }
}

/// What a move writes beside the row's state: the owner facts it takes,
/// and the termination record it adds. Each `None` keeps what the row
/// has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MoveFacts<'a> {
    /// The worker id that replaces the row's; a row the move inserts needs
    /// it.
    pub worker_id: Option<&'a str>,
    /// The time that replaces the row's; a row the move inserts needs it.
    pub assigned_at: Option<i64>,
    /// The ending's own time, written with an ender the row did not
    /// record.
    pub ended_at: Option<i64>,
    /// The termination request, written only on a row that records none:
    /// the first is kept.
    pub request: Option<&'a TerminationRequest>,
}

impl ControlPlaneStore {
    /// Move `invocation_id`'s ownership row to `to`, only from `expected`:
    /// insert it when `expected` is no row, and update it when the row
    /// still holds exactly `expected`, its ender and whether it records a
    /// request included. `facts` says what else the move writes; a row
    /// this inserts needs its worker and time.
    ///
    /// One statement either way, so the condition and the write cannot be
    /// separated. True when the row moved; false when it was not in
    /// `expected` and nothing was written, which tells the caller to read
    /// the row again and decide again (`control_plane::ownership::apply`).
    pub async fn compare_and_set_ownership(
        &self,
        invocation_id: &str,
        expected: Option<RowState>,
        to: RowState,
        facts: MoveFacts<'_>,
    ) -> Result<bool, ControlPlaneStoreError> {
        // `apply` moves only between states a row may hold; a write of any
        // other is a caller's mistake, which the store's checks refuse.
        debug_assert!(to.is_valid(), "an ownership row cannot hold {}", to.name());
        let request = facts.request;
        let written = match expected {
            None => {
                let (Some(worker_id), Some(assigned_at)) = (facts.worker_id, facts.assigned_at)
                else {
                    return Err(ControlPlaneStoreError::Malformed(format!(
                        "the ownership row of {invocation_id} cannot be inserted without its \
                         worker and time"
                    )));
                };
                sqlx::query(
                    "INSERT INTO coordination_invocation_owner \
                         (invocation_id, worker_id, assigned_at, status, archive_state, \
                          ended_by, ended_at, termination_requested_at, \
                          termination_requested_by, termination_reason) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT(invocation_id) DO NOTHING",
                )
                .bind(invocation_id)
                .bind(worker_id)
                .bind(assigned_at)
                .bind(to.status.as_str())
                .bind(to.archive.as_str())
                .bind(to.ended_by.map(EndedBy::as_str))
                .bind(facts.ended_at)
                .bind(request.map(|request| request.requested_at))
                .bind(request.and_then(|request| request.requested_by.as_deref()))
                .bind(request.and_then(|request| request.reason.as_deref()))
                .execute(&self.pool)
                .await?
            }
            Some(expected) => {
                // SQLite evaluates every assignment against the row as it
                // was, so the request's caller and reason are written only
                // where no request was on record, with its time.
                sqlx::query(
                    "UPDATE coordination_invocation_owner \
                     SET status = ?, archive_state = ?, \
                         worker_id = COALESCE(?, worker_id), \
                         assigned_at = COALESCE(?, assigned_at), \
                         ended_by = ?, \
                         ended_at = COALESCE(?, ended_at), \
                         termination_requested_by = CASE WHEN termination_requested_at IS NULL \
                             THEN ? ELSE termination_requested_by END, \
                         termination_reason = CASE WHEN termination_requested_at IS NULL \
                             THEN ? ELSE termination_reason END, \
                         termination_requested_at = COALESCE(termination_requested_at, ?) \
                     WHERE invocation_id = ? AND status = ? AND archive_state = ? \
                       AND ended_by IS ? \
                       AND (termination_requested_at IS NOT NULL) = ?",
                )
                .bind(to.status.as_str())
                .bind(to.archive.as_str())
                .bind(facts.worker_id)
                .bind(facts.assigned_at)
                .bind(to.ended_by.map(EndedBy::as_str))
                .bind(facts.ended_at)
                .bind(request.and_then(|request| request.requested_by.as_deref()))
                .bind(request.and_then(|request| request.reason.as_deref()))
                .bind(request.map(|request| request.requested_at))
                .bind(invocation_id)
                .bind(expected.status.as_str())
                .bind(expected.archive.as_str())
                .bind(expected.ended_by.map(EndedBy::as_str))
                .bind(expected.requested)
                .execute(&self.pool)
                .await?
            }
        };
        Ok(written.rows_affected() == 1)
    }

    /// Whether `invocation_id` has an ownership row, readable or not: how an
    /// operator's drop of a run no projection event names an agent for
    /// tells it from an id that never existed (issue 1070), without moving
    /// or removing the row.
    pub async fn has_invocation_owner(
        &self,
        invocation_id: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM coordination_invocation_owner WHERE invocation_id = ?",
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    /// Write `status`, any text, over `invocation_id`'s ownership row, as a
    /// hand edit or a newer build would: how a test holds a row this build
    /// cannot read. True when a row was there to write over.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn overwrite_owner_status_for_test(
        &self,
        invocation_id: &str,
        status: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        let written = sqlx::query(
            "UPDATE coordination_invocation_owner SET status = ? WHERE invocation_id = ?",
        )
        .bind(status)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?;
        Ok(written.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod record_tests;
