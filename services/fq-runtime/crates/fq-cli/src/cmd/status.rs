use crate::*;

/// Per-store SQLite database paths under the configured cache
/// directory (the #262 split layout: `worker.db`, `control-plane.db`,
/// `projection.db`). Stored next to the pricing JSON rather than in
/// their own subdirectory.
pub(crate) fn runtime_db_paths(config: &Config) -> fq_runtime::RuntimeDbPaths {
    fq_runtime::RuntimeDbPaths::under(&config.cache.directory)
}

/// Migrate a leftover v1 single-file `events.db` into the split
/// layout, then hand back the per-store paths. Every command that
/// opens a store for *writing* calls this first; read-only commands
/// never mutate the state directory and surface a "run `fq run`"
/// hint instead (see `open_views`).
pub(crate) async fn ensure_split_dbs(
    config: &Config,
) -> anyhow::Result<fq_runtime::RuntimeDbPaths> {
    match fq_runtime::split_legacy_events_db(&config.cache.directory).await? {
        fq_runtime::SplitOutcome::Completed(stats) => {
            println!(
                "migrated legacy events.db into worker.db + control-plane.db + projection.db \
                 ({stats}); events.db.pre-split kept as rollback"
            );
        }
        fq_runtime::SplitOutcome::NotNeeded => {}
    }
    Ok(runtime_db_paths(config))
}

/// Best-effort host label for the worker registration row.
/// Operator-informational only — the value isn't load-bearing
/// in v1 and a placeholder is fine when no hostname is
/// available. v2 will likely prefer a syscall-backed lookup.
pub(crate) fn local_host_label() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "local".to_string())
}

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
                    match views.recovery(now_ms, 30_000).await {
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
            match views.recovery(now_ms, 30_000).await {
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
pub(crate) fn render_stream_health_human(health: &fq_runtime::health::StreamHealth) {
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
            println!("  bytes:            {}", human_bytes(*bytes));
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
pub(crate) fn render_recovery_guidance(ambiguous_count: i64, stale_worker_count: i64) -> String {
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

// ============================================================
// fq doctor — one-shot durable-execution health report
// ============================================================

/// Stuck-work threshold: an in-flight invocation whose
/// `invocation_state.updated_at` is older than this many ms is
/// flagged "stuck" by `fq doctor`. Reuses the control-plane's
/// stale-worker value (`DEFAULT_STALE_THRESHOLD_MS = 30_000`,
/// `coordination_consumer.rs:66`) rather than inventing a third
/// hard-coded constant — an invocation that has not touched its
/// WAL row in as long as a worker has not heartbeated is the same
/// order of "not making progress" signal.
const DOCTOR_STUCK_THRESHOLD_MS: i64 = 30_000;

/// Worker liveness counts plus the ids of any stale workers so
/// the operator can act without a second `fq workers list` call.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DoctorWorkers {
    pub(crate) alive: i64,
    pub(crate) stale: i64,
    pub(crate) shutdown: i64,
    /// Worker ids currently past the stale threshold.
    pub(crate) stale_ids: Vec<String>,
}

/// In-flight / current-execution view, read from the worker-local
/// `invocation_state` table (the reliable live view — the CP owner
/// table's `in_flight` status is not populated by trigger dispatch
/// yet; see issue #50).
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DoctorExecutions {
    pub(crate) in_flight: i64,
    /// In-flight invocations with a fresh open dispatch (tool or LLM) —
    /// actively working, however silent their WAL row (#130).
    pub(crate) working: i64,
    /// Short ids of the working invocations, same convention as
    /// `stuck_ids`.
    pub(crate) working_ids: Vec<String>,
    /// In-flight invocations whose `updated_at` is older than
    /// [`DOCTOR_STUCK_THRESHOLD_MS`].
    pub(crate) stuck: i64,
    /// Short ids of the stuck invocations, for triage.
    pub(crate) stuck_ids: Vec<String>,
}

/// Availability of the dead-letter section. Gated on issue #49:
/// Dead-lettered triggers (#49): transient pre-WAL failures that
/// exhausted the trigger consumer's delivery bound. The dispatcher
/// consumes the exhausted trigger and emits a terminal `failed` event
/// with kind [`DEAD_LETTER_KIND`]; this counts that bucket, so the
/// report needs no extra query. The event's annotations carry the
/// trigger subject and payload for requeue/diagnosis.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorDeadLetters {
    pub(crate) exhausted_triggers: i64,
}

/// The projection's failure-kind string for a dead-lettered trigger —
/// `FailureKind::TriggerExhausted` serialized with the wire vocabulary.
const DEAD_LETTER_KIND: &str = "trigger_exhausted";

/// The full doctor report. Serialisable for `--json`; built by the
/// pure [`build_doctor_report`] so the checks are unit-testable
/// without a live DB.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorReport {
    pub(crate) workers: DoctorWorkers,
    pub(crate) executions: DoctorExecutions,
    /// Ambiguous invocations needing operator triage (CP owner
    /// table, `status='ambiguous'`).
    pub(crate) ambiguous: i64,
    /// Terminal failures grouped by `FailureKind` (from the
    /// projection `events` table, `event_type='failed'`).
    pub(crate) failures: Vec<DoctorFailure>,
    pub(crate) dead_letters: DoctorDeadLetters,
}

/// One failure-kind bucket in the report. Mirrors
/// [`fq_runtime::views::FailureView`] but owns its data so the report
/// is a self-contained serialisable value.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorFailure {
    pub(crate) error_kind: String,
    pub(crate) count: i64,
}

impl DoctorReport {
    /// Total terminal failures across all kinds.
    pub(crate) fn failure_total(&self) -> i64 {
        self.failures.iter().map(|f| f.count).sum()
    }

    /// True when any check reports a problem worth an operator's
    /// attention: stale workers, stuck in-flight work, ambiguous
    /// invocations, or permanent failures. In-flight work that is
    /// merely running (not stuck) is healthy, not an issue.
    pub(crate) fn has_issues(&self) -> bool {
        self.workers.stale > 0
            || self.executions.stuck > 0
            || self.ambiguous > 0
            || self.failure_total() > 0
    }
}

/// Pure: assemble a [`DoctorReport`] from the already-fetched read
/// views, so it can be unit-tested without a database. The stuck
/// determination (threshold + clock-skew handling) lives in
/// [`fq_runtime::views::Views::executions`]; this builder only
/// aggregates and shortens ids for triage.
pub(crate) fn build_doctor_report(
    workers: &[fq_runtime::views::WorkerView],
    executions: &fq_runtime::views::ExecutionsView,
    ambiguous: i64,
    failures: &[fq_runtime::views::FailureView],
) -> DoctorReport {
    let mut w = DoctorWorkers::default();
    for row in workers {
        match row.status.as_str() {
            "alive" => w.alive += 1,
            "stale" => {
                w.stale += 1;
                w.stale_ids.push(row.worker_id.clone());
            }
            "shutdown" => w.shutdown += 1,
            // The control-plane only records the three statuses above;
            // an unknown value would mean a store/view drift — count it
            // as stale so it surfaces as an issue rather than vanishing.
            _ => {
                w.stale += 1;
                w.stale_ids.push(row.worker_id.clone());
            }
        }
    }

    // Short ids (8 chars) for triage, matching the human report.
    let short = |ids: &[String]| -> Vec<String> {
        ids.iter().map(|id| id.chars().take(8).collect()).collect()
    };
    let ex = DoctorExecutions {
        in_flight: executions.in_flight,
        working: executions.working,
        working_ids: short(&executions.working_ids),
        stuck: executions.stuck,
        stuck_ids: short(&executions.stuck_ids),
    };

    let failures: Vec<DoctorFailure> = failures
        .iter()
        .map(|f| DoctorFailure {
            error_kind: f.error_kind.clone(),
            count: f.count,
        })
        .collect();

    let dead_letters = DoctorDeadLetters {
        exhausted_triggers: failures
            .iter()
            .filter(|f| f.error_kind == DEAD_LETTER_KIND)
            .map(|f| f.count)
            .sum(),
    };

    DoctorReport {
        workers: w,
        executions: ex,
        ambiguous,
        failures,
        dead_letters,
    }
}

/// Pure: render the human-readable `fq doctor` report, mirroring
/// `render_recovery_guidance` — an overall verdict, then per-failing-
/// check the count plus the copy-paste next-step command. Returns
/// `All clear.` when every check is green (the dead-letter line is
/// always shown as pending #49 — it is informational, not a problem).
pub(crate) fn render_doctor_report_human(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str("factor-q doctor\n\n");

    // Verdict line.
    if report.has_issues() {
        out.push_str("Verdict: issues found — see below.\n\n");
    } else {
        out.push_str("Verdict: All clear.\n\n");
    }

    // Workers.
    out.push_str(&format!(
        "Workers: {} alive, {} stale, {} shutdown\n",
        report.workers.alive, report.workers.stale, report.workers.shutdown
    ));
    if report.workers.stale > 0 {
        out.push_str("  -> `fq workers list --stale-only` to inspect\n");
    }

    // Executions.
    out.push_str(&format!(
        "Current executions: {} in-flight ({} working, {} stuck)\n",
        report.executions.in_flight, report.executions.working, report.executions.stuck
    ));
    if report.executions.stuck > 0 {
        out.push_str(&format!(
            "  -> {} not advanced in >{}s: {}\n",
            report.executions.stuck,
            DOCTOR_STUCK_THRESHOLD_MS / 1000,
            report.executions.stuck_ids.join(", ")
        ));
        out.push_str(
            "  -> `fq invocation show <id>` to inspect, `fq invocation drop <id>` to triage\n",
        );
    }

    // Ambiguous.
    out.push_str(&format!("Ambiguous invocations: {}\n", report.ambiguous));
    if report.ambiguous > 0 {
        out.push_str("  -> `fq invocation list --status=ambiguous` to inspect\n");
        out.push_str("  -> `fq invocation drop <id>` to triage individually\n");
    }

    // Permanent failures.
    let failure_total = report.failure_total();
    out.push_str(&format!("Permanent failures: {failure_total}\n"));
    if failure_total > 0 {
        for f in &report.failures {
            out.push_str(&format!("  {}: {}\n", f.error_kind, f.count));
        }
        out.push_str("  -> `fq invocation list --status=failed` to inspect\n");
    }

    // Dead-letters (#49): exhausted triggers the dispatcher consumed.
    if report.dead_letters.exhausted_triggers > 0 {
        out.push_str(&format!(
            "Dead-letters: {} exhausted trigger(s)\n",
            report.dead_letters.exhausted_triggers
        ));
        out.push_str(
            "  -> `fq dead-letters list` to inspect; `fq dead-letters requeue <agent>` to re-run\n",
        );
    } else {
        out.push_str("Dead-letters: none\n");
    }

    out
}

/// `fq doctor`: aggregate the DB-backed durable-execution health
/// signals into one report. Read-only against the SQLite projection
/// DB — no NATS round-trip — so it works with `fq run` stopped.
///
/// Opens the three read-only stores against the single projection DB
/// file, reads each check's source, then hands the raw rows to the
/// pure [`build_doctor_report`] / [`render_doctor_report_human`] so
/// the aggregation and formatting stay testable.
pub(crate) async fn doctor(
    global: &GlobalArgs,
    json: bool,
    fail_on_issues: bool,
) -> anyhow::Result<()> {
    let views = open_views(global).await?;
    let now_ms = chrono::Utc::now().timestamp_millis();

    let workers = views.workers().await?;
    let executions = views
        .executions(
            now_ms,
            DOCTOR_STUCK_THRESHOLD_MS,
            fq_runtime::views::DEFAULT_LONG_DISPATCH_THRESHOLD_MS,
        )
        .await?;
    let ambiguous = views
        .recovery(now_ms, DOCTOR_STUCK_THRESHOLD_MS)
        .await?
        .ambiguous;
    let failures = views.failures().await?;

    let report = build_doctor_report(&workers, &executions, ambiguous, &failures);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_doctor_report_human(&report));
    }

    if fail_on_issues && report.has_issues() {
        // Opt-in non-zero exit for `&&` health-gates and cron. The
        // anyhow error path already maps to ExitCode::FAILURE in main.
        anyhow::bail!("doctor found issues (see report above)");
    }
    Ok(())
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Publish `invocation.ambiguous` at most once per invocation (#64).
///
/// Claims the worker store's one-shot stamp (`ambiguous_reported_at`)
/// before publishing, so a restart that re-classifies the same
/// invocation as ambiguous — or re-fails the same resume — does not
/// re-fire the event. `stamp_key` is the store's invocation-id string
/// (normally equal to `invocation_id`; the scan path passes the raw
/// row id so a malformed stored uuid still stamps its own row).
///
/// Claim-then-publish is deliberately at-most-once: a publish failure
/// after a successful claim is logged and not retried. A claim *error*
/// (store unavailable) publishes anyway — it doesn't prove the event
/// was already sent, and a possible duplicate beats re-silencing the
/// failure mode #64 exists to make loud.
pub(crate) async fn publish_ambiguous_once(
    worker_store: &fq_runtime::WorkerStore,
    bus: &EventBus,
    agent_id: AgentId,
    invocation_id: Uuid,
    stamp_key: &str,
    payload: fq_runtime::events::InvocationAmbiguousPayload,
) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    match worker_store
        .mark_ambiguous_reported(stamp_key, now_ms)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                invocation_id = %invocation_id,
                "invocation.ambiguous already reported for this invocation; not re-firing"
            );
            return;
        }
        Err(err) => {
            tracing::error!(
                invocation_id = %invocation_id,
                error = %err,
                "failed to claim ambiguous-report stamp; publishing anyway (may duplicate)"
            );
        }
    }
    let event = Event::new(
        agent_id,
        invocation_id,
        EventPayload::InvocationAmbiguous(payload),
    );
    if let Err(err) = bus.publish(&event).await {
        tracing::error!(
            invocation_id = %invocation_id,
            error = %err,
            "failed to publish invocation.ambiguous (stamp already claimed; will not retry)"
        );
    }
}
