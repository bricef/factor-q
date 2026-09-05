//! The daemon loop: what `fqd` does once its arguments are parsed.
//!
//! Split from `lib.rs` because a new file may not carry a size budget —
//! only the god-files that predate the gate have those, and this crate
//! is a day old.

use std::sync::Arc;

use anyhow::Context;
use fq_runtime::llm::{GenAiClient, LlmClient};
use fq_runtime::{ControlPlaneStore, EventBus, McpClientManager, PricingTable, ProjectionStore};
use uuid::Uuid;

use crate::boot::{ensure_split_dbs, local_host_label, workspace_provider};
use crate::cli::GlobalArgs;
use crate::pricing::build_validated_pricing;
use crate::version::FQ_VERSION;

/// Dial the broker. The token comes from the environment variable
/// `[nats] token_env` names and reaches the bus as its own argument;
/// `config.nats.url` was refused at validation if it carried userinfo,
/// so the URL in this error context — like the banner that prints it and
/// the `system.startup` payload that records it — is credential-free by
/// construction rather than by redaction (#540).
async fn connect_bus(config: &fq_runtime::Config) -> anyhow::Result<EventBus> {
    let token = config.nats.resolve_token()?;
    EventBus::connect_with_token(&config.nats.url, token.as_deref())
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))
}

/// Lifecycle events are published on `fq.system.*` so operators
/// can see from the event stream when the daemon started, why it
/// stopped, and which hosted task (if any) failed. Nine tasks are
/// supervised together in `hosted.rs` — the projection, coordination,
/// heartbeat and archive-ack consumers, the advisory watch, the
/// heartbeat producer, the archive-retry and retention sweepers, and
/// the trigger dispatcher. Any one dying unexpectedly triggers an
/// immediate shutdown of the rest and a non-zero process exit, rather
/// than silently limping along with a broken dispatcher or projector.
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
    // never see (principle 3 — an unenforced declared boundary is a
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

    // **The instance lock, and it is the listener itself.**
    //
    // Bound before anything else this process does that another
    // process could see: before the broker connection, before the
    // stores are opened, before the worker registers, before recovery
    // publishes `system.recovery` and takes ownership of resumable
    // invocations. Two daemons cannot hold one address, and a daemon
    // without an edge is not an operable configuration — `fq` has no
    // path to it — so the address is a proxy for the whole instance.
    //
    // Losing the race therefore costs nothing to clean up. Before this
    // ordering, a bind failure (the port still held by a predecessor
    // that had not finished draining, most often) unwound past a live
    // worker row, cancelled resume tasks mid-step, and published no
    // `system.shutdown`: the daemon that could not start left the mess
    // (review B5). Now it exits here, naming the address, having
    // touched nothing.
    //
    // No lock file. A file records an intention and outlives the
    // process that wrote it; a bound socket is the fact itself and the
    // kernel releases it.
    let edge_listener = fq_edge::EdgeListener::bind(&config.edge.bind)
        .await
        .context(
            "the edge could not take its address, so this daemon is not starting. \
             Another `fqd` may still hold it — check `fq status`, or wait for a \
             draining predecessor to exit — or change `[edge] bind` in fqd.toml.",
        )?;
    println!("  edge:             {}", edge_listener.local_addr());

    // Signal handlers are installed here, once, and held for the life
    // of the process (#509): the drain reads the same streams, so a
    // second SIGTERM escalates it instead of falling into a
    // registration nobody is listening to.
    let signals = crate::signals::ShutdownSignals::install();

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

    // Connect NATS (ensures the streams exist); `connect_bus` has the token.
    let bus = connect_bus(&config).await?;

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
    let registration = crate::worker_registration::WorkerRegistration::register(
        cp_store.clone(),
        worker_id.clone(),
        &host_label,
        now_ms,
    )
    .await?;
    println!("  worker:           {} (host: {})", worker_id, host_label);

    // ---- Past this line the control plane has a live row for this
    // process, and every way out must say what became of it. The
    // block below is that single exit path: whatever it returns,
    // `settle_unclaimed` runs, and it is a no-op when the teardown
    // inside already accounted for the row. Adding a fallible step
    // anywhere in here cannot reintroduce the orphaned-worker bug,
    // because there is no path that skips the settle. ----
    let outcome = run_registered(Registered {
        runtime_id,
        version,
        config,
        bus,
        db_paths,
        store,
        cp_store,
        worker_store,
        registry,
        worker_id,
        registration: registration.clone(),
        edge_listener,
        signals,
        agents_loaded,
        now_ms,
    })
    .await;
    registration.settle_unclaimed().await;
    outcome
}

/// Everything `run_daemon` had worked out by the time its worker row
/// existed. One struct because the alternative is fifteen arguments,
/// and the split exists to give the registration a single exit path
/// rather than to give these fields a home.
struct Registered {
    runtime_id: Uuid,
    version: &'static str,
    config: fq_runtime::Config,
    bus: EventBus,
    db_paths: fq_runtime::RuntimeDbPaths,
    store: Arc<ProjectionStore>,
    cp_store: Arc<ControlPlaneStore>,
    worker_store: Arc<fq_runtime::WorkerStore>,
    registry: Arc<fq_runtime::AgentRegistry>,
    worker_id: fq_runtime::worker::WorkerId,
    registration: crate::worker_registration::WorkerRegistration,
    edge_listener: fq_edge::EdgeListener,
    signals: crate::signals::ShutdownSignals,
    agents_loaded: u32,
    now_ms: i64,
}

/// The rest of the assembly, and then the run. Everything here is
/// fallible and every failure lands on the registration guard.
async fn run_registered(r: Registered) -> anyhow::Result<()> {
    let Registered {
        runtime_id,
        version,
        config,
        bus,
        db_paths,
        store,
        cp_store,
        worker_store,
        registry,
        worker_id,
        registration,
        edge_listener,
        signals,
        agents_loaded,
        now_ms,
    } = r;

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
    let mut mcp_manager = McpClientManager::with_server_root(config.state.directory.join("mcp"));
    let tools = crate::shared_servers::start_shared_servers(
        &registry,
        &mut mcp_manager,
        config.tools.exec.to_exec_config(),
    )
    .await;
    let mcp_tool_count = tools.len() - fq_runtime::tools::BUILTIN_TOOL_COUNT;
    if mcp_tool_count > 0 {
        println!("  MCP tools:        {mcp_tool_count}");
    }

    let tools = Arc::new(tools);
    // Retry transient LLM errors (rate limits, transport failures) with
    // backoff instead of failing the whole invocation (issue #10). This is
    // the daemon path — the one the fleet actually runs on.
    let llm: Arc<dyn LlmClient> = Arc::new(fq_runtime::llm::RetryingLlmClient::new(
        GenAiClient::from_providers(&config.providers)?,
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
                    .mcp_server_root(config.state.directory.join("mcp"))
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
    // Bridging a server's log record onto the event bus as a
    // daemon-scoped event (ADR-0020 / plan B2) happens in there too.
    let notification_channels = mcp_manager.take_notifications().await;
    if !notification_channels.is_empty() {
        crate::shared_servers::drain_notifications(
            &mut mcp_manager,
            context.clone(),
            bus.clone(),
            runtime_id,
            config.tools.exec.to_exec_config(),
            notification_channels,
        );
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

    crate::hosted::run_hosted(crate::hosted::Assembled {
        runtime_id,
        version,
        config,
        bus,
        db_paths,
        store,
        cp_store,
        worker_store,
        registry,
        llm,
        pricing,
        resume_runner,
        worker,
        worker_id,
        registration,
        edge_listener,
        signals,
        mcp_manager,
        agents_loaded,
        pricing_entries,
        resume_handles,
    })
    .await
}
