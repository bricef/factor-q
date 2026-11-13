use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus};

/// Rows one retention DELETE takes before it lets other writers in.
const SWEEP_BATCH_ROWS: i64 = 10_000;

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
}

/// One claim row, read back whole for an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerClaimRow {
    pub stream: String,
    pub stream_epoch: i64,
    pub stream_seq: u64,
    pub claimant: String,
    /// `state = 'durably_started'`; otherwise the row is `claimed`.
    pub durably_started: bool,
    /// Nullable in the schema: a claim written before claims named their
    /// invocation at claim time has none until it is marked.
    pub invocation_id: Option<String>,
    pub claimed_at: i64,
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
/// when an invocation is archived, found ambiguous or dropped, and is
/// never swept, so its presence says the run got that far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsettledTriggerClaim {
    pub claim: TriggerClaimRow,
    pub owner_status: Option<OwnerStatus>,
}

fn row_to_claim(row: &sqlx::sqlite::SqliteRow) -> TriggerClaimRow {
    TriggerClaimRow {
        stream: row.get("stream"),
        stream_epoch: row.get("stream_epoch"),
        stream_seq: row.get::<i64, _>("stream_seq") as u64,
        claimant: row.get("claimant"),
        durably_started: row.get::<String, _>("state") == "durably_started",
        invocation_id: row.get("invocation_id"),
        claimed_at: row.get("claimed_at"),
    }
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
            "SELECT claimant, state, invocation_id, claimed_at FROM trigger_claim \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .fetch_one(&self.pool)
        .await?;
        if row.get::<String, _>("state") == "durably_started" {
            Ok(TriggerClaim::Started {
                invocation_id: row.get("invocation_id"),
            })
        } else {
            Ok(TriggerClaim::Held {
                claimant: row.get("claimant"),
                invocation_id: row.get("invocation_id"),
                claimed_at: row.get("claimed_at"),
            })
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

    pub async fn mark_trigger_started(
        &self,
        key: TriggerKey<'_>,
        invocation_id: &str,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            "UPDATE trigger_claim SET state = 'durably_started', invocation_id = ? \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ?",
        )
        .bind(invocation_id)
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Release only unfinished claims; durable-start rows are the dedupe record.
    pub async fn release_trigger_claim(
        &self,
        key: TriggerKey<'_>,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            "DELETE FROM trigger_claim \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? AND state = 'claimed'",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete every `durably_started` row claimed before `cutoff_ms`,
    /// whatever its stream incarnation. Returns how many went.
    ///
    /// A started row is the dedupe record: it stops a redelivered or
    /// replayed copy of its trigger from starting a second run, and has to
    /// live as long as the message can come back. A claim is never older
    /// than its message — `claimed_at` is the dispatcher's clock when it
    /// handled a delivery of a message already on the stream, and an
    /// adoption only moves it later — and the stream drops a message older
    /// than its maximum age. So once the cutoff is past that age, which
    /// `[state] trigger_claim_retention_days` enforces at load, a row older
    /// than it names a message that is gone. `ClaimJudgement::message`
    /// reads a claim's message the same way for `fq doctor`.
    ///
    /// Deletes in batches, like the projection's sweeps: the first sweep
    /// on a long-lived instance faces every row it has ever claimed, and
    /// one unbounded DELETE would hold the write lock against the
    /// dispatcher's claims for its whole length. `claimed_at` has no
    /// index, so each batch scans until it has found its rows; old rows
    /// sit at the front of the table, so the scan finds them early.
    pub async fn sweep_started_trigger_claims(
        &self,
        cutoff_ms: i64,
    ) -> Result<u64, ControlPlaneStoreError> {
        self.sweep_started_trigger_claims_batched(cutoff_ms, SWEEP_BATCH_ROWS)
            .await
    }

    pub(super) async fn sweep_started_trigger_claims_batched(
        &self,
        cutoff_ms: i64,
        batch: i64,
    ) -> Result<u64, ControlPlaneStoreError> {
        let mut total = 0u64;
        loop {
            let deleted = sqlx::query(
                "DELETE FROM trigger_claim WHERE rowid IN \
                 (SELECT rowid FROM trigger_claim \
                  WHERE state = 'durably_started' AND claimed_at < ? LIMIT ?)",
            )
            .bind(cutoff_ms)
            .bind(batch)
            .execute(&self.pool)
            .await?
            .rows_affected();
            total += deleted;
            if deleted < batch as u64 {
                return Ok(total);
            }
        }
    }

    /// Delete every `claimed` row claimed before `cutoff_ms` and held by
    /// anyone but `live_claimant`, whatever its stream incarnation.
    /// Returns how many went.
    ///
    /// `live_claimant` is the worker id the sweeping daemon's own
    /// dispatcher claims under. Its `claimed` rows are never deleted,
    /// however old: one may be a trigger it is still holding behind a
    /// paused model or a full agent, and nothing on the row says whether
    /// it is. A row under any other id was left by a process that has
    /// exited (one daemon dispatches at a time, under a new id per start).
    /// Older than the cutoff, its message has left the stream for the
    /// reason [`Self::sweep_started_trigger_claims`] gives, so no
    /// redelivery can adopt it and nothing can follow from it.
    ///
    /// Batched for the same reason.
    pub async fn sweep_abandoned_trigger_claims(
        &self,
        cutoff_ms: i64,
        live_claimant: &str,
    ) -> Result<u64, ControlPlaneStoreError> {
        self.sweep_abandoned_trigger_claims_batched(cutoff_ms, live_claimant, SWEEP_BATCH_ROWS)
            .await
    }

    pub(super) async fn sweep_abandoned_trigger_claims_batched(
        &self,
        cutoff_ms: i64,
        live_claimant: &str,
        batch: i64,
    ) -> Result<u64, ControlPlaneStoreError> {
        let mut total = 0u64;
        loop {
            let deleted = sqlx::query(
                "DELETE FROM trigger_claim WHERE rowid IN \
                 (SELECT rowid FROM trigger_claim \
                  WHERE state = 'claimed' AND claimant <> ? AND claimed_at < ? LIMIT ?)",
            )
            .bind(live_claimant)
            .bind(cutoff_ms)
            .bind(batch)
            .execute(&self.pool)
            .await?
            .rows_affected();
            total += deleted;
            if deleted < batch as u64 {
                return Ok(total);
            }
        }
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
            "SELECT stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at \
             FROM trigger_claim WHERE invocation_id = ? ORDER BY rowid DESC LIMIT 1",
        )
        .bind(invocation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_claim))
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
    /// ownership rows.
    pub async fn unsettled_trigger_claims(
        &self,
    ) -> Result<Vec<UnsettledTriggerClaim>, ControlPlaneStoreError> {
        let rows = sqlx::query(
            "SELECT c.stream AS stream, c.stream_epoch AS stream_epoch, \
                    c.stream_seq AS stream_seq, c.claimant AS claimant, c.state AS state, \
                    c.invocation_id AS invocation_id, c.claimed_at AS claimed_at, \
                    o.status AS owner_status \
             FROM trigger_claim c \
             LEFT JOIN coordination_invocation_owner o ON o.invocation_id = c.invocation_id \
             WHERE c.state = 'claimed' \
             ORDER BY c.claimed_at, c.stream_seq",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let owner_status = row
                    .get::<Option<String>, _>("owner_status")
                    .map(|s| {
                        OwnerStatus::parse(&s).ok_or_else(|| {
                            ControlPlaneStoreError::Malformed(format!("unknown owner status `{s}`"))
                        })
                    })
                    .transpose()?;
                Ok(UnsettledTriggerClaim {
                    claim: row_to_claim(row),
                    owner_status,
                })
            })
            .collect()
    }
}
