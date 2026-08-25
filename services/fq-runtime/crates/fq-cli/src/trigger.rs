//! The `fq trigger` verb: hand work to the daemon (plan Phase 4, verb 6).
//!
//! **The in-process mode is gone (decision D-1).** `fq trigger` used to
//! default to running the reducer *in this process*: it loaded the agent
//! registry off the caller's disk, opened and wrote the worker WAL,
//! started the agent's MCP servers as children of the CLI, loaded and
//! validated the pricing table, and talked to the provider directly. That
//! is a second execution path with a second set of answers — a client
//! could run an agent definition the daemon had never loaded, against
//! pricing the daemon had never validated, writing a WAL the daemon was
//! also writing — and it is exactly what ADR-0031's thin client cannot
//! contain. One runtime, one place work runs.
//!
//! What is left is the request: the daemon publishes the trigger onto the
//! durable stream and its dispatcher picks it up. The daemon half is
//! `trigger_command.rs`.

use anyhow::Context;
use serde_json::Value;

use crate::cli::GlobalArgs;
use crate::edge_call::edge_invoke;

/// Ask the daemon to dispatch a trigger to an agent.
pub(crate) async fn publish_trigger(
    global: &GlobalArgs,
    agent_name: &str,
    payload: Option<&str>,
) -> anyhow::Result<()> {
    // Validate the agent id's *shape* locally before dialling: a typo is
    // answered offline, in the same breath as `fq agent validate`, rather
    // than costing a round trip. Whether the agent exists is the daemon's
    // question and is deliberately not asked here — a trigger for an
    // unknown agent is a dead letter, which is a durable record an
    // operator can find, not a client-side refusal that leaves none.
    fq_ops::agent::AgentId::new(agent_name)
        .with_context(|| format!("invalid agent name '{agent_name}'"))?;

    // A payload that is not JSON is the string itself — the shorthand
    // that makes `fq trigger fixer "look at #12"` work.
    let trigger_payload: Value = match payload {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
        None => Value::Null,
    };

    edge_invoke(
        global,
        fq_ops::OpId::Verb(fq_ops::VerbId::Trigger(fq_ops::Trigger::Publish)),
        serde_json::json!({
            "agent_id": agent_name,
            "payload": trigger_payload,
        }),
    )
    .await?
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // The trigger is durable when the daemon answers, so this says what
    // happened rather than what was published where: the subject it rides
    // is the daemon's own transport, and a client that does not publish
    // has no business naming it.
    println!("Published trigger for '{agent_name}'.");
    println!("The daemon will pick this up and dispatch it.");
    Ok(())
}
