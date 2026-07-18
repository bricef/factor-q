use super::*;

pub(crate) fn print_version(json: bool) {
    let build_date = FQ_BUILD_EPOCH
        .parse::<i64>()
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if json {
        let info = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": FQ_GIT_SHA,
            "build_date": build_date,
            "target": FQ_TARGET,
        });
        println!("{}", serde_json::to_string_pretty(&info).unwrap());
    } else {
        println!("fq {}", env!("CARGO_PKG_VERSION"));
        println!("  commit:      {FQ_GIT_SHA}");
        println!("  build date:  {build_date}");
        println!("  target:      {FQ_TARGET}");
    }
}

/// Template files embedded in the binary. Each entry is `(destination,
/// contents)` and is written verbatim when `fq init` runs.
pub(crate) const FQ_TOML_TEMPLATE: &str = include_str!("../templates/fq.toml");

/// Build the `${workspace}` provider from `[workspace]` (parallel-workers
/// Phase 0): with `per_invocation = true` each invocation gets a fresh
/// empty directory under `path`; otherwise every invocation binds to
/// `path` itself. No `path` configured → no binding, and agents that use
/// the token fail loudly at invocation start. Pure filesystem either way
/// — what goes into a workspace is the agent's business.
pub(crate) fn workspace_provider(
    config: &fq_runtime::Config,
) -> Option<std::sync::Arc<dyn fq_runtime::worker::workspace::WorkspaceProvider>> {
    use fq_runtime::worker::workspace::{PerInvocationWorkspace, StaticWorkspace};
    let ws = &config.workspace;
    let path = ws.path.clone()?;
    if ws.per_invocation {
        Some(std::sync::Arc::new(PerInvocationWorkspace::new(path)))
    } else {
        Some(std::sync::Arc::new(StaticWorkspace::new(path)))
    }
}
pub(crate) async fn register_mcp_server(
    tools: &mut ToolRegistry,
    manager: &mut McpClientManager,
    decl: &fq_runtime::agent::McpServerDeclaration,
) -> anyhow::Result<()> {
    let config = McpServerConfig {
        name: decl.server.clone(),
        command: decl.command.clone().unwrap_or_default(),
        args: decl.args.clone(),
        env: decl.env.clone(),
        url: decl.url.clone(),
    };
    for tool in manager.start_server(config).await? {
        if let Err(error) = tools.register(tool) {
            tracing::warn!(server = %decl.server, %error, "refusing MCP tool registration");
        }
    }
    Ok(())
}

pub(crate) const README_TEMPLATE: &str = include_str!("../templates/README.md");
pub(crate) const SAMPLE_AGENT_TEMPLATE: &str = include_str!("../templates/sample-agent.md");
pub(crate) const DOCKER_COMPOSE_TEMPLATE: &str = include_str!("../templates/docker-compose.yml");

/// Initialise a new factor-q project in the current working directory.
///
/// Writes four files (plus an `agents/` directory):
/// - `fq.toml`
/// - `README.md`
/// - `docker-compose.yml` (NATS with JetStream)
/// - `agents/sample-agent.md`
///
/// Errors and exits if any of the target files already exist, unless
/// `--force` is set.
pub(crate) fn init_project(force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let fq_toml = cwd.join("fq.toml");
    let readme = cwd.join("README.md");
    let agents_dir = cwd.join("agents");
    let sample_agent = agents_dir.join("sample-agent.md");
    let docker_compose = cwd.join("docker-compose.yml");

    // Detect conflicts up front so the user sees all of them at once
    // rather than fixing them one by one.
    if !force {
        let mut conflicts: Vec<&Path> = Vec::new();
        if fq_toml.exists() {
            conflicts.push(&fq_toml);
        }
        if readme.exists() {
            conflicts.push(&readme);
        }
        if sample_agent.exists() {
            conflicts.push(&sample_agent);
        }
        if docker_compose.exists() {
            conflicts.push(&docker_compose);
        }
        if !conflicts.is_empty() {
            let listing = conflicts
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "the following files already exist:\n{listing}\n\n\
                 Use `fq init --force` to overwrite them."
            );
        }
    }

    std::fs::create_dir_all(&agents_dir)
        .with_context(|| format!("failed to create {}", agents_dir.display()))?;
    write_file(&fq_toml, FQ_TOML_TEMPLATE)?;
    write_file(&readme, README_TEMPLATE)?;
    write_file(&docker_compose, DOCKER_COMPOSE_TEMPLATE)?;
    write_file(&sample_agent, SAMPLE_AGENT_TEMPLATE)?;

    println!("Initialised factor-q project in {}", cwd.display());
    println!();
    println!("Created:");
    println!("  fq.toml");
    println!("  README.md");
    println!("  docker-compose.yml");
    println!("  agents/");
    println!("  agents/sample-agent.md");
    println!();
    println!("Next steps:");
    println!("  1. Start NATS (JetStream) in the background:");
    println!("     docker compose up -d");
    println!("  2. Export your LLM provider API key, e.g.:");
    println!("     export ANTHROPIC_API_KEY='sk-ant-...'");
    println!("  3. Trigger the sample agent:");
    println!("     fq trigger sample-agent \"Say hello in one sentence.\"");
    Ok(())
}

pub(crate) fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

pub(crate) fn list_agents(global: &GlobalArgs) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let dir = &config.agents.directory;

    if !dir.exists() {
        let absolute = if dir.is_absolute() {
            dir.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| normalise(&cwd.join(dir)))
                .unwrap_or_else(|_| dir.clone())
        };
        println!(
            "Agent directory {} does not exist (resolved: {}).",
            dir.display(),
            absolute.display()
        );
        return Ok(());
    }

    let registry = AgentRegistry::load_from_directory(dir, config.agents.default_model.as_deref())?;

    if registry.is_empty() && registry.errors().is_empty() {
        println!("No agents found in {}", dir.display());
        return Ok(());
    }

    if !registry.is_empty() {
        println!("Loaded {} agent(s) from {}:", registry.len(), dir.display());
        let mut agents: Vec<_> = registry.iter().collect();
        agents.sort_by(|a, b| a.agent.id().as_str().cmp(b.agent.id().as_str()));
        for loaded in agents {
            println!(
                "  {:<30} model={} tools={} path={}",
                loaded.agent.id().as_str(),
                loaded.agent.model(),
                loaded.agent.tools().len(),
                loaded.path.display()
            );
        }
    }

    if !registry.errors().is_empty() {
        println!();
        println!("Errors ({}):", registry.errors().len());
        for err in registry.errors() {
            println!("  {err}");
        }
    }

    Ok(())
}

pub(crate) fn validate_agent(path: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|err| anyhow::anyhow!("failed to read {}: {err}", path.display()))?;

    match parse_agent(&content) {
        Ok(agent) => {
            println!("✓ {} is valid", path.display());
            println!("  id:      {}", agent.id().as_str());
            println!("  model:   {}", agent.model());
            println!("  tools:   {}", agent.tools().len());
            if let Some(budget) = agent.budget() {
                println!("  budget:  ${budget:.2}");
            }
            // #35: valid, but do not let "✓ is valid" imply the declared
            // network boundary holds — nothing enforces it yet.
            if let Some(declared) = agent.sandbox().unenforced_network() {
                println!();
                println!("  ⚠ sandbox.network is declared but NOT enforced (#35)");
                println!("    declared: {}", declared.join(", "));
                println!("    This agent has ambient network access — it can reach any");
                println!("    host regardless. Enforcement: #208 (proxy), #209 (ADR-0010).");
            }
            Ok(())
        }
        Err(err) => Err(anyhow::anyhow!("{} is invalid: {err}", path.display())),
    }
}

/// Trigger an agent by name. Loads the registry, resolves the agent,
/// connects to NATS, loads the pricing table, then drives the
/// reducer runner against a real LLM client.
pub(crate) async fn trigger_agent(
    global: &GlobalArgs,
    agent_name: &str,
    payload: Option<&str>,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;

    // Resolve and load the registry.
    let registry = AgentRegistry::load_from_directory(
        &config.agents.directory,
        config.agents.default_model.as_deref(),
    )
    .context("failed to load agent registry")?;
    let agent_id =
        AgentId::new(agent_name).with_context(|| format!("invalid agent name '{agent_name}'"))?;
    let loaded = registry.get_loaded(&agent_id).ok_or_else(|| {
        let available: Vec<String> = registry
            .iter()
            .map(|l| l.agent.id().as_str().to_string())
            .collect();
        anyhow::anyhow!(
            "agent '{agent_name}' not found in {}. Available: {}",
            config.agents.directory.display(),
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        )
    })?;
    println!(
        "Loaded agent '{}' from {}",
        loaded.agent.id(),
        loaded.path.display()
    );

    // Connect to NATS.
    println!("Connecting to NATS at {}...", config.nats.url);
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    // Load pricing (cached path on disk, fetches on startup), merge
    // config overrides, and enforce the coverage guarantee (ADR-0004).
    let cache_path = config.cache.directory.join("pricing.json");
    let pricing =
        build_validated_pricing(&config, &registry, PricingTable::load(&cache_path).await)?;
    let pricing = Arc::new(pricing);
    println!("Loaded {} pricing entries", pricing.len());

    // Real LLM client — genai resolves API keys from provider-specific
    // environment variables (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc).
    // Routes each model to the [providers.<name>] that declares it,
    // honouring per-provider base_url/api_key_env (ADR-0003).
    let llm = GenAiClient::from_providers(&config.providers);
    // Retry transient LLM errors (rate limits, transport failures) with
    // backoff instead of failing the whole invocation (issue #10).
    let llm = fq_runtime::llm::RetryingLlmClient::new(llm, config.worker.llm_retry.clone());

    // Parse trigger payload: try JSON first, fall back to string literal.
    let trigger_payload: Value = match payload {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
        None => Value::Null,
    };

    // Build tool registry: built-ins + MCP servers declared by this agent.
    let mut tools = ToolRegistry::with_builtins_exec(config.tools.exec.to_exec_config());
    let mut mcp_manager = McpClientManager::new();
    for decl in loaded.agent.mcp_servers() {
        // A server the agent grants an inbound capability (sampling /
        // elicitation / roots) runs per-invocation, wired by the runner
        // (ADR-0018) — not shared here.
        if loaded.agent.grants_inbound_capability(&decl.server) {
            continue;
        }
        if let Err(err) = register_mcp_server(&mut tools, &mut mcp_manager, decl).await {
            tracing::warn!(
                server = %decl.server,
                error = %err,
                "failed to start MCP server, its tools will be unavailable"
            );
        }
    }
    if !loaded.agent.mcp_servers().is_empty() {
        println!(
            "  MCP tools:        {} (from {} server(s))",
            tools.len() - fq_runtime::tools::BUILTIN_TOOL_COUNT,
            loaded.agent.mcp_servers().len()
        );
    }

    let tools = Arc::new(tools);
    println!("Running agent...");
    // Tool/LLM dispatches are persisted through the worker
    // WAL. The store opens against the same worker.db the
    // daemon would use; if `fq run` is also active the same
    // file is shared (locks at the SQLite layer).
    let db_paths = ensure_split_dbs(&config).await?;
    let worker_store = Arc::new(
        // allow-direct-store-open: `fq trigger` is a one-shot worker — it writes the WAL.
        fq_runtime::WorkerStore::open(&db_paths.worker)
            .await
            .with_context(|| {
                format!(
                    "failed to open worker store at {}",
                    db_paths.worker.display()
                )
            })?,
    );
    // Each ad-hoc `fq trigger` is a one-shot worker instance.
    // The runtime-id-shaped worker_id matches the daemon's
    // naming so any archive hand-off the runner performs
    // routes the ack subject
    // (`fq.worker.{id}.invocation.archive_acked`) consistently.
    let cli_worker_id = fq_runtime::worker::WorkerId::new(uuid::Uuid::now_v7().to_string())
        .expect("uuid is a valid worker id");
    let runner = fq_runtime::ReducerRunner::new(
        Arc::new(
            fq_runtime::ReducerContext::builder()
                .tools(tools)
                .resources(mcp_manager.resource_reader())
                .build(),
        ),
        Arc::new(
            fq_runtime::RunnerConfig::builder()
                .bus(bus)
                .pricing(pricing)
                .store(worker_store)
                .worker_id(cli_worker_id)
                .max_iterations(config.max_iterations)
                .enforce_pricing(true)
                .workspace(workspace_provider(&config))
                .build(),
        ),
        fq_runtime::Harness::new(),
    );
    let outcome_result = runner
        .run(
            &loaded.agent,
            &llm,
            TriggerSource::Manual,
            None,
            trigger_payload,
        )
        .await;
    let outcome = match outcome_result {
        Ok(outcome) => outcome,
        Err(fq_runtime::ExecutorError::Llm(fq_runtime::LlmError::Auth(msg))) => {
            mcp_manager.shutdown().await;
            anyhow::bail!(
                "LLM authentication failed.\n\
                 \n\
                 This usually means the provider-specific API key environment\n\
                 variable is not set. For Anthropic, export ANTHROPIC_API_KEY\n\
                 before running `fq trigger`.\n\
                 \n\
                 Underlying error: {msg}"
            );
        }
        Err(err) => {
            mcp_manager.shutdown().await;
            return Err(err.into());
        }
    };

    mcp_manager.shutdown().await;

    println!();
    match outcome {
        InvocationOutcome::Completed {
            invocation_id,
            response,
            cost,
            duration_ms,
        } => {
            println!("✓ Completed in {duration_ms}ms (cost: ${cost:.6})");
            println!("  invocation: {invocation_id}");
            if let Some(content) = response.content {
                println!();
                println!("Response:");
                for line in content.lines() {
                    println!("  {line}");
                }
            }
        }
        InvocationOutcome::BudgetExceeded {
            invocation_id,
            cost,
        } => {
            println!("✗ Budget exceeded: cost ${cost:.6}");
            println!("  invocation: {invocation_id}");
        }
        InvocationOutcome::Suspended { invocation_id } => {
            // A drain suspended the run at a step boundary; the row
            // stays in-flight for recovery to resume under the next
            // binary. An in-process `fq trigger` has no drain source, so
            // this is effectively unreachable here — but the match is
            // total.
            println!("⏸ Suspended at a step boundary (drained); recovery will resume it");
            println!("  invocation: {invocation_id}");
        }
    }

    Ok(())
}

/// Tail the event stream from NATS, formatting each event as a single
/// readable line.
pub(crate) async fn tail_events(global: &GlobalArgs, subject: &str) -> anyhow::Result<()> {
    let config = global.resolve_config()?;

    println!("Connecting to NATS at {}...", config.nats.url);
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    println!("Subscribing to {subject}");
    println!("Press Ctrl-C to exit.");
    println!();

    let mut stream = bus
        .subscribe(subject.to_string())
        .await
        .context("failed to subscribe to events")?;

    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => print_event(&event),
            Err(err) => eprintln!("deserialise error: {err}"),
        }
    }

    Ok(())
}

/// Format one event as a single readable line.
pub(crate) fn print_event(event: &Event) {
    let timestamp = event.envelope.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let invocation = event.envelope.invocation_id.as_simple().to_string();
    let invocation_short: String = invocation.chars().take(8).collect();

    let summary = match &event.payload {
        EventPayload::Triggered(p) => format!("triggered source={:?}", p.trigger_source),
        EventPayload::InvocationSummary(p) => {
            format!("invocation.summary [{:?}] {}", p.kind, p.summary)
        }
        EventPayload::LlmRequest(p) => format!(
            "llm.request model={} messages={}",
            p.model,
            p.messages.len()
        ),
        EventPayload::LlmResponse(p) => {
            // Cost rides on the llm.response envelope (envelope-refactor
            // plan step 3). Render it inline when present so the
            // operator gets the same per-call cost visibility the
            // separate cost event used to provide.
            let cost_suffix = event
                .envelope
                .cost
                .as_ref()
                .map(|c| {
                    format!(
                        " cost=${:.6} cumulative=${:.6}",
                        c.total_cost, c.cumulative_invocation_cost
                    )
                })
                .unwrap_or_default();
            format!(
                "llm.response tokens={}/{} stop={:?}{cost_suffix}",
                p.usage.input_tokens, p.usage.output_tokens, p.stop_reason
            )
        }
        EventPayload::ToolCall(p) => format!("tool.call {}", p.tool_name),
        EventPayload::ToolDispatched(p) => format!("tool.dispatched {}", p.tool_name),
        EventPayload::LlmDispatched(p) => format!("llm.dispatched model={}", p.model),
        EventPayload::ToolResult(p) => {
            format!("tool.result {}", if p.is_error { "error" } else { "ok" })
        }
        EventPayload::HostNotice(p) => format!("host.notice kind={} {}", p.kind, p.body),
        EventPayload::Completed(p) => format!(
            "completed duration={}ms cost=${:.6}",
            p.total_duration_ms, p.total_cost
        ),
        EventPayload::Failed(p) => {
            format!("failed {:?} {}", p.error_kind, p.error_message)
        }
        EventPayload::InvocationAmbiguous(p) => format!(
            "invocation.ambiguous entity={} call_id={}",
            p.stuck_entity, p.stuck_call_id
        ),
        EventPayload::InvocationArchived(p) => format!(
            "invocation.archived worker_id={} phase={}",
            p.worker_id, p.final_phase
        ),
        EventPayload::InvocationArchiveAcked(p) => {
            format!("invocation.archive_acked worker_id={}", p.worker_id)
        }
        EventPayload::SystemStartup(p) => format!(
            "system.startup version={} agents={} nats={}",
            p.version, p.agents_loaded, p.nats_url
        ),
        EventPayload::SystemShutdown(p) => {
            format!("system.shutdown reason={} clean={}", p.reason, p.clean)
        }
        EventPayload::SystemRecovery(p) => format!(
            "system.recovery total={} safe_resume={} safe_replay={} ambiguous={}",
            p.total, p.safe_resume, p.safe_replay, p.ambiguous
        ),
        EventPayload::SystemTaskFailed(p) => format!(
            "system.task_failed task={} error={}",
            p.task_name, p.error_message
        ),
        EventPayload::WorkerHeartbeat(p) => format!("worker.heartbeat worker_id={}", p.worker_id),
        EventPayload::WorkerOrphaned(p) => format!(
            "worker.orphaned worker_id={} last_heartbeat_ms={}",
            p.worker_id, p.last_heartbeat_ms
        ),
        EventPayload::McpServerLog(p) => {
            format!("mcp.log server={} level={} {}", p.server, p.level, p.data)
        }
        EventPayload::InvocationOperatorRecovered(p) => format!(
            "invocation.operator_recovered action={} phase={}{}",
            p.action,
            p.final_phase,
            p.reason
                .as_deref()
                .map(|r| format!(" reason={r:?}"))
                .unwrap_or_default()
        ),
    };

    println!(
        "{timestamp} [{invocation_short}] {agent}: {summary}",
        agent = event.envelope.agent_id
    );
}

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
pub(crate) struct StatusReport {
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
