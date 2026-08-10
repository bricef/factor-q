//! `trigger.publish`, daemon-side (plan Phase 4, verb 6): dispatching
//! work to an agent, as a declared command on the authenticated edge.
//!
//! The client used to connect to the broker and publish
//! `fq.trigger.<agent>` itself. That made every operator a NATS
//! publisher — credentials, subject vocabulary and stream layout all in
//! the thin client — for a fact the daemon already owns. Now the daemon
//! publishes and the client asks it to.
//!
//! **The receipt is empty because a trigger has no atom yet — not
//! because it has no identity.** That identity now exists: the daemon
//! mints a UUIDv7 as it publishes and it rides the `Fq-Trigger-Id`
//! header beside the body, so `EventBus::publish_trigger` hands back a
//! [`fq_runtime::PublishedTrigger`] naming what was queued (the body
//! itself stays exactly what the wire contract says it is — one opaque
//! JSON value external publishers write directly).
//!
//! What is still missing is the *atom*. The domain model's verb table
//! says this command "references the appended trigger atom"; there is
//! no such atom to reference. `Domain::Trigger` carries verbs and an
//! authority scope, not a catalogue entry, so there is no `trigger.get`
//! — and an `AtomRef.key` must be the key that domain's Get takes.
//! Naming a trigger in a receipt before anything can resolve that name
//! would hand a caller a reference it cannot follow, so the receipt
//! stays empty until Trigger is declared. What the command says is what
//! is true: it did something, and there is nothing to point a caller at
//! yet. The invocation the trigger becomes *is* nameable, and appears
//! under `invocation.list` once the dispatcher picks it up.

use fq_edge::wire::WireError;

/// The typed input of `trigger.publish` on the wire.
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct PublishCommandInput {
    agent_id: String,
    /// The trigger body, verbatim. Any JSON: agents receive it as the
    /// user message their run opens with.
    #[serde(default)]
    payload: serde_json::Value,
}

/// Register `trigger.publish` on the daemon's edge.
pub(crate) fn register_trigger_command(
    registry: &mut fq_edge::EdgeRegistry,
    bus: fq_runtime::EventBus,
) -> anyhow::Result<()> {
    let decl = fq_ops::Command::new::<PublishCommandInput>(
        fq_ops::Trigger::Publish,
        fq_ops::Authority {
            verb: fq_ops::Verb::Write,
            scope: fq_ops::Domain::Trigger,
        },
        "Dispatch a trigger to an agent via the durable trigger stream.",
        fq_ops::Stability::Experimental,
    )
    .description(
        "At-least-once delivery with a bounded budget: the trigger is durable \
         when this answers, and a delivery that keeps failing is dead-lettered \
         (`fq dead-letters list`) rather than retried forever. The answer means \
         accepted, not run — an agent this daemon does not have is a dead letter, \
         not a refusal here. Appends no atom: a queued trigger is consumed rather \
         than kept, so there is nothing to name; the invocation it becomes is.",
    );
    registry
        .command::<PublishCommandInput, _, _>(decl, move |input: PublishCommandInput| {
            let bus = bus.clone();
            async move {
                // The daemon validates what it is asked to publish: an
                // id the subject grammar cannot carry must never reach
                // the broker, and the client's own check is a courtesy
                // ahead of this one, not a substitute for it.
                let agent = fq_runtime::AgentId::new(&input.agent_id).map_err(|e| {
                    WireError::InvalidInput {
                        op: "trigger.publish".into(),
                        message: format!("invalid agent name `{}`: {e}", input.agent_id),
                    }
                })?;
                let published = bus
                    .publish_trigger(agent.as_str(), &input.payload)
                    .await
                    .map_err(|e| WireError::Internal {
                        message: format!("failed to publish trigger for `{agent}`: {e}"),
                    })?;
                // The trigger now has a name — `published.id`, minted by
                // the publish and on the message as a header. It goes in
                // the receipt's `AtomRef` the moment there is a
                // `trigger.get` to resolve it against, and not before.
                tracing::info!(
                    agent_id = %agent,
                    trigger_id = %published.id,
                    stream_seq = published.stream_seq,
                    "published trigger"
                );
                Ok(fq_ops::Receipt::empty())
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}
