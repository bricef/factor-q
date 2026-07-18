
/// Publish a trigger to NATS instead of running the executor
/// in-process. A running `fq run` daemon picks up the trigger and
/// dispatches it to the named agent.
async fn publish_trigger(
    global: &GlobalArgs,
    agent_name: &str,
    payload: Option<&str>,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;

    // Validate the agent id format locally before going to NATS.
    // This catches typos early without round-tripping through the
    // cluster.
    fq_runtime::AgentId::new(agent_name)
        .with_context(|| format!("invalid agent name '{agent_name}'"))?;

    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    let trigger_payload: Value = match payload {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
        None => Value::Null,
    };

    bus.publish_trigger(agent_name, &trigger_payload)
        .await
        .with_context(|| format!("failed to publish trigger for '{agent_name}'"))?;

    println!(
        "Published trigger for '{}' on {}",
        agent_name,
        fq_runtime::bus::trigger_subject(agent_name)
    );
    println!("A running `fq run` daemon will pick this up and dispatch it.");
    Ok(())
}

/// `fq dead-letters list`: dead-lettered triggers, newest first,
/// scanned from the event stream (the projection stores no
/// annotations, so this — unlike `fq events query` — needs NATS).
async fn list_dead_letters(
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
async fn requeue_dead_letter(
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
fn truncate_json(value: &serde_json::Value, max: usize) -> String {
    let s = value.to_string();
    if s.len() > max {
        format!("{}…", &s[..s.floor_char_boundary(max)])
    } else {
        s
    }
}

/// Query the SQLite projection for events matching the given
/// filters. Read-only — does not require the projector to be
/// currently running, only that it has been run at some point.
/// Open the read-only `Views` handle every CLI read command formats over
/// (the CLI is a formatter over `fq_runtime::views`, not a read layer of
/// its own — see the operator-dashboard plan, layer 1).
async fn open_views(global: &GlobalArgs) -> anyhow::Result<Views> {
    let config = global.resolve_config()?;
    let db_paths = runtime_db_paths(&config);
    // Read commands never mutate the state directory, so a v1
    // single-file layout is surfaced as a hint rather than migrated
    // here — exactly the writable paths run the split.
    let legacy = fq_runtime::db::legacy_db_path(&config.cache.directory);
    if !db_paths.all_exist() && legacy.exists() {
        anyhow::bail!(
            "found legacy single-file database at {}: run `fq run` (or any writing \
             command) once to migrate to the per-store layout",
            legacy.display()
        );
    }
    Views::open(&db_paths).await.with_context(|| {
        format!(
            "failed to open stores under {}: has `fq run` been started?",
            config.cache.directory.display()
        )
    })
}

async fn query_events(
    global: &GlobalArgs,
    agent: Option<&str>,
    event_type: Option<&str>,
    since: Option<&str>,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let views = open_views(global).await?;
    let rows = views.events(agent, event_type, since, limit).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No events matched.");
        return Ok(());
    }

    println!(
        "{:<20} {:<40} {:<14} {:<12} invocation",
        "timestamp", "agent", "event", "cost"
    );
    for row in rows {
        let ts = row.timestamp.get(..19).unwrap_or(&row.timestamp);
        let inv_short: String = row.invocation_id.chars().take(8).collect();
        let cost = row
            .total_cost
            .map(|c| format!("${c:.6}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<40} {:<14} {:<12} {}",
            ts, row.agent_id, row.event_type, cost, inv_short
        );
    }
    Ok(())
}

/// Show per-agent cost totals from the SQLite projection.
async fn show_costs(
    global: &GlobalArgs,
    agent: Option<&str>,
    since: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let views = open_views(global).await?;
    let report = views.costs(agent, since, false).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if report.agents.is_empty() {
        println!("No cost events recorded.");
        return Ok(());
    }

    println!(
        "{:<30} {:<10} {:<14} {:<14} {:<14} {:<14} total_cost",
        "agent", "events", "input_tokens", "output_tokens", "cache_read", "cache_write"
    );
    for row in &report.agents {
        println!(
            "{:<30} {:<10} {:<14} {:<14} {:<14} {:<14} ${:.6}",
            row.agent_id,
            row.event_count,
            row.total_input_tokens,
            row.total_output_tokens,
            row.total_cache_read_tokens,
            row.total_cache_write_tokens,
            row.total_cost
        );
    }
    println!();
    println!("Total across all agents: ${:.6}", report.total_cost);
    Ok(())
}

/// Collapse `.` and `..` components from a path without touching the
/// filesystem. Used to produce a clean display path for error messages
/// when `canonicalize` is not an option (path may not exist).
fn normalise(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ============================================================
// fq invocation subcommand
// ============================================================

