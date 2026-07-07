use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use fq_runtime::agent::{AgentId, AgentRegistry, definition::parse_agent};
use fq_runtime::control_plane::projection::store::EventFilter;
use fq_runtime::events::{
    Event, EventPayload, SystemShutdownPayload, SystemStartupPayload, SystemTaskFailedPayload,
    TriggerSource,
};
use fq_runtime::llm::{GenAiClient, LlmClient};
use fq_runtime::worker::InvocationOutcome;
use fq_runtime::{
    Config, EventBus, McpClientManager, McpServerConfig, PricingTable, ProjectionConsumer,
    ProjectionStore, SharedRegistry, ToolRegistry, TriggerDispatcher,
};
use futures::StreamExt;
use serde_json::Value;
use tracing::error;
use tracing_subscriber::{EnvFilter, fmt};
use uuid::Uuid;

const DEFAULT_CONFIG_PATH: &str = "fq.toml";

#[derive(Parser)]
#[command(
    name = "fq",
    about = "factor-q agent runtime",
    version,
    long_version = env!("FQ_VERSION_LONG")
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,

    #[command(subcommand)]
    command: Commands,
}

/// Global arguments available on every subcommand. Each flag has a
/// corresponding environment variable, and together they override values
/// loaded from the config file.
///
/// Precedence: CLI flag > env var > config file > default.
#[derive(Args, Clone)]
struct GlobalArgs {
    /// Path to the factor-q config file
    #[arg(long, env = "FQ_CONFIG", default_value = DEFAULT_CONFIG_PATH, global = true)]
    config: PathBuf,

    /// Override the agents directory from config
    #[arg(long, env = "FQ_AGENTS_DIR", global = true)]
    agents_dir: Option<PathBuf>,

    /// Override the NATS URL from config
    #[arg(long, env = "FQ_NATS_URL", global = true)]
    nats_url: Option<String>,

    /// Override the cache directory from config
    #[arg(long, env = "FQ_CACHE_DIR", global = true)]
    cache_dir: Option<PathBuf>,
}

impl GlobalArgs {
    /// Load the config file (or defaults) and apply CLI/env overrides on top.
    fn resolve_config(&self) -> anyhow::Result<Config> {
        let mut config = Config::load_or_default(&self.config)?;
        if let Some(dir) = &self.agents_dir {
            config.agents.directory = dir.clone();
        }
        if let Some(url) = &self.nats_url {
            config.nats.url = url.clone();
        }
        if let Some(dir) = &self.cache_dir {
            config.cache.directory = dir.clone();
        }
        Ok(config)
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Initialise a new factor-q project in the current directory
    Init {
        /// Overwrite existing files if they already exist
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Run the runtime in the foreground
    Run,
    /// Ask a running `fq run` daemon to hot-reload its agent
    /// definitions from disk, without a restart. Publishes a
    /// control message on `fq.control.reload`; the daemon re-reads
    /// the agents directory and atomically swaps the registry the
    /// dispatcher reads. The reload affects the NEXT trigger only
    /// — in-flight invocations keep the config they snapshotted at
    /// trigger time (ADR-0020 refresh-between-invocations).
    Reload,
    /// Poll a GitHub repo's open issues and, for each one labelled
    /// `ready`, relabel it `ready`->`in-progress` and publish a
    /// trigger for the target agent (issue #6). The relabel is the
    /// idempotency mechanism — a re-seen issue is no longer `ready`,
    /// so it cannot double-fire. Runs a slow poll loop (default 60s)
    /// until Ctrl-C. Configure via `[watcher]` in fq.toml; the CLI
    /// flags below override the config.
    Watch {
        /// `owner/name` of the repo to poll (overrides `[watcher] repo`).
        #[arg(long)]
        repo: Option<String>,
        /// Poll interval in seconds (clamped up to a 60s floor).
        #[arg(long)]
        interval: Option<u64>,
        /// Run a single poll and exit, instead of looping. Useful for
        /// a cron-driven deployment or a smoke check.
        #[arg(long)]
        once: bool,
    },
    /// Trigger an agent manually
    Trigger {
        /// Agent name
        agent: String,
        /// Optional payload (JSON or plain string)
        payload: Option<String>,
        /// Publish the trigger to NATS (`fq.trigger.<agent>`) and let a
        /// running `fq run` daemon dispatch it, instead of running
        /// the runner in-process.
        #[arg(long)]
        via_nats: bool,
    },
    /// Agent management commands
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Event inspection commands
    Events {
        #[command(subcommand)]
        command: EventCommands,
    },
    /// Show cost breakdown
    Costs {
        /// Filter by agent
        #[arg(long)]
        agent: Option<String>,
        /// Filter by time
        #[arg(long)]
        since: Option<String>,
    },
    /// Show a health overview of the runtime (NATS, streams,
    /// consumers, projection)
    Status,
    /// Invocation triage commands
    Invocation {
        #[command(subcommand)]
        command: InvocationCommands,
    },
    /// Worker inspection commands
    Workers {
        #[command(subcommand)]
        command: WorkerCommands,
    },
    /// Print version and build information
    Version {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkerCommands {
    /// List workers from the coordination store.
    List {
        /// Show only stale workers (last heartbeat past the
        /// configured threshold).
        #[arg(long, conflicts_with = "alive_only")]
        stale_only: bool,
        /// Show only alive workers.
        #[arg(long, conflicts_with = "stale_only")]
        alive_only: bool,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Show one worker's detail: host, status, heartbeat age,
    /// and current in-flight invocation count.
    Show {
        /// Worker id to inspect.
        id: String,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// List registered agent definitions
    List,
    /// Validate an agent definition file
    Validate {
        /// Path to agent definition
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum InvocationCommands {
    /// List invocations from the coordination store. By default
    /// shows in-flight, ambiguous, completed, and failed rows;
    /// use `--include-archived` to also show fully-archived
    /// invocations.
    List {
        /// Filter by ownership status. Accepts
        /// `in_flight | ambiguous | completed | failed`.
        #[arg(long)]
        status: Option<String>,
        /// Also list rows from `invocation_archive` (terminal
        /// invocations whose worker-side row is gone).
        #[arg(long)]
        include_archived: bool,
        /// Maximum number of rows to return.
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Show the detail of one invocation: owner row, archive
    /// row (if present), and the last few events from the
    /// projection.
    Show {
        /// Invocation id to inspect.
        id: String,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Operator-issued terminal transition for an invocation.
    /// Publishes `invocation.operator_recovered` so audit can
    /// distinguish operator-initiated terminations from
    /// worker-initiated ones. Works on any current state
    /// (kill-switch behaviour).
    Drop {
        /// Invocation id to drop.
        id: String,
        /// Free-form reason recorded on the event payload.
        #[arg(long)]
        reason: Option<String>,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum EventCommands {
    /// Tail the event stream in real time
    Tail {
        /// Subject filter (defaults to all factor-q events)
        #[arg(long, default_value = "fq.>")]
        subject: String,
    },
    /// Query the event history from the SQLite projection
    Query {
        /// Filter by agent
        #[arg(long)]
        agent: Option<String>,
        /// Filter by event type (triggered, llm_request, llm_response,
        /// tool_call, tool_result, cost, completed, failed)
        #[arg(long, name = "type")]
        event_type: Option<String>,
        /// Only show events at or after this RFC3339 timestamp
        #[arg(long)]
        since: Option<String>,
        /// Maximum number of rows to return
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Restore the default SIGPIPE disposition for query-style commands
    // so `fq status | head` dies silently like any Unix filter instead
    // of panicking on EPIPE (Rust's startup sets SIGPIPE to ignore,
    // which turns a closed pipe into a write error that `println!`
    // panics on). The daemon and the in-process trigger keep the
    // ignore disposition: long-running paths must not be killable by a
    // closed stdout, and the shell tool's child processes inherit
    // whatever disposition is in effect at spawn time.
    #[cfg(unix)]
    if !matches!(
        cli.command,
        Commands::Run | Commands::Trigger { .. } | Commands::Watch { .. }
    ) {
        // SAFETY: changing a process signal disposition before any
        // output has been written; no handler is installed, only the
        // kernel default is restored.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Commands::Init { force } => init_project(force)?,
        Commands::Run => run_daemon(&cli.global).await?,
        Commands::Reload => reload_daemon(&cli.global).await?,
        Commands::Watch {
            repo,
            interval,
            once,
        } => run_watcher(&cli.global, repo.as_deref(), interval, once).await?,
        Commands::Trigger {
            agent,
            payload,
            via_nats,
        } => {
            if via_nats {
                publish_trigger(&cli.global, &agent, payload.as_deref()).await?
            } else {
                trigger_agent(&cli.global, &agent, payload.as_deref()).await?
            }
        }
        Commands::Agent { command } => match command {
            AgentCommands::List => list_agents(&cli.global)?,
            AgentCommands::Validate { path } => validate_agent(&path)?,
        },
        Commands::Events { command } => match command {
            EventCommands::Tail { subject } => tail_events(&cli.global, &subject).await?,
            EventCommands::Query {
                agent,
                event_type,
                since,
                limit,
            } => {
                query_events(
                    &cli.global,
                    agent.as_deref(),
                    event_type.as_deref(),
                    since.as_deref(),
                    limit,
                )
                .await?
            }
        },
        Commands::Costs { agent, since } => {
            show_costs(&cli.global, agent.as_deref(), since.as_deref()).await?
        }
        Commands::Status => show_status(&cli.global).await?,
        Commands::Invocation { command } => match command {
            InvocationCommands::List {
                status,
                include_archived,
                limit,
                json,
            } => {
                invocation_list(
                    &cli.global,
                    status.as_deref(),
                    include_archived,
                    limit,
                    json,
                )
                .await?
            }
            InvocationCommands::Show { id, json } => {
                invocation_show(&cli.global, &id, json).await?
            }
            InvocationCommands::Drop { id, reason, json } => {
                invocation_drop(&cli.global, &id, reason.as_deref(), json).await?
            }
        },
        Commands::Workers { command } => match command {
            WorkerCommands::List {
                stale_only,
                alive_only,
                json,
            } => workers_list(&cli.global, stale_only, alive_only, json).await?,
            WorkerCommands::Show { id, json } => workers_show(&cli.global, &id, json).await?,
        },
        Commands::Version { json } => print_version(json),
    }
    Ok(())
}

/// Build-time version metadata, emitted by `build.rs`.
const FQ_GIT_SHA: &str = env!("FQ_GIT_SHA");
const FQ_BUILD_EPOCH: &str = env!("FQ_BUILD_EPOCH");
const FQ_TARGET: &str = env!("FQ_TARGET");

/// Print version + build information: semver, commit, build date, target.
fn print_version(json: bool) {
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
const FQ_TOML_TEMPLATE: &str = include_str!("templates/fq.toml");
const README_TEMPLATE: &str = include_str!("templates/README.md");
const SAMPLE_AGENT_TEMPLATE: &str = include_str!("templates/sample-agent.md");
const DOCKER_COMPOSE_TEMPLATE: &str = include_str!("templates/docker-compose.yml");

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
fn init_project(force: bool) -> anyhow::Result<()> {
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

fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn list_agents(global: &GlobalArgs) -> anyhow::Result<()> {
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

    let registry = AgentRegistry::load_from_directory(dir)?;

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

fn validate_agent(path: &Path) -> anyhow::Result<()> {
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
            Ok(())
        }
        Err(err) => Err(anyhow::anyhow!("{} is invalid: {err}", path.display())),
    }
}

/// Trigger an agent by name. Loads the registry, resolves the agent,
/// connects to NATS, loads the pricing table, then drives the
/// reducer runner against a real LLM client.
async fn trigger_agent(
    global: &GlobalArgs,
    agent_name: &str,
    payload: Option<&str>,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;

    // Resolve and load the registry.
    let registry = AgentRegistry::load_from_directory(&config.agents.directory)
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

    // Load pricing (cached path on disk, fetches on startup).
    let cache_path = config.cache.directory.join("pricing.json");
    let pricing = Arc::new(PricingTable::load(&cache_path).await);
    println!("Loaded {} pricing entries", pricing.len());

    // Real LLM client — genai resolves API keys from provider-specific
    // environment variables (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc).
    // Honours [providers.anthropic] base_url when set.
    let llm = match &config.providers.anthropic {
        Some(anthropic) => GenAiClient::from_anthropic_config(anthropic),
        None => GenAiClient::new(),
    };

    // Parse trigger payload: try JSON first, fall back to string literal.
    let trigger_payload: Value = match payload {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
        None => Value::Null,
    };

    // Build tool registry: built-ins + MCP servers declared by this agent.
    let mut tools = ToolRegistry::with_builtins();
    let mut mcp_manager = McpClientManager::new();
    for decl in loaded.agent.mcp_servers() {
        // A server the agent grants an inbound capability (sampling /
        // elicitation / roots) runs per-invocation, wired by the runner
        // (ADR-0018) — not shared here.
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
                    tools.register(tool);
                }
            }
            Err(err) => {
                tracing::warn!(
                    server = %decl.server,
                    error = %err,
                    "failed to start MCP server, its tools will be unavailable"
                );
            }
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
    // WAL. The store opens against the same events.db the
    // daemon would use; if `fq run` is also active the same
    // file is shared (locks at the SQLite layer).
    let db_path = projection_path(&config);
    let worker_store = Arc::new(
        fq_runtime::WorkerStore::open(&db_path)
            .await
            .with_context(|| format!("failed to open worker store at {}", db_path.display()))?,
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
    }

    Ok(())
}

/// Tail the event stream from NATS, formatting each event as a single
/// readable line.
async fn tail_events(global: &GlobalArgs, subject: &str) -> anyhow::Result<()> {
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
fn print_event(event: &Event) {
    let timestamp = event.envelope.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let invocation = event.envelope.invocation_id.as_simple().to_string();
    let invocation_short: String = invocation.chars().take(8).collect();

    let summary = match &event.payload {
        EventPayload::Triggered(p) => format!("triggered source={:?}", p.trigger_source),
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

/// Default location for the SQLite projection database, relative to
/// the configured cache directory. Stored next to the pricing JSON
/// rather than in its own subdirectory — one file, one location.
fn projection_path(config: &Config) -> PathBuf {
    config.cache.directory.join("events.db")
}

/// Best-effort host label for the worker registration row.
/// Operator-informational only — the value isn't load-bearing
/// in v1 and a placeholder is fine when no hostname is
/// available. v2 will likely prefer a syscall-backed lookup.
fn local_host_label() -> String {
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
async fn show_status(global: &GlobalArgs) -> anyhow::Result<()> {
    use async_nats::jetstream;

    let config = global.resolve_config()?;

    println!("factor-q status");
    println!();
    println!("Config");
    println!("  NATS URL:         {}", config.nats.url);
    println!("  agents dir:       {}", config.agents.directory.display());
    println!("  cache dir:        {}", config.cache.directory.display());

    // NATS.
    println!();
    println!("NATS");
    let client = match fq_runtime::bus::connect_with_url_credentials(&config.nats.url).await {
        Ok(c) => {
            println!("  connection:       ✓ connected at {}", config.nats.url);
            c
        }
        Err(err) => {
            println!("  connection:       ✗ failed: {err}");
            anyhow::bail!("cannot reach NATS at {}", config.nats.url);
        }
    };
    let js = jetstream::new(client);

    report_stream(&js, "fq-events", "fq-projector").await;
    report_stream(&js, "fq-triggers", "fq-dispatcher").await;

    // Projection.
    println!();
    println!("Projection");
    let db_path = projection_path(&config);
    println!("  path:             {}", db_path.display());
    if !db_path.exists() {
        println!("  state:            not initialised (run `fq run` to create)");
    } else {
        match ProjectionStore::open_read_only(&db_path).await {
            Ok(store) => match store.count().await {
                Ok(count) => {
                    println!("  rows:             {count}");
                }
                Err(err) => {
                    println!("  rows:             ✗ failed to query: {err}");
                }
            },
            Err(err) => {
                println!("  state:            ✗ failed to open: {err}");
            }
        }
    }

    // Recovery state (step 9). Points the operator at the
    // commands they'd need if anything is off; renders
    // "All clear." otherwise.
    println!();
    println!("Recovery state");
    if !db_path.exists() {
        println!("  (no coordination data — `fq run` has not initialised the store)");
    } else {
        match fq_runtime::ControlPlaneStore::open_read_only(&db_path).await {
            Ok(cp_store) => {
                let ambiguous = match cp_store
                    .list_invocations_with_status(
                        fq_runtime::control_plane::store::OwnerStatus::Ambiguous,
                    )
                    .await
                {
                    Ok(rows) => rows.len() as i64,
                    Err(err) => {
                        println!("  ✗ failed to count ambiguous invocations: {err}");
                        return Ok(());
                    }
                };
                let now_ms = chrono::Utc::now().timestamp_millis();
                let stale = match cp_store.list_stale_workers(now_ms, 30_000).await {
                    Ok(rows) => rows.len() as i64,
                    Err(err) => {
                        println!("  ✗ failed to count stale workers: {err}");
                        return Ok(());
                    }
                };
                print!("{}", render_recovery_guidance(ambiguous, stale));
            }
            Err(err) => {
                println!("  ✗ failed to open control-plane store: {err}");
            }
        }
    }
    Ok(())
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
             \x20\x20  -> `fq workers list --stale-only` to inspect\n"
        ));
    }
    out
}

/// Report the state of a single JetStream stream and one of its
/// durable consumers. Prints lag for the consumer if its
/// delivered sequence lags the stream's last sequence.
async fn report_stream(
    js: &async_nats::jetstream::Context,
    stream_name: &str,
    primary_consumer: &str,
) {
    println!();
    println!("Stream: {stream_name}");
    let mut stream = match js.get_stream(stream_name).await {
        Ok(s) => s,
        Err(err) => {
            println!("  state:            ✗ stream not found: {err}");
            return;
        }
    };
    let info = match stream.info().await {
        Ok(info) => info.clone(),
        Err(err) => {
            println!("  state:            ✗ failed to fetch info: {err}");
            return;
        }
    };
    println!("  messages:         {}", info.state.messages);
    println!("  bytes:            {}", human_bytes(info.state.bytes));
    println!("  first seq:        {}", info.state.first_sequence);
    println!("  last seq:         {}", info.state.last_sequence);

    // The primary consumer (fq-projector or fq-dispatcher).
    match stream
        .get_consumer::<async_nats::jetstream::consumer::pull::Config>(primary_consumer)
        .await
    {
        Ok(mut consumer) => match consumer.info().await {
            Ok(cinfo) => {
                let delivered = cinfo.delivered.stream_sequence;
                let lag = info.state.last_sequence.saturating_sub(delivered);
                let status = if lag == 0 {
                    "✓ caught up"
                } else if lag < 10 {
                    "◐ slightly behind"
                } else {
                    "✗ lagging"
                };
                println!(
                    "  consumer {primary_consumer}: {status} (delivered {}, lag {})",
                    delivered, lag
                );
                if cinfo.num_ack_pending > 0 {
                    println!("    ack pending:    {}", cinfo.num_ack_pending);
                }
                if cinfo.num_pending > 0 {
                    println!("    num pending:    {}", cinfo.num_pending);
                }
            }
            Err(err) => {
                println!("  consumer {primary_consumer}: ✗ info failed: {err}");
            }
        },
        Err(_) => {
            println!("  consumer {primary_consumer}: not present (no `fq run` has initialised it)");
        }
    }
}

fn human_bytes(bytes: u64) -> String {
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

/// Long-running foreground runtime. Connects to NATS, opens the
/// projection store, spawns two tokio tasks — the projection
/// consumer and the NATS trigger dispatcher — and waits for
/// either Ctrl-C or a premature task failure.
///
/// Lifecycle events are published on `fq.system.*` so operators
/// can see from the event stream when the daemon started, why it
/// stopped, and which hosted task (if any) failed. A task that
/// dies unexpectedly triggers an immediate shutdown of the other
/// task and a non-zero process exit, rather than silently
/// limping along with a broken dispatcher or projector.
async fn run_daemon(global: &GlobalArgs) -> anyhow::Result<()> {
    let runtime_id = Uuid::now_v7();
    let version = env!("CARGO_PKG_VERSION");

    let config = global.resolve_config()?;
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
    let registry = fq_runtime::AgentRegistry::load_from_directory(agents_dir)
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

    // Open the three stores backing v1's single-file collapse.
    // ProjectionStore (rebuildable from NATS), ControlPlaneStore
    // (coordination/schedules/archive — source of truth), and
    // WorkerStore (in-flight state and WAL — source of truth).
    // See data-architecture.md §11 for the v1→v2 split path.
    let db_path = projection_path(&config);
    println!("  projection db:    {}", db_path.display());
    let store = Arc::new(
        ProjectionStore::open(&db_path)
            .await
            .with_context(|| format!("failed to open projection at {}", db_path.display()))?,
    );
    let cp_store = Arc::new(
        fq_runtime::ControlPlaneStore::open(&db_path)
            .await
            .with_context(|| {
                format!(
                    "failed to open control-plane store at {}",
                    db_path.display()
                )
            })?,
    );
    let worker_store = Arc::new(
        fq_runtime::WorkerStore::open(&db_path)
            .await
            .with_context(|| format!("failed to open worker store at {}", db_path.display()))?,
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
                let event = Event::new(
                    agent_id,
                    uuid::Uuid::parse_str(&inv.state.invocation_id).unwrap_or_else(|_| {
                        // Fall back to a fresh uuid if the
                        // stored id ever isn't valid (shouldn't
                        // happen — every id is a v7 uuid).
                        uuid::Uuid::now_v7()
                    }),
                    EventPayload::InvocationAmbiguous(
                        fq_runtime::events::InvocationAmbiguousPayload {
                            stuck_entity: entity.to_string(),
                            stuck_call_id: call_id,
                            note:
                                "worker startup categorisation found a `dispatched` row without `completed`"
                                    .to_string(),
                        },
                    ),
                );
                if let Err(err) = bus.publish(&event).await {
                    tracing::warn!(
                        invocation_id = %inv.state.invocation_id,
                        error = %err,
                        "failed to publish invocation.ambiguous"
                    );
                }
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

    // Load pricing.
    let pricing_cache = config.cache.directory.join("pricing.json");
    let pricing = Arc::new(PricingTable::load(&pricing_cache).await);
    let pricing_entries = pricing.len() as u32;
    println!(
        "  pricing entries:  {} (cache: {})",
        pricing_entries,
        pricing_cache.display()
    );

    // Build tool registry: built-ins + MCP servers from all agents.
    let mut tools = ToolRegistry::with_builtins();
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
                        tools.register(tool);
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
    let llm: Arc<dyn LlmClient> = Arc::new(match &config.providers.anthropic {
        Some(anthropic) => GenAiClient::from_anthropic_config(anthropic),
        None => GenAiClient::new(),
    });
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
    let resume_runner: Arc<fq_runtime::ReducerRunner<fq_runtime::Harness>> =
        Arc::new(fq_runtime::ReducerRunner::new(
            context.clone(),
            Arc::new(
                fq_runtime::RunnerConfig::builder()
                    .bus(bus.clone())
                    .pricing(pricing)
                    .store(worker_store.clone())
                    .worker_id(worker_id.clone())
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
        let refresher = mcp_manager.tool_refresher();
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
        tokio::spawn(async move {
            match runner.resume(&agent, llm_arc.as_ref(), inv_id).await {
                Ok(outcome) => tracing::info!(
                    invocation_id = %inv_id,
                    ?outcome,
                    "resume completed"
                ),
                Err(err) => tracing::warn!(
                    invocation_id = %inv_id,
                    error = %err,
                    "resume failed"
                ),
            }
        });
    }
    if resume_count > 0 {
        println!("  resume tasks:     {resume_count} spawned");
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
                            Some(_) => reload_agents(&reload_registry, &reload_dir).await,
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

    // Spawn the trigger dispatcher.
    let (disp_shutdown_tx, disp_shutdown_rx) = tokio::sync::oneshot::channel();
    let dispatcher = TriggerDispatcher::new(bus.clone(), shared_registry, worker, llm);
    let mut dispatcher_handle = tokio::spawn(async move { dispatcher.run(disp_shutdown_rx).await });

    println!();
    println!("Runtime ready. Press Ctrl-C to stop.");
    println!("  - projection consumer is materialising events into SQLite");
    println!("  - trigger dispatcher is listening on fq.trigger.*");
    println!("  - control-reload listener is listening on fq.control.reload");

    // Wait for either a Ctrl-C or one of the hosted tasks exiting
    // prematurely. We watch the task handles in the same select so
    // a silent-failing task is caught immediately instead of at
    // shutdown time.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let (shutdown_reason, clean_exit, failed_task): (
        &'static str,
        bool,
        Option<(&'static str, String)>,
    ) = tokio::select! {
        res = &mut ctrl_c => {
            match res {
                Ok(()) => {
                    println!();
                    println!("Received Ctrl-C, shutting down...");
                    ("ctrl_c", true, None)
                }
                Err(err) => {
                    tracing::error!(error = %err, "failed to listen for Ctrl-C");
                    ("ctrl_c_error", false, None)
                }
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
            let err_msg = describe_task_result("trigger dispatcher", result);
            ("task_failed", false, Some(("trigger_dispatcher", err_msg)))
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

    // Signal all tasks to shut down. Any one may already be done
    // (the one that returned from the select), but sending on a
    // oneshot whose receiver was dropped is a no-op.
    let _ = proj_shutdown_tx.send(());
    let _ = coord_shutdown_tx.send(());
    let _ = hb_consumer_shutdown_tx.send(());
    let _ = hb_producer_shutdown_tx.send(());
    let _ = archive_ack_shutdown_tx.send(());
    let _ = archive_retry_shutdown_tx.send(());
    let _ = retention_shutdown_tx.send(());
    let _ = disp_shutdown_tx.send(());
    let _ = reload_shutdown_tx.send(());

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
    match tokio::time::timeout(std::time::Duration::from_secs(5), hb_consumer_handle).await {
        Ok(Ok(Ok(()))) => println!("  heartbeat consumer stopped cleanly."),
        Ok(Ok(Err(err))) => {
            tracing::error!(error = %err, "heartbeat consumer exited with error")
        }
        Ok(Err(err)) => tracing::error!(error = %err, "heartbeat consumer task panicked"),
        Err(_) => tracing::warn!("heartbeat consumer did not shut down within 5s"),
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
    match tokio::time::timeout(std::time::Duration::from_secs(5), dispatcher_handle).await {
        Ok(Ok(Ok(()))) => println!("  trigger dispatcher stopped cleanly."),
        Ok(Ok(Err(err))) => tracing::error!(error = %err, "trigger dispatcher exited with error"),
        Ok(Err(err)) => tracing::error!(error = %err, "trigger dispatcher task panicked"),
        Err(_) => tracing::warn!("trigger dispatcher did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), reload_handle).await {
        Ok(Ok(())) => println!("  control-reload listener stopped cleanly."),
        Ok(Err(err)) => tracing::error!(error = %err, "control-reload listener task panicked"),
        Err(_) => tracing::warn!("control-reload listener did not shut down within 5s"),
    }

    // Shut down MCP server processes.
    mcp_manager.shutdown().await;

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
async fn reload_agents(shared: &SharedRegistry, agents_dir: &Path) {
    match AgentRegistry::load_from_directory(agents_dir) {
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
fn describe_task_result<E: std::fmt::Display>(
    name: &str,
    result: Result<Result<(), E>, tokio::task::JoinError>,
) -> String {
    match result {
        Ok(Ok(())) => format!("{name} exited before a shutdown signal was sent"),
        Ok(Err(err)) => format!("{name} failed: {err}"),
        Err(join_err) => format!("{name} task panicked: {join_err}"),
    }
}

/// Publish a `fq.control.reload` control message so a running
/// `fq run` daemon hot-reloads its agent definitions. Fire-and-
/// forget: the message is ephemeral core-NATS, so if no daemon is
/// listening it is a silent no-op (this command still reports
/// success — it confirms the signal was published, not that a
/// daemon acted on it). Watch `fq events tail` or the daemon logs
/// to confirm the reload took effect.
async fn reload_daemon(global: &GlobalArgs) -> anyhow::Result<()> {
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

/// Run the GitHub issue watcher (`fq watch`, issue #6). Resolves the
/// `[watcher]` config with CLI overrides, connects to NATS, and either
/// runs one poll (`--once`) or the slow poll loop until Ctrl-C.
///
/// The watcher relabels each `ready` issue to `in-progress` *before*
/// publishing its trigger, which is what makes it fire exactly once.
async fn run_watcher(
    global: &GlobalArgs,
    repo_override: Option<&str>,
    interval_override: Option<u64>,
    once: bool,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let mut watcher_config = config.watcher.clone();
    if let Some(repo) = repo_override {
        watcher_config.repo = Some(repo.to_string());
    }
    if let Some(interval) = interval_override {
        watcher_config.poll_interval_secs = interval;
    }

    let repo = watcher_config.repo.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "no repo configured for the watcher. Set `[watcher] repo = \"owner/name\"` \
             in fq.toml or pass `--repo owner/name`."
        )
    })?;

    // Validate the target agent id early so a typo fails before we
    // connect to anything.
    fq_runtime::AgentId::new(&watcher_config.target_agent)
        .with_context(|| format!("invalid target_agent '{}'", watcher_config.target_agent))?;

    println!("factor-q issue watcher");
    println!("  repo:             {repo}");
    println!("  ready label:      {}", watcher_config.ready_label);
    println!("  in-progress:      {}", watcher_config.in_progress_label);
    println!("  target agent:     {}", watcher_config.target_agent);
    println!(
        "  poll interval:    {}s",
        watcher_config.effective_poll_interval_secs()
    );
    println!(
        "  max per poll:     {}",
        watcher_config.max_triggers_per_poll
    );

    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    let source = fq_runtime::GhCliIssueSource::new(repo, watcher_config.ready_label.clone());
    let watcher = fq_runtime::Watcher::new(source, bus, watcher_config);

    if once {
        let triggered = watcher.poll_once().await?;
        println!("Polled once: triggered {triggered} issue(s).");
        return Ok(());
    }

    println!();
    println!("Watching. Press Ctrl-C to stop.");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let mut handle = tokio::spawn(async move { watcher.run(shutdown_rx).await });
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    tokio::select! {
        res = &mut ctrl_c => {
            if let Err(err) = res {
                tracing::error!(error = %err, "failed to listen for Ctrl-C");
            }
            println!();
            println!("Received Ctrl-C, stopping watcher...");
            let _ = shutdown_tx.send(());
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(Ok(()))) => println!("  watcher stopped cleanly."),
                Ok(Ok(Err(err))) => tracing::error!(error = %err, "watcher exited with error"),
                Ok(Err(err)) => tracing::error!(error = %err, "watcher task panicked"),
                Err(_) => tracing::warn!("watcher did not shut down within 5s"),
            }
        }
        result = &mut handle => {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err.into()),
                Err(err) => return Err(anyhow::anyhow!("watcher task panicked: {err}")),
            }
        }
    }
    Ok(())
}

/// Query the SQLite projection for events matching the given
/// filters. Read-only — does not require the projector to be
/// currently running, only that it has been run at some point.
async fn query_events(
    global: &GlobalArgs,
    agent: Option<&str>,
    event_type: Option<&str>,
    since: Option<&str>,
    limit: i64,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    let store = ProjectionStore::open_read_only(&db_path)
        .await
        .with_context(|| {
            format!(
                "failed to open projection at {}: has `fq run` been started?",
                db_path.display()
            )
        })?;

    let filter = EventFilter {
        agent,
        event_type,
        since,
    };
    let rows = store.query_events(&filter, limit).await?;

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
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    let store = ProjectionStore::open_read_only(&db_path)
        .await
        .with_context(|| {
            format!(
                "failed to open projection at {}: has `fq run` been started?",
                db_path.display()
            )
        })?;

    let summary = store.cost_summary(agent, since).await?;
    if summary.is_empty() {
        println!("No cost events recorded.");
        return Ok(());
    }

    println!(
        "{:<30} {:<10} {:<14} {:<14} total_cost",
        "agent", "events", "input_tokens", "output_tokens"
    );
    let mut grand_total = 0.0;
    for row in summary {
        println!(
            "{:<30} {:<10} {:<14} {:<14} ${:.6}",
            row.agent_id,
            row.event_count,
            row.total_input_tokens,
            row.total_output_tokens,
            row.total_cost
        );
        grand_total += row.total_cost;
    }
    println!();
    println!("Total across all agents: ${grand_total:.6}");
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

/// Parse a `--status` filter into an `OwnerStatus`. Returns
/// `Err` on unknown values so the CLI exits with a clear
/// message rather than silently matching no rows.
fn parse_invocation_status_filter(
    s: &str,
) -> anyhow::Result<fq_runtime::control_plane::store::OwnerStatus> {
    use fq_runtime::control_plane::store::OwnerStatus;
    match s {
        "in_flight" => Ok(OwnerStatus::InFlight),
        "ambiguous" => Ok(OwnerStatus::Ambiguous),
        "completed" => Ok(OwnerStatus::Completed),
        "failed" => Ok(OwnerStatus::Failed),
        other => Err(anyhow::anyhow!(
            "unknown status filter `{other}` — try in_flight | ambiguous | completed | failed"
        )),
    }
}

#[derive(serde::Serialize, Clone)]
struct InvocationListItem {
    invocation_id: String,
    agent_id: Option<String>,
    worker_id: String,
    status: String,
    assigned_at_ms: i64,
    archived: bool,
}

/// One human-readable line for an invocation list row. Pure;
/// covered by unit tests.
fn format_invocation_list_row_human(item: &InvocationListItem) -> String {
    let inv_short: String = item.invocation_id.chars().take(8).collect();
    let agent = item.agent_id.as_deref().unwrap_or("?");
    let agent_trim: String = agent.chars().take(22).collect();
    let worker_trim: String = item.worker_id.chars().take(22).collect();
    let archived_flag = if item.archived { "yes" } else { "no" };
    format!(
        "{:<11} {:<10} {:<24} {:<24} {}",
        inv_short, item.status, agent_trim, worker_trim, archived_flag
    )
}

async fn invocation_list(
    global: &GlobalArgs,
    status: Option<&str>,
    include_archived: bool,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    let cp_store = fq_runtime::ControlPlaneStore::open_read_only(&db_path)
        .await
        .with_context(|| {
            format!(
                "failed to open control-plane store at {}",
                db_path.display()
            )
        })?;
    let proj_store = ProjectionStore::open_read_only(&db_path)
        .await
        .with_context(|| format!("failed to open projection at {}", db_path.display()))?;

    let status_filter = status.map(parse_invocation_status_filter).transpose()?;
    let owners = cp_store.list_invocations(status_filter, limit).await?;

    let mut items: Vec<InvocationListItem> = Vec::with_capacity(owners.len());
    for owner in &owners {
        let agent_id = proj_store
            .agent_id_for_invocation(&owner.invocation_id)
            .await?;
        items.push(InvocationListItem {
            invocation_id: owner.invocation_id.clone(),
            agent_id,
            worker_id: owner.worker_id.clone(),
            status: owner.status.as_str().to_string(),
            assigned_at_ms: owner.assigned_at,
            archived: false,
        });
    }

    if include_archived {
        let archives = cp_store.list_archives_recent(limit).await?;
        for arc in archives {
            if items.iter().any(|i| i.invocation_id == arc.invocation_id) {
                continue;
            }
            items.push(InvocationListItem {
                invocation_id: arc.invocation_id,
                agent_id: Some(arc.agent_id),
                worker_id: String::new(),
                status: arc.final_phase,
                assigned_at_ms: arc.archived_at,
                archived: true,
            });
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        let what = status
            .map(|s| format!("with status={s} "))
            .unwrap_or_default();
        println!("0 invocations {what}— nothing to list.");
    } else {
        println!(
            "{:<11} {:<10} {:<24} {:<24} arch",
            "invocation", "status", "agent", "worker"
        );
        for item in &items {
            println!("{}", format_invocation_list_row_human(item));
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct InvocationArchiveSummary {
    final_phase: String,
    started_at_ms: i64,
    terminal_at_ms: i64,
    archived_at_ms: i64,
}

#[derive(serde::Serialize)]
struct EventSummary {
    timestamp: String,
    event_type: String,
}

#[derive(serde::Serialize)]
struct InvocationDetail {
    invocation_id: String,
    agent_id: Option<String>,
    owner: Option<InvocationListItem>,
    archive: Option<InvocationArchiveSummary>,
    recent_events: Vec<EventSummary>,
}

async fn invocation_show(global: &GlobalArgs, id: &str, json: bool) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    let cp_store = fq_runtime::ControlPlaneStore::open_read_only(&db_path).await?;
    let proj_store = ProjectionStore::open_read_only(&db_path).await?;

    let owner = cp_store.get_invocation_owner(id).await?;
    let archive = cp_store.get_archive(id).await?;
    let agent_id = proj_store.agent_id_for_invocation(id).await?;

    if owner.is_none() && archive.is_none() && agent_id.is_none() {
        eprintln!("no invocation found with id={id}");
        std::process::exit(1);
    }

    // Limit recent events to those for this invocation. The
    // projection has no per-invocation query method today; the
    // generic query is fast enough for triage volumes.
    let recent_events: Vec<EventSummary> = proj_store
        .query_events(
            &EventFilter {
                agent: agent_id.as_deref(),
                event_type: None,
                since: None,
            },
            200,
        )
        .await?
        .into_iter()
        .filter(|e| e.invocation_id == id)
        .take(20)
        .map(|e| EventSummary {
            timestamp: e.timestamp,
            event_type: e.event_type,
        })
        .collect();

    let owner_item = owner.as_ref().map(|o| InvocationListItem {
        invocation_id: o.invocation_id.clone(),
        agent_id: agent_id.clone(),
        worker_id: o.worker_id.clone(),
        status: o.status.as_str().to_string(),
        assigned_at_ms: o.assigned_at,
        archived: false,
    });
    let archive_summary = archive.as_ref().map(|a| InvocationArchiveSummary {
        final_phase: a.final_phase.clone(),
        started_at_ms: a.started_at,
        terminal_at_ms: a.terminal_at,
        archived_at_ms: a.archived_at,
    });

    let detail = InvocationDetail {
        invocation_id: id.to_string(),
        agent_id: agent_id.clone(),
        owner: owner_item,
        archive: archive_summary,
        recent_events,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        println!("Invocation: {}", detail.invocation_id);
        if let Some(a) = &detail.agent_id {
            println!("  agent:    {a}");
        }
        if let Some(o) = &detail.owner {
            println!("  status:   {}", o.status);
            println!("  worker:   {}", o.worker_id);
        } else {
            println!("  status:   (no coordination row)");
        }
        if let Some(a) = &detail.archive {
            println!(
                "  archived: phase={} terminal_at_ms={} archived_at_ms={}",
                a.final_phase, a.terminal_at_ms, a.archived_at_ms
            );
        }
        if !detail.recent_events.is_empty() {
            println!("\nRecent events:");
            for e in &detail.recent_events {
                let ts = e.timestamp.get(..19).unwrap_or(&e.timestamp);
                println!("  {ts}  {}", e.event_type);
            }
        }
    }
    Ok(())
}

#[derive(serde::Serialize, Debug)]
struct InvocationDropResult {
    invocation_id: String,
    agent_id: String,
    event_id: String,
    reason: Option<String>,
}

/// Look up the agent for an invocation, build the
/// `invocation.operator_recovered` event with `action="drop"`,
/// publish it, and return the result struct. Extracted from the
/// CLI handler so tests can drive the publish path without
/// constructing `GlobalArgs` / config files.
async fn publish_invocation_drop(
    bus: &EventBus,
    proj_store: &ProjectionStore,
    invocation_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<InvocationDropResult> {
    let res = fq_runtime::control_plane::operator::drop_invocation(
        bus,
        proj_store,
        invocation_id,
        reason,
    )
    .await?;
    Ok(InvocationDropResult {
        invocation_id: res.invocation_id,
        agent_id: res.agent_id,
        event_id: res.event_id,
        reason: res.reason,
    })
}

async fn invocation_drop(
    global: &GlobalArgs,
    id: &str,
    reason: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    let proj_store = ProjectionStore::open_read_only(&db_path).await?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    let result = publish_invocation_drop(&bus, &proj_store, id, reason).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "Dropped invocation {id} (agent={}, event_id={}).",
            result.agent_id, result.event_id
        );
        if let Some(r) = &result.reason {
            println!("Reason: {r}");
        }
        println!("Follow with `fq invocation show {id}` to confirm the archive row.");
    }
    Ok(())
}

#[cfg(test)]
mod invocation_tests {
    use super::*;

    #[test]
    fn parse_invocation_status_filter_accepts_known_values() {
        use fq_runtime::control_plane::store::OwnerStatus;
        assert!(matches!(
            parse_invocation_status_filter("in_flight").unwrap(),
            OwnerStatus::InFlight
        ));
        assert!(matches!(
            parse_invocation_status_filter("ambiguous").unwrap(),
            OwnerStatus::Ambiguous
        ));
        assert!(matches!(
            parse_invocation_status_filter("completed").unwrap(),
            OwnerStatus::Completed
        ));
        assert!(matches!(
            parse_invocation_status_filter("failed").unwrap(),
            OwnerStatus::Failed
        ));
    }

    #[test]
    fn parse_invocation_status_filter_rejects_unknown() {
        let err = parse_invocation_status_filter("garbage").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("garbage"));
        assert!(msg.contains("in_flight"));
    }

    #[test]
    fn format_invocation_list_row_human_renders_short_id_and_truncated_fields() {
        let item = InvocationListItem {
            invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
            agent_id: Some("a".repeat(40)),
            worker_id: "worker-42".to_string(),
            status: "ambiguous".to_string(),
            assigned_at_ms: 1_700_000_000_000,
            archived: false,
        };
        let line = format_invocation_list_row_human(&item);
        assert!(line.starts_with("019e3b32"), "expected 8-char id prefix");
        assert!(line.contains("ambiguous"));
        assert!(line.contains("worker-42"));
        assert!(line.contains("no"));
        // Agent string was truncated to 22 chars.
        assert!(line.contains(&"a".repeat(22)));
        assert!(!line.contains(&"a".repeat(23)));
    }

    #[test]
    fn format_invocation_list_row_human_marks_archived() {
        let item = InvocationListItem {
            invocation_id: "inv".to_string(),
            agent_id: Some("a".to_string()),
            worker_id: String::new(),
            status: "completed".to_string(),
            assigned_at_ms: 0,
            archived: true,
        };
        let line = format_invocation_list_row_human(&item);
        assert!(
            line.trim_end().ends_with("yes"),
            "archived flag should be 'yes', got: {line:?}"
        );
    }

    #[tokio::test]
    async fn publish_invocation_drop_emits_operator_recovered_for_agent() {
        // NATS-gated end-to-end of the publish path: seed a
        // ProjectionStore with one event so the agent lookup
        // works, then call publish_invocation_drop and capture
        // the event on the agent-scoped operator_recovered
        // subject.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use fq_runtime::events::{EventPayload as EP, TriggerSource, TriggeredPayload};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("events.db");
        let proj_store = ProjectionStore::open(&db_path).await.unwrap();

        let agent_id = AgentId::new(format!("op-drop-cli-{}", Uuid::now_v7().simple())).unwrap();
        let invocation_id = Uuid::now_v7();

        // Seed one event so agent_id_for_invocation has something
        // to find. Pick triggered — the most representative
        // first event for an invocation.
        let seed = Event::new(
            agent_id.clone(),
            invocation_id,
            EP::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: serde_json::Value::Null,
                config_snapshot: fq_runtime::Agent::builder()
                    .id(agent_id.as_str())
                    .model("claude-haiku")
                    .system_prompt("test")
                    .build()
                    .unwrap()
                    .to_snapshot(),
            }),
        );
        proj_store.insert_event(&seed).await.unwrap();

        let bus = EventBus::connect(&url).await.expect("connect NATS");
        let mut sub = bus
            .subscribe(format!(
                "fq.agent.{}.invocation.operator_recovered",
                agent_id.as_str()
            ))
            .await
            .expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let result = publish_invocation_drop(
            &bus,
            &proj_store,
            &invocation_id.to_string(),
            Some("test reason"),
        )
        .await
        .expect("publish_invocation_drop");
        assert_eq!(result.agent_id, agent_id.as_str());
        assert_eq!(result.reason.as_deref(), Some("test reason"));

        let captured = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next())
            .await
            .expect("event timeout")
            .expect("stream closed")
            .expect("deserialise");
        assert_eq!(captured.envelope.invocation_id, invocation_id);
        match &captured.payload {
            EventPayload::InvocationOperatorRecovered(p) => {
                assert_eq!(p.action, "drop");
                assert_eq!(p.final_phase, "failed");
                assert_eq!(p.reason.as_deref(), Some("test reason"));
            }
            other => panic!("expected InvocationOperatorRecovered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_invocation_drop_errors_when_agent_unknown() {
        // No seeded events → no agent_id → error.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };

        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let proj_store = ProjectionStore::open(&dir.path().join("events.db"))
            .await
            .unwrap();
        let bus = EventBus::connect(&url).await.expect("connect NATS");

        let fake_inv = Uuid::now_v7().to_string();
        let err = publish_invocation_drop(&bus, &proj_store, &fake_inv, None)
            .await
            .expect_err("expected error for unknown invocation");
        let msg = format!("{err}");
        assert!(msg.contains("no events found"), "got: {msg}");
    }

    #[test]
    fn invocation_list_item_serialises_to_stable_json_shape() {
        let item = InvocationListItem {
            invocation_id: "inv-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            worker_id: "worker-1".to_string(),
            status: "in_flight".to_string(),
            assigned_at_ms: 42,
            archived: false,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["invocation_id"], "inv-1");
        assert_eq!(v["agent_id"], "agent-1");
        assert_eq!(v["worker_id"], "worker-1");
        assert_eq!(v["status"], "in_flight");
        assert_eq!(v["assigned_at_ms"], 42);
        assert_eq!(v["archived"], false);
    }
}

// ============================================================
// fq workers subcommand
// ============================================================

#[derive(serde::Serialize, Clone)]
struct WorkerListItem {
    worker_id: String,
    host: String,
    status: String,
    last_heartbeat_ms: i64,
    heartbeat_age_ms: i64,
    in_flight_count: i64,
}

/// Human-readable heartbeat age. Stays in step with the
/// stale-worker sweep threshold so the operator can eyeball
/// what's about to go stale: anything past the threshold
/// (default 30s) is rendered as `"stale"` regardless of the
/// exact age — agrees with `coordination_worker.status`.
fn format_heartbeat_age_human(age_ms: i64, stale_threshold_ms: i64) -> String {
    if age_ms < 0 {
        return "future".to_string();
    }
    if age_ms >= stale_threshold_ms {
        return "stale".to_string();
    }
    let secs = age_ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn format_worker_list_row_human(item: &WorkerListItem, stale_threshold_ms: i64) -> String {
    let age = format_heartbeat_age_human(item.heartbeat_age_ms, stale_threshold_ms);
    format!(
        "{:<28} {:<8} {:<10} {:<8} {}",
        item.worker_id, item.status, age, item.in_flight_count, item.host
    )
}

async fn workers_list(
    global: &GlobalArgs,
    stale_only: bool,
    alive_only: bool,
    json: bool,
) -> anyhow::Result<()> {
    use fq_runtime::control_plane::store::WorkerStatus;

    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    let cp_store = fq_runtime::ControlPlaneStore::open_read_only(&db_path).await?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    // The threshold the CP uses to flip a worker from alive
    // to stale; this is the same DEFAULT_STALE_THRESHOLD_MS
    // used by the coordination consumer.
    let stale_threshold_ms = 30_000_i64;

    let workers = cp_store.list_workers().await?;
    let mut items: Vec<WorkerListItem> = Vec::with_capacity(workers.len());
    for w in workers {
        if stale_only && w.status != WorkerStatus::Stale {
            continue;
        }
        if alive_only && w.status != WorkerStatus::Alive {
            continue;
        }
        let in_flight = cp_store
            .list_invocations_for_worker(&w.worker_id)
            .await?
            .into_iter()
            .filter(|o| {
                matches!(
                    o.status,
                    fq_runtime::control_plane::store::OwnerStatus::InFlight
                        | fq_runtime::control_plane::store::OwnerStatus::Ambiguous
                )
            })
            .count() as i64;
        items.push(WorkerListItem {
            worker_id: w.worker_id,
            host: w.host,
            status: w.status.as_str().to_string(),
            last_heartbeat_ms: w.last_heartbeat,
            heartbeat_age_ms: now_ms - w.last_heartbeat,
            in_flight_count: in_flight,
        });
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        println!("0 workers — nothing to list.");
    } else {
        println!(
            "{:<28} {:<8} {:<10} {:<8} host",
            "worker", "status", "hb-age", "in-flight"
        );
        for item in &items {
            println!("{}", format_worker_list_row_human(item, stale_threshold_ms));
        }
    }
    Ok(())
}

async fn workers_show(global: &GlobalArgs, id: &str, json: bool) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    let cp_store = fq_runtime::ControlPlaneStore::open_read_only(&db_path).await?;
    let stale_threshold_ms = 30_000_i64;

    let worker = cp_store.get_worker(id).await?;
    let Some(w) = worker else {
        eprintln!("no worker found with id={id}");
        std::process::exit(1);
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    let owners = cp_store.list_invocations_for_worker(id).await?;
    let in_flight = owners
        .iter()
        .filter(|o| {
            matches!(
                o.status,
                fq_runtime::control_plane::store::OwnerStatus::InFlight
                    | fq_runtime::control_plane::store::OwnerStatus::Ambiguous
            )
        })
        .count() as i64;

    let item = WorkerListItem {
        worker_id: w.worker_id.clone(),
        host: w.host.clone(),
        status: w.status.as_str().to_string(),
        last_heartbeat_ms: w.last_heartbeat,
        heartbeat_age_ms: now_ms - w.last_heartbeat,
        in_flight_count: in_flight,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&item)?);
    } else {
        println!("Worker: {}", item.worker_id);
        println!("  host:      {}", item.host);
        println!("  status:    {}", item.status);
        println!(
            "  hb-age:    {}",
            format_heartbeat_age_human(item.heartbeat_age_ms, stale_threshold_ms)
        );
        println!("  in-flight: {}", item.in_flight_count);
        if !owners.is_empty() {
            println!("\nInvocations owned:");
            for o in owners.iter().take(20) {
                let inv: String = o.invocation_id.chars().take(11).collect();
                println!("  {inv}  {}", o.status.as_str());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod workers_tests {
    use super::*;

    #[test]
    fn format_heartbeat_age_human_under_threshold_shows_seconds() {
        assert_eq!(format_heartbeat_age_human(500, 30_000), "0s");
        assert_eq!(format_heartbeat_age_human(12_345, 30_000), "12s");
        assert_eq!(format_heartbeat_age_human(59_999, 30_000), "stale");
    }

    #[test]
    fn format_heartbeat_age_human_minutes_and_hours() {
        // Stale threshold widened so the larger ages don't get
        // clobbered to "stale".
        assert_eq!(format_heartbeat_age_human(150_000, 1_000_000), "2m");
        assert_eq!(format_heartbeat_age_human(3_700_000, 10_000_000), "1h");
    }

    #[test]
    fn format_heartbeat_age_human_past_threshold_is_stale() {
        // 60s with threshold 30s.
        assert_eq!(format_heartbeat_age_human(60_000, 30_000), "stale");
    }

    #[test]
    fn format_heartbeat_age_human_handles_clock_skew() {
        // Negative age = worker's clock is ahead. Render
        // explicitly rather than displaying a nonsense
        // negative second count.
        assert_eq!(format_heartbeat_age_human(-1000, 30_000), "future");
    }

    #[test]
    fn render_recovery_guidance_all_clear() {
        let out = render_recovery_guidance(0, 0);
        assert!(out.contains("All clear"), "got: {out:?}");
        // No command hints when nothing's pending.
        assert!(
            !out.contains("fq invocation"),
            "should not hint commands: {out:?}"
        );
        assert!(
            !out.contains("fq workers"),
            "should not hint commands: {out:?}"
        );
    }

    #[test]
    fn render_recovery_guidance_for_ambiguous_only() {
        let out = render_recovery_guidance(3, 0);
        assert!(out.contains("Ambiguous invocations: 3"));
        assert!(out.contains("fq invocation list --status=ambiguous"));
        assert!(out.contains("fq invocation drop"));
        assert!(!out.contains("Stale workers"), "got: {out:?}");
        assert!(!out.contains("All clear"));
    }

    #[test]
    fn render_recovery_guidance_for_stale_only() {
        let out = render_recovery_guidance(0, 2);
        assert!(out.contains("Stale workers: 2"));
        assert!(out.contains("fq workers list --stale-only"));
        assert!(!out.contains("Ambiguous"), "got: {out:?}");
        assert!(!out.contains("All clear"));
    }

    #[test]
    fn render_recovery_guidance_for_both() {
        let out = render_recovery_guidance(1, 1);
        assert!(out.contains("Ambiguous invocations: 1"));
        assert!(out.contains("Stale workers: 1"));
        assert!(out.contains("fq invocation drop"));
        assert!(out.contains("fq workers list --stale-only"));
    }

    #[test]
    fn worker_list_item_serialises_to_stable_json_shape() {
        let item = WorkerListItem {
            worker_id: "w-1".to_string(),
            host: "host-1".to_string(),
            status: "alive".to_string(),
            last_heartbeat_ms: 1_700_000_000_000,
            heartbeat_age_ms: 1_500,
            in_flight_count: 3,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["worker_id"], "w-1");
        assert_eq!(v["host"], "host-1");
        assert_eq!(v["status"], "alive");
        assert_eq!(v["last_heartbeat_ms"], 1_700_000_000_000_i64);
        assert_eq!(v["heartbeat_age_ms"], 1_500);
        assert_eq!(v["in_flight_count"], 3);
    }
}

#[cfg(test)]
mod reload_tests {
    use super::*;
    use tempfile::tempdir;

    fn write_agent(dir: &Path, name: &str) {
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!("---\nname: {name}\nmodel: claude-haiku\nbudget: 1.0\n---\n\nTest agent."),
        )
        .unwrap();
    }

    /// A reload re-reads the agents directory and swaps the shared
    /// handle in place: the same `SharedRegistry` the dispatcher
    /// holds now points at the freshly-loaded registry, so the next
    /// trigger sees the new agent set.
    #[tokio::test]
    async fn reload_agents_swaps_in_new_definitions() {
        let dir = tempdir().unwrap();
        write_agent(dir.path(), "first");

        let initial = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert_eq!(initial.len(), 1);
        let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

        // Add a second agent on disk, then reload.
        write_agent(dir.path(), "second");
        reload_agents(&shared, dir.path()).await;

        let after = shared.read().await.clone();
        assert_eq!(after.len(), 2, "reload should pick up the new agent");
        assert!(after.get(&AgentId::new("second").unwrap()).is_some());
    }

    /// A reload against a directory that has gone missing keeps the
    /// current registry rather than blanking it — a bad edit can't
    /// knock out a running daemon.
    #[tokio::test]
    async fn reload_agents_keeps_current_registry_on_load_error() {
        let dir = tempdir().unwrap();
        write_agent(dir.path(), "keep");
        let initial = AgentRegistry::load_from_directory(dir.path()).unwrap();
        assert_eq!(initial.len(), 1);
        let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

        // Point the reload at a directory that does not exist.
        let missing = dir.path().join("does-not-exist");
        reload_agents(&shared, &missing).await;

        let after = shared.read().await.clone();
        assert_eq!(after.len(), 1, "failed reload must keep the old registry");
        assert!(after.get(&AgentId::new("keep").unwrap()).is_some());
    }
}
