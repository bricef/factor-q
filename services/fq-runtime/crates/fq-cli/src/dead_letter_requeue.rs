//! `dead_letter.requeue`, daemon-side (plan Phase 4, verb 8): re-run a
//! trigger that exhausted its delivery budget — once.
//!
//! # It is keyed on the trigger, and that is why it can be idempotent
//!
//! A dead letter has no identity of its own. It is addressed by the
//! log sequence it was recorded at (`DeadLetterState`), and a position
//! is not a name — which is the gap #464 tracks and the reason no
//! command mints a DeadLetter reference. This command does not need
//! one: the thing being requeued is a **trigger**, triggers are named
//! (step A) and recorded permanently (step B), and the dead letter
//! carries the name in its `trigger_id` annotation.
//!
//! So the key is the original trigger's identity, and the record that
//! makes the second call fail is the `requeued_from` column on the
//! requeued trigger's own row — uniquely indexed, so "a dead letter is
//! requeued at most once" is a property of the database rather than of
//! a check someone remembered to run.
//!
//! # A new identity, and the row is written here
//!
//! `publish_trigger_named` exists so a caller that already holds an id
//! can republish under it. A requeue deliberately does not: `triggers`
//! is `INSERT OR IGNORE` on the identity, so republishing under the
//! original's name would write no new row and leave nothing at all to
//! distinguish "published once" from "published, then requeued".
//!
//! The row is therefore written *here*, before the publish, rather than
//! by the projection when it later folds the dispatcher's event. Two
//! reasons, and the second is the load-bearing one: an operator who
//! requeues twice in a second must be refused the second time, and a
//! record that appears only after a worker picks the trigger up would
//! not be there yet; and the write is what claims the key, so reserving
//! before publishing is what stops two concurrent requeues from both
//! running the agent.
//!
//! # Its receipt names a Trigger
//!
//! A requeue produces a trigger, so it says so — `Receipt::naming` with
//! the `TriggerKey` shape `trigger.get` takes. Naming and not
//! positioning, for `trigger.publish`'s reason: the publish ack is a
//! coordinate on the *trigger* stream, and a receipt's watermark is
//! documented as the number a caller passes as `min_seq`, so putting
//! the ack there would have a caller gate a read on a log the reader
//! never consults. Here there is not even a wait to gate: the record
//! exists before this command answers, so the key resolves immediately.

use std::sync::Arc;

use fq_edge::wire::WireError;
use fq_runtime::control_plane::projection::ProjectionStore;
use fq_runtime::dead_letter::DeadLetter;
use fq_runtime::events::{Event, subjects};
use fq_runtime::trigger::Trigger;

use crate::trigger_command::TriggerKey;

/// The op's rendered name, quoted in every refusal it makes.
const OP: &str = "dead_letter.requeue";

/// The typed input of `dead_letter.requeue` on the wire.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct RequeueCommandInput {
    /// The agent whose dead letter to requeue.
    agent_id: String,
    /// Select by the original trigger's sequence on the trigger stream
    /// — the number `dead_letter.list` prints. Absent selects the
    /// agent's most recent dead letter.
    ///
    /// A position, not an identity: it is a coordinate on a different
    /// stream, it is not unique across agents, and it can be absent
    /// altogether, which is why it selects rather than keys.
    #[serde(default)]
    trigger_seq: Option<u64>,
}

fn internal(e: impl std::fmt::Display) -> WireError {
    WireError::Internal {
        message: e.to_string(),
    }
}

/// `dead_letter.requeue`'s declaration — the value, apart from the
/// handler, so a test can read the contract text the surface publishes
/// without standing up a bus and a store to bind it to.
fn requeue_declaration() -> fq_ops::Command {
    fq_ops::Command::new::<RequeueCommandInput>(
        fq_ops::DeadLetter::Requeue,
        fq_ops::Authority {
            // Write over Trigger, not over DeadLetter: what this
            // command appends is a trigger, and the authority to
            // re-run work is the authority to publish it. Granting a
            // reader of dead letters the power to re-run them is
            // exactly the conflation a declared scope prevents.
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Trigger,
        },
        "Re-run a dead-lettered trigger: publish it again, once, with a fresh delivery budget.",
        fq_ops::Stability::Experimental,
    )
    .description(concat!(
        "IDEMPOTENT ON THE ORIGINAL TRIGGER. A dead letter can be requeued \
         once; asking again is refused with a `Conflict` that NAMES THE \
         TRIGGER the first call produced, so the second attempt is a \
         redirection rather than a dead end — hand that id to `trigger.get`. \
         The key is the original trigger's identity, which the dead letter \
         carries, and the record is the `requeued_from` field on the requeued \
         trigger, uniquely indexed. ",
        "THE RECEIPT NAMES A TRIGGER, not a dead letter: its `AtomRef` carries \
         `{\"trigger_id\": \"…\"}` for the trigger this call made, which is \
         exactly the key `trigger.get` takes. It is a NEW identity — a requeue \
         is a later trigger and sorts as one — and the requeued trigger's \
         `requeued_from` names the one it re-ran. The original is untouched. \
         Unlike `trigger.publish`, the record exists before this answers, so \
         the key resolves with no wait and there is no watermark to gate on. ",
        "A DEAD LETTER WITH NO `trigger_id` IS REFUSED, `Unlocatable`, and \
         nothing is published. Two kinds have none: anything dead-lettered \
         before triggers were named, and one the advisory path recorded after \
         the original had aged off the trigger stream — it reads the identity \
         and never invents one. Such a dead letter can only be re-run as new \
         work, with `trigger.publish`, and that is deliberately your call to \
         make rather than this command's: requeueing it would hand back the \
         guarantee's name without the guarantee. ",
        "The payload is the dead letter's own record of the trigger, verbatim. \
         `trigger_seq` selects which dead letter by the original's trigger-\
         stream position (what `dead_letter.list` prints); absent takes the \
         agent's most recent.",
    ))
}

/// Register `dead_letter.requeue` on the daemon's edge.
pub(crate) fn register_dead_letter_requeue(
    registry: &mut fq_edge::EdgeRegistry,
    bus: fq_runtime::EventBus,
    projection: Arc<ProjectionStore>,
) -> anyhow::Result<()> {
    registry
        .command::<RequeueCommandInput, _, _>(
            requeue_declaration(),
            move |input: RequeueCommandInput| {
                let bus = bus.clone();
                let projection = projection.clone();
                async move { requeue(&bus, &projection, input).await }
            },
        )
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;
    Ok(())
}

/// One requeue, end to end.
async fn requeue(
    bus: &fq_runtime::EventBus,
    projection: &ProjectionStore,
    input: RequeueCommandInput,
) -> Result<fq_ops::Receipt, WireError> {
    // The daemon validates what it is asked to select by: an id the
    // subject grammar cannot carry names no dead letter, and building a
    // consumer filter out of it would be malformed rather than empty.
    let agent = fq_runtime::AgentId::new(&input.agent_id).map_err(|e| WireError::InvalidInput {
        op: OP.into(),
        message: format!("invalid agent name `{}`: {e}", input.agent_id),
    })?;
    let event = select_dead_letter(bus, agent.as_str(), input.trigger_seq).await?;

    // The identity, the subject and the payload all come off this one
    // event, through the lens that already defines what a recorded
    // event says about a trigger. One read, one source: there is no
    // second place for the payload to come from and therefore no second
    // answer it could disagree with.
    let original = Trigger::from_event(&event).ok_or_else(|| unnamed(&event))?;

    let requeued = Trigger::requeue_of(&original, fq_runtime::trigger::subject(&agent));
    let claimed = projection
        .reserve_requeue(&requeued, agent.as_str(), &chrono::Utc::now().to_rfc3339())
        .await
        .map_err(internal)?;
    if !claimed {
        return Err(already_requeued(projection, &original).await);
    }

    if let Err(err) = bus
        .publish_trigger_named(agent.as_str(), requeued.id, &requeued.payload)
        .await
    {
        // The claim was for a publish that never happened; give it
        // back so the operator can try again. Best-effort by nature —
        // a crash here leaves a reservation that blocks a re-attempt,
        // which is the direction that never runs an agent twice.
        let _ = projection
            .release_requeue(requeued.id, original.id)
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "releasing a failed requeue claim"));
        return Err(publish_failed(err, &original));
    }
    tracing::info!(
        agent_id = %agent,
        trigger_id = %requeued.id,
        requeued_from = %original.id,
        "requeued dead-lettered trigger"
    );

    Ok(fq_ops::Receipt::naming(
        fq_ops::Domain::Trigger,
        serde_json::json!(TriggerKey {
            trigger_id: requeued.id.to_string()
        }),
    ))
}

/// The dead letter this request selects, as the event that recorded it.
///
/// **One slot, not a page.** This used to call the operator module's
/// listing with `usize::MAX`, which materialised every dead letter the
/// agent had ever accumulated in order to take one of them — the
/// unbounded read that `dead_letter.list` had capped at the edge and
/// this path had kept. A cap would have been the wrong fix: it would
/// have made `trigger_seq` silently unable to reach anything older than
/// the last N, reported as "no such sequence". The scan is a walk of
/// the agent's `failed` subject either way, and what it holds is now
/// one event rather than all of them.
///
/// Bounded at the far end by the tip observed at entry — the last
/// sequence *this subject* carries, not the stream's, which is the hang
/// the Turn, Event and DeadLetter atoms each had to fix.
async fn select_dead_letter(
    bus: &fq_runtime::EventBus,
    agent: &str,
    trigger_seq: Option<u64>,
) -> Result<Event, WireError> {
    use futures::StreamExt;
    let subject = subjects::agent_failed(agent);
    let tip = bus
        .last_event_seq_matching(&subject)
        .await
        .map_err(internal)?;
    let mut selected: Option<Event> = None;
    if tip > 0 {
        let mut events = bus.events_from(&subject, 1).await.map_err(internal)?;
        while let Some(next) = events.next().await {
            let (seq, event) = next.map_err(internal)?;
            // The atom's own predicate decides what is a dead letter, so
            // this scan and the listing cannot come to disagree about
            // which events the operator is choosing between.
            if let Some(dead) = DeadLetter::from_event(&event)
                && trigger_seq.is_none_or(|want| dead.trigger_stream_seq == Some(want))
            {
                // Later replaces earlier: the stream runs oldest-first
                // and the selection is the newest match, which is what
                // the listing leads with and what "the most recent dead
                // letter" means.
                selected = Some(event);
            }
            if seq >= tip {
                break;
            }
        }
    }
    selected.ok_or_else(|| match trigger_seq {
        Some(seq) => WireError::NotFound {
            op: OP.into(),
            message: format!(
                "no dead letter for agent `{agent}` with trigger sequence {seq} — \
                 `fq dead-letters list` shows the sequences this agent has"
            ),
        },
        None => WireError::NotFound {
            op: OP.into(),
            message: format!("no dead-lettered triggers for agent `{agent}`"),
        },
    })
}

/// The refusal for a dead letter that names no trigger.
///
/// `Unlocatable` and not `InvalidInput`, because the request was right:
/// this dead letter exists, it lists, and the operator selected it
/// correctly. What is missing is the identity that would make a requeue
/// idempotent, and no edit to the request can supply one — so a verdict
/// that invites an edit would send them in a circle.
fn unnamed(event: &Event) -> WireError {
    WireError::Unlocatable {
        op: OP.into(),
        message: format!(
            "the dead letter recorded by event `{}` names no trigger, so it cannot be requeued \
             idempotently — there would be nothing to refuse a second attempt on, and requeueing \
             it twice would run the agent twice. Two dead letters are like this and neither is a \
             defect: one recorded before triggers were named, and one the advisory path built \
             after the original had already aged off the trigger stream, which reads the \
             identity and never invents one. Its payload is still on the record \
             (`dead_letter.list`), so re-running it is `trigger.publish` — new work, named \
             honestly as new work.",
            event.envelope.event_id
        ),
    }
}

/// The refusal for a dead letter that has already been requeued —
/// carrying the reference that makes it useful.
async fn already_requeued(projection: &ProjectionStore, original: &Trigger) -> WireError {
    match projection.requeue_of(&original.id.to_string()).await {
        Ok(Some(existing)) => WireError::Conflict {
            op: OP.into(),
            message: format!(
                "trigger `{}` has already been requeued, as trigger `{existing}` — nothing was \
                 published. Read it back with `trigger.get {{\"trigger_id\": \"{existing}\"}}`; \
                 its `requeued_from` names this one. A dead letter is requeued at most once on \
                 purpose: the second call is usually a caller unsure whether the first landed, \
                 and the answer to that is the name of what it made. To run the work again \
                 anyway, publish it as new work with `trigger.publish`.",
                original.id
            ),
        },
        // The claim was refused and yet nothing holds it: not a state
        // this can produce, so it is reported as the fault it is
        // rather than retried into a second publish.
        Ok(None) => internal(format!(
            "requeue of trigger `{}` was refused but no requeue of it is recorded",
            original.id
        )),
        Err(e) => internal(e),
    }
}

/// The failure of the publish itself, after the claim was released.
fn publish_failed(err: fq_runtime::bus::BusError, original: &Trigger) -> WireError {
    match err {
        // The payload is the dead letter's, not the caller's, so the
        // message says whose it is: a trigger accepted before the limit
        // existed can be too large to republish under it, and no edit
        // to this request changes that.
        fq_runtime::bus::BusError::TriggerPayloadTooLarge { size, limit } => {
            WireError::InvalidInput {
                op: OP.into(),
                message: format!(
                    "the payload recorded for trigger `{}` is {size} bytes, over the {limit}-byte \
                     limit on an accepted trigger, so it cannot be republished. Nothing was \
                     published and nothing was recorded.",
                    original.id
                ),
            }
        }
        other => internal(format!(
            "failed to republish trigger `{}`: {other}",
            original.id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bounds and guarantees a caller has to be able to read off
    /// the published surface rather than discover by being refused.
    /// Drift between what a command promises and what it does is
    /// exactly how a surface comes to say less than it means.
    #[test]
    fn the_surface_declares_the_guarantee_it_keeps() {
        let decl = requeue_declaration();
        for claim in [
            // The guarantee itself, and the fact that the refusal is
            // useful rather than a dead end.
            "IDEMPOTENT",
            "Conflict",
            "trigger.get",
            "requeued_from",
            // The receipt's domain and shape.
            "trigger_id",
            // The refusal a caller cannot otherwise anticipate.
            "Unlocatable",
            "trigger.publish",
        ] {
            assert!(
                decl.description.contains(claim),
                "the declared contract must mention `{claim}`; got {:?}",
                decl.description
            );
        }
        // Declared under the domain it selects from, authorised over
        // the domain it writes to. Both, because they differ.
        assert_eq!(decl.domain, fq_ops::Domain::DeadLetter);
        assert_eq!(
            decl.authority,
            fq_ops::Authority {
                verb: fq_ops::Verb::Write,
                scope: fq_ops::Domain::Trigger,
            }
        );
        assert_eq!(decl.op().to_string(), OP);
    }

    /// **The receipt's key is the key `trigger.get` takes.** The whole
    /// point of naming a Trigger rather than inventing a dead-letter
    /// reference is that the caller can follow it, so the shape is
    /// asserted against the atom's declared key rather than against a
    /// hand-written expectation.
    #[test]
    fn the_receipt_key_is_the_shape_trigger_get_accepts() {
        let id = uuid::Uuid::now_v7();
        let receipt = fq_ops::Receipt::naming(
            fq_ops::Domain::Trigger,
            serde_json::json!(TriggerKey {
                trigger_id: id.to_string()
            }),
        );
        let atom = receipt.atoms.first().expect("one named atom");
        assert_eq!(atom.domain, fq_ops::Domain::Trigger);
        let parsed: TriggerKey =
            serde_json::from_value(atom.key.clone()).expect("`trigger.get` accepts the key");
        assert_eq!(parsed.trigger_id, id.to_string());
        // No watermark: the record is already there, so there is
        // nothing for a caller to wait on — and a wrong number here
        // would let them believe they had waited.
        assert!(receipt.watermarks.is_empty());
        assert_eq!(receipt.watermark(fq_ops::Domain::Trigger), None);
    }

    /// A requeue is a *new* trigger carrying the same work and a record
    /// of where it came from. Republishing under the original's id was
    /// the obvious shape and is the one that cannot work: the row is
    /// keyed on the identity, so it would write nothing and leave no
    /// record for a second attempt to fail on.
    #[test]
    fn a_requeue_is_a_new_trigger_that_remembers_the_old_one() {
        let original = Trigger::mint(
            fq_runtime::events::TriggerSource::Subject,
            Some("fq.trigger.researcher".into()),
            serde_json::json!({"task": "look at #12"}),
        );
        let requeued = Trigger::requeue_of(&original, "fq.trigger.researcher".into());
        assert_ne!(requeued.id, original.id, "a requeue is a later trigger");
        assert!(requeued.id > original.id, "and UUIDv7 order says so");
        assert_eq!(requeued.requeued_from, Some(original.id));
        assert_eq!(
            requeued.payload, original.payload,
            "the same work, or it is not a requeue"
        );
        // The original is untouched, and carries no lineage of its own.
        assert_eq!(original.requeued_from, None);
    }
}
