//! The daemon loop: what `fqd` does once its arguments are parsed.
//!
//! Split from `lib.rs` because a new file may not carry a size budget —
//! only the god-files that predate the gate have those, and this crate
//! is a day old.

use std::sync::Arc;

use anyhow::Context;
use fq_runtime::events::{
    Event, EventPayload, SystemShutdownPayload, SystemStartupPayload, SystemTaskFailedPayload,
};
use fq_runtime::llm::{GenAiClient, LlmClient};
use fq_runtime::worker::{DrainReason, DrainRequest};
use fq_runtime::{
    ControlPlaneStore, EventBus, McpClientManager, McpServerConfig, PricingTable,
    ProjectionConsumer, ProjectionStore, SharedRegistry, ToolRegistry, TriggerDispatcher,
};
use uuid::Uuid;

use crate::OperatorDeps;
use crate::boot::{ensure_split_dbs, local_host_label, runtime_db_paths, workspace_provider};
use crate::cli::GlobalArgs;
use crate::operator_surface::operator_registry;
use crate::pricing::build_validated_pricing;
use crate::resume::ResumeControl;
use crate::signals::{describe_task_result, wait_for_shutdown_signal};
use crate::version::FQ_VERSION;

/// Lifecycle events are published on `fq.system.*` so operators
/// can see from the event stream when the daemon started, why it
/// stopped, and which hosted task (if any) failed. A task that
/// dies unexpectedly triggers an immediate shutdown of the other
/// task and a non-zero process exit, rather than silently
/// limping along with a broken dispatcher or projector.
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
    println!("  state directory:  {}", config.state.directory.display());

    // Load agents eagerly. A missing directory is an error: the
    // dispatcher would otherwise silently drop every trigger.
    let agents_dir = &config.agents.directory;
    if !agents_dir.exists() {
        anyhow::bail!(
            "agent directory {} does not exist. Create it or pass --agents-dir.",
            agents_dir.display()
        );
    }
    // The daemon owns the registry: this read is what the Agent view
    // later serves and what `fq reload` replaces.
    // allow-runtime-internals: run_daemon IS the runtime — it builds the live registry.
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

    // Reconcile worker rows left live by operator terminal transitions made
    // before this binary was deployed. Do this before recovery classification
    // so a pre-existing orphan cannot be reported ambiguous again.
    crate::recovery::reconcile_terminal_owners(&worker_store, &cp_store, now_ms).await?;

    // Worker recovery: scan in-flight invocations from the worker store,
    // classify each, restore ownership for auto-resumed cases, and emit
    // `invocation.ambiguous` events for cases that cannot be recovered.
    // `in_flight_ids` is the keep set the startup workspace prune reads.
    let (recoverable, in_flight_ids) =
        crate::recovery::classify_in_flight(&worker_store, &cp_store, &bus, runtime_id, &worker_id)
            .await?;

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
            let config = McpServerConfig {
                name: decl.server.clone(),
                command: decl.command.clone().unwrap_or_default(),
                args: decl.args.clone(),
                env: decl.env.clone(),
                url: decl.url.clone(),
            };
            match mcp_manager.start_server(config).await {
                Ok(mcp_tools) => {
                    for tool in mcp_tools {
                        if let Err(error) = tools.register(tool) {
                            tracing::warn!(server = %decl.server, %error, "refusing MCP tool registration");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        server = %decl.server,
                        agent = %loaded.agent.id(),
                        error = %err,
                        "failed to start MCP server, its tools will be unavailable"
                    );
                }
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

    // Live-drop liveness now lives in the `invocation.drop` command
    // handler on the edge (plan Phase 4, verb 18), which holds this same
    // runner. The `fq.control.invocation.drop` listener that used to
    // answer it is gone, and with it a failure class: core NATS reported
    // "no responders" the instant nobody owned that subject, and the CLI
    // read that as "inactive/stuck, drop directly" — so every window
    // where the subject was unowned (notably a restart racing startup
    // recovery, exactly when operators reach for `drop`) silently
    // defeated the guard. An unreachable edge is a connection error, not
    // a licence to bypass; the guard now fails closed by construction.

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
    // invocation found by the recovery scan.
    let resume_handles = crate::recovery::spawn_resume_tasks(
        recoverable,
        &registry,
        &resume_runner,
        &llm,
        &bus,
        &worker_store,
    );

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

    // Spawn the projection consumer, advancing the watermark as it
    // applies events — the fold-as-of-W coordinate reads gate on for
    // read-your-writes (plan Phase 3a).
    let (watermark_tx, projection_watermark) = fq_runtime::watermark::channel();
    let (proj_shutdown_tx, proj_shutdown_rx) = tokio::sync::oneshot::channel();
    let projection_consumer =
        ProjectionConsumer::new(bus.clone(), store.clone()).with_watermark(watermark_tx);
    let mut projection_handle =
        tokio::spawn(async move { projection_consumer.run(proj_shutdown_rx).await });

    // Spawn the coordination consumer. Subscribes to
    // invocation lifecycle events and maintains the
    // coordination_invocation_owner / coordination_worker
    // state. Stale-worker sweep runs on a timer.
    let (coord_shutdown_tx, coord_shutdown_rx) = tokio::sync::oneshot::channel();
    let (coord_watermark_tx, coordination_watermark) = fq_runtime::watermark::channel();
    let coord_consumer = fq_runtime::CoordinationConsumer::new(bus.clone(), cp_store.clone())
        .with_runtime_id(runtime_id)
        .with_worker_store(worker_store.clone())
        .with_self_worker_id(worker_id.as_str().to_string())
        // The coordination half of the read horizon (Phase 3c):
        // gated reads of the Invocation view wait on this mark too,
        // because the fold spans stores this consumer writes.
        .with_watermark(coord_watermark_tx);
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

    // Spawn the retention sweeps. Two windows, both from `[state]`.
    // `retention_days` bounds invocation_archive and projected `events`
    // rows (step 10), except cost-bearing rows, kept indefinitely so
    // spend figures survive retention. `stale_worker_retention_days`
    // bounds stale coordination_worker rows, which accrue one per
    // daemon restart and used to need `fq workers prune`. Either at
    // < 0 skips its own sweep; both < 0 exits the task.
    let (retention_shutdown_tx, retention_shutdown_rx) = tokio::sync::oneshot::channel();
    let retention_sweeper = fq_runtime::control_plane::retention::RetentionSweeper::new(
        cp_store.clone(),
        &config.state,
    )
    .with_projection_store(store.clone());
    let mut retention_handle =
        tokio::spawn(async move { retention_sweeper.run(retention_shutdown_rx).await });

    // Build the swappable registry handle the dispatcher reads. The
    // dispatcher reads it per-trigger, so `fq reload` can hot-swap the
    // inner Arc for a freshly-loaded registry and have the *next*
    // trigger pick it up. In-flight invocations snapshot their config
    // at trigger time and are undisturbed by a swap (ADR-0020
    // refresh-between-invocations precedent).
    let shared_registry: SharedRegistry = Arc::new(tokio::sync::RwLock::new(registry));

    let drain_probe: Arc<dyn fq_runtime::Worker> = resume_runner.clone();

    // The stop switch `control.down` throws (`fq down`, issue #63). Two
    // listener tasks used to stand here — one per control subject, each
    // resubscribing on loss — and both are gone with the subjects: the
    // machinery verbs are commands on the edge, so the daemon answers
    // them on the transport it already serves rather than on a
    // best-effort core-NATS channel whose loss it had to survive. What
    // is left is the one-shot the select below still waits on.
    let (down_requested_tx, mut down_requested_rx) = tokio::sync::oneshot::channel::<bool>();
    let down_signal: crate::control_commands::DownSignal =
        Arc::new(tokio::sync::Mutex::new(Some(down_requested_tx)));

    // Operator resume listener (`fq invocation resume`, #373). Best-effort
    // core-NATS like reload/down: non-fatal, resubscribes on loss, and its
    // handle is not watched in the main select.
    let resume_control = ResumeControl {
        bus: bus.clone(),
        worker_store: worker_store.clone(),
        cp_store: cp_store.clone(),
        runner: resume_runner.clone(),
        registry: shared_registry.clone(),
        llm: llm.clone(),
    };
    let (resume_listener_handle, resume_listener_shutdown_tx) =
        crate::listeners::spawn_resume_listener(resume_control);

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

    // The authenticated operator edge (ADR-0006 + ADR-0031, plan
    // Phase 2): TLS + capability tokens over tarpc
    // `invoke`/`next_batch`. Identity (certificate + token root)
    // persists under the state dir; the first run mints it and prints
    // the admin token exactly once — see `edge_identity`. Same
    // supervision posture as the read service: outside the supervised
    // set — an operator surface dying must not take the runtime down.
    let edge_addr = if config.edge.enabled {
        let (identity, _identity_dir) = crate::edge_identity::resolve(&config)?;
        // The operator surface: real declarations over the daemon's
        // read views, gated at the projection watermark (Phase 3).
        let edge_views = Arc::new(
            fq_runtime::views::Views::open(&db_paths) // allow-runtime-internals: allow-direct-store-open: daemon's own
                .await
                .context("edge: failed to open the read views")?,
        );
        let edge_registry = Arc::new(operator_registry(
            edge_views,
            fq_runtime::watermark::Horizon::new(vec![
                projection_watermark.clone(),
                coordination_watermark.clone(),
            ]),
            std::time::Duration::from_millis(config.edge.min_seq_wait_ms),
            OperatorDeps {
                bus: bus.clone(),
                projection: store.clone(),
                control_plane: cp_store.clone(),
                // The daemon's own store paths, so `control.status`
                // answers about the process reporting rather than the
                // machine asking.
                db_paths: Arc::new(runtime_db_paths(&config)),
                legacy_events_db: Arc::new(fq_runtime::db::legacy_db_path(&config.cache.directory)),
                // The same runner the dispatcher and startup recovery
                // drive invocations with — `invocation.drop` asks it
                // whether the target is live, and arms its halt.
                runner: resume_runner.clone(),
                // The same hot-swapped handle `fq reload` updates and
                // the dispatcher reads, so `fq agent list` answers
                // with the definitions this daemon would run.
                agents: shared_registry.clone(),
                machinery: crate::control_commands::MachineryDeps {
                    agents: shared_registry.clone(),
                    agents_dir: config.agents.directory.clone(),
                    default_model: config.agents.default_model.clone(),
                    // The same runner `fq down`'s drain suspends and the
                    // teardown below waits on.
                    worker: resume_runner.clone(),
                    down: down_signal,
                },
            },
        )?);
        let (edge_addr, edge_serving) = fq_edge::bind(&config.edge.bind, &identity, edge_registry)
            .await
            .context("edge: failed to bind (check [edge] in fq.toml)")?;
        tokio::spawn(async move {
            edge_serving.await;
            tracing::warn!("edge exited; the operator edge is down until the daemon restarts");
        });
        Some(edge_addr)
    } else {
        None
    };

    println!();
    println!("Runtime ready. Press Ctrl-C to stop.");
    println!("  - projection consumer is materialising events into SQLite");
    println!("  - trigger dispatcher is listening on fq.trigger.*");
    if let Some(addr) = edge_addr {
        println!("  - edge is listening on {addr}");
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
        // The `control.down` command asked for an operator-initiated clean
        // stop (issue #63). `now == true` skips the drain (SIGINT-equivalent
        // clean stop); `now == false` drains to a step boundary first (the
        // command handler already flipped the drain signal). Both are clean
        // exits, so the teardown deregisters the worker either way.
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
    let _ = resume_listener_shutdown_tx.send(());

    match tokio::time::timeout(std::time::Duration::from_secs(5), projection_handle).await {
        Ok(Ok(Ok(()))) => println!("  projection consumer stopped cleanly."),
        Ok(Ok(Err(err))) => tracing::error!(error = %err, "projection consumer exited with error"),
        Ok(Err(err)) => tracing::error!(error = %err, "projection consumer task panicked"),
        Err(_) => tracing::warn!("projection consumer did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), coord_handle).await {
        Ok(Ok(Ok(()))) => println!("  coordination consumer stopped cleanly."),
        Ok(Ok(Err(err))) => {
            tracing::error!(error = %err, "coordination consumer exited with error")
        }
        Ok(Err(err)) => tracing::error!(error = %err, "coordination consumer task panicked"),
        Err(_) => tracing::warn!("coordination consumer did not shut down within 5s"),
    }
    if let Some(handle) = summary_handle {
        match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
            Ok(Ok(Ok(()))) => println!("  summary consumer stopped cleanly."),
            Ok(Ok(Err(err))) => tracing::error!(error = %err, "summary consumer exited with error"),
            Ok(Err(err)) => tracing::error!(error = %err, "summary consumer task panicked"),
            Err(_) => tracing::warn!("summary consumer did not shut down within 5s"),
        }
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), hb_consumer_handle).await {
        Ok(Ok(Ok(()))) => println!("  heartbeat consumer stopped cleanly."),
        Ok(Ok(Err(err))) => {
            tracing::error!(error = %err, "heartbeat consumer exited with error")
        }
        Ok(Err(err)) => tracing::error!(error = %err, "heartbeat consumer task panicked"),
        Err(_) => tracing::warn!("heartbeat consumer did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), advisory_handle).await {
        Ok(Ok(Ok(()))) => println!("  advisory watch stopped cleanly."),
        Ok(Ok(Err(err))) => {
            tracing::error!(error = %err, "advisory watch exited with error")
        }
        Ok(Err(err)) => tracing::error!(error = %err, "advisory watch task panicked"),
        Err(_) => tracing::warn!("advisory watch did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), hb_producer_handle).await {
        Ok(Ok(Ok(()))) => println!("  heartbeat producer stopped cleanly."),
        Ok(Ok(Err(err))) => {
            tracing::error!(error = %err, "heartbeat producer exited with error")
        }
        Ok(Err(err)) => tracing::error!(error = %err, "heartbeat producer task panicked"),
        Err(_) => tracing::warn!("heartbeat producer did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), archive_ack_handle).await {
        Ok(Ok(Ok(()))) => println!("  archive-ack consumer stopped cleanly."),
        Ok(Ok(Err(err))) => {
            tracing::error!(error = %err, "archive-ack consumer exited with error")
        }
        Ok(Err(err)) => tracing::error!(error = %err, "archive-ack consumer task panicked"),
        Err(_) => tracing::warn!("archive-ack consumer did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), archive_retry_handle).await {
        Ok(Ok(Ok(()))) => println!("  archive retry sweeper stopped cleanly."),
        Ok(Ok(Err(err))) => {
            tracing::error!(error = %err, "archive retry sweeper exited with error")
        }
        Ok(Err(err)) => tracing::error!(error = %err, "archive retry sweeper task panicked"),
        Err(_) => tracing::warn!("archive retry sweeper did not shut down within 5s"),
    }
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

    match tokio::time::timeout(std::time::Duration::from_secs(5), resume_listener_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::error!(error = %err, "control-resume listener task panicked"),
        Err(_) => tracing::warn!("control-resume listener did not shut down within 5s"),
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
