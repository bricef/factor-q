//! `fq status`: the health overview — NATS and its streams, the per-store
//! database paths, and the recovery guidance block.
//!
//! Split out of `lib.rs` (#189). Deliberately does not try to detect whether a
//! daemon is running; that would need a pidfile or a heartbeat event, both
//! more surface area than a status command should own.

use std::path::PathBuf;

use fq_runtime::control_plane::coordination_consumer::DEFAULT_STALE_THRESHOLD_MS;
use fq_runtime::views::Views;

use crate::cli::GlobalArgs;
use crate::runtime_db_paths;

/// Operational status of the runtime, derived from three sources:
///
/// 1. The NATS JetStream API (via the async-nats client that the
///    EventBus already uses) — gives stream message counts, byte
///    totals, and consumer positions.
/// 2. The SQLite projection — row count and latest projected
///    timestamp, so we can compare against NATS and surface
///    projection lag directly.
/// 3. Local config paths that we expect to be present.
///
/// Deliberately does not try to detect whether a `fq run` daemon
/// is currently running; that would need a pidfile or a heartbeat
/// event, both of which are more surface area than a status
/// command should own. Operators can use `ps`/`systemctl` for
/// process state.
/// The `fq status --json` shape: config echoes, the typed stream probe
/// ([`fq_runtime::health`]), and the DB-backed counts. `nats_connected`
/// is always true in an emitted report — an unreachable broker fails
/// the command, exactly like the human path.
#[derive(serde::Serialize)]
struct StatusReport {
    nats_url: String,
    agents_dir: PathBuf,
    cache_dir: PathBuf,
    nats_connected: bool,
    streams: Vec<fq_runtime::health::StreamHealth>,
    worker_path: PathBuf,
    control_plane_path: PathBuf,
    projection_path: PathBuf,
    /// A v1 single-file `events.db` still awaiting the split
    /// migration (`fq run` performs it).
    legacy_events_db: Option<PathBuf>,
    initialised: bool,
    projection_rows: Option<i64>,
    recovery: Option<fq_runtime::views::RecoveryView>,
    /// First store-side failure, when any (rows/recovery unreadable).
    store_error: Option<String>,
}

pub(crate) async fn show_status(global: &GlobalArgs, json: bool) -> anyhow::Result<()> {
    use async_nats::jetstream;
    use fq_runtime::health;

    let config = global.resolve_config()?;

    if !json {
        println!("factor-q status");
        println!();
        println!("Config");
        println!("  NATS URL:         {}", config.nats.url);
        println!("  agents dir:       {}", config.agents.directory.display());
        println!("  cache dir:        {}", config.cache.directory.display());

        // NATS.
        println!();
        println!("NATS");
    }
    let client = match fq_runtime::bus::connect_with_url_credentials(&config.nats.url).await {
        Ok(c) => {
            if !json {
                println!("  connection:       ✓ connected at {}", config.nats.url);
            }
            c
        }
        Err(err) => {
            if !json {
                println!("  connection:       ✗ failed: {err}");
            }
            anyhow::bail!("cannot reach NATS at {}", config.nats.url);
        }
    };
    let js = jetstream::new(client);
    let streams = health::probe_core_streams(&js).await;
    let db_paths = runtime_db_paths(&config);
    let legacy = fq_runtime::db::legacy_db_path(&config.cache.directory);

    if json {
        let initialised = db_paths.all_exist();
        let mut projection_rows = None;
        let mut recovery = None;
        let mut store_error = None;
        if initialised {
            match Views::open(&db_paths).await {
                Ok(views) => {
                    match views.event_count().await {
                        Ok(count) => projection_rows = Some(count),
                        Err(err) => store_error = Some(format!("failed to query rows: {err}")),
                    }
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    match views.recovery(now_ms, DEFAULT_STALE_THRESHOLD_MS).await {
                        Ok(r) => recovery = Some(r),
                        Err(err) => {
                            store_error
                                .get_or_insert(format!("failed to read recovery state: {err}"));
                        }
                    }
                }
                Err(err) => store_error = Some(format!("failed to open: {err}")),
            }
        }
        let report = StatusReport {
            nats_url: config.nats.url.clone(),
            agents_dir: config.agents.directory.clone(),
            cache_dir: config.cache.directory.clone(),
            nats_connected: true,
            streams,
            worker_path: db_paths.worker.clone(),
            control_plane_path: db_paths.control_plane.clone(),
            projection_path: db_paths.projection.clone(),
            legacy_events_db: legacy.exists().then_some(legacy),
            initialised,
            projection_rows,
            recovery,
            store_error,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    for stream in &streams {
        render_stream_health_human(stream);
    }

    // Stores + recovery state, over the read views.
    println!();
    println!("Stores");
    println!("  worker db:        {}", db_paths.worker.display());
    println!("  control-plane db: {}", db_paths.control_plane.display());
    println!("  projection db:    {}", db_paths.projection.display());
    if legacy.exists() {
        println!(
            "  legacy events.db: {} (pending split — run `fq run` to migrate)",
            legacy.display()
        );
    }
    if !db_paths.all_exist() {
        println!("  state:            not initialised (run `fq run` to create)");
        println!();
        println!("Recovery state");
        println!("  (no coordination data — `fq run` has not initialised the store)");
        return Ok(());
    }
    match Views::open(&db_paths).await {
        Ok(views) => {
            match views.event_count().await {
                Ok(count) => println!("  projection rows:  {count}"),
                Err(err) => println!("  projection rows:  ✗ failed to query: {err}"),
            }

            // Recovery state (step 9). Points the operator at the
            // commands they'd need if anything is off; renders
            // "All clear." otherwise.
            println!();
            println!("Recovery state");
            let now_ms = chrono::Utc::now().timestamp_millis();
            match views.recovery(now_ms, DEFAULT_STALE_THRESHOLD_MS).await {
                Ok(r) => print!("{}", render_recovery_guidance(r.ambiguous, r.stale_workers)),
                Err(err) => println!("  ✗ failed to read recovery state: {err}"),
            }
        }
        Err(err) => {
            println!("  state:            ✗ failed to open: {err}");
        }
    }
    Ok(())
}

/// Render one probed stream exactly as `fq status` always has — the
/// probe data is typed now ([`fq_runtime::health`], #105 layer 2) but
/// the human output is unchanged.
fn render_stream_health_human(health: &fq_runtime::health::StreamHealth) {
    use fq_runtime::health::{ConsumerHealth, StreamHealth};

    println!();
    println!("Stream: {}", health.stream());
    match health {
        StreamHealth::Unavailable { error, .. } => {
            println!("  state:            ✗ {error}");
        }
        StreamHealth::Available {
            messages,
            bytes,
            first_seq,
            last_seq,
            consumer,
            ..
        } => {
            println!("  messages:         {messages}");
            println!(
                "  bytes:            {}",
                fq_tools::builtin::exec::human_bytes(*bytes)
            );
            println!("  first seq:        {first_seq}");
            println!("  last seq:         {last_seq}");
            match consumer {
                ConsumerHealth::Active {
                    name,
                    delivered,
                    lag,
                    ack_pending,
                    num_pending,
                    num_redelivered,
                } => {
                    let status = if *lag == 0 {
                        "✓ caught up"
                    } else if *lag < 10 {
                        "◐ slightly behind"
                    } else {
                        "✗ lagging"
                    };
                    println!("  consumer {name}: {status} (delivered {delivered}, lag {lag})");
                    if *ack_pending > 0 {
                        println!("    ack pending:    {ack_pending}");
                    }
                    if *num_pending > 0 {
                        println!("    num pending:    {num_pending}");
                    }
                    if *num_redelivered > 0 {
                        println!(
                            "    redelivered:    {num_redelivered} (retrying; bound {})",
                            fq_runtime::bus::TRIGGER_MAX_DELIVER
                        );
                    }
                }
                ConsumerHealth::Error { name, error } => {
                    println!("  consumer {name}: ✗ info failed: {error}");
                }
                ConsumerHealth::Missing { name } => {
                    println!("  consumer {name}: not present (no `fq run` has initialised it)");
                }
            }
        }
    }
}

/// Pure: render the recovery-guidance block of `fq status`
/// from two counts. The text includes the next-step commands
/// so the operator can copy-paste rather than remember syntax.
fn render_recovery_guidance(ambiguous_count: i64, stale_worker_count: i64) -> String {
    if ambiguous_count == 0 && stale_worker_count == 0 {
        return "  All clear.\n".to_string();
    }
    let mut out = String::new();
    if ambiguous_count > 0 {
        out.push_str(&format!(
            "  Ambiguous invocations: {ambiguous_count}\n\
             \x20\x20  -> `fq invocation list --status=ambiguous` to inspect\n\
             \x20\x20  -> `fq invocation drop <id>` to triage individually\n"
        ));
    }
    if stale_worker_count > 0 {
        out.push_str(&format!(
            "  Stale workers: {stale_worker_count}\n\
             \x20\x20  -> `fq workers list --stale-only` to inspect\n\
             \x20\x20  -> `fq workers prune` to remove them (`--dry-run` to preview)\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests;
