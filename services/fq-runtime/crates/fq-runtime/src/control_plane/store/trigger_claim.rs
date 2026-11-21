use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus};
use crate::worker::WalRun;

mod consumed;
mod sweep;

pub use consumed::ConsumedWhy;
pub(super) use consumed::TRIGGER_CLAIM_CONSUMED_STATE_SQL;

/// The stable identity of one trigger delivery, as the claim table keys it.
///
/// The sequence alone is not stable across a stream recreate: JetStream
/// restarts sequences at 1, so a rebuilt `fq-triggers` would meet the
/// `durably_started` rows of the old one and have its first N triggers
/// acked and dropped as duplicates — silently, since a dropped duplicate
/// starts nothing by design. The stream's creation time separates one
/// incarnation from the next, which makes rows left by a stream that no
/// longer exists inert rather than wrong, and removes the need for an
/// operator to remember to clear the table by hand. ADR-0032's KV claim
/// keys on the same three facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerKey<'a> {
    /// The stream the delivery came from.
    pub stream: &'a str,
    /// Nanoseconds since the Unix epoch at which *this incarnation* of
    /// that stream was created, read from stream info at connect
    /// ([`crate::bus::EventBus::trigger_stream_epoch`]). Zero when no
    /// stream info was available — focused tests that drive a
    /// dispatcher over a store by hand — which is itself a distinct
    /// incarnation and so cannot collide with a live one.
    pub stream_epoch: i64,
    /// The message's sequence within that incarnation.
    pub stream_seq: u64,
}

/// Result of arbitrating one broker delivery by its stable identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerClaim {
    Won,
    /// A `claimed` row already exists: the claim as that row holds it,
    /// read in the statement that found it, so a refused delivery can be
    /// reported with what the claim says rather than with a later read.
    Held {
        claimant: String,
        /// The invocation id the claim names. `None` on a row written
        /// before claims named their invocation at claim time.
        invocation_id: Option<String>,
        /// When the row was claimed, or last adopted, epoch ms.
        claimed_at: i64,
    },
    Started {
        invocation_id: String,
    },
    /// The trigger was consumed without its run starting: dealt with, and
    /// not to be handled again.
    Consumed {
        /// The invocation that consumed it.
        invocation_id: Option<String>,
        /// How it was consumed.
        why: ConsumedWhy,
    },
}

/// What [`ControlPlaneStore::record_trigger_start`] found on the claim, and
/// so what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedStart {
    /// A row naming the run: marked, as the mark would have.
    Marked,
    /// No row: one recording the start was written.
    Written,
    /// A `claimed` row naming another invocation, or none: the start was
    /// recorded on it in that invocation's place.
    WrittenOver { claimed_for: Option<String> },
    /// A row recording another run's start: left as it is.
    AlreadyStarted { invocation_id: Option<String> },
    /// A row recording the trigger consumed without a run: the start
    /// recorded over it, since the run did start, and a consumed claim
    /// would let the trigger's dead letter be requeued and run again.
    OverConsumed {
        consumed_by: Option<String>,
        why: ConsumedWhy,
    },
}

/// Where a claim row stands: the `state` column.
///
/// A reader decides by matching on it rather than asking whether the row
/// has started, so that a state added later is an arm that reader has to
/// write, not a value that reads as one of these (issue 1055). The
/// compiler forces the readers that match: `claim_trigger`,
/// `record_trigger_start`, the dead-letter watch's two reads of the claim,
/// the invocation view's mapping and the state's `Display`, which log lines
/// name it by. It forces neither of two others. [`Self::parse`], the row's
/// mapping, matches on the column's text, so a value added to the column
/// is not an arm it has to write; what makes such a value fail safe is
/// that it refuses every value it does not name, and the row reads as an
/// error rather than as a state it is not. And the SQL statements that
/// filter on `state = '…'` are text the compiler never sees: a state added
/// later has to be written into each of them by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimState {
    /// A copy has taken the delivery, and nothing of its run is settled:
    /// `state = 'claimed'`.
    Claimed,
    /// The run the row names has made its first WAL write:
    /// `state = 'durably_started'`.
    DurablyStarted,
    /// The trigger was dealt with and no run started: `state = 'consumed'`,
    /// with `consumed_why` saying how. A copy that comes back is acked and
    /// dropped, the dead-letter watch writes no dead letter for it, and its
    /// own dead letter, if the dispatcher wrote one, can be requeued, since
    /// it never ran (issue 1055).
    Consumed { why: ConsumedWhy },
}

impl ClaimState {
    /// The state a row's `state` and `consumed_why` columns hold, or `None`
    /// for a value this build does not know, which is refused rather than
    /// read as one it does (as [`OwnerStatus::parse`] refuses a status).
    /// The `state` column's `CHECK` admits only the three values and sets
    /// `consumed_why` exactly when the state is `consumed`, so `None` means
    /// a table edited by hand. A reason this build does not know is kept as
    /// it is ([`ConsumedWhy::Other`]).
    pub fn parse(state: &str, consumed_why: Option<&str>) -> Option<Self> {
        match (state, consumed_why) {
            ("claimed", None) => Some(ClaimState::Claimed),
            ("durably_started", None) => Some(ClaimState::DurablyStarted),
            ("consumed", Some(why)) => Some(ClaimState::Consumed {
                why: ConsumedWhy::of_column(why),
            }),
            _ => None,
        }
    }

    /// [`Self::parse`] over a row read with its `state` and
    /// `consumed_why` columns, refusing what it does not know as malformed.
    fn of_row(row: &sqlx::sqlite::SqliteRow) -> Result<Self, ControlPlaneStoreError> {
        let state = row.get::<String, _>("state");
        let consumed_why = row.get::<Option<String>, _>("consumed_why");
        Self::parse(&state, consumed_why.as_deref()).ok_or_else(|| {
            ControlPlaneStoreError::Malformed(format!(
                "unknown trigger claim state `{state}` (consumed_why {consumed_why:?})"
            ))
        })
    }
}

/// The state as the `state` column spells it, and a consumed claim's reason
/// beside it, for a log line that names it.
impl std::fmt::Display for ClaimState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimState::Claimed => f.write_str("claimed"),
            ClaimState::DurablyStarted => f.write_str("durably_started"),
            ClaimState::Consumed { why } => write!(f, "consumed ({why})"),
        }
    }
}

/// One claim row, read back whole for an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerClaimRow {
    pub stream: String,
    pub stream_epoch: i64,
    pub stream_seq: u64,
    pub claimant: String,
    /// Where the row stands.
    pub state: ClaimState,
    /// Nullable in the schema: a claim written before claims named their
    /// invocation at claim time has none. An adoption names one, as does a
    /// start recorded over it ([`ControlPlaneStore::record_trigger_start`]);
    /// the mark never matches such a row and so never names one.
    pub invocation_id: Option<String>,
    pub claimed_at: i64,
    /// On a consumed claim, when the claim recorded that an operator's drop
    /// of the run it names wrote that run's ownership row, epoch ms; `None`
    /// on every other claim (issue 1055). Kept beside the reason, never in
    /// its place: the reason says how the trigger was dealt with, and this
    /// says the ownership row a reader finds for the run is the drop's, not
    /// a start. So a reader of a consumed claim that records a drop takes
    /// only a start in the worker's WAL as a start.
    pub operator_dropped_at: Option<i64>,
}

impl TriggerClaimRow {
    /// The row's key in the claim table.
    pub fn key(&self) -> TriggerKey<'_> {
        TriggerKey {
            stream: &self.stream,
            stream_epoch: self.stream_epoch,
            stream_seq: self.stream_seq,
        }
    }
}

/// A `claimed` row, with the coordination status of the invocation it
/// names when the control plane records one. An ownership row is written
/// when an invocation ends (a `completed`, or a `failed` outside `setup`),
/// is archived, found ambiguous or dropped, and is never swept, so its
/// presence says the run got that far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsettledTriggerClaim {
    pub claim: TriggerClaimRow,
    /// The status the ownership row holds, as stored, when there is a row.
    /// Left unparsed here: startup recovery needs only to know the row
    /// exists, so a status this build cannot read must not stop a boot
    /// (`TriggerClaims::recover`). A reader that uses the status itself
    /// parses it with [`Self::owner_status`].
    pub owner_row_status: Option<String>,
}

impl UnsettledTriggerClaim {
    /// The ownership row's status, parsed: `None` when there is no row, an
    /// error when the row holds a status this build does not know.
    pub fn owner_status(&self) -> Result<Option<OwnerStatus>, ControlPlaneStoreError> {
        self.owner_row_status
            .as_deref()
            .map(|status| {
                OwnerStatus::parse(status).ok_or_else(|| {
                    ControlPlaneStoreError::Malformed(format!("unknown owner status `{status}`"))
                })
            })
            .transpose()
    }
}

/// What the two records that can hold a run say of the one a claim names:
/// the worker's WAL and the control plane's ownership table.
///
/// **This is the one definition of a start on record**, and three readers
/// share it, so they cannot disagree about it:
///
/// - startup recovery marks the claim of a run that has one
///   (`TriggerClaims::recover`);
/// - `fq doctor` calls such a claim started and never marked
///   (`Views::stranded_trigger_claims`);
/// - the trigger dispatcher, meeting a claim of its own that no copy of
///   the trigger is handling any more, acks the copy and marks the claim
///   when the run has one, and takes the claim again when it has none
///   (issue 967).
///
/// A run has a start on record when either record holds it. The WAL holds
/// it from its first WAL write until the control plane acknowledges its
/// archive. The dispatcher writes the ownership row just after that first
/// write, at the run's durable start (issue 721); the run's ending writes
/// it if that write did not land, and the archive hand-off before the
/// acknowledgement if neither did, and nothing sweeps that row, so a run
/// that has left the WAL is on the ownership table. A `failed` in `setup`,
/// published only while the run's WAL holds no row, writes no row, so it
/// records no start.
/// An operator's drop writes an ownership row too, even
/// for a run that never made its first WAL write, which counts it as
/// started on purpose: a dropped run must not be run again. A consumed
/// claim that records the drop ([`TriggerClaimRow::operator_dropped_at`])
/// is the one exception, read by the dead-letter watch and requeue: that
/// row is the drop's, the claim already says the trigger must not run
/// again, and only the WAL can show a start (issue 1055). A reader of
/// both records reads the WAL first, so an archive landing between the two
/// reads cannot hide the run from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunRecords {
    /// Where the worker's WAL holds the run, if it holds a row for it.
    pub wal: Option<WalRun>,
    /// The ownership row's status as stored, if there is a row. Left
    /// unparsed: only the row's presence decides, so a status this build
    /// cannot read does not hide a start.
    pub owner_row_status: Option<String>,
}

impl RunRecords {
    /// The records as the WAL alone gives them, when the ownership row was
    /// not read.
    pub fn in_the_wal(wal: Option<WalRun>) -> Self {
        Self {
            wal,
            owner_row_status: None,
        }
    }

    /// Whether the run has a start on record: either record holds it.
    pub fn start_on_record(&self) -> bool {
        self.wal.is_some() || self.owner_row_status.is_some()
    }

    /// Where the run is on record, for a log line, or `None` when it is on
    /// neither record.
    pub fn where_on_record(&self) -> Option<String> {
        match (self.wal, &self.owner_row_status) {
            (Some(WalRun::InFlight), _) => Some("in flight in the worker's WAL".to_string()),
            (Some(WalRun::Finished), _) => {
                Some("finished, in the worker's WAL until its archive is acknowledged".to_string())
            }
            (None, Some(status)) => Some(format!("on the control plane's record as {status}")),
            (None, None) => None,
        }
    }
}

fn row_to_claim(row: &sqlx::sqlite::SqliteRow) -> Result<TriggerClaimRow, ControlPlaneStoreError> {
    Ok(TriggerClaimRow {
        stream: row.get("stream"),
        stream_epoch: row.get("stream_epoch"),
        stream_seq: row.get::<i64, _>("stream_seq") as u64,
        claimant: row.get("claimant"),
        state: ClaimState::of_row(row)?,
        invocation_id: row.get("invocation_id"),
        claimed_at: row.get("claimed_at"),
        operator_dropped_at: row.get("operator_dropped_at"),
    })
}

impl ControlPlaneStore {
    /// Claim a trigger using one insert-if-absent statement, then read the
    /// winning row.
    ///
    /// The row names `invocation_id`, the id the claimant will start the
    /// invocation under, from the moment it is written rather than from
    /// the durable start. That is what lets startup recovery match this row
    /// to its run's WAL row or ownership row and mark it started, through
    /// [`Self::mark_unsettled_trigger_started`], when the process that
    /// started the run did not mark it (#959, #967). A losing claim's id is
    /// never written.
    pub async fn claim_trigger(
        &self,
        key: TriggerKey<'_>,
        worker_id: &str,
        invocation_id: &str,
        now_ms: i64,
    ) -> Result<TriggerClaim, ControlPlaneStoreError> {
        let inserted = sqlx::query(
            "INSERT INTO trigger_claim \
             (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at) \
             VALUES (?, ?, ?, ?, 'claimed', ?, ?) \
             ON CONFLICT(stream, stream_epoch, stream_seq) DO NOTHING",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(worker_id)
        .bind(invocation_id)
        .bind(now_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 1 {
            return Ok(TriggerClaim::Won);
        }
        let row = sqlx::query(
            "SELECT claimant, state, consumed_why, invocation_id, claimed_at FROM trigger_claim \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .fetch_one(&self.pool)
        .await?;
        match ClaimState::of_row(&row)? {
            ClaimState::DurablyStarted => Ok(TriggerClaim::Started {
                invocation_id: row.get("invocation_id"),
            }),
            ClaimState::Consumed { why } => Ok(TriggerClaim::Consumed {
                invocation_id: row.get("invocation_id"),
                why,
            }),
            ClaimState::Claimed => Ok(TriggerClaim::Held {
                claimant: row.get("claimant"),
                invocation_id: row.get("invocation_id"),
                claimed_at: row.get("claimed_at"),
            }),
        }
    }

    /// Adopt an unfinished claim from a previous worker process, naming the
    /// adopter's own `invocation_id` in place of the previous holder's.
    pub async fn take_over_trigger_claim(
        &self,
        key: TriggerKey<'_>,
        claimant: &str,
        worker_id: &str,
        invocation_id: &str,
        now_ms: i64,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET claimant = ?, invocation_id = ?, claimed_at = ? \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? \
             AND claimant = ? AND state = 'claimed'",
        )
        .bind(worker_id)
        .bind(invocation_id)
        .bind(now_ms)
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(claimant)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Record on the claim for `key` that the run `invocation_id` names has
    /// durably started, if the claim names that run. Returns whether it
    /// did: `false` when no row for `key` names `invocation_id`, because
    /// the row is gone or names another invocation, which this leaves as it
    /// is (issue 1046).
    ///
    /// The dispatcher calls it at a run's first WAL write, for the claim
    /// that delivery wrote naming the run. A claim already marked for the
    /// run is matched too, so a retry of a mark that landed but was reported
    /// as refused says it marked. For a mark that matched nothing, the
    /// dispatcher records the start with [`Self::record_trigger_start`].
    pub async fn mark_trigger_started(
        &self,
        key: TriggerKey<'_>,
        invocation_id: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET state = 'durably_started' \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? AND invocation_id = ? \
             AND state IN ('claimed', 'durably_started')",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Record that the run `invocation_id` names has durably started, for a
    /// trigger whose claim [`Self::mark_trigger_started`] did not find
    /// naming that run. What it does depends on what the claim for `key`
    /// holds, read and written in one transaction that holds the write lock
    /// throughout:
    ///
    /// - **No row:** writes one, `durably_started`, naming the run and
    ///   held by `claimant`.
    /// - **A `claimed` row naming another invocation, or none:** records
    ///   this run's start on it in that invocation's place, held by
    ///   `claimant`. Left `claimed`, it could be released from under a run
    ///   that started, by the copy that holds it if that copy's own run
    ///   never starts.
    /// - **A `consumed` row, naming any invocation:** records this run's
    ///   start over it. The run did start, and left `consumed`, the
    ///   trigger's dead letter could be requeued and run again.
    /// - **A `durably_started` row naming another invocation:** leaves it.
    ///   A start is on record for the trigger already, which is all a copy
    ///   that comes back needs to be refused.
    /// - **A row naming the run,** in either state: marks it, as the mark
    ///   would have, changing only its state. Already started for the run,
    ///   it is still this run's start, not another's.
    ///
    /// The start this records is a fact: the run has made its first WAL
    /// write. The claim is what refuses a redelivered or replayed copy of
    /// its trigger, so a claim that does not record a start the run made
    /// lets the trigger run again (issue 1046).
    pub async fn record_trigger_start(
        &self,
        key: TriggerKey<'_>,
        claimant: &str,
        invocation_id: &str,
        now_ms: i64,
    ) -> Result<RecordedStart, ControlPlaneStoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            "SELECT state, consumed_why, invocation_id FROM trigger_claim \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .fetch_optional(&mut *tx)
        .await?;
        let found = match row {
            Some(row) => Some((
                ClaimState::of_row(&row)?,
                row.get::<Option<String>, _>("invocation_id"),
            )),
            None => None,
        };
        let recorded = match found {
            // First, whatever invocation it names: the run started.
            Some((ClaimState::Consumed { why }, consumed_by)) => {
                RecordedStart::OverConsumed { consumed_by, why }
            }
            Some((ClaimState::Claimed | ClaimState::DurablyStarted, Some(named)))
                if named == invocation_id =>
            {
                RecordedStart::Marked
            }
            Some((ClaimState::DurablyStarted, started)) => {
                // Read only; rolled back here rather than when the
                // transaction is dropped, so the write lock goes now.
                tx.rollback().await?;
                return Ok(RecordedStart::AlreadyStarted {
                    invocation_id: started,
                });
            }
            Some((ClaimState::Claimed, claimed_for)) => RecordedStart::WrittenOver { claimed_for },
            None => RecordedStart::Written,
        };
        let write = match &recorded {
            RecordedStart::Written => sqlx::query(
                "INSERT INTO trigger_claim \
                 (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at) \
                 VALUES (?, ?, ?, ?, 'durably_started', ?, ?)",
            )
            .bind(key.stream)
            .bind(key.stream_epoch)
            .bind(key.stream_seq as i64)
            .bind(claimant)
            .bind(invocation_id)
            .bind(now_ms),
            // The row names the run already, and this daemon's claim of it
            // wrote that name: only its state changes.
            RecordedStart::Marked => sqlx::query(
                "UPDATE trigger_claim SET state = 'durably_started' \
                 WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
            )
            .bind(key.stream)
            .bind(key.stream_epoch)
            .bind(key.stream_seq as i64),
            // `WrittenOver` or `OverConsumed` (`AlreadyStarted` returned
            // above): the row now records this run's start, which this
            // daemon claimed, so it names this daemon as the claimant too.
            // It keeps its `claimed_at`: the retention sweep counts a
            // claim's age from it, and it is never later than this.
            _ => sqlx::query(
                "UPDATE trigger_claim \
                 SET claimant = ?, invocation_id = ?, state = 'durably_started', \
                 consumed_why = NULL, operator_dropped_at = NULL \
                 WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
            )
            .bind(claimant)
            .bind(invocation_id)
            .bind(key.stream)
            .bind(key.stream_epoch)
            .bind(key.stream_seq as i64),
        };
        write.execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// Record that the run a `claimed` row names has durably started, if
    /// the row is still `claimed` and still names `invocation_id`. Returns
    /// whether the row changed.
    ///
    /// Startup recovery calls this for each row of
    /// [`Self::unsettled_trigger_claims`] whose run has a start on record,
    /// through `TriggerClaims::recover`, whose result is what a dispatcher
    /// needs before it can arbitrate at all: a redelivered or replayed
    /// trigger that meets the marked row is dropped instead of started a
    /// second time (#959, #967). The row is found by its key, so each mark
    /// is one primary-key write; finding it by the invocation id instead
    /// would read the whole table, which has no index on that column
    /// (#989). The id in the condition leaves alone a row that changed
    /// after it was read: adopted under another id, released, or marked
    /// already.
    pub async fn mark_unsettled_trigger_started(
        &self,
        key: TriggerKey<'_>,
        invocation_id: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET state = 'durably_started' \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? \
             AND state = 'claimed' AND invocation_id = ?",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Take again a `claimed` row `claimant` holds, naming `invocation_id`
    /// in place of `abandoned_invocation_id` and restarting its age, if the
    /// row is still `claimed` by `claimant` and still names the abandoned
    /// id. Returns whether it did.
    ///
    /// The dispatcher calls it for a claim of its own that no copy of the
    /// trigger is handling any more and whose run has no start on record
    /// (issue 967). The id in the condition is what makes it a
    /// compare-and-set between two copies that both found the claim
    /// abandoned: [`Self::take_over_trigger_claim`] matches on the claimant
    /// alone, which another claimant's row allows, but every copy that
    /// re-takes this row claims under the same id.
    pub async fn retake_trigger_claim(
        &self,
        key: TriggerKey<'_>,
        claimant: &str,
        abandoned_invocation_id: &str,
        invocation_id: &str,
        now_ms: i64,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET invocation_id = ?, claimed_at = ? \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? \
             AND claimant = ? AND state = 'claimed' AND invocation_id = ?",
        )
        .bind(invocation_id)
        .bind(now_ms)
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(claimant)
        .bind(abandoned_invocation_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Give back the claim for `key` that `invocation_id` holds: delete the
    /// row while it is still `claimed` and still names that invocation.
    /// Returns whether it did.
    ///
    /// A started row is never deleted: it is the dedupe record. Nor is a row
    /// naming another invocation, however it came to (issue 1061): since a
    /// copy that meets this daemon's own claim with nothing on record can
    /// take it again under a new id (`own_claim`), a release that lands
    /// late, as one the settlement gave up waiting for does, would
    /// otherwise delete the claim of the copy that took it again. A row
    /// written before claims named their run names no invocation, and no
    /// copy of a running dispatcher holds one: its claims, adoptions and
    /// re-takes all write the copy's own id. A release therefore leaves it,
    /// to be adopted or swept.
    pub async fn release_trigger_claim(
        &self,
        key: TriggerKey<'_>,
        invocation_id: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "DELETE FROM trigger_claim \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? AND state = 'claimed' \
             AND invocation_id = ?",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// The claim that names `invocation_id`: the trigger delivery that
    /// started it, or is to.
    ///
    /// `invocation_id` has no index, so this scans the table. That is why
    /// the link is read for one invocation at a time and is not a column
    /// of any list: a per-row lookup in a list would scan the table once
    /// per row. An id is named by at most one row — every claim writes a
    /// fresh one, and an adoption writes the adopter's own — so the scan
    /// stops at its first match, and it runs newest row first (`rowid`
    /// descending, which SQLite walks without a sort) because the run an
    /// operator asks about is usually recent: a claim written lately is
    /// found after reading only the rows written since. A miss, or a run
    /// whose claim is old, still reads the whole table, which the retention
    /// sweep bounds. The ordering can go once the column is indexed (see
    /// #989).
    pub async fn trigger_claim_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<TriggerClaimRow>, ControlPlaneStoreError> {
        let row = sqlx::query(
            "SELECT stream, stream_epoch, stream_seq, claimant, state, consumed_why, \
                    operator_dropped_at, invocation_id, claimed_at \
             FROM trigger_claim WHERE invocation_id = ? ORDER BY rowid DESC LIMIT 1",
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_claim).transpose()
    }

    /// Every `claimed` row, oldest first, joined in the same statement to
    /// the coordination status of the invocation each one names.
    ///
    /// One pass over the table however many rows it returns: the join is
    /// on the ownership table's primary key. `claimed` rows are the
    /// triggers some process has in hand, plus any left behind, so the
    /// answer stays small while the table grows by one `durably_started`
    /// row per trigger.
    ///
    /// `fq doctor`'s stranded-claim check reads the claims it judges
    /// through this, and so does startup recovery (`TriggerClaims::recover`)
    /// for the ones it marks, so the two see the same rows and the same
    /// ownership rows. The ownership status comes back as stored; the
    /// check parses it, recovery needs only to know a row exists.
    pub async fn unsettled_trigger_claims(
        &self,
    ) -> Result<Vec<UnsettledTriggerClaim>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            "SELECT c.stream AS stream, c.stream_epoch AS stream_epoch, \
                    c.stream_seq AS stream_seq, c.claimant AS claimant, c.state AS state, \
                    c.consumed_why AS consumed_why, \
                    c.operator_dropped_at AS operator_dropped_at, \
                    c.invocation_id AS invocation_id, c.claimed_at AS claimed_at, \
                    o.status AS owner_status \
             FROM trigger_claim c \
             LEFT JOIN coordination_invocation_owner o ON o.invocation_id = c.invocation_id \
             WHERE c.state = 'claimed' \
             ORDER BY c.claimed_at, c.stream_seq",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_unsettled).collect()
    }

    /// One claim, if it is still `claimed`, joined to its run's ownership
    /// row exactly as [`Self::unsettled_trigger_claims`] joins every one.
    ///
    /// The dispatcher reads it to settle a claim of its own that no copy of
    /// the trigger is handling any more (issue 967), so it judges the row
    /// and the ownership record recovery and `fq doctor` judge. One
    /// primary-key read, and one on the ownership table's primary key.
    pub async fn unsettled_trigger_claim(
        &self,
        key: TriggerKey<'_>,
    ) -> Result<Option<UnsettledTriggerClaim>, ControlPlaneStoreError> {
        let row = sqlx::query(
            "SELECT c.stream AS stream, c.stream_epoch AS stream_epoch, \
                    c.stream_seq AS stream_seq, c.claimant AS claimant, c.state AS state, \
                    c.consumed_why AS consumed_why, \
                    c.operator_dropped_at AS operator_dropped_at, \
                    c.invocation_id AS invocation_id, c.claimed_at AS claimed_at, \
                    o.status AS owner_status \
             FROM trigger_claim c \
             LEFT JOIN coordination_invocation_owner o ON o.invocation_id = c.invocation_id \
             WHERE c.state = 'claimed' \
             AND c.stream = ? AND c.stream_epoch = ? AND c.stream_seq = ?",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_unsettled).transpose()
    }

    /// The claim row for `key`, whatever its state, read without writing:
    /// `None` when nothing claims the delivery.
    ///
    /// The dead-letter watch reads it before it calls a trigger exhausted
    /// (issue 967): a row marked started is a start on record, and a
    /// `claimed` row names the run whose records it reads next. One
    /// primary-key read.
    pub async fn trigger_claim(
        &self,
        key: TriggerKey<'_>,
    ) -> Result<Option<TriggerClaimRow>, ControlPlaneStoreError> {
        let row = sqlx::query(
            "SELECT stream, stream_epoch, stream_seq, claimant, state, consumed_why, \
                    operator_dropped_at, invocation_id, claimed_at \
             FROM trigger_claim WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_claim).transpose()
    }
}

fn row_to_unsettled(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<UnsettledTriggerClaim, ControlPlaneStoreError> {
    Ok(UnsettledTriggerClaim {
        claim: row_to_claim(row)?,
        owner_row_status: row.get("owner_status"),
    })
}
