use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use fq_runtime::agent::{AgentId, AgentRegistry, definition::parse_agent};
use fq_runtime::control_plane::store::WorkerStatus;
use fq_runtime::events::{
    Event, EventPayload, SystemShutdownPayload, SystemStartupPayload, SystemTaskFailedPayload,
    TriggerSource,
};
use fq_runtime::llm::{GenAiClient, LlmClient};
use fq_runtime::views::Views;
use fq_runtime::worker::{DrainReason, DrainRequest, InvocationOutcome};
use fq_runtime::{
    Config, ControlPlaneStore, EventBus, McpClientManager, McpServerConfig, PricingTable,
    ProjectionConsumer, ProjectionStore, SharedRegistry, ToolRegistry, TriggerDispatcher,
};
use futures::StreamExt;
use serde_json::Value;
use tracing::error;
use tracing_subscriber::{EnvFilter, fmt};
use uuid::Uuid;

const DEFAULT_CONFIG_PATH: &str = "fq.toml";

/// Merge `[providers.<name>.pricing]` overrides over the loaded LiteLLM
/// table, then enforce the ADR-0004 coverage guarantee: every declared
/// model is priced, and every agent model + `agents.default_model` is
/// declared. Fail-fast — the daemon refuses to run rather than let an
/// undeclared or unpriced model silently track its cost as $0 and defeat
/// budget enforcement. Returns the merged table on success.
fn build_validated_pricing(
    config: &Config,
    registry: &AgentRegistry,
    base: PricingTable,
) -> anyhow::Result<PricingTable> {
    let mut pricing = base;
    let mut overrides = 0usize;
    for (model, ov) in config.providers.pricing_overrides() {
        pricing.insert(model.to_string(), ov.to_pricing());
        overrides += 1;
    }
    if overrides > 0 {
        println!("Applied {overrides} model pricing override(s) from config");
    }
    let agent_models: Vec<(String, String)> = registry
        .iter()
        .map(|l| {
            (
                l.agent.id().as_str().to_string(),
                l.agent.model().to_string(),
            )
        })
        .collect();
    fq_runtime::config::validate_model_registry(
        &config.providers,
        config.agents.default_model.as_deref(),
        &agent_models,
        &pricing,
    )?;
    Ok(pricing)
}

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

/// How the tracing subscriber renders log lines. `Text` is the
/// human-readable ANSI default; `Json` emits one structured JSON
/// object per line for machine parsing (issue #36).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum LogFormat {
    Text,
    Json,
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

    /// Log output format for the tracing subscriber. `text` (the
    /// default) is human-readable ANSI; `json` emits one JSON object
    /// per log line for machine parsing by a log aggregator.
    #[arg(long, env = "FQ_LOG_FORMAT", value_enum, default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,
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
    /// Ask a running `fq run` daemon to drain gracefully and exit
    /// (ADR-0027). Publishes a control message on `fq.control.drain`;
    /// the daemon stops consuming new triggers and lets each in-flight
    /// invocation suspend at its next step boundary — state already on
    /// the WAL — then exits, so the next binary's recovery resumes them
    /// with no lost or re-run work. Bounded by `drain_deadline_ms`; past
    /// it the stragglers are hard-stopped and recovery picks them up.
    Drain,
    /// Cleanly stop a running `fq run` daemon and confirm it exited
    /// (issue #63) — the operator-facing stop verb, so nobody reaches
    /// for `pkill -INT`. Publishes a control message on `fq.control.down`;
    /// the daemon drains in-flight work to the next step boundary
    /// (bounded by `drain_deadline_ms`, like `fq drain`), tears down its
    /// infrastructure, deregisters the worker, and exits. This command
    /// then waits — bounded — for the daemon's `fq.system.shutdown`
    /// event and reports the runtime that stopped, or a timeout error.
    /// Use `fq drain` for a redeploy (suspend-for-handoff); use `fq down`
    /// to switch the daemon off.
    Down {
        /// Skip the drain: clean infra teardown + worker deregister +
        /// immediate exit, accepting that in-flight invocations become
        /// recoverable-on-next-start (equivalent to today's SIGINT, but
        /// as a proper confirmable command). Alias: `--no-drain`.
        #[arg(long, visible_alias = "no-drain")]
        now: bool,
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
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Show a health overview of the runtime (NATS, streams,
    /// consumers, projection)
    Status {
        /// Emit the structured report as JSON instead of the
        /// human-readable overview.
        #[arg(long)]
        json: bool,
    },
    /// Aggregate the runtime's durable-execution health signals
    /// into one operator-readable report: worker liveness,
    /// in-flight/stuck work, ambiguous invocations, and permanent
    /// failures grouped by kind. Read-only against the SQLite
    /// projection DB — no NATS round-trip, so it works with
    /// `fq run` stopped. Composes (does not duplicate) `fq status`,
    /// `fq workers list`, and `fq invocation list`.
    Doctor {
        /// Emit the structured `DoctorReport` as JSON instead of
        /// the human-readable report.
        #[arg(long)]
        json: bool,
        /// Exit non-zero when any check reports a problem, for use
        /// in `&&` health-gates and cron/monitoring. Off by default
        /// so existing scripts keep their exit-0 behaviour.
        #[arg(long)]
        fail_on_issues: bool,
    },
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
    /// Remove stale worker registrations; alive and shutdown workers are untouched.
    Prune {
        /// Report workers that would be removed without changing the store.
        #[arg(long)]
        dry_run: bool,
    },
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
    /// Show the full conversation transcript for an invocation: the
    /// LLM turns and tool calls WITH their payloads (assistant text,
    /// tool parameters, tool results), reconstructed from the worker
    /// WAL. Unlike `show`/`events query`, which print headers only.
    /// Read-only; snapshot mode needs no NATS. `--follow` appends new
    /// turns live from the event bus until Ctrl-C.
    ///
    /// NOTE: tool output is shown verbatim and is NOT redacted — a
    /// transcript may contain secrets that appeared in a tool result
    /// (e.g. a command that printed a credential). Treat it as sensitive.
    Transcript {
        /// Invocation id to inspect.
        id: String,
        /// After printing the snapshot, block and append new turns
        /// live from `fq.agent.<agent_id>.>` until Ctrl-C.
        #[arg(long, short = 'f')]
        follow: bool,
        /// Output format.
        #[arg(long, value_enum, default_value = "pretty")]
        format: TranscriptFormat,
        /// Do not truncate large payloads (alias: --no-truncate).
        #[arg(long, visible_alias = "no-truncate")]
        full: bool,
    },
}

/// Output format for `fq invocation transcript`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
enum TranscriptFormat {
    /// Human-readable text (default).
    Pretty,
    /// Machine-readable ordered JSON array (never truncated).
    Json,
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
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
}

/// Initialise the global tracing subscriber. Both branches share the
/// same `EnvFilter` wiring — `RUST_LOG` (or `info` by default) governs
/// levels identically — and differ only in how each event is rendered:
///
/// - [`LogFormat::Text`] keeps the human-readable ANSI output (the
///   default, so existing behaviour is unchanged).
/// - [`LogFormat::Json`] emits one JSON object per log line so a log
///   aggregator (ELK, Loki, Datadog) can query the structured fields
///   directly instead of regex-scraping (issue #36).
fn init_tracing(format: LogFormat) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Text => fmt().with_env_filter(env_filter).init(),
        LogFormat::Json => fmt().with_env_filter(env_filter).json().init(),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Initialise the tracing subscriber now that args are parsed, so
    // `--log-format` / FQ_LOG_FORMAT can pick the renderer. Nothing logs
    // before this point. EnvFilter / RUST_LOG wiring is identical in both
    // modes (issue #36).
    init_tracing(cli.global.log_format);

    // Restore the default SIGPIPE disposition for query-style commands
    // so `fq status | head` dies silently like any Unix filter instead
    // of panicking on EPIPE (Rust's startup sets SIGPIPE to ignore,
    // which turns a closed pipe into a write error that `println!`
    // panics on). The daemon and the in-process trigger keep the
    // ignore disposition: long-running paths must not be killable by a
    // closed stdout, and the exec tool's child processes inherit
    // whatever disposition is in effect at spawn time.
    #[cfg(unix)]
    if !matches!(cli.command, Commands::Run | Commands::Trigger { .. }) {
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
        Commands::Drain => drain_daemon(&cli.global).await?,
        Commands::Down { now } => down_daemon(&cli.global, now).await?,
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
                json,
            } => {
                query_events(
                    &cli.global,
                    agent.as_deref(),
                    event_type.as_deref(),
                    since.as_deref(),
                    limit,
                    json,
                )
                .await?
            }
        },
        Commands::Costs { agent, since, json } => {
            show_costs(&cli.global, agent.as_deref(), since.as_deref(), json).await?
        }
        Commands::Status { json } => show_status(&cli.global, json).await?,
        Commands::Doctor {
            json,
            fail_on_issues,
        } => doctor(&cli.global, json, fail_on_issues).await?,
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
            InvocationCommands::Transcript {
                id,
                follow,
                format,
                full,
            } => invocation_transcript(&cli.global, &id, follow, format, full).await?,
        },
        Commands::Workers { command } => match command {
            WorkerCommands::List {
                stale_only,
                alive_only,
                json,
            } => workers_list(&cli.global, stale_only, alive_only, json).await?,
            WorkerCommands::Show { id, json } => workers_show(&cli.global, &id, json).await?,
            WorkerCommands::Prune { dry_run } => workers_prune(&cli.global, dry_run).await?,
        },
        Commands::Version { json } => print_version(json),
    }
    Ok(())
}

/// Build-time version metadata, emitted by `build.rs`.
const FQ_GIT_SHA: &str = env!("FQ_GIT_SHA");
const FQ_BUILD_EPOCH: &str = env!("FQ_BUILD_EPOCH");
const FQ_TARGET: &str = env!("FQ_TARGET");
/// Semver + commit (valid semver build metadata), so the **running**
/// daemon reports which build it is — the `system.startup` event and
/// banner carry the SHA, not just the semver. Lets a deploy check
/// confirm the live process is on the expected commit.
const FQ_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("FQ_GIT_SHA"));

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

/// Build the `${workspace}` provider from `[workspace]` (parallel-workers
/// Phase 0): with `per_invocation = true` each invocation gets a fresh
/// empty directory under `path`; otherwise every invocation binds to
/// `path` itself. No `path` configured → no binding, and agents that use
/// the token fail loudly at invocation start. Pure filesystem either way
/// — what goes into a workspace is the agent's business.
fn workspace_provider(
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
    projection_path: PathBuf,
    initialised: bool,
    projection_rows: Option<i64>,
    recovery: Option<fq_runtime::views::RecoveryView>,
    /// First store-side failure, when any (rows/recovery unreadable).
    store_error: Option<String>,
}

async fn show_status(global: &GlobalArgs, json: bool) -> anyhow::Result<()> {
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
    let db_path = projection_path(&config);

    if json {
        let initialised = db_path.exists();
        let mut projection_rows = None;
        let mut recovery = None;
        let mut store_error = None;
        if initialised {
            match Views::open(&db_path).await {
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
            projection_path: db_path,
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

    // Projection + recovery state, over the read views.
    println!();
    println!("Projection");
    println!("  path:             {}", db_path.display());
    if !db_path.exists() {
        println!("  state:            not initialised (run `fq run` to create)");
        println!();
        println!("Recovery state");
        println!("  (no coordination data — `fq run` has not initialised the store)");
        return Ok(());
    }
    match Views::open(&db_path).await {
        Ok(views) => {
            match views.event_count().await {
                Ok(count) => println!("  rows:             {count}"),
                Err(err) => println!("  rows:             ✗ failed to query: {err}"),
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
struct DoctorWorkers {
    alive: i64,
    stale: i64,
    shutdown: i64,
    /// Worker ids currently past the stale threshold.
    stale_ids: Vec<String>,
}

/// In-flight / current-execution view, read from the worker-local
/// `invocation_state` table (the reliable live view — the CP owner
/// table's `in_flight` status is not populated by trigger dispatch
/// yet; see issue #50).
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq, Default)]
struct DoctorExecutions {
    in_flight: i64,
    /// In-flight invocations with a fresh open dispatch (tool or LLM) —
    /// actively working, however silent their WAL row (#130).
    working: i64,
    /// Short ids of the working invocations, same convention as
    /// `stuck_ids`.
    working_ids: Vec<String>,
    /// In-flight invocations whose `updated_at` is older than
    /// [`DOCTOR_STUCK_THRESHOLD_MS`].
    stuck: i64,
    /// Short ids of the stuck invocations, for triage.
    stuck_ids: Vec<String>,
}

/// Availability of the dead-letter section. Gated on issue #49:
/// the trigger consumer sets no `max_deliver` and has no DLQ /
/// advisory source, so there is nothing to query yet. `doctor`
/// renders this honestly rather than fabricating a count.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "state")]
enum DoctorDeadLetters {
    /// Not yet available; blocked on the named issue.
    PendingIssue { issue: u32 },
}

/// The full doctor report. Serialisable for `--json`; built by the
/// pure [`build_doctor_report`] so the checks are unit-testable
/// without a live DB.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
struct DoctorReport {
    workers: DoctorWorkers,
    executions: DoctorExecutions,
    /// Ambiguous invocations needing operator triage (CP owner
    /// table, `status='ambiguous'`).
    ambiguous: i64,
    /// Terminal failures grouped by `FailureKind` (from the
    /// projection `events` table, `event_type='failed'`).
    failures: Vec<DoctorFailure>,
    dead_letters: DoctorDeadLetters,
}

/// One failure-kind bucket in the report. Mirrors
/// [`fq_runtime::views::FailureView`] but owns its data so the report
/// is a self-contained serialisable value.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
struct DoctorFailure {
    error_kind: String,
    count: i64,
}

impl DoctorReport {
    /// Total terminal failures across all kinds.
    fn failure_total(&self) -> i64 {
        self.failures.iter().map(|f| f.count).sum()
    }

    /// True when any check reports a problem worth an operator's
    /// attention: stale workers, stuck in-flight work, ambiguous
    /// invocations, or permanent failures. In-flight work that is
    /// merely running (not stuck) is healthy, not an issue.
    fn has_issues(&self) -> bool {
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
fn build_doctor_report(
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

    let failures = failures
        .iter()
        .map(|f| DoctorFailure {
            error_kind: f.error_kind.clone(),
            count: f.count,
        })
        .collect();

    DoctorReport {
        workers: w,
        executions: ex,
        ambiguous,
        failures,
        // Gated: the dead-letter source does not exist until #49.
        dead_letters: DoctorDeadLetters::PendingIssue { issue: 49 },
    }
}

/// Pure: render the human-readable `fq doctor` report, mirroring
/// `render_recovery_guidance` — an overall verdict, then per-failing-
/// check the count plus the copy-paste next-step command. Returns
/// `All clear.` when every check is green (the dead-letter line is
/// always shown as pending #49 — it is informational, not a problem).
fn render_doctor_report_human(report: &DoctorReport) -> String {
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

    // Dead-letters — gated on #49; never a fabricated count.
    match report.dead_letters {
        DoctorDeadLetters::PendingIssue { issue } => {
            out.push_str(&format!(
                "Dead-letters: n/a (not yet available, pending #{issue})\n"
            ));
        }
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
async fn doctor(global: &GlobalArgs, json: bool, fail_on_issues: bool) -> anyhow::Result<()> {
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
    let cp_store = Arc::new(ControlPlaneStore::open(&db_path).await.with_context(|| {
        format!(
            "failed to open control-plane store at {}",
            db_path.display()
        )
    })?);
    // Pool ceiling scales with the fan-out bound (#70): each
    // dispatcher-run invocation is WAL-chatty, plus headroom for the
    // sweepers. Startup recovery is NOT covered — it spawns one resume
    // per recoverable invocation, unbounded, sharing this pool — so a
    // large post-crash backlog queues on pool acquisition (sqlx queues
    // rather than errors up to its acquire timeout). SQLite serialises
    // the writes regardless; the ceiling only bounds waiting.
    let pool_ceiling = (config.worker.max_concurrent_invocations as u32 + 3).max(4);
    let worker_store = Arc::new(
        fq_runtime::WorkerStore::open_with_pool(&db_path, pool_ceiling)
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
                    .pricing(pricing)
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
        resume_handles.push(tokio::spawn(async move {
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

    // Spawn the control-drain listener (ADR-0027). On a `fq.control.drain`
    // message it flips the shared drain signal — suspending in-flight
    // invocations at their next step boundary and stopping the dispatcher
    // from consuming new triggers — then signals the main select to run
    // the bounded drain and exit. Best-effort core-NATS like reload;
    // non-fatal, so its handle is not watched in the select. `drain_probe`
    // is a separate handle kept here to let the select classify a
    // dispatcher-exit-while-draining as a clean drain.
    let drain_probe: Arc<dyn fq_runtime::Worker> = resume_runner.clone();
    let (drain_requested_tx, mut drain_requested_rx) = tokio::sync::oneshot::channel::<()>();
    let (drain_listener_shutdown_tx, mut drain_listener_shutdown_rx) =
        tokio::sync::oneshot::channel::<()>();
    let drain_bus = bus.clone();
    let drain_worker: Arc<dyn fq_runtime::Worker> = resume_runner.clone();
    let drain_handle = tokio::spawn(async move {
        let mut drain_requested_tx = Some(drain_requested_tx);
        'resubscribe: loop {
            let mut sub = match drain_bus.subscribe_control_drain().await {
                Ok(sub) => sub,
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        "failed to subscribe to control drain; retrying in 5s"
                    );
                    tokio::select! {
                        biased;
                        _ = &mut drain_listener_shutdown_rx => break 'resubscribe,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                    }
                }
            };
            tokio::select! {
                biased;
                _ = &mut drain_listener_shutdown_rx => break 'resubscribe,
                msg = sub.next() => {
                    match msg {
                        Some(_) => {
                            tracing::info!(
                                "drain requested; suspending in-flight invocations at their \
                                 next step boundary and no longer consuming new triggers"
                            );
                            drain_worker
                                .request_drain(DrainRequest::new(DrainReason::Deploy))
                                .await;
                            if let Some(tx) = drain_requested_tx.take() {
                                let _ = tx.send(());
                            }
                            break 'resubscribe;
                        }
                        None => {
                            tracing::warn!("control-drain subscription ended; resubscribing");
                            continue 'resubscribe;
                        }
                    }
                }
            }
        }
    });

    // Spawn the control-down listener (`fq down`, issue #63). On a
    // `fq.control.down` message it reads the body to pick the stop mode:
    // drain (suspend in-flight work to a step boundary, then exit) or
    // `now` (clean teardown + deregister + immediate exit). It requests
    // the drain up front in drain mode — identical to the `fq drain`
    // path — then signals the main select with the chosen mode so the
    // teardown deregisters the worker and publishes `fq.system.shutdown`
    // either way. Best-effort core-NATS like reload/drain; non-fatal, so
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
        shared_registry,
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
            fq_runtime::views::Views::open(&projection_path(&config))
                .await
                .context("read service: failed to open the read views")?,
        );
        let (rs_addr, rs_serving) = fq_runtime::read_service::bind(
            &config.read_service.bind,
            views,
            bus.jetstream(),
            std::time::Duration::from_millis(config.read_service.probe_timeout_ms),
            FQ_VERSION.to_string(),
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
    println!("  - control-drain listener is listening on fq.control.drain");
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
                    // bounded-wait teardown below, exactly like `fq drain`. A
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
        // A `fq drain` control message asked for a graceful drain
        // (ADR-0027). The listener has already flipped the shared drain
        // signal — in-flight invocations are suspending and the dispatcher
        // has stopped consuming — so exit the select and run the bounded
        // drain in the teardown below.
        _ = &mut drain_requested_rx => ("drain", true, None),
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
            // before the listener's `drain_requested` signal is polled.)
            if drain_probe.drain_status() == fq_runtime::worker::DrainState::Draining {
                ("drain", true, None)
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
    // Both `fq drain` and SIGTERM run the bounded drain (SIGTERM flipped the
    // drain signal in the select above); a signal-error or task failure does
    // not.
    // `fq down` (drain mode) and `fq down --now` both exit cleanly and
    // deregister the worker; only the drain-mode variants wait out the
    // bounded drain. `down_now` is a fast clean stop like Ctrl-C.
    let drained = matches!(shutdown_reason, "drain" | "sigterm" | "down");
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
    let _ = hb_producer_shutdown_tx.send(());
    let _ = archive_ack_shutdown_tx.send(());
    let _ = archive_retry_shutdown_tx.send(());
    let _ = retention_shutdown_tx.send(());
    let _ = disp_shutdown_tx.send(());
    let _ = reload_shutdown_tx.send(());
    let _ = drain_listener_shutdown_tx.send(());
    let _ = down_listener_shutdown_tx.send(());

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
    match tokio::time::timeout(std::time::Duration::from_secs(5), drain_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::error!(error = %err, "control-drain listener task panicked"),
        Err(_) => tracing::warn!("control-drain listener did not shut down within 5s"),
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
async fn wait_for_shutdown_signal() -> &'static str {
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
async fn reload_agents(shared: &SharedRegistry, agents_dir: &Path, default_model: Option<&str>) {
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
async fn drain_daemon(global: &GlobalArgs) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
    bus.publish_control_drain()
        .await
        .context("failed to publish control drain")?;
    println!(
        "Published drain signal on {}.",
        fq_runtime::bus::CONTROL_DRAIN_SUBJECT
    );
    println!(
        "A running `fq run` daemon will stop consuming triggers, suspend in-flight \
         invocations at a step boundary, and exit; recovery resumes them on the next start."
    );
    Ok(())
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
async fn down_daemon(global: &GlobalArgs, now: bool) -> anyhow::Result<()> {
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

/// Query the SQLite projection for events matching the given
/// filters. Read-only — does not require the projector to be
/// currently running, only that it has been run at some point.
/// Open the read-only `Views` handle every CLI read command formats over
/// (the CLI is a formatter over `fq_runtime::views`, not a read layer of
/// its own — see the operator-dashboard plan, layer 1).
async fn open_views(global: &GlobalArgs) -> anyhow::Result<Views> {
    let config = global.resolve_config()?;
    let db_path = projection_path(&config);
    Views::open(&db_path).await.with_context(|| {
        format!(
            "failed to open stores at {}: has `fq run` been started?",
            db_path.display()
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
    let report = views.costs(agent, since).await?;

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

/// One human-readable line for an invocation list row. Pure;
/// covered by unit tests.
fn format_invocation_list_row_human(item: &fq_runtime::views::InvocationSummaryView) -> String {
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
    let status_filter = status.map(parse_invocation_status_filter).transpose()?;
    let views = open_views(global).await?;
    let items = views
        .invocation_index(status_filter, include_archived, limit)
        .await?;

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

async fn invocation_show(global: &GlobalArgs, id: &str, json: bool) -> anyhow::Result<()> {
    let views = open_views(global).await?;
    let Some(detail) = views.invocation(id).await? else {
        eprintln!("no invocation found with id={id}");
        std::process::exit(1);
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
        // The "what is it doing right now" block, from the worker WAL —
        // present only while the invocation is in flight.
        if let Some(live) = &detail.live {
            println!("\nLive execution:");
            println!("  phase:      {}", live.phase);
            println!("  step:       {}", live.step_index);
            println!("  updated_at: {} ms", live.updated_at_ms);
            for t in live.tools.iter().filter(|t| t.status != "completed") {
                println!("  tool:       {} [{}]", t.tool_name, t.status);
            }
            for l in live.llms.iter().filter(|l| l.status != "completed") {
                println!("  llm:        {} [{}]", l.model, l.status);
            }
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
    control_store: &ControlPlaneStore,
    invocation_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<InvocationDropResult> {
    let res = fq_runtime::control_plane::operator::drop_invocation(
        bus,
        proj_store,
        control_store,
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
    let control_store = ControlPlaneStore::open(&db_path).await?;
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    let result = publish_invocation_drop(&bus, &proj_store, &control_store, id, reason).await?;

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

/// Render the full payload-bearing transcript for one invocation.
///
/// Snapshot mode (default): open the worker WAL read-only against
/// `events.db`, collect the ordered `llm_dispatch` + `tool_dispatch`
/// rows for the invocation, and render them with payloads. Read-only
/// and NATS-free. `--follow` additionally subscribes to the invocation's
/// agent subject and appends new turns live until Ctrl-C.
async fn invocation_transcript(
    global: &GlobalArgs,
    id: &str,
    follow: bool,
    format: TranscriptFormat,
    full: bool,
) -> anyhow::Result<()> {
    use fq_runtime::transcript::{
        DEFAULT_TRUNCATE_BYTES, assistant_entry, collect_transcript, dedup_key, render_pretty,
        snapshot_keys, tool_result_entry,
    };

    let as_json = matches!(format, TranscriptFormat::Json);
    if follow && as_json {
        anyhow::bail!("--follow is not supported with --format json (json emits a snapshot array)");
    }
    let truncate_bytes = if full {
        None
    } else {
        Some(DEFAULT_TRUNCATE_BYTES)
    };

    let config = global.resolve_config()?;
    let db_path = projection_path(&config);

    // For --follow, subscribe to the invocation's agent subject BEFORE
    // reading the WAL snapshot, so a turn that completes in the gap
    // between the read and the subscription is not lost: anything
    // published in that window is caught by both the snapshot and the
    // live stream, then deduped at the seam. Snapshot-only mode needs no
    // NATS. The returned stream owns its connection, so `bus` may drop.
    let follow_stream = if follow {
        let proj_store = ProjectionStore::open_read_only(&db_path).await?;
        let agent_id = proj_store
            .agent_id_for_invocation(id)
            .await?
            .ok_or_else(|| {
                let hint = if id.len() != 36 {
                    " (not a full invocation id — see `fq invocation list --json`)"
                } else {
                    ""
                };
                anyhow::anyhow!(
                    "cannot follow invocation {id}: no agent recorded for it in the projection{hint}"
                )
            })?;
        let bus = EventBus::connect(&config.nats.url)
            .await
            .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;
        let subject = format!("fq.agent.{agent_id}.>");
        let stream = bus
            .subscribe(subject.clone())
            .await
            .with_context(|| format!("failed to subscribe to {subject}"))?;
        Some((subject, stream))
    } else {
        None
    };

    // Worker WAL snapshot, read-only. A missing DB is the actionable
    // NotInitialised error, never a panic.
    let worker_store = fq_runtime::WorkerStore::open_read_only(&db_path)
        .await
        .with_context(|| {
            format!(
                "failed to open worker store at {}: has `fq run`/`fq trigger` been run?",
                db_path.display()
            )
        })?;

    let llm_rows = worker_store.list_llm_dispatches_for_invocation(id).await?;
    let tool_rows = worker_store.list_tool_dispatches_for_invocation(id).await?;

    // An empty snapshot is a hard error only for the one-shot view; under
    // --follow it is valid (tailing an invocation that has not dispatched
    // anything yet), so fall through to the live loop.
    if llm_rows.is_empty() && tool_rows.is_empty() && !follow {
        eprintln!(
            "no transcript found for invocation id={id} (no LLM or tool dispatches recorded)"
        );
        // A full invocation id is 36 chars; `fq invocation list` shows an
        // abbreviated one, so a copied id often won't match. Point at the
        // machine-readable form that carries the full id.
        if id.len() != 36 {
            eprintln!(
                "note: `{id}` is not a full invocation id — `fq invocation list` abbreviates it; \
                 use `fq invocation list --json` to get the full id."
            );
        }
        std::process::exit(1);
    }

    let entries = collect_transcript(&llm_rows, &tool_rows);

    if as_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    print!("{}", render_pretty(&entries, truncate_bytes));

    // Snapshot-only mode: done. Otherwise take the live stream that was
    // subscribed above (before the snapshot) and tail it.
    let Some((subject, mut stream)) = follow_stream else {
        return Ok(());
    };

    println!();
    println!("── following {subject} (invocation {id}); Ctrl-C to exit ──");

    let mut seen = snapshot_keys(&entries);
    while let Some(result) = stream.next().await {
        let event = match result {
            Ok(e) => e,
            Err(err) => {
                eprintln!("deserialise error: {err}");
                continue;
            }
        };
        if event.envelope.invocation_id.to_string() != id {
            continue;
        }
        let ts_ms = event.envelope.timestamp.timestamp_millis();
        let entry = match &event.payload {
            EventPayload::LlmResponse(p) => {
                let cost = event.envelope.cost.as_ref().map(|c| c.total_cost);
                Some(assistant_entry(ts_ms, p_model(&event), cost, p))
            }
            EventPayload::ToolResult(p) => {
                // The live event carries the result; tool name/params
                // rode the earlier tool.call. Best-effort: label by the
                // correlation id when we can't recover the name.
                Some(tool_result_entry(
                    ts_ms,
                    format!("(tool_call {})", p.tool_call_id),
                    serde_json::Value::Null,
                    p,
                ))
            }
            _ => None,
        };
        if let Some(entry) = entry {
            if let Some(key) = dedup_key(&entry)
                && !seen.insert(key)
            {
                continue;
            }
            print!(
                "{}",
                render_pretty(std::slice::from_ref(&entry), truncate_bytes)
            );
        }
    }

    Ok(())
}

/// The model string for a live event, if the payload carries one.
fn p_model(event: &Event) -> String {
    match &event.payload {
        EventPayload::LlmResponse(_) => event
            .envelope
            .cost
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_else(|| "?".to_string()),
        _ => "?".to_string(),
    }
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
        let item = fq_runtime::views::InvocationSummaryView {
            invocation_id: "019e3b328fd47de1aae0bb91bb24528d".to_string(),
            agent_id: Some("a".repeat(40)),
            worker_id: "worker-42".to_string(),
            status: "ambiguous".to_string(),
            assigned_at_ms: 1_700_000_000_000,
            started_at_ms: 1_700_000_000_000,
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
        let item = fq_runtime::views::InvocationSummaryView {
            invocation_id: "inv".to_string(),
            agent_id: Some("a".to_string()),
            worker_id: String::new(),
            status: "completed".to_string(),
            assigned_at_ms: 0,
            started_at_ms: 0,
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

        let control_store = ControlPlaneStore::open(&db_path).await.unwrap();
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
            &control_store,
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
    async fn publish_invocation_drop_removes_agentless_owner() {
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("events.db");
        let proj_store = ProjectionStore::open(&db_path).await.unwrap();
        let control_store = ControlPlaneStore::open(&db_path).await.unwrap();
        let fake_inv = Uuid::now_v7().to_string();
        control_store
            .register_worker("orphan-worker", "test", 1)
            .await
            .unwrap();
        control_store
            .assign_invocation(&fake_inv, "orphan-worker", 1)
            .await
            .unwrap();
        let bus = EventBus::connect(&url).await.expect("connect NATS");

        let result = publish_invocation_drop(&bus, &proj_store, &control_store, &fake_inv, None)
            .await
            .expect("agent-less owner should drop");
        assert_eq!(result.agent_id, "operator");
        assert!(
            control_store
                .get_invocation_owner(&fake_inv)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn publish_invocation_drop_errors_when_nothing_known() {
        // No projection event *and* no coordination owner row: a truly
        // unknown id must still error rather than emit a phantom
        // operator-recovered event for something that never existed.
        let Ok(url) = std::env::var("FQ_NATS_URL") else {
            eprintln!("skipping: FQ_NATS_URL not set");
            return;
        };
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("events.db");
        let proj_store = ProjectionStore::open(&db_path).await.unwrap();
        let control_store = ControlPlaneStore::open(&db_path).await.unwrap();
        let bus = EventBus::connect(&url).await.expect("connect NATS");

        let fake_inv = Uuid::now_v7().to_string();
        let err = publish_invocation_drop(&bus, &proj_store, &control_store, &fake_inv, None)
            .await
            .expect_err("unknown invocation should error");
        assert!(format!("{err}").contains("not found"), "got: {err}");
    }

    /// The `--json` list shape is an operator contract: the swap from the
    /// CLI-local struct to `views::InvocationSummaryView` (#105 layer 1)
    /// must not move these fields.
    #[test]
    fn invocation_summary_view_serialises_to_stable_json_shape() {
        let item = fq_runtime::views::InvocationSummaryView {
            invocation_id: "inv-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            worker_id: "worker-1".to_string(),
            status: "in_flight".to_string(),
            assigned_at_ms: 42,
            started_at_ms: 41,
            archived: false,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["invocation_id"], "inv-1");
        assert_eq!(v["agent_id"], "agent-1");
        assert_eq!(v["worker_id"], "worker-1");
        assert_eq!(v["status"], "in_flight");
        assert_eq!(v["assigned_at_ms"], 42);
        assert_eq!(v["started_at_ms"], 41);
        assert_eq!(v["archived"], false);
    }
}

// ============================================================
// fq workers subcommand
// ============================================================

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

fn format_worker_list_row_human(
    item: &fq_runtime::views::WorkerView,
    now_ms: i64,
    stale_threshold_ms: i64,
) -> String {
    let age = format_heartbeat_age_human(now_ms - item.last_heartbeat_ms, stale_threshold_ms);
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
    let now_ms = chrono::Utc::now().timestamp_millis();
    // The threshold the CP uses to flip a worker from alive
    // to stale; this is the same DEFAULT_STALE_THRESHOLD_MS
    // used by the coordination consumer.
    let stale_threshold_ms = 30_000_i64;

    let views = open_views(global).await?;
    let items: Vec<_> = views
        .workers()
        .await?
        .into_iter()
        .filter(|w| !(stale_only && w.status != "stale"))
        .filter(|w| !(alive_only && w.status != "alive"))
        .collect();

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
            println!(
                "{}",
                format_worker_list_row_human(item, now_ms, stale_threshold_ms)
            );
        }
    }
    Ok(())
}

async fn workers_prune(global: &GlobalArgs, dry_run: bool) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let store = ControlPlaneStore::open(&projection_path(&config)).await?;
    let stale: Vec<String> = store
        .list_workers()
        .await?
        .into_iter()
        .filter(|worker| worker.status == WorkerStatus::Stale)
        .map(|worker| worker.worker_id)
        .collect();
    if dry_run {
        println!(
            "Would remove {} stale worker(s): {}",
            stale.len(),
            stale.join(", ")
        );
    } else if stale.is_empty() {
        println!("0 stale workers removed.");
    } else {
        let removed = store.prune_stale_workers().await?;
        println!(
            "Removed {} stale worker(s): {}",
            removed.len(),
            removed.join(", ")
        );
    }
    Ok(())
}

async fn workers_show(global: &GlobalArgs, id: &str, json: bool) -> anyhow::Result<()> {
    let stale_threshold_ms = 30_000_i64;
    let views = open_views(global).await?;
    let Some(detail) = views.worker(id).await? else {
        eprintln!("no worker found with id={id}");
        std::process::exit(1);
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let w = &detail.worker;
        println!("Worker: {}", w.worker_id);
        println!("  host:      {}", w.host);
        println!("  status:    {}", w.status);
        println!(
            "  hb-age:    {}",
            format_heartbeat_age_human(now_ms - w.last_heartbeat_ms, stale_threshold_ms)
        );
        println!("  in-flight: {}", w.in_flight_count);
        if !detail.owned.is_empty() {
            println!("\nInvocations owned:");
            for o in detail.owned.iter().take(20) {
                let inv: String = o.invocation_id.chars().take(11).collect();
                println!("  {inv}  {}", o.status);
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
        assert!(out.contains("fq workers prune"));
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
        assert!(out.contains("fq workers prune"));
    }

    /// The `--json` worker shape after the swap to `views::WorkerView`
    /// (#105 layer 1). Deliberate change from the old CLI-local item:
    /// gains `registered_at_ms` and `in_flight_count`, drops the
    /// now-dependent `heartbeat_age_ms` (consumers derive age from
    /// `last_heartbeat_ms`; the view stays wall-clock-free).
    #[test]
    fn worker_view_serialises_to_stable_json_shape() {
        let item = fq_runtime::views::WorkerView {
            worker_id: "w-1".to_string(),
            host: "host-1".to_string(),
            registered_at_ms: 1_600_000_000_000,
            last_heartbeat_ms: 1_700_000_000_000,
            status: "alive".to_string(),
            in_flight_count: 3,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["worker_id"], "w-1");
        assert_eq!(v["host"], "host-1");
        assert_eq!(v["status"], "alive");
        assert_eq!(v["registered_at_ms"], 1_600_000_000_000_i64);
        assert_eq!(v["last_heartbeat_ms"], 1_700_000_000_000_i64);
        assert_eq!(v["in_flight_count"], 3);
        assert!(v.get("heartbeat_age_ms").is_none());
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

        let initial = AgentRegistry::load_from_directory(dir.path(), None).unwrap();
        assert_eq!(initial.len(), 1);
        let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

        // Add a second agent on disk, then reload.
        write_agent(dir.path(), "second");
        reload_agents(&shared, dir.path(), None).await;

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
        let initial = AgentRegistry::load_from_directory(dir.path(), None).unwrap();
        assert_eq!(initial.len(), 1);
        let shared: SharedRegistry = Arc::new(tokio::sync::RwLock::new(Arc::new(initial)));

        // Point the reload at a directory that does not exist.
        let missing = dir.path().join("does-not-exist");
        reload_agents(&shared, &missing, None).await;

        let after = shared.read().await.clone();
        assert_eq!(after.len(), 1, "failed reload must keep the old registry");
        assert!(after.get(&AgentId::new("keep").unwrap()).is_some());
    }
}

#[cfg(test)]
mod log_format_tests {
    use super::*;
    use clap::Parser;

    /// The default (no flag, no env) is `text`, preserving the
    /// existing human-readable output.
    #[test]
    fn log_format_defaults_to_text() {
        let cli = Cli::parse_from(["fq", "run"]);
        assert_eq!(cli.global.log_format, LogFormat::Text);
    }

    /// `--log-format json` parses to the JSON renderer.
    #[test]
    fn log_format_json_flag_parses() {
        let cli = Cli::parse_from(["fq", "--log-format", "json", "run"]);
        assert_eq!(cli.global.log_format, LogFormat::Json);
    }

    /// `--log-format text` parses to the text renderer.
    #[test]
    fn log_format_text_flag_parses() {
        let cli = Cli::parse_from(["fq", "--log-format", "text", "run"]);
        assert_eq!(cli.global.log_format, LogFormat::Text);
    }

    /// The flag is global — it can follow the subcommand too.
    #[test]
    fn log_format_flag_is_global() {
        let cli = Cli::parse_from(["fq", "status", "--log-format", "json"]);
        assert_eq!(cli.global.log_format, LogFormat::Json);
    }

    /// An unknown value is rejected rather than silently defaulting.
    #[test]
    fn log_format_rejects_unknown_value() {
        let result = Cli::try_parse_from(["fq", "--log-format", "yaml", "run"]);
        let err = match result {
            Ok(_) => panic!("unknown log-format value should be rejected"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("yaml") || msg.contains("possible values"),
            "got: {msg}"
        );
    }

    /// The JSON formatter layer builds and renders a structured event
    /// as parseable JSON with the fields intact. Uses a
    /// `tracing_subscriber::fmt` layer with a captured writer rather
    /// than the process-global subscriber (which can only be set once),
    /// but exercises the same `.json()` renderer `init_tracing` wires up.
    #[test]
    fn json_layer_emits_parseable_json_with_fields() {
        use std::sync::{Arc, Mutex};
        use tracing::subscriber;
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for SharedBuf {
            type Writer = SharedBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .json()
            .with_writer(buf.clone())
            .finish();

        subscriber::with_default(subscriber, || {
            tracing::warn!(
                invocation_id = "inv-42",
                worker_id = "w-1",
                "structured event"
            );
        });

        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = raw.lines().next().expect("expected at least one log line");
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("each log line must be a JSON object");
        assert_eq!(parsed["level"], "WARN");
        assert_eq!(parsed["fields"]["message"], "structured event");
        assert_eq!(parsed["fields"]["invocation_id"], "inv-42");
        assert_eq!(parsed["fields"]["worker_id"], "w-1");
    }
}

#[cfg(test)]
mod doctor_tests {
    use super::*;
    use fq_runtime::views::{ExecutionsView, FailureView, WorkerView};

    fn worker(id: &str, status: &str, last_heartbeat: i64) -> WorkerView {
        WorkerView {
            worker_id: id.to_string(),
            host: "h".to_string(),
            registered_at_ms: 0,
            last_heartbeat_ms: last_heartbeat,
            status: status.to_string(),
            in_flight_count: 0,
        }
    }

    /// The in-flight/stuck determination itself (threshold, clock skew)
    /// is `views::Views::executions`' job and is covered by its tests;
    /// doctor receives the finished counts.
    fn executions(in_flight: i64, stuck_ids: &[&str]) -> ExecutionsView {
        ExecutionsView {
            in_flight,
            working: 0,
            working_ids: vec![],
            stuck: stuck_ids.len() as i64,
            stuck_ids: stuck_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    const NOW: i64 = 1_000_000;

    #[test]
    fn all_clear_when_everything_healthy() {
        let workers = vec![worker("w1", "alive", NOW)];
        let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

        assert!(!report.has_issues());
        assert_eq!(report.workers.alive, 1);
        assert_eq!(report.workers.stale, 0);
        assert_eq!(report.executions.in_flight, 0);
        assert_eq!(report.failure_total(), 0);
        assert_eq!(
            report.dead_letters,
            DoctorDeadLetters::PendingIssue { issue: 49 }
        );

        let out = render_doctor_report_human(&report);
        assert!(out.contains("All clear."), "got: {out}");
        // Dead-letter section is always shown, gated on #49.
        assert!(out.contains("pending #49"), "got: {out}");
    }

    #[test]
    fn running_in_flight_work_is_not_an_issue() {
        // In-flight but not stuck is healthy.
        let report = build_doctor_report(&[], &executions(1, &[]), 0, &[]);
        assert_eq!(report.executions.in_flight, 1);
        assert_eq!(report.executions.stuck, 0);
        assert!(!report.has_issues());
    }

    #[test]
    fn stale_workers_flagged_with_ids() {
        let workers = vec![
            worker("alive-1", "alive", NOW),
            worker("stale-1", "stale", NOW - 60_000),
            worker("gone-1", "shutdown", 0),
        ];
        let report = build_doctor_report(&workers, &ExecutionsView::default(), 0, &[]);

        assert_eq!(report.workers.alive, 1);
        assert_eq!(report.workers.stale, 1);
        assert_eq!(report.workers.shutdown, 1);
        assert_eq!(report.workers.stale_ids, vec!["stale-1".to_string()]);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(out.contains("1 alive, 1 stale, 1 shutdown"), "got: {out}");
        assert!(out.contains("fq workers list --stale-only"), "got: {out}");
        assert!(!out.contains("All clear."), "got: {out}");
    }

    #[test]
    fn stuck_in_flight_flagged() {
        let report = build_doctor_report(&[], &executions(2, &["stuck-abcdef01"]), 0, &[]);

        assert_eq!(report.executions.in_flight, 2);
        assert_eq!(report.executions.stuck, 1);
        // Short id (8 chars) recorded for triage.
        assert_eq!(report.executions.stuck_ids, vec!["stuck-ab".to_string()]);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(
            out.contains("2 in-flight (0 working, 1 stuck)"),
            "got: {out}"
        );
        assert!(out.contains("fq invocation drop"), "got: {out}");
    }

    /// Working invocations (fresh open dispatch, #130) surface in the human
    /// report but are healthy — no issue, no remediation hint.
    #[test]
    fn working_in_flight_shown_but_not_an_issue() {
        let ex = ExecutionsView {
            in_flight: 2,
            working: 1,
            working_ids: vec!["019f5b3f-31fb-7ae0-b130-3d65ccf40375".to_string()],
            stuck: 0,
            stuck_ids: vec![],
        };
        let report = build_doctor_report(&[], &ex, 0, &[]);

        assert!(!report.has_issues());
        // Short id (8 chars), same convention as stuck_ids.
        assert_eq!(report.executions.working_ids, vec!["019f5b3f".to_string()]);

        let out = render_doctor_report_human(&report);
        assert!(
            out.contains("2 in-flight (1 working, 0 stuck)"),
            "got: {out}"
        );
        assert!(!out.contains("fq invocation drop"), "got: {out}");
    }

    #[test]
    fn ambiguous_flagged() {
        let report = build_doctor_report(&[], &ExecutionsView::default(), 3, &[]);
        assert_eq!(report.ambiguous, 3);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(out.contains("Ambiguous invocations: 3"), "got: {out}");
        assert!(
            out.contains("fq invocation list --status=ambiguous"),
            "got: {out}"
        );
    }

    #[test]
    fn permanent_failures_grouped_by_kind() {
        let failures = vec![
            FailureView {
                error_kind: "budgetexceeded".to_string(),
                count: 2,
            },
            FailureView {
                error_kind: "toolerror".to_string(),
                count: 1,
            },
        ];
        let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &failures);

        assert_eq!(report.failure_total(), 3);
        assert!(report.has_issues());

        let out = render_doctor_report_human(&report);
        assert!(out.contains("Permanent failures: 3"), "got: {out}");
        assert!(out.contains("budgetexceeded: 2"), "got: {out}");
        assert!(out.contains("toolerror: 1"), "got: {out}");
        assert!(
            out.contains("fq invocation list --status=failed"),
            "got: {out}"
        );
    }

    #[test]
    fn report_serialises_to_stable_json_shape() {
        let report = build_doctor_report(
            &[worker("w1", "alive", NOW)],
            &executions(1, &[]),
            1,
            &[FailureView {
                error_kind: "runtimeerror".to_string(),
                count: 4,
            }],
        );
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["workers"]["alive"], 1);
        assert_eq!(v["executions"]["in_flight"], 1);
        assert_eq!(v["ambiguous"], 1);
        assert_eq!(v["failures"][0]["error_kind"], "runtimeerror");
        assert_eq!(v["failures"][0]["count"], 4);
        assert_eq!(v["dead_letters"]["state"], "pending_issue");
        assert_eq!(v["dead_letters"]["issue"], 49);
    }

    #[test]
    fn dead_letters_never_fabricates_a_count() {
        let report = build_doctor_report(&[], &ExecutionsView::default(), 0, &[]);
        // The gated variant carries no count field at all.
        let v = serde_json::to_value(&report).unwrap();
        assert!(v["dead_letters"].get("count").is_none());
        let out = render_doctor_report_human(&report);
        assert!(out.contains("n/a"), "got: {out}");
    }
}
