use sqlx::Row;

use super::{ControlPlaneStore, ControlPlaneStoreError};

/// Result of arbitrating one broker delivery by its stable stream identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerClaim {
    Won,
    Held { claimant: String },
    Started { invocation_id: String },
}

impl ControlPlaneStore {
    /// Claim a trigger using one insert-if-absent statement, then read the
    /// winning row. The stream name is part of the key until ADR-0032's
    /// stream epoch replaces it.
    pub async fn claim_trigger(
        &self,
        stream: &str,
        seq: u64,
        worker_id: &str,
        now_ms: i64,
    ) -> Result<TriggerClaim, ControlPlaneStoreError> {
        let inserted = sqlx::query(
            "INSERT INTO trigger_claim (stream, stream_seq, claimant, state, invocation_id, claimed_at) \
             VALUES (?, ?, ?, 'claimed', NULL, ?) ON CONFLICT(stream, stream_seq) DO NOTHING",
        )
        .bind(stream)
        .bind(seq as i64)
        .bind(worker_id)
        .bind(now_ms)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 1 {
            return Ok(TriggerClaim::Won);
        }
        let row = sqlx::query(
            "SELECT claimant, state, invocation_id FROM trigger_claim WHERE stream = ? AND stream_seq = ?",
        )
        .bind(stream)
        .bind(seq as i64)
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
        stream: &str,
        seq: u64,
        claimant: &str,
        worker_id: &str,
        now_ms: i64,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET claimant = ?, claimed_at = ? \
             WHERE stream = ? AND stream_seq = ? AND claimant = ? AND state = 'claimed'",
        )
        .bind(worker_id)
        .bind(now_ms)
        .bind(stream)
        .bind(seq as i64)
        .bind(claimant)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn mark_trigger_started(
        &self,
        stream: &str,
        seq: u64,
        invocation_id: &str,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            "UPDATE trigger_claim SET state = 'durably_started', invocation_id = ? \
             WHERE stream = ? AND stream_seq = ?",
        )
        .bind(invocation_id)
        .bind(stream)
        .bind(seq as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Release only unfinished claims; durable-start rows are the dedupe record.
    pub async fn release_trigger_claim(
        &self,
        stream: &str,
        seq: u64,
    ) -> Result<(), ControlPlaneStoreError> {
        sqlx::query(
            "DELETE FROM trigger_claim WHERE stream = ? AND stream_seq = ? AND state = 'claimed'",
        )
        .bind(stream)
        .bind(seq as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
