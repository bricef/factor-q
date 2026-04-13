use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use fq_runtime::agent::{definition::parse_agent, AgentId, AgentRegistry};
use fq_runtime::events::{
    Event, EventPayload, SystemShutdownPayload, SystemStartupPayload, SystemTaskFailedPayload,
    TriggerSource,
};
use fq_runtime::executor::InvocationOutcome;
use fq_runtime::llm::{GenAiClient, LlmClient};
use fq_runtime::projection::store::EventFilter;
use fq_runtime::{
    AgentExecutor, Config, EventBus, McpClientManager, McpServerConfig, PricingTable,
    ProjectionConsumer, ProjectionStore, ToolRegistry, TriggerDispatcher,
};
use uuid::Uuid;
use futures::StreamExt;
use serde_json::Value;
use tracing::error;
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_CONFIG_PATH: &str = "fq.toml";

#[derive(Parser)]
#[command(name = "fq", about = "factor-q agent runtime")]
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
    /// Trigger an agent manually
    Trigger {
        /// Agent name
        agent: String,
        /// Optional payload (JSON or plain string)
        payload: Option<String>,
        /// Publish the trigger to NATS (fq.trigger.<agent>) and let a
        /// running `fq run` daemon dispatch it, instead of running
        /// the executor in-process.
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
            } => query_events(&cli.global, agent.as_deref(), event_type.as_deref(), since.as_deref(), limit).await?,
        },
        Commands::Costs { agent, since } => {
            show_costs(&cli.global, agent.as_deref(), since.as_deref()).await?
        }
        Commands::Status => show_status(&cli.global).await?,
    }
    Ok(())
}

/// Template files embedded in the binary. Each entry is `(destination,
/// contents)` and is written verbatim when `fq init` runs.
const FQ_TOML_TEMPLATE: &str = include_str!("templates/fq.toml");
const README_TEMPLATE: &str = include_str!("templates/README.md");
const SAMPLE_AGENT_TEMPLATE: &str = include_str!("templates/sample-agent.md");

/// Initialise a new factor-q project in the current working directory.
///
/// Writes three files (plus an `agents/` directory):
/// - `fq.toml`
/// - `README.md`
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
    write_file(&sample_agent, SAMPLE_AGENT_TEMPLATE)?;

    println!("Initialised factor-q project in {}", cwd.display());
    println!();
    println!("Created:");
    println!("  fq.toml");
    println!("  README.md");
    println!("  agents/");
    println!("  agents/sample-agent.md");
    println!();
    println!("Next steps:");
    println!("  1. Start a NATS server with JetStream enabled");
    println!("     (see README.md for the deployment guide link)");
    println!("  2. Export your LLM provider API key, e.g.:");
    println!("     export ANTHROPIC_API_KEY='sk-ant-...'");
    println!("  3. Trigger the sample agent:");
    println!("     fq trigger sample-agent \"Say hello in one sentence.\"");
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))
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
/// connects to NATS, loads the pricing table, then runs the executor
/// with a stub LLM client that returns a canned response (until the
/// genai adapter is wired in).
async fn trigger_agent(
    global: &GlobalArgs,
    agent_name: &str,
    payload: Option<&str>,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;

    // Resolve and load the registry.
    let registry = AgentRegistry::load_from_directory(&config.agents.directory)
        .context("failed to load agent registry")?;
    let agent_id = AgentId::new(agent_name).with_context(|| format!("invalid agent name '{agent_name}'"))?;
    let loaded = registry
        .get_loaded(&agent_id)
        .ok_or_else(|| {
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
    let llm = GenAiClient::new();

    // Parse trigger payload: try JSON first, fall back to string literal.
    let trigger_payload: Value = match payload {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
        None => Value::Null,
    };

    // Build tool registry: built-ins + MCP servers declared by this agent.
    let mut tools = ToolRegistry::with_builtins();
    let mut mcp_manager = McpClientManager::new();
    for decl in loaded.agent.mcp_servers() {
        let config = McpServerConfig {
            name: decl.server.clone(),
            command: decl.command.clone(),
            args: decl.args.clone(),
            env: decl.env.clone(),
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
            tools.len() - 3, // subtract the 3 builtins
            loaded.agent.mcp_servers().len()
        );
    }

    let tools = Arc::new(tools);
    let executor = AgentExecutor::new(bus, pricing, tools);
    println!("Running agent...");
    let outcome = match executor
        .run(
            &loaded.agent,
            &llm,
            TriggerSource::Manual,
            None,
            trigger_payload,
        )
        .await
    {
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
    let timestamp = event.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let invocation = event.invocation_id.as_simple().to_string();
    let invocation_short: String = invocation.chars().take(8).collect();

    let summary = match &event.payload {
        EventPayload::Triggered(p) => format!("triggered source={:?}", p.trigger_source),
        EventPayload::LlmRequest(p) => format!(
            "llm.request model={} messages={}",
            p.model,
            p.messages.len()
        ),
        EventPayload::LlmResponse(p) => format!(
            "llm.response tokens={}/{} stop={:?}",
            p.usage.input_tokens, p.usage.output_tokens, p.stop_reason
        ),
        EventPayload::ToolCall(p) => format!("tool.call {}", p.tool_name),
        EventPayload::ToolResult(p) => format!(
            "tool.result {}",
            if p.is_error { "error" } else { "ok" }
        ),
        EventPayload::Cost(p) => format!(
            "cost ${:.6} cumulative=${:.6}",
            p.total_cost, p.cumulative_invocation_cost
        ),
        EventPayload::Completed(p) => format!(
            "completed duration={}ms cost=${:.6}",
            p.total_duration_ms, p.total_cost
        ),
        EventPayload::Failed(p) => {
            format!("failed {:?} {}", p.error_kind, p.error_message)
        }
        EventPayload::SystemStartup(p) => format!(
            "system.startup version={} agents={} nats={}",
            p.version, p.agents_loaded, p.nats_url
        ),
        EventPayload::SystemShutdown(p) => format!(
            "system.shutdown reason={} clean={}",
            p.reason, p.clean
        ),
        EventPayload::SystemTaskFailed(p) => format!(
            "system.task_failed task={} error={}",
            p.task_name, p.error_message
        ),
    };

    println!(
        "{timestamp} [{invocation_short}] {agent}: {summary}",
        agent = event.agent_id
    );
}

/// Default location for the SQLite projection database, relative to
/// the configured cache directory. Stored next to the pricing JSON
/// rather than in its own subdirectory — one file, one location.
fn projection_path(config: &Config) -> PathBuf {
    config.cache.directory.join("events.db")
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
    let client = match async_nats::connect(&config.nats.url).await {
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
    Ok(())
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

    // Open the projection store.
    let db_path = projection_path(&config);
    println!("  projection db:    {}", db_path.display());
    let store = Arc::new(
        ProjectionStore::open(&db_path)
            .await
            .with_context(|| format!("failed to open projection at {}", db_path.display()))?,
    );

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
            let config = McpServerConfig {
                name: decl.server.clone(),
                command: decl.command.clone(),
                args: decl.args.clone(),
                env: decl.env.clone(),
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
    let mcp_tool_count = tools.len() - 3; // subtract the 3 builtins
    if mcp_tool_count > 0 {
        println!("  MCP tools:        {mcp_tool_count}");
    }

    let tools = Arc::new(tools);
    let llm: Arc<dyn LlmClient> = Arc::new(GenAiClient::new());
    let executor = Arc::new(AgentExecutor::new(bus.clone(), pricing, tools));

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

    // Spawn the trigger dispatcher.
    let (disp_shutdown_tx, disp_shutdown_rx) = tokio::sync::oneshot::channel();
    let dispatcher = TriggerDispatcher::new(bus.clone(), registry, executor, llm);
    let mut dispatcher_handle = tokio::spawn(async move { dispatcher.run(disp_shutdown_rx).await });

    println!();
    println!("Runtime ready. Press Ctrl-C to stop.");
    println!("  - projection consumer is materialising events into SQLite");
    println!("  - trigger dispatcher is listening on fq.trigger.*");

    // Wait for either a Ctrl-C or one of the hosted tasks exiting
    // prematurely. We watch the task handles in the same select so
    // a silent-failing task is caught immediately instead of at
    // shutdown time.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let (shutdown_reason, clean_exit, failed_task): (&'static str, bool, Option<(&'static str, String)>) = tokio::select! {
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
        result = &mut dispatcher_handle => {
            let err_msg = describe_task_result("trigger dispatcher", result);
            ("task_failed", false, Some(("trigger_dispatcher", err_msg)))
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

    // Signal both tasks to shut down. Either one may already be
    // done (the one that returned from the select), but sending
    // on a oneshot whose receiver was dropped is a no-op.
    let _ = proj_shutdown_tx.send(());
    let _ = disp_shutdown_tx.send(());

    match tokio::time::timeout(std::time::Duration::from_secs(5), projection_handle).await {
        Ok(Ok(Ok(()))) => println!("  projection consumer stopped cleanly."),
        Ok(Ok(Err(err))) => tracing::error!(error = %err, "projection consumer exited with error"),
        Ok(Err(err)) => tracing::error!(error = %err, "projection consumer task panicked"),
        Err(_) => tracing::warn!("projection consumer did not shut down within 5s"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), dispatcher_handle).await {
        Ok(Ok(Ok(()))) => println!("  trigger dispatcher stopped cleanly."),
        Ok(Ok(Err(err))) => tracing::error!(error = %err, "trigger dispatcher exited with error"),
        Ok(Err(err)) => tracing::error!(error = %err, "trigger dispatcher task panicked"),
        Err(_) => tracing::warn!("trigger dispatcher did not shut down within 5s"),
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
        "{:<20} {:<40} {:<14} {:<12} {}",
        "timestamp", "agent", "event", "cost", "invocation"
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
        "{:<30} {:<10} {:<14} {:<14} {}",
        "agent", "events", "input_tokens", "output_tokens", "total_cost"
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
