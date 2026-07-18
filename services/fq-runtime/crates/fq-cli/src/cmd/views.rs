use crate::*;

/// Query the SQLite projection for events matching the given
/// filters. Read-only — does not require the projector to be
/// currently running, only that it has been run at some point.
/// Open the read-only `Views` handle every CLI read command formats over
/// (the CLI is a formatter over `fq_runtime::views`, not a read layer of
/// its own — see the operator-dashboard plan, layer 1).
pub(crate) async fn open_views(global: &GlobalArgs) -> anyhow::Result<Views> {
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

pub(crate) async fn query_events(
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
pub(crate) async fn show_costs(
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
pub(crate) fn normalise(path: &Path) -> PathBuf {
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
