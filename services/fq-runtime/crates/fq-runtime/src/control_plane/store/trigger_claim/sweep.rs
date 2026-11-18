//! The claim table's retention sweeps: started rows, and rows an exited
//! process left `claimed`, both past the window.

use super::super::{ControlPlaneStore, ControlPlaneStoreError};

/// Rows one retention DELETE takes before it lets other writers in.
const SWEEP_BATCH_ROWS: i64 = 10_000;

impl ControlPlaneStore {
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

    pub(in crate::control_plane::store) async fn sweep_started_trigger_claims_batched(
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

    pub(in crate::control_plane::store) async fn sweep_abandoned_trigger_claims_batched(
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
}
