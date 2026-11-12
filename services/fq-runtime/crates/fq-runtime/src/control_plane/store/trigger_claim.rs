use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError, OwnerStatus};

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
    Held { claimant: String },
    Started { invocation_id: String },
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
    /// the durable start. That is what lets startup recovery find this row
    /// from an in-flight WAL row and mark it started, through
    /// [`Self::mark_trigger_started_by_invocation`], when the process that
    /// made the WAL write died before marking it (#959). A losing claim's
    /// id is never written.
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
            "SELECT claimant, state, invocation_id FROM trigger_claim \
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

    /// Record that the invocation a `claimed` row names has durably
    /// started, finding the row by that invocation's id rather than by the
    /// trigger's key.
    ///
    /// Startup recovery calls this for every invocation it finds in flight,
    /// through `TriggerClaims::recover`, whose result is what a dispatcher
    /// needs before it can arbitrate at all: an in-flight WAL row is proof
    /// of a durable start whose claim update a crash may have prevented, and
    /// a redelivered trigger that meets the marked row is dropped instead of
    /// started a second time (#959). Returns whether a row changed. An
    /// invocation no claim names (a direct run) changes nothing, and neither
    /// does a row already `durably_started`.
    pub async fn mark_trigger_started_by_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET state = 'durably_started' \
             WHERE invocation_id = ? AND state = 'claimed'",
        )
        .bind(invocation_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
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

    /// The claim that names `invocation_id`: the trigger delivery that
    /// started it, or is to.
    ///
    /// `invocation_id` has no index, so this reads the table through.
    /// That is why the link is read for one invocation at a time and is
    /// not a column of any list: a per-row lookup in a list would scan the
    /// table once per row. An id is named by at most one row — every
    /// claim writes a fresh one, and an adoption writes the adopter's
    /// own — so the scan stops at the first match.
    pub async fn trigger_claim_for_invocation(
        &self,
        invocation_id: &str,
    ) -> Result<Option<TriggerClaimRow>, ControlPlaneStoreError> {
        let row = sqlx::query(
            "SELECT stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at \
             FROM trigger_claim WHERE invocation_id = ? LIMIT 1",
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
