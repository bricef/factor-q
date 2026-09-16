use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError};

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

impl ControlPlaneStore {
    /// Claim a trigger using one insert-if-absent statement, then read the
    /// winning row.
    pub async fn claim_trigger(
        &self,
        key: TriggerKey<'_>,
        worker_id: &str,
        now_ms: i64,
    ) -> Result<TriggerClaim, ControlPlaneStoreError> {
        let inserted = sqlx::query(
            "INSERT INTO trigger_claim \
             (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at) \
             VALUES (?, ?, ?, ?, 'claimed', NULL, ?) \
             ON CONFLICT(stream, stream_epoch, stream_seq) DO NOTHING",
        )
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(worker_id)
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

    /// Adopt an unfinished claim from a previous worker process.
    pub async fn take_over_trigger_claim(
        &self,
        key: TriggerKey<'_>,
        claimant: &str,
        worker_id: &str,
        now_ms: i64,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET claimant = ?, claimed_at = ? \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? \
             AND claimant = ? AND state = 'claimed'",
        )
        .bind(worker_id)
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
}
