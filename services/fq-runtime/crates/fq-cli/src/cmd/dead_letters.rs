use crate::*;

/// `fq dead-letters list`: dead-lettered triggers, newest first,
/// scanned from the event stream (the projection stores no
/// annotations, so this — unlike `fq events query` — needs NATS).
pub(crate) async fn list_dead_letters(
    global: &GlobalArgs,
    agent: Option<&str>,
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
    let dead = fq_runtime::control_plane::operator::list_dead_letters(&bus, agent, limit).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&dead)?);
        return Ok(());
    }
    if dead.is_empty() {
        println!("No dead-lettered triggers.");
        return Ok(());
    }
    println!("{} dead-lettered trigger(s), newest first:\n", dead.len());
    for d in &dead {
        let seq = d
            .trigger_stream_seq
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {}  {}  seq={} via {}",
            d.timestamp.format("%Y-%m-%d %H:%M:%S"),
            d.agent_id,
            seq,
            d.source,
        );
        println!("      {}", d.error_message);
        println!("      payload: {}", truncate_json(&d.trigger_payload, 120));
    }
    println!("\n-> `fq dead-letters requeue <agent> [--trigger-seq N]` to re-run one");
    Ok(())
}

/// `fq dead-letters requeue`: re-publish a dead-lettered trigger as a
/// fresh trigger. Not idempotent — see the command help.
pub(crate) async fn requeue_dead_letter(
    global: &GlobalArgs,
    agent: &str,
    trigger_seq: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    fq_runtime::AgentId::new(agent).with_context(|| format!("invalid agent name '{agent}'"))?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
    let result =
        fq_runtime::control_plane::operator::requeue_dead_letter(&bus, agent, trigger_seq).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "Requeued dead-lettered trigger for '{}' as trigger seq {} (from event {}).",
        result.agent_id, result.new_trigger_seq, result.source_event_id
    );
    println!("  payload: {}", truncate_json(&result.trigger_payload, 120));
    println!("A running `fq run` daemon will pick this up with a fresh delivery budget.");
    Ok(())
}

/// Compact single-line JSON, truncated for terminal display.
pub(crate) fn truncate_json(value: &serde_json::Value, max: usize) -> String {
    let s = value.to_string();
    if s.len() > max {
        format!("{}…", &s[..s.floor_char_boundary(max)])
    } else {
        s
    }
}
