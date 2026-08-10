//! `trigger.publish`, daemon-side (plan Phase 4, verb 6): dispatching
//! work to an agent, as a declared command on the authenticated edge.
//!
//! The client used to connect to the broker and publish
//! `fq.trigger.<agent>` itself. That made every operator a NATS
//! publisher — credentials, subject vocabulary and stream layout all in
//! the thin client — for a fact the daemon already owns. Now the daemon
//! publishes and the client asks it to.
//!
//! **The receipt is empty because a trigger has no identity.** The domain
//! model's verb table says this one "references the appended trigger
//! atom"; there is no such atom to reference. `Domain::Trigger` carries
//! verbs and an authority scope, not a catalogue entry, so there is no
//! `trigger.get` and no key one would take — and the thing that was
//! appended cannot supply one either. The trigger wire contract
//! (`docs/design/committed/trigger-wire-contract.md`) makes the message
//! body *the payload itself*, a single opaque JSON value that external
//! publishers like the github-watcher adapter write directly, so nothing
//! in it is an id; `EventBus::publish_trigger` hands back only the
//! JetStream ack sequence, which is a position in a log rather than a
//! name for a thing ([`fq_ops::AtomRef`]).
//!
//! So the receipt is empty rather than carrying that position. Putting a
//! sequence in an `AtomRef.key` is exactly the mistake the receipt
//! refactor removed, and minting a trigger id here would be a
//! wire-contract change with external consumers — **that decision is
//! being taken separately and is deliberately not taken here.** What the
//! command says is what is true: it did something, and there is nothing
//! to point a caller at yet. The invocation the trigger becomes *is*
//! nameable, and appears under `invocation.list` once the dispatcher
//! picks it up.

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
                bus.publish_trigger(agent.as_str(), &input.payload)
                    .await
                    .map_err(|e| WireError::Internal {
                        message: format!("failed to publish trigger for `{agent}`: {e}"),
                    })?;
                Ok(fq_ops::Receipt::empty())
            }
        })
        .map_err(|e| anyhow::anyhow!("operator registry: {e}"))?;

    Ok(())
}
