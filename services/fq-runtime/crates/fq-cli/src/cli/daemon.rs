use super::*;
use uuid::Uuid;

async fn join_with_timeout<E: std::fmt::Display>(
    handle: tokio::task::JoinHandle<Result<(), E>>,
    clean_message: &str,
    error_message: &str,
    panic_message: &str,
    timeout_message: &str,
) {
    match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
        Ok(Ok(Ok(()))) => println!("{clean_message}"),
        Ok(Ok(Err(err))) => tracing::error!(error = %err, "{error_message}"),
        Ok(Err(err)) => tracing::error!(error = %err, "{panic_message}"),
        Err(_) => tracing::warn!("{timeout_message}"),
    }
}

pub(crate) async fn run_daemon(global: &GlobalArgs) -> anyhow::Result<()> {
    let runtime_id = Uuid::now_v7();
    // Includes the commit (FQ_VERSION = semver+sha), so the running
    // daemon's startup event/banner identifies its exact build.
    let version = FQ_VERSION;

    let config = global.resolve_config()?;

    // Fail loud on the unsafe combination (parallel-workers Phase 1):
    // concurrent invocations sharing one workspace directory clobber
    // each other's files silently. The precondition must hold in
    // config, not live only as a template comment existing deployments
    // never see (principle 7 — an unenforced declared boundary is a
    // silent success with wider-than-intended reach).
    if config.worker.max_concurrent_invocations > 1
        && !(config.workspace.per_invocation && config.workspace.path.is_some())
    {
        anyhow::bail!(
            "worker.max_concurrent_invocations = {} requires per-invocation \
             workspaces: set [workspace] path and per_invocation = true, or \
             drop the bound back to 1. Concurrent invocations sharing one \
             workspace directory would overwrite each other's files.",
            config.worker.max_concurrent_invocations
        );
    }

    println!("factor-q runtime starting");
    println!("  runtime id:       {runtime_id}");
    println!("  version:          {version}");
    println!("  NATS:             {}", config.nats.url);
    println!("  agent directory:  {}", config.agents.directory.display());
    println!("  cache directory:  {}", config.cache.directory.display());

    // Load agents eagerly. A missing directory is an error: the
    // dispatcher would otherwise silently drop every trigger.
    let agents_dir = &config.agents.directory;
    if !agents_dir.exists() {
        anyhow::bail!(
            "agent directory {} does not exist. Create it or pass --agents-dir.",
            agents_dir.display()
        );
    }
    let registry = fq_runtime::AgentRegistry::load_from_directory(
        agents_dir,
        config.agents.default_model.as_deref(),
    )
    .with_context(|| format!("failed to load agents from {}", agents_dir.display()))?;
    if !registry.errors().is_empty() {
        for err in registry.errors() {
            tracing::warn!(error = %err, "agent load error");
        }
    }
    let agents_loaded = registry.len() as u32;
    println!(
        "  agents loaded:    {} (errors: {})",
        agents_loaded,
        registry.errors().len()
    );
    let registry = Arc::new(registry);

    // Connect NATS (ensures both streams exist).
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    // Open the three per-store databases (#262 split layout):
    // ProjectionStore (rebuildable from NATS), ControlPlaneStore
    // (coordination/schedules/archive — source of truth), and
    // WorkerStore (in-flight state and WAL — source of truth).
    // A leftover v1 single-file events.db is migrated first; see
    // data-architecture.md §11 and fq_runtime::db.
    let db_paths = ensure_split_dbs(&config).await?;
    println!("  worker db:        {}", db_paths.worker.display());
    println!("  control-plane db: {}", db_paths.control_plane.display());
    println!("  projection db:    {}", db_paths.projection.display());
    let store = Arc::new(
        // allow-direct-store-open: run_daemon IS the runtime — it writes projections.
        ProjectionStore::open(&db_paths.projection)
            .await
            .with_context(|| {
                format!(
                    "failed to open projection at {}",
                    db_paths.projection.display()
                )
            })?,
    );
    let cp_store = Arc::new(
        // allow-direct-store-open: run_daemon hosts the control plane (writer).
        ControlPlaneStore::open(&db_paths.control_plane)
            .await
            .with_context(|| {
                format!(
                    "failed to open control-plane store at {}",
                    db_paths.control_plane.display()
                )
            })?,
    );
    // allow-direct-store-open: run_daemon hosts the control plane (writer).
    // Pool ceiling scales with the fan-out bound (#70): each
    // dispatcher-run invocation is WAL-chatty, plus headroom for the
    // sweepers. Startup recovery is NOT covered — it spawns one resume
    // per recoverable invocation, unbounded, sharing this pool — so a
    // large post-crash backlog queues on pool acquisition (sqlx queues
    // rather than errors up to its acquire timeout). SQLite serialises
    // the writes regardless; the ceiling only bounds waiting.
    let pool_ceiling = (config.worker.max_concurrent_invocations as u32 + 3).max(4);
    let worker_store = Arc::new(
        // allow-direct-store-open: run_daemon owns the worker WAL (writer).
        fq_runtime::WorkerStore::open_with_pool(&db_paths.worker, pool_ceiling)
            .await
            .with_context(|| {
                format!(
                    "failed to open worker store at {}",
                    db_paths.worker.display()
                )
            })?,
    );
    println!(
        "  control plane:    v{}",
        fq_runtime::CONTROL_PLANE_SCHEMA_VERSION
    );
    println!("  worker schema:    v{}", fq_runtime::WORKER_SCHEMA_VERSION);

    // v1 single-process: this daemon plays both control-plane
    // and worker. Self-register the worker side with the
    // control-plane so the membership table reflects reality.
    // The worker_id is the runtime_id; v2 will introduce
    // separate worker ids when workers run in their own processes.
    // The worker_id is the runtime_id formatted as a UUID
    // string. UUIDs are NATS-subject-token safe (alphanumeric +
    // hyphens), so the WorkerId::new call is infallible — but we
    // unwrap explicitly so a future change that produces an
    // unsafe form fails loudly rather than silently.
    let worker_id = fq_runtime::worker::WorkerId::new(runtime_id.to_string())
        .expect("runtime UUID is a valid WorkerId");
    let host_label = local_host_label();
    let now_ms = chrono::Utc::now().timestamp_millis();
    cp_store
        .register_worker(worker_id.as_str(), &host_label, now_ms)
        .await
        .context("failed to self-register worker with control-plane")?;
    println!("  worker:           {} (host: {})", worker_id, host_label);

    // Worker recovery: scan in-flight invocations from the
    // worker store, classify each, log the summary, and emit
    // `invocation.ambiguous` events for the cases that can't
    // be auto-recovered. Safe-resume / safe-replay execution
    // is logged but deferred to a follow-up commit; the
    // categorisation alone is the load-bearing contract that
    // the rest of the runtime can rely on (data-architecture.md
    // §3.1 / §7.1).
    let classified = fq_runtime::worker::scan_in_flight(worker_store.as_ref())
        .await
        .context("failed to scan in-flight invocations")?;
    let mut counts = fq_runtime::worker::CategoryCounts::default();
    for inv in &classified {
        counts.record(inv.category.clone());
    }
    // Always emit system.recovery so historical recovery
    // counts are queryable through the projection (even when
    // there's nothing to recover — counts would all be zero
    // and that's still informational for `fq events query`).
    let recovery_event = Event::system(
        runtime_id,
        EventPayload::SystemRecovery(fq_runtime::events::SystemRecoveryPayload {
            runtime_id,
            worker_id: worker_id.as_str().to_string(),
            safe_resume: counts.safe_resume,
            safe_replay: counts.safe_replay,
            ambiguous: counts.ambiguous,
            total: counts.total(),
        }),
    );
    if let Err(err) = bus.publish(&recovery_event).await {
        tracing::warn!(error = %err, "failed to publish system.recovery event");
    }
    if counts.total() > 0 {
        println!(
            "  in-flight:        {} ({} safe-resume, {} safe-replay, {} ambiguous)",
            counts.total(),
            counts.safe_resume,
            counts.safe_replay,
            counts.ambiguous,
        );
        for inv in &classified {
            if let Some((entity, call_id)) = inv.ambiguous_context() {
                // Re-validate the agent_id pulled from the store
                // before publishing it. If the stored value somehow
                // fails AgentId validation, skip the recovery event
                // and surface the problem in logs — better than
                // panicking or emitting a malformed event.
                let agent_id = match AgentId::new(inv.state.agent_id.clone()) {
                    Ok(id) => id,
                    Err(err) => {
                        tracing::error!(
                            stored_agent_id = %inv.state.agent_id,
                            error = %err,
                            "stored agent_id fails validation; skipping ambiguous-recovery event"
                        );
                        continue;
                    }
                };
                let event_invocation_id = uuid::Uuid::parse_str(&inv.state.invocation_id)
                    .unwrap_or_else(|_| {
                        // Fall back to a fresh uuid if the
                        // stored id ever isn't valid (shouldn't
                        // happen — every id is a v7 uuid).
                        uuid::Uuid::now_v7()
                    });
                publish_ambiguous_once(
                    worker_store.as_ref(),
                    &bus,
                    agent_id,
                    event_invocation_id,
                    &inv.state.invocation_id,
                    fq_runtime::events::InvocationAmbiguousPayload {
                        stuck_entity: entity.to_string(),
                        stuck_call_id: call_id,
                        note:
                            "worker startup categorisation found a `dispatched` row without `completed`"
                                .to_string(),
                    },
                )
                .await;
                // The coordination consumer (spawned below)
                // picks up the `invocation.ambiguous` event we
                // just published and upserts the
                // coordination_invocation_owner row. v1
                // collapsed-process used to write directly
                // here; that's now the consumer's job, which
                // matches v2's split-process expectation
                // (worker emits, control-plane writes).
            }
        }
    }

    // Every in-flight invocation — resumable *or* ambiguous — keeps its
    // workspace: resume continues from it, and `fq recover` triage may
    // need to inspect it. The startup prune below sweeps workspaces of
    // everything else (terminal or unknown).
    let in_flight_ids: std::collections::HashSet<String> = classified
        .iter()
        .map(|c| c.state.invocation_id.clone())
        .collect();

    // Stash the recoverable invocations for resume after the
    // runner is constructed below.
    let recoverable: Vec<_> = classified
        .into_iter()
        .filter(|c| {
            matches!(
                c.category,
                fq_runtime::worker::RecoveryCategory::SafeResume
                    | fq_runtime::worker::RecoveryCategory::SafeReplay
            )
        })
        .collect();

    // Load pricing, merge config overrides, and enforce the coverage
    // guarantee (ADR-0004) — fail-fast before serving any trigger.
    let pricing_cache = config.cache.directory.join("pricing.json");
    let pricing = Arc::new(build_validated_pricing(
        &config,
        &registry,
        PricingTable::load(&pricing_cache).await,
    )?);
    let pricing_entries = pricing.len() as u32;
    println!(
        "  pricing entries:  {} (cache: {})",
        pricing_entries,
        pricing_cache.display()
    );

    // Build tool registry: built-ins + MCP servers from all agents.
    let mut tools = ToolRegistry::with_builtins_exec(config.tools.exec.to_exec_config());
    let mut mcp_manager = McpClientManager::new();
    for loaded in registry.iter() {
        for decl in loaded.agent.mcp_servers() {
            // Grant-bearing servers run per-invocation, wired by the
            // runner (ADR-0018) — not shared at daemon boot.
            if loaded.agent.grants_inbound_capability(&decl.server) {
                continue;
            }
            if let Err(err) = register_mcp_server(&mut tools, &mut mcp_manager, decl).await {
                tracing::warn!(
                    server = %decl.server,
                    agent = %loaded.agent.id(),
                    error = %err,
                    "failed to start MCP server, its tools will be unavailable"
                );
            }
        }
    }
    let mcp_tool_count = tools.len() - fq_runtime::tools::BUILTIN_TOOL_COUNT;
    if mcp_tool_count > 0 {
        println!("  MCP tools:        {mcp_tool_count}");
    }

    let tools = Arc::new(tools);
    // Retry transient LLM errors (rate limits, transport failures) with
    // backoff instead of failing the whole invocation (issue #10). This is
    // the daemon path — the one the fleet actually runs on.
    let llm: Arc<dyn LlmClient> = Arc::new(fq_runtime::llm::RetryingLlmClient::new(
        GenAiClient::from_providers(&config.providers),
        config.worker.llm_retry.clone(),
    ));
    // One ReducerRunner serves two roles: the dispatcher uses
    // it as the Worker for new triggers, and the recovery path
    // uses it directly (via the concrete type) for auto-resume
    // of in-flight invocations. Both paths share the same WAL
    // / archive / coordination wiring.
    let context = Arc::new(
        fq_runtime::ReducerContext::builder()
            .tools(tools)
            .resources(mcp_manager.resource_reader())
            .build(),
    );
    // The `${workspace}` binding (parallel-workers Phase 0): a fresh
    // directory per invocation when enabled, the shared one otherwise.
    let workspace = workspace_provider(&config);
    let resume_runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>> =
        Arc::new(fq_runtime::ReducerRunner::new(
            context.clone(),
            Arc::new(
                fq_runtime::RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(pricing.clone())
                    .store(worker_store.clone())
                    .worker_id(worker_id.clone())
                    .max_iterations(config.max_iterations)
                    .enforce_pricing(true)
                    .workspace(workspace.clone())
                    .build(),
            ),
            fq_runtime::Harness::new(),
        ));
    let worker: Arc<dyn fq_runtime::Worker> = resume_runner.clone();

    // Drain the shared servers' notification streams for the life of
    // the daemon (ADR-0020): logs/progress fold into tracing, and a
    // `tools/list_changed` installs a rebuilt registry into the shared
    // context so the *next* invocation picks it up. The manager keeps
    // its `&mut` lifecycle here for shutdown.
    let notification_channels = mcp_manager.take_notifications().await;
    if !notification_channels.is_empty() {
        let refresher = mcp_manager.tool_refresher(config.tools.exec.to_exec_config());
        let drain_context = context.clone();
        let log_bus = bus.clone();
        tokio::spawn(fq_runtime::mcp::drain_server_notifications(
            notification_channels,
            refresher,
            move |registry| drain_context.install_tools(Arc::new(registry)),
            move |server, level, logger, data| {
                // Bridge the server's log record onto the event bus as a
                // daemon-scoped event (ADR-0020 / plan B2). Fire-and-forget:
                // a failed publish is logged, never blocks the drain.
                let bus = log_bus.clone();
                let event = Event::system(
                    runtime_id,
                    EventPayload::McpServerLog(fq_runtime::events::McpServerLogPayload {
                        server,
                        level,
                        logger,
                        data,
                    }),
                );
                tokio::spawn(async move {
                    if let Err(err) = bus.publish(&event).await {
                        tracing::warn!(error = %err, "failed to publish MCP server log event");
                    }
                });
            },
        ));
    }

    // Spawn auto-resume tasks for each safe-resume / safe-replay
    // invocation found by the recovery scan. Ambiguous cases
    // were already surfaced; safe cases proceed automatically.
    // Each resume runs as a detached task — if one fails, others
    // continue. The resume-runner shares the same NATS bus and
    // tool registry as new triggers.
    let resume_count = recoverable.len();
    // Track the resume tasks' handles so a graceful drain (ADR-0027) can
    // wait for them to suspend at a step boundary before exiting. On a
    // signal-driven shutdown they stay detached (abandoned, as before).
    let mut resume_handles: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(resume_count);
    for inv in recoverable {
        let inv_id = match uuid::Uuid::parse_str(&inv.state.invocation_id) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(
                    invocation_id = %inv.state.invocation_id,
                    error = %err,
                    "invalid invocation_id; skipping resume"
                );
                continue;
            }
        };
        let agent_id = match fq_runtime::AgentId::new(&inv.state.agent_id) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(
                    agent_id = %inv.state.agent_id,
                    error = %err,
                    "invalid agent_id; skipping resume"
                );
                continue;
            }
        };
        let loaded = match registry.get_loaded(&agent_id) {
            Some(l) => l,
            None => {
                tracing::warn!(
                    agent_id = %inv.state.agent_id,
                    "agent not in registry; skipping resume — drop the invocation manually"
                );
                continue;
            }
        };
        let agent = loaded.agent.clone();
        let runner = resume_runner.clone();
        let llm_arc = llm.clone();
        let bus = bus.clone();
        let wstore = worker_store.clone();
        resume_handles.push(tokio::spawn(async move {
            match runner.resume(&agent, llm_arc.as_ref(), inv_id).await {
                Ok(outcome) => tracing::info!(
                    invocation_id = %inv_id,
                    ?outcome,
                    "resume completed"
                ),
                Err(err) => {
                    let note = format!("automatic resume failed: {err}");
                    tracing::error!(invocation_id = %inv_id, agent_id = %agent_id, error = %err, "resume failed; emitting invocation.ambiguous");
                    publish_ambiguous_once(
                        wstore.as_ref(),
                        &bus,
                        agent_id,
                        inv_id,
                        &inv_id.to_string(),
                        fq_runtime::events::InvocationAmbiguousPayload {
                            stuck_entity: "recovery".to_string(),
                            stuck_call_id: inv_id.to_string(),
                            note,
                        },
                    )
                    .await;
                }
            }
        }));
    }
    if resume_count > 0 {
        println!("  resume tasks:     {resume_count} spawned");
    }

    // Sweep workspaces whose invocation is no longer in flight (plan §1:
    // the prune belongs with the recovery scan). Safe to run while the
    // resume tasks are starting — their ids are in the keep set. A
    // failing sweep is a warning, never a startup blocker.
    if let Some(provider) = &workspace
        && let Err(err) = provider.prune(&in_flight_ids).await
    {
        tracing::warn!(error = %err, "workspace prune failed at startup");
    }

    // Publish a system.startup event before spawning any tasks.
    // If this fails the daemon cannot produce lifecycle events at
    // all, which is a bad starting point — bail out loudly.
    let startup_event = Event::system(
        runtime_id,
        EventPayload::SystemStartup(SystemStartupPayload {
            runtime_id,
            version: version.to_string(),
            nats_url: config.nats.url.clone(),
            agents_loaded,
            pricing_entries,
        }),
    );
    bus.publish(&startup_event)
        .await
        .context("failed to publish system.startup event")?;

    // Spawn the projection consumer.
    let (proj_shutdown_tx, proj_shutdown_rx) = tokio::sync::oneshot::channel();
    let projection_consumer = ProjectionConsumer::new(bus.clone(), store.clone());
    let mut projection_handle =
        tokio::spawn(async move { projection_consumer.run(proj_shutdown_rx).await });

    // Spawn the coordination consumer. Subscribes to
    // invocation lifecycle events and maintains the
    // coordination_invocation_owner / coordination_worker
    // state. Stale-worker sweep runs on a timer.
    let (coord_shutdown_tx, coord_shutdown_rx) = tokio::sync::oneshot::channel();
    let coord_consumer = fq_runtime::CoordinationConsumer::new(bus.clone(), cp_store.clone())
        .with_runtime_id(runtime_id)
        .with_self_worker_id(worker_id.as_str().to_string());
    let mut coord_handle = tokio::spawn(async move { coord_consumer.run(coord_shutdown_rx).await });

    // Spawn the worker heartbeat consumer (control-plane side).
    // Receives `fq.worker.*.heartbeat` events and updates
    // `coordination_worker.last_heartbeat` so the stale-worker
    // sweep actually has fresh data to work with.
    let (hb_consumer_shutdown_tx, hb_consumer_shutdown_rx) = tokio::sync::oneshot::channel();
    let hb_consumer = fq_runtime::HeartbeatConsumer::new(bus.clone(), cp_store.clone());
    let mut hb_consumer_handle =
        tokio::spawn(async move { hb_consumer.run(hb_consumer_shutdown_rx).await });

    // Spawn the invocation summary consumer (#216) when `[summary]`
    // names a model. Reuses the daemon's LLM client (routing is
    // per-model) and pricing table; its spend is emitted under the
    // reserved `summary` agent id, never against an invocation.
    let (summary_shutdown_tx, summary_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let summary_handle = config.summary.model.clone().map(|model| {
        println!("  summariser:       {model}");
        let sc = fq_runtime::SummaryConsumer::new(
            bus.clone(),
            llm.clone(),
            pricing.clone(),
            model,
            config.summary.max_line_chars,
        );
        tokio::spawn(async move { sc.run(summary_shutdown_rx).await })
    });

    // Spawn the advisory watch (#169). Drains the captured JetStream
    // MAX_DELIVERIES advisories for the trigger stream and emits the
    // dead-letter events the dispatcher's inline path cannot: a crash
    // during the final delivery, and pre-bound poison triggers at
    // consumer-upgrade time.
    let (advisory_shutdown_tx, advisory_shutdown_rx) = tokio::sync::oneshot::channel();
    let advisory_watch = fq_runtime::AdvisoryWatch::new(bus.clone());
    let mut advisory_handle =
        tokio::spawn(async move { advisory_watch.run(advisory_shutdown_rx).await });

    // Spawn the worker heartbeat producer (worker side). Fires
    // a heartbeat immediately and then every 10s (the default
    // interval). Without this, the coordination consumer's
    // stale-worker sweep would mass-mark every worker stale at
    // 30s. In v2 this task moves into the dedicated Worker
    // process; in v1 it lives in the daemon alongside the other
    // managed tasks.
    let (hb_producer_shutdown_tx, hb_producer_shutdown_rx) = tokio::sync::oneshot::channel();
    let hb_producer =
        fq_runtime::worker::HeartbeatProducer::new(bus.clone(), worker_id.clone(), runtime_id);
    let mut hb_producer_handle =
        tokio::spawn(async move { hb_producer.run(hb_producer_shutdown_rx).await });

    // Spawn the archive-ack consumer (worker side). Listens on
    // `fq.worker.{worker_id}.invocation.archive_acked`; on
    // receipt deletes the matching local invocation_state row.
    // The companion retry sweeper (below) republishes
    // invocation.archived if an ack never arrives, so missed
    // acks are recovered without a durable consumer.
    let (archive_ack_shutdown_tx, archive_ack_shutdown_rx) = tokio::sync::oneshot::channel();
    let archive_ack_consumer =
        fq_runtime::ArchiveAckConsumer::new(bus.clone(), worker_id.clone(), worker_store.clone());
    let mut archive_ack_handle =
        tokio::spawn(async move { archive_ack_consumer.run(archive_ack_shutdown_rx).await });

    // Spawn the archive retry sweeper. Periodically lists
    // pending hand-offs and republishes invocation.archived
    // until the control-plane acks. Cadence + warn threshold
    // come from `[worker]` in fq.toml.
    let (archive_retry_shutdown_tx, archive_retry_shutdown_rx) = tokio::sync::oneshot::channel();
    let archive_retry_sweeper =
        fq_runtime::ArchiveRetrySweeper::new(bus.clone(), worker_id.clone(), worker_store.clone())
            .with_retry_interval_ms(config.worker.archive_retry_interval_ms)
            .with_warn_after_ms(config.worker.archive_warn_after_ms);
    let mut archive_retry_handle =
        tokio::spawn(async move { archive_retry_sweeper.run(archive_retry_shutdown_rx).await });

    // Spawn the control-plane retention sweep (step 10).
    // Deletes invocation_archive rows older than
    // state.retention_days. Setting retention_days < 0
    // disables the task (it exits immediately on startup);
    // see `[state]` in fq.toml.
    let (retention_shutdown_tx, retention_shutdown_rx) = tokio::sync::oneshot::channel();
    let retention_sweeper = fq_runtime::control_plane::retention::RetentionSweeper::new(
        cp_store.clone(),
        config.state.retention_days,
        config.state.sweep_interval_seconds,
    );
    let mut retention_handle =
        tokio::spawn(async move { retention_sweeper.run(retention_shutdown_rx).await });

    // Build the swappable registry handle the dispatcher reads. The
    // dispatcher reads it per-trigger, so `fq reload` can hot-swap the
    // inner Arc for a freshly-loaded registry and have the *next*
    // trigger pick it up. In-flight invocations snapshot their config
    // at trigger time and are undisturbed by a swap (ADR-0020
    // refresh-between-invocations precedent).
    let shared_registry: SharedRegistry = Arc::new(tokio::sync::RwLock::new(registry));

    // Spawn the control-reload listener. On each `fq.control.reload`
    // message it re-reads the agents directory and atomically swaps
    // the shared registry handle. Load failures (missing dir, all
    // agents invalid) are logged and the current registry is kept, so
    // a bad edit can never leave the daemon with no agents. Best-effort
    // core-NATS subscription: reload signals are ephemeral.
    //
    // Non-fatal tier: hot-reload is a convenience, not a critical
    // task — the daemon keeps dispatching triggers perfectly well
    // without it. So, unlike the dispatcher/consumer tasks, losing
    // the reload channel must NOT tear the runtime down. If the
    // subscription ever drops we log and resubscribe rather than
    // exiting, and (see the main select! below) this task's handle is
    // deliberately not watched as a daemon-fatal arm.
    let (reload_shutdown_tx, mut reload_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let reload_bus = bus.clone();
    let reload_registry = shared_registry.clone();
    let reload_dir = config.agents.directory.clone();
    let reload_default_model = config.agents.default_model.clone();
    let reload_handle = tokio::spawn(async move {
        'resubscribe: loop {
            let mut sub = match reload_bus.subscribe_control_reload().await {
                Ok(sub) => sub,
                Err(err) => {
                    // Can't establish the subscription. Log and wait a
                    // beat before retrying rather than spinning or
                    // exiting — hot-reload is best-effort, its absence
                    // never justifies killing the daemon.
                    tracing::error!(
                        error = %err,
                        "failed to subscribe to control reload; retrying in 5s"
                    );
                    tokio::select! {
                        biased;
                        _ = &mut reload_shutdown_rx => {
                            tracing::info!("control-reload listener received shutdown signal");
                            break 'resubscribe;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    }
                }
            };
            loop {
                tokio::select! {
                    biased;
                    _ = &mut reload_shutdown_rx => {
                        tracing::info!("control-reload listener received shutdown signal");
                        break 'resubscribe;
                    }
                    msg = sub.next() => {
                        match msg {
                            Some(_) => {
                            reload_agents(
                                &reload_registry,
                                &reload_dir,
                                reload_default_model.as_deref(),
                            )
                            .await
                        }
                            None => {
                                // Subscription dropped. This is not a
                                // daemon-fatal condition — resubscribe
                                // and carry on so hot-reload recovers on
                                // its own.
                                tracing::warn!(
                                    "control-reload subscription ended; resubscribing"
                                );
                                continue 'resubscribe;
                            }
                        }
                    }
                }
            }
        }
    });

    let drain_probe: Arc<dyn fq_runtime::Worker> = resume_runner.clone();

    // Spawn the control-down listener (`fq down`, issue #63). On a
    // `fq.control.down` message it reads the body to pick the stop mode:
    // drain (suspend in-flight work to a step boundary, then exit) or
    // `now` (clean teardown + deregister + immediate exit). It requests
    // the drain up front in drain mode using the existing drain machinery,
    // then signals the main select with the chosen mode so the
    // teardown deregisters the worker and publishes `fq.system.shutdown`
    // either way. Best-effort core-NATS like reload; non-fatal, so
    // its handle is not watched in the select.
    let (down_requested_tx, mut down_requested_rx) = tokio::sync::oneshot::channel::<bool>();
    let (down_listener_shutdown_tx, mut down_listener_shutdown_rx) =
        tokio::sync::oneshot::channel::<()>();
    let down_bus = bus.clone();
    let down_worker: Arc<dyn fq_runtime::Worker> = resume_runner.clone();
    let down_handle = tokio::spawn(async move {
        let mut down_requested_tx = Some(down_requested_tx);
        'resubscribe: loop {
            let mut sub = match down_bus.subscribe_control_down().await {
                Ok(sub) => sub,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to subscribe to control down; retrying in 5s"
                    );
                    tokio::select! {
                        biased;
                        _ = &mut down_listener_shutdown_rx => break 'resubscribe,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    }
                }
            };
            tokio::select! {
                biased;
                _ = &mut down_listener_shutdown_rx => break 'resubscribe,
                msg = sub.next() => {
                    match msg {
                        Some(msg) => {
                            let now = fq_runtime::bus::down_mode_now_from_body(&msg.payload);
                            if now {
                                tracing::info!(
                                    "down requested (--now); tearing down cleanly, \
                                     deregistering the worker, and exiting without draining"
                                );
                            } else {
                                tracing::info!(
                                    "down requested; draining in-flight invocations to a step \
                                     boundary, then exiting"
                                );
                                down_worker
                                    .request_drain(DrainRequest::new(DrainReason::Operator))
                                    .await;
                            }
                            if let Some(tx) = down_requested_tx.take() {
                                let _ = tx.send(now);
                            }
                            break 'resubscribe;
                        }
                        None => {
                            tracing::warn!("control-down subscription ended; resubscribing");
                            continue 'resubscribe;
                        }
                    }
                }
            }
        }
    });

    // Spawn the trigger dispatcher. Its concurrency bound (#70) is
    // config, default 1 (serial) until the Phase-2 concurrency gate.
    let (disp_shutdown_tx, disp_shutdown_rx) = tokio::sync::oneshot::channel();
    let dispatcher = TriggerDispatcher::new(
        bus.clone(),
        shared_registry.clone(),
        worker,
        llm,
        config.worker.max_concurrent_invocations,
    );
    let mut dispatcher_handle = tokio::spawn(async move { dispatcher.run(disp_shutdown_rx).await });

    // The read-only operator service (#105 layer 2): a localhost tarpc
    // surface over `views` plus the JetStream probe. Deliberately OUTSIDE
    // the supervised task set below — an ops read surface dying must not
    // take the runtime down; it logs and stays down until restart.
    let read_service_addr = if config.read_service.enabled {
        let views = Arc::new(
            fq_runtime::views::Views::open(&db_paths)
                .await
                .context("read service: failed to open the read views")?,
        );
        let (rs_addr, rs_serving) = fq_runtime::read_service::bind(
            &config.read_service.bind,
            views,
            bus.jetstream(),
            std::time::Duration::from_millis(config.read_service.probe_timeout_ms),
            FQ_VERSION.to_string(),
            // The same hot-swapped handle `fq reload` updates, so the
            // dashboard's agents pages reflect reloads live.
            shared_registry,
        )
        .await
        .context("read service: failed to bind (check [read_service] in fq.toml)")?;
        tokio::spawn(async move {
            rs_serving.await;
            tracing::warn!("read service exited; reads are down until the daemon restarts");
        });
        Some(rs_addr)
    } else {
        None
    };

    println!();
    println!("Runtime ready. Press Ctrl-C to stop.");
    println!("  - projection consumer is materialising events into SQLite");
    println!("  - trigger dispatcher is listening on fq.trigger.*");
    println!("  - control-reload listener is listening on fq.control.reload");
    println!("  - control-down listener is listening on fq.control.down");
    if let Some(addr) = read_service_addr {
        println!("  - read service is listening on {addr}");
    }

    // Wait for either a shutdown signal (Ctrl-C / SIGTERM) or one of
    // the hosted tasks exiting prematurely. We watch the task handles
    // in the same select so a silent-failing task is caught
    // immediately instead of at shutdown time.
    let (shutdown_reason, clean_exit, failed_task): (
        &'static str,
        bool,
        Option<(&'static str, String)>,
    ) = tokio::select! {
        reason = wait_for_shutdown_signal() => {
            match reason {
                "ctrl_c" => {
                    println!();
                    println!("Received Ctrl-C, shutting down...");
                    ("ctrl_c", true, None)
                }
                "sigterm" => {
                    // SIGTERM means "terminate gracefully", so treat it as a
                    // graceful drain (ADR-0027): flip the shared drain signal
                    // — in-flight invocations suspend at their next step
                    // boundary, the dispatcher stops consuming — then run the
                    // bounded-wait teardown below, exactly like `fq down`. A
                    // second SIGTERM restores the default disposition and
                    // hard-stops (the force-abort escape). Ctrl-C stays a fast
                    // stop for interactive use.
                    println!();
                    println!("Received SIGTERM, draining...");
                    drain_probe
                        .request_drain(DrainRequest::new(DrainReason::Deploy))
                        .await;
                    ("sigterm", true, None)
                }
                // Listener could not be installed/received; the helper
                // already logged the cause. Treat as an unclean exit.
                other => (other, false, None),
            }
        }
        // A `fq down` control message asked for an operator-initiated clean
        // stop (issue #63). `now == true` skips the drain (SIGINT-equivalent
        // clean stop); `now == false` drains to a step boundary first (the
        // listener already flipped the drain signal). Both are clean exits,
        // so the teardown deregisters the worker either way.
        maybe_now = &mut down_requested_rx => {
            match maybe_now {
                Ok(true) => ("down_now", true, None),
                Ok(false) => ("down", true, None),
                // Sender dropped without a value — should not happen, but
                // treat as a clean drain-style stop rather than a failure.
                Err(_) => ("down", true, None),
            }
        }
        result = &mut projection_handle => {
            let err_msg = describe_task_result("projection consumer", result);
            ("task_failed", false, Some(("projection_consumer", err_msg)))
        }
        result = &mut coord_handle => {
            let err_msg = describe_task_result("coordination consumer", result);
            ("task_failed", false, Some(("coordination_consumer", err_msg)))
        }
        result = &mut hb_consumer_handle => {
            let err_msg = describe_task_result("heartbeat consumer", result);
            ("task_failed", false, Some(("heartbeat_consumer", err_msg)))
        }
        result = &mut advisory_handle => {
            let err_msg = describe_task_result("advisory watch", result);
            ("task_failed", false, Some(("advisory_watch", err_msg)))
        }
        result = &mut hb_producer_handle => {
            let err_msg = describe_task_result("heartbeat producer", result);
            ("task_failed", false, Some(("heartbeat_producer", err_msg)))
        }
        result = &mut archive_ack_handle => {
            let err_msg = describe_task_result("archive-ack consumer", result);
            (
                "task_failed",
                false,
                Some(("archive_ack_consumer", err_msg)),
            )
        }
        result = &mut archive_retry_handle => {
            let err_msg = describe_task_result("archive retry sweeper", result);
            (
                "task_failed",
                false,
                Some(("archive_retry_sweeper", err_msg)),
            )
        }
        result = &mut retention_handle => {
            // RetentionSweeper::run returns () — a panic
            // shows up as Err(JoinError).
            match result {
                Ok(()) => (
                    "task_failed",
                    false,
                    Some(("retention_sweeper", "exited cleanly".to_string())),
                ),
                Err(err) => (
                    "task_failed",
                    false,
                    Some(("retention_sweeper", format!("task panicked: {err}"))),
                ),
            }
        }
        result = &mut dispatcher_handle => {
            // The dispatcher normally exits only on a fatal error. But a
            // graceful drain makes it stop consuming on its own once the
            // drain signal is set (PR-2), so if we're draining, its exit
            // is the clean drain path — not a task failure. (This also
            // covers the race where the dispatcher finishes draining
            // before the listener's `down_requested` signal is polled.)
            if drain_probe.drain_status() == fq_runtime::worker::DrainState::Draining {
                ("down", true, None)
            } else {
                let err_msg = describe_task_result("trigger dispatcher", result);
                ("task_failed", false, Some(("trigger_dispatcher", err_msg)))
            }
        }
    };
    // NOTE: the control-reload listener handle is intentionally NOT
    // watched here. Hot-reload is a non-fatal convenience: its task
    // ending (subscription loss it can't recover, or a panic) must not
    // classify as a daemon-fatal `task_failed` and tear the runtime
    // down. It is signalled to stop and joined during the shutdown
    // sequence below like the other tasks.

    // If a task failed, publish a system.task_failed event with
    // its details before we tear everything else down.
    if let Some((task_name, error_message)) = failed_task.as_ref() {
        tracing::error!(
            task = task_name,
            error = error_message.as_str(),
            "hosted task exited unexpectedly"
        );
        let failed_event = Event::system(
            runtime_id,
            EventPayload::SystemTaskFailed(SystemTaskFailedPayload {
                runtime_id,
                task_name: task_name.to_string(),
                error_message: error_message.clone(),
            }),
        );
        if let Err(err) = bus.publish(&failed_event).await {
            tracing::error!(error = %err, "failed to publish system.task_failed event");
        }
    }

    // On a graceful drain (ADR-0027), wait — bounded by `drain_deadline_ms`
    // — for the invocation-bearing tasks (the dispatcher's in-flight run
    // and the recovery-resume tasks) to suspend at a step boundary. They
    // stop on their own because the drain signal is already set; past the
    // deadline the stragglers are hard-stopped and the next binary's
    // recovery resumes them.
    // SIGTERM and `fq down` drain mode run the bounded drain; a signal-error
    // or task failure does not.
    // `fq down` (drain mode) and `fq down --now` both exit cleanly and
    // deregister the worker; only the drain-mode variants wait out the
    // bounded drain. `down_now` is a fast clean stop like Ctrl-C.
    let drained = matches!(shutdown_reason, "sigterm" | "down");
    let drain_deadline = drained.then(|| {
        tokio::time::Instant::now() + std::time::Duration::from_millis(config.drain_deadline_ms)
    });
    if drained {
        println!();
        println!(
            "Draining — waiting up to {}ms for in-flight invocations to suspend...",
            config.drain_deadline_ms
        );
    }

    // Signal all tasks to shut down. Any one may already be done
    // (the one that returned from the select), but sending on a
    // oneshot whose receiver was dropped is a no-op.
    let _ = proj_shutdown_tx.send(());
    let _ = coord_shutdown_tx.send(());
    let _ = hb_consumer_shutdown_tx.send(());
    let _ = summary_shutdown_tx.send(());
    let _ = advisory_shutdown_tx.send(());
    let _ = hb_producer_shutdown_tx.send(());
    let _ = archive_ack_shutdown_tx.send(());
    let _ = archive_retry_shutdown_tx.send(());
    let _ = retention_shutdown_tx.send(());
    let _ = disp_shutdown_tx.send(());
    let _ = reload_shutdown_tx.send(());
    let _ = down_listener_shutdown_tx.send(());

    join_with_timeout(
        projection_handle,
        "  projection consumer stopped cleanly.",
        "projection consumer exited with error",
        "projection consumer task panicked",
        "projection consumer did not shut down within 5s",
    )
    .await;
    join_with_timeout(
        coord_handle,
        "  coordination consumer stopped cleanly.",
        "coordination consumer exited with error",
        "coordination consumer task panicked",
        "coordination consumer did not shut down within 5s",
    )
    .await;
    if let Some(handle) = summary_handle {
        join_with_timeout(
            handle,
            "  summary consumer stopped cleanly.",
            "summary consumer exited with error",
            "summary consumer task panicked",
            "summary consumer did not shut down within 5s",
        )
        .await;
    }
    join_with_timeout(
        hb_consumer_handle,
        "  heartbeat consumer stopped cleanly.",
        "heartbeat consumer exited with error",
        "heartbeat consumer task panicked",
        "heartbeat consumer did not shut down within 5s",
    )
    .await;
    join_with_timeout(
        advisory_handle,
        "  advisory watch stopped cleanly.",
        "advisory watch exited with error",
        "advisory watch task panicked",
        "advisory watch did not shut down within 5s",
    )
    .await;
    join_with_timeout(
        hb_producer_handle,
        "  heartbeat producer stopped cleanly.",
        "heartbeat producer exited with error",
        "heartbeat producer task panicked",
        "heartbeat producer did not shut down within 5s",
    )
    .await;
    join_with_timeout(
        archive_ack_handle,
        "  archive-ack consumer stopped cleanly.",
        "archive-ack consumer exited with error",
        "archive-ack consumer task panicked",
        "archive-ack consumer did not shut down within 5s",
    )
    .await;
    join_with_timeout(
        archive_retry_handle,
        "  archive retry sweeper stopped cleanly.",
        "archive retry sweeper exited with error",
        "archive retry sweeper task panicked",
        "archive retry sweeper did not shut down within 5s",
    )
    .await;
    match tokio::time::timeout(std::time::Duration::from_secs(5), retention_handle).await {
        Ok(Ok(())) => println!("  retention sweep stopped cleanly."),
        Ok(Err(err)) => tracing::error!(error = %err, "retention sweep task panicked"),
        Err(_) => tracing::warn!("retention sweep did not shut down within 5s"),
    }
    // Dispatcher: on a drain, wait up to the shared drain deadline for it
    // to stop consuming and its in-flight invocation to suspend; on a
    // signal shutdown, the usual 5s.
    let dispatcher_join_deadline = drain_deadline
        .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(5));
    match tokio::time::timeout_at(dispatcher_join_deadline, dispatcher_handle).await {
        Ok(Ok(Ok(()))) => println!("  trigger dispatcher stopped cleanly."),
        Ok(Ok(Err(err))) => tracing::error!(error = %err, "trigger dispatcher exited with error"),
        Ok(Err(err)) => tracing::error!(error = %err, "trigger dispatcher task panicked"),
        Err(_) => tracing::warn!("trigger dispatcher did not shut down in time"),
    }

    // Recovery-resume tasks are joined only on a drain: wait (up to the
    // same shared deadline) for each to suspend at a step boundary. Past
    // the deadline they are abandoned — the next binary's recovery resumes
    // them (as ambiguous, via ordinary crash-recovery). On a signal
    // shutdown they stay detached, unchanged.
    if let Some(deadline) = drain_deadline {
        let (mut suspended, mut hard_stopped) = (0usize, 0usize);
        for handle in resume_handles {
            match tokio::time::timeout_at(deadline, handle).await {
                Ok(_) => suspended += 1,
                Err(_) => hard_stopped += 1,
            }
        }
        if hard_stopped > 0 {
            tracing::warn!(
                suspended,
                hard_stopped,
                "drain deadline elapsed; hard-stopped invocations will be resumed by \
                 recovery on the next start"
            );
        } else if suspended > 0 {
            println!("  drained {suspended} in-flight invocation(s) cleanly.");
        }
    }

    match tokio::time::timeout(std::time::Duration::from_secs(5), reload_handle).await {
        Ok(Ok(())) => println!("  control-reload listener stopped cleanly."),
        Ok(Err(err)) => tracing::error!(error = %err, "control-reload listener task panicked"),
        Err(_) => tracing::warn!("control-reload listener did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), down_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::error!(error = %err, "control-down listener task panicked"),
        Err(_) => tracing::warn!("control-down listener did not shut down within 5s"),
    }

    // Shut down MCP server processes.
    mcp_manager.shutdown().await;

    // On a clean, signal-driven shutdown, deregister the worker so its
    // coordination row reflects a graceful exit (`shutdown`) instead of
    // being left `alive` to age into `stale` — the accumulation this
    // fixes. Symmetric with the startup `register_worker`; best-effort,
    // a failure here must never block the shutdown. A crash / task-
    // failure exit (`clean_exit == false`) is deliberately left to the
    // stale sweep, which is the honest signal that it did not exit
    // cleanly.
    if clean_exit && let Err(err) = cp_store.mark_worker_shutdown(worker_id.as_str()).await {
        tracing::warn!(error = %err, "failed to mark worker as gracefully shut down");
    }

    // Publish a system.shutdown event on the way out. Best-effort —
    // if NATS is already unreachable we just log and continue.
    let shutdown_event = Event::system(
        runtime_id,
        EventPayload::SystemShutdown(SystemShutdownPayload {
            runtime_id,
            reason: shutdown_reason.to_string(),
            clean: clean_exit,
        }),
    );
    if let Err(err) = bus.publish(&shutdown_event).await {
        tracing::warn!(error = %err, "failed to publish system.shutdown event");
    }

    if failed_task.is_some() {
        anyhow::bail!("runtime exited because a hosted task failed");
    }
    Ok(())
}

/// Wait for an OS shutdown signal and report which one fired.
///
/// The caller maps the two signals to different shutdown paths (ADR-0027):
///
/// - **SIGTERM** — what process managers, `docker stop`, systemd, and
///   orchestrators send to stop a service — triggers a **graceful drain**:
///   in-flight invocations suspend at a step boundary and the daemon exits,
///   bounded by `drain_deadline_ms`. The orchestrator's own SIGKILL grace
///   period must be ≥ that deadline or it truncates the drain; a second
///   SIGTERM restores the default disposition and hard-stops. See the
///   deploy plan for per-orchestrator grace settings.
/// - **SIGINT (Ctrl-C)** — interactive stop — is a fast clean shutdown that
///   does not wait out in-flight work (crash-recovery resumes it).
///
/// Either way the daemon exits cleanly (worker deregistered), unlike the
/// abrupt default SIGTERM disposition that orphans the worker + in-flight
/// invocations as recovery cruft.
///
/// Returns a static reason string for the `system.shutdown` event:
/// `"ctrl_c"`, `"sigterm"`, or `"signal_error"` when a listener could
/// not be installed or errored.
pub(crate) async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "failed to install SIGTERM handler; listening for Ctrl-C only"
                );
                return match tokio::signal::ctrl_c().await {
                    Ok(()) => "ctrl_c",
                    Err(err) => {
                        tracing::error!(error = %err, "failed to listen for Ctrl-C");
                        "signal_error"
                    }
                };
            }
        };
        tokio::select! {
            res = tokio::signal::ctrl_c() => match res {
                Ok(()) => "ctrl_c",
                Err(err) => {
                    tracing::error!(error = %err, "failed to listen for Ctrl-C");
                    "signal_error"
                }
            },
            _ = sigterm.recv() => "sigterm",
        }
    }
    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => "ctrl_c",
            Err(err) => {
                tracing::error!(error = %err, "failed to listen for Ctrl-C");
                "signal_error"
            }
        }
    }
}

/// Re-read the agents directory and atomically swap the shared
/// registry the dispatcher reads. Invoked by the daemon's
/// control-reload listener on each `fq.control.reload` message.
///
/// Failure policy: a reload never leaves the daemon worse off. A
/// missing directory or a load error is logged and the *current*
/// registry is kept — a bad edit can't knock out a running daemon.
/// Per-file parse errors are logged but the successfully-parsed
/// agents are still installed (matching `AgentRegistry`'s
/// partial-success semantics). The swap only affects the NEXT
/// trigger; in-flight invocations keep the config they snapshotted
/// at trigger time (ADR-0020 refresh-between-invocations).
pub(crate) async fn reload_agents(
    shared: &SharedRegistry,
    agents_dir: &Path,
    default_model: Option<&str>,
) {
    match AgentRegistry::load_from_directory(agents_dir, default_model) {
        Ok(registry) => {
            let count = registry.len();
            let error_count = registry.errors().len();
            for err in registry.errors() {
                tracing::warn!(error = %err, "agent load error during reload");
            }
            *shared.write().await = Arc::new(registry);
            tracing::info!(
                agents = count,
                errors = error_count,
                "reloaded agent definitions from disk"
            );
        }
        Err(err) => {
            tracing::error!(
                error = %err,
                dir = %agents_dir.display(),
                "agent reload failed; keeping the current registry"
            );
        }
    }
}

/// Convert a joined task result into a short error message. A
/// clean early-exit (task returned Ok(())) is reported as a
/// descriptive string so operators see *something* explaining
/// why a task stopped without being asked to.
pub(crate) fn describe_task_result<E: std::fmt::Display>(
    name: &str,
    result: Result<Result<(), E>, tokio::task::JoinError>,
) -> String {
    match result {
        Ok(Ok(())) => format!("{name} exited before a shutdown signal was sent"),
        Ok(Err(err)) => format!("{name} failed: {err}"),
        Err(join_err) => format!("{name} task panicked: {join_err}"),
    }
}

/// `fq down` (issue #63): cleanly stop a running daemon and confirm it
/// exited. Subscribes to `fq.system.shutdown` *before* publishing the
/// control-down message so the daemon's exit event can't be missed in
/// the gap, publishes the down request (drain mode, or `--now` to skip
/// the drain), then waits — bounded — for a `SystemShutdown` event and
/// reports the runtime that stopped.
///
/// Confirmation is scoped to what v1 can honestly observe: the daemon's
/// own clean-exit event (published after the worker is deregistered), not
/// an OS-level process check — there is no PID/supervisor registry yet
/// (the `fq up`/supervisor story is explicitly out of scope for this
/// ticket). A timeout is a loud, actionable error rather than a false
/// "stopped".
pub(crate) async fn down_daemon(global: &GlobalArgs, now: bool) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    // Subscribe to the daemon shutdown event BEFORE publishing the down
    // request, so a daemon that stops fast can't exit in the window
    // between publish and subscribe and leave us waiting forever.
    let mut shutdown_stream = bus
        .subscribe(fq_runtime::events::subjects::SYSTEM_SHUTDOWN.to_string())
        .await
        .context("failed to subscribe to system shutdown events")?;

    // Also watch worker heartbeats (`fq.worker.*.heartbeat`): a running
    // daemon publishes one on start and every ~10s, which lets us tell
    // "no daemon is listening" (fast-fail) apart from "a daemon is
    // stopping" (wait out the drain) below.
    let mut heartbeat_stream = bus
        .subscribe("fq.worker.*.heartbeat".to_string())
        .await
        .context("failed to subscribe to worker heartbeats")?;

    bus.publish_control_down(now)
        .await
        .context("failed to publish control down")?;

    if now {
        println!(
            "Published stop (--now) on {}.",
            fq_runtime::bus::CONTROL_DOWN_SUBJECT
        );
        println!(
            "A running `fq run` daemon will tear down cleanly, deregister its worker, \
             and exit immediately; in-flight invocations are resumed by recovery \
             on the next start."
        );
    } else {
        println!(
            "Published stop on {}.",
            fq_runtime::bus::CONTROL_DOWN_SUBJECT
        );
        println!(
            "A running `fq run` daemon will drain in-flight invocations to a step boundary, \
             deregister its worker, and exit."
        );
    }

    // Bound the confirmation wait by the drain deadline (plus headroom for
    // the daemon's own teardown/publish) in drain mode; `--now` should be
    // near-instant but gets the same generous ceiling so a busy daemon is
    // not misreported as hung.
    let wait = std::time::Duration::from_millis(config.drain_deadline_ms)
        + std::time::Duration::from_secs(10);
    // Liveness gate: a running daemon emits a worker heartbeat on start
    // and every ~10s (`worker::heartbeat::DEFAULT_INTERVAL_MS`). If neither
    // its shutdown nor any heartbeat arrives within ~2 intervals (capped by
    // the full wait), nothing is listening — fast-fail instead of blocking
    // out the whole deadline.
    let liveness_window = std::time::Duration::from_secs(20).min(wait);
    println!(
        "Waiting up to {}s for the daemon to confirm it has stopped...",
        wait.as_secs()
    );

    enum Confirm {
        NoDaemon,
        StreamClosed,
        TimedOut,
    }

    let start = tokio::time::Instant::now();
    let liveness_deadline = start + liveness_window;
    let full_deadline = start + wait;
    let mut seen_daemon = false;

    let result = loop {
        // Hold to the short liveness gate until we see a sign of life;
        // after that, wait out the full drain-deadline ceiling.
        let deadline = if seen_daemon {
            full_deadline
        } else {
            liveness_deadline
        };
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                break if seen_daemon {
                    Confirm::TimedOut
                } else {
                    Confirm::NoDaemon
                };
            }
            msg = shutdown_stream.next() => match msg {
                Some(Ok(event)) => {
                    if let EventPayload::SystemShutdown(p) = &event.payload {
                        println!(
                            "✓ Daemon stopped (runtime {}, reason={}, clean={}).",
                            p.runtime_id, p.reason, p.clean
                        );
                        return Ok(());
                    }
                }
                // A single undeserialisable event must not end the wait —
                // keep listening for the shutdown event.
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "skipping undeserialisable event while waiting");
                }
                None => break Confirm::StreamClosed,
            },
            hb = heartbeat_stream.next() => {
                // Any heartbeat proves a daemon is up and (having received
                // the down request) stopping; wait out the full deadline for
                // its shutdown event. A closed heartbeat stream is benign.
                if hb.is_some() {
                    seen_daemon = true;
                }
            }
        }
    };

    match result {
        Confirm::NoDaemon => anyhow::bail!(
            "no running `fq run` daemon detected — no worker heartbeat on \
             `fq.worker.*.heartbeat` within {}s, so `fq down` is a no-op. \
             Is the daemon running? (`fq status`)",
            liveness_window.as_secs()
        ),
        Confirm::StreamClosed => anyhow::bail!(
            "the shutdown event stream closed before the daemon confirmed it stopped; \
             check `fq status` / `fq workers list` for the daemon's state"
        ),
        Confirm::TimedOut => anyhow::bail!(
            "timed out after {}s: a daemon was heartbeating but did not confirm it \
             stopped — check `fq status` and `fq workers list` for a lingering worker.",
            wait.as_secs()
        ),
    }
}

pub(crate) async fn reload_daemon(global: &GlobalArgs) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
    bus.publish_control_reload()
        .await
        .context("failed to publish control reload")?;
    println!(
        "Published reload signal on {}.",
        fq_runtime::bus::CONTROL_RELOAD_SUBJECT
    );
    println!(
        "A running `fq run` daemon will re-read {} and swap its registry for the next trigger.",
        config.agents.directory.display()
    );
    Ok(())
}
