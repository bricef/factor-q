//! The claim's consumed state: a trigger dealt with and no run started
//! (issue 1055). How it was consumed, the write that records it, and the
//! schema rung that lets the claim table hold it.

use super::TriggerKey;
use crate::control_plane::store::{ControlPlaneStore, ControlPlaneStoreError};

/// The claim table rebuilt to admit a third state, `consumed`: a trigger
/// dealt with and no run started, with `consumed_why` set exactly when the
/// state is `consumed` (issue 1055), and `operator_dropped_at` beside it,
/// set only on a consumed claim, when the claim records that an operator's
/// drop of the run it names wrote that run's ownership row (epoch ms, when
/// the claim recorded it). SQLite cannot change a `CHECK` in place, so the
/// table is copied into a new one and renamed over the old, inside the
/// rung's transaction. Every row keeps its state, none is consumed, and
/// none records a drop. The rows are copied in `rowid` order, so they keep
/// their order in the table: `trigger_claim_for_invocation` takes the
/// latest row naming a run by it, and the retention sweep finds old rows
/// first.
pub(in crate::control_plane::store) const TRIGGER_CLAIM_CONSUMED_STATE_SQL: &str = r#"
CREATE TABLE trigger_claim_with_consumed (
    stream              TEXT NOT NULL,
    stream_epoch        INTEGER NOT NULL,
    stream_seq          INTEGER NOT NULL,
    claimant            TEXT NOT NULL,
    state               TEXT NOT NULL
                        CHECK (state IN ('claimed', 'durably_started', 'consumed')),
    invocation_id       TEXT,
    claimed_at          INTEGER NOT NULL,
    consumed_why        TEXT,
    operator_dropped_at INTEGER,
    CHECK ((state = 'consumed') = (consumed_why IS NOT NULL)),
    CHECK (operator_dropped_at IS NULL OR state = 'consumed'),
    PRIMARY KEY (stream, stream_epoch, stream_seq)
);
INSERT INTO trigger_claim_with_consumed
    (stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at)
    SELECT stream, stream_epoch, stream_seq, claimant, state, invocation_id, claimed_at
    FROM trigger_claim ORDER BY rowid;
DROP TABLE trigger_claim;
ALTER TABLE trigger_claim_with_consumed RENAME TO trigger_claim;
"#;

/// Why a trigger was consumed without its run starting: the answer its
/// delivery got. Stored in `consumed_why` under the stable spellings
/// [`Self::as_str`] gives, which the operator surface publishes too. A
/// spelling this build does not know is kept as it is ([`Self::Other`])
/// rather than refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumedWhy {
    /// Its subject is not one the dispatcher can address.
    RefusedSubject,
    /// Its subject names an agent id that is not a valid one.
    InvalidAgentId,
    /// Its agent is not loaded.
    UnknownAgent,
    /// Its payload does not parse.
    InvalidPayload,
    /// It was requeued from a hold, its replacement published under a new
    /// sequence.
    RequeuedFromHold,
    /// Its agent was removed while it waited in a hold.
    AgentRemoved,
    /// Its run failed for good before its first WAL write.
    FailedForGood,
    /// Its run was halted by an operator before its first WAL write.
    HaltedInSetup,
    /// Its run returned before its first WAL write without failing.
    ReturnedUnstarted,
    /// It ran out of deliveries, and the dispatcher published its dead
    /// letter.
    DeadLettered,
    /// A spelling this build does not know, kept as it is. Not there for
    /// another build's sake: it is there so that one row whose reason this
    /// build cannot interpret does not fail every reader of the claim
    /// table, as one unreadable ownership row once failed `fq invocation
    /// list`, `fq doctor` and the retention sweep (issue 1082). The readers
    /// take it as any reason that writes no dead letter.
    Other(String),
}

impl ConsumedWhy {
    /// The stable spelling stored and published for it.
    pub fn as_str(&self) -> &str {
        match self {
            ConsumedWhy::RefusedSubject => "refused_subject",
            ConsumedWhy::InvalidAgentId => "invalid_agent_id",
            ConsumedWhy::UnknownAgent => "unknown_agent",
            ConsumedWhy::InvalidPayload => "invalid_payload",
            ConsumedWhy::RequeuedFromHold => "requeued_from_hold",
            ConsumedWhy::AgentRemoved => "agent_removed",
            ConsumedWhy::FailedForGood => "failed_for_good",
            ConsumedWhy::HaltedInSetup => "halted_in_setup",
            ConsumedWhy::ReturnedUnstarted => "returned_unstarted",
            ConsumedWhy::DeadLettered => "dead_lettered",
            ConsumedWhy::Other(spelling) => spelling,
        }
    }

    /// What a stored spelling names.
    pub fn of_column(spelling: &str) -> Self {
        match spelling {
            "refused_subject" => ConsumedWhy::RefusedSubject,
            "invalid_agent_id" => ConsumedWhy::InvalidAgentId,
            "unknown_agent" => ConsumedWhy::UnknownAgent,
            "invalid_payload" => ConsumedWhy::InvalidPayload,
            "requeued_from_hold" => ConsumedWhy::RequeuedFromHold,
            "agent_removed" => ConsumedWhy::AgentRemoved,
            "failed_for_good" => ConsumedWhy::FailedForGood,
            "halted_in_setup" => ConsumedWhy::HaltedInSetup,
            "returned_unstarted" => ConsumedWhy::ReturnedUnstarted,
            "dead_lettered" => ConsumedWhy::DeadLettered,
            other => ConsumedWhy::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for ConsumedWhy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ControlPlaneStore {
    /// Record that the trigger the claim for `key` names was consumed without
    /// its run starting, as `why` says, if the claim is still `claimed` and
    /// still names `invocation_id`. Returns whether it did. Scoped to its own
    /// invocation as the release is ([`Self::release_trigger_claim`]): a
    /// claim taken again since, or one already started, is left.
    ///
    /// The row is then the trigger's record that it was dealt with, kept as
    /// long as a started one is ([`Self::sweep_settled_trigger_claims`]): a
    /// copy of the trigger that comes back, because the answer to its
    /// delivery was lost, meets it and is acked and dropped rather than
    /// handled again (issue 1055).
    pub async fn consume_trigger_claim(
        &self,
        key: TriggerKey<'_>,
        invocation_id: &str,
        why: &ConsumedWhy,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET state = 'consumed', consumed_why = ? \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? AND state = 'claimed' \
             AND invocation_id = ?",
        )
        .bind(why.as_str())
        .bind(key.stream)
        .bind(key.stream_epoch)
        .bind(key.stream_seq as i64)
        .bind(invocation_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Record that the run a `consumed` claim names has durably started, if
    /// the claim is still `consumed` and still names `invocation_id`:
    /// "consumed" says no run started, and a start on record is the truth.
    /// Returns whether the row changed. The dead-letter watch and
    /// `dead_letter.requeue` call it when they find that run's start on
    /// record (`ExhaustionCheck`); left consumed, a requeue could run the
    /// work a second time.
    pub async fn mark_consumed_trigger_started(
        &self,
        key: TriggerKey<'_>,
        invocation_id: &str,
    ) -> Result<bool, ControlPlaneStoreError> {
        Ok(sqlx::query(
            "UPDATE trigger_claim SET state = 'durably_started', consumed_why = NULL, \
             operator_dropped_at = NULL \
             WHERE stream = ? AND stream_epoch = ? AND stream_seq = ? AND state = 'consumed' \
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
}
