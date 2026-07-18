use super::*;

use tracing_subscriber::{EnvFilter, fmt};
use uuid::Uuid;

pub(crate) const DEFAULT_CONFIG_PATH: &str = "fq.toml";

/// Merge `[providers.<name>.pricing]` overrides over the loaded LiteLLM
/// table, then enforce the ADR-0004 coverage guarantee: every declared
/// model is priced, and every agent model + `agents.default_model` is
/// declared. Fail-fast — the daemon refuses to run rather than let an
/// undeclared or unpriced model silently track its cost as $0 and defeat
/// budget enforcement. Returns the merged table on success.
pub(crate) fn build_validated_pricing(
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
    let mut agent_models: Vec<(String, String)> = registry
        .iter()
        .map(|l| {
            (
                l.agent.id().as_str().to_string(),
                l.agent.model().to_string(),
            )
        })
        .collect();
    // The summariser's model (#216) is held to the same guarantee as
    // agent models: routed by a provider and priced, or refuse to
    // start — its spend is cost-accounted like everyone else's.
    if let Some(model) = &config.summary.model {
        agent_models.push(("summary".to_string(), model.clone()));
    }
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
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: GlobalArgs,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

/// How the tracing subscriber renders log lines. `Text` is the
/// human-readable ANSI default; `Json` emits one structured JSON
/// object per line for machine parsing (issue #36).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum LogFormat {
    Text,
    Json,
}

/// Global arguments available on every subcommand. Each flag has a
/// corresponding environment variable, and together they override values
/// loaded from the config file.
///
/// Precedence: CLI flag > env var > config file > default.
#[derive(Args, Clone)]
pub(crate) struct GlobalArgs {
    /// Path to the factor-q config file
    #[arg(long, env = "FQ_CONFIG", default_value = DEFAULT_CONFIG_PATH, global = true)]
    pub(crate) config: PathBuf,

    /// Override the agents directory from config
    #[arg(long, env = "FQ_AGENTS_DIR", global = true)]
    pub(crate) agents_dir: Option<PathBuf>,

    /// Override the NATS URL from config
    #[arg(long, env = "FQ_NATS_URL", global = true)]
    pub(crate) nats_url: Option<String>,

    /// Override the cache directory from config
    #[arg(long, env = "FQ_CACHE_DIR", global = true)]
    pub(crate) cache_dir: Option<PathBuf>,

    /// Log output format for the tracing subscriber. `text` (the
    /// default) is human-readable ANSI; `json` emits one JSON object
    /// per log line for machine parsing by a log aggregator.
    #[arg(long, env = "FQ_LOG_FORMAT", value_enum, default_value_t = LogFormat::Text, global = true)]
    pub(crate) log_format: LogFormat,
}

impl GlobalArgs {
    /// Load the config file (or defaults) and apply CLI/env overrides on top.
    pub(crate) fn resolve_config(&self) -> anyhow::Result<Config> {
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
pub(crate) enum Commands {
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
    /// Cleanly stop a running `fq run` daemon and confirm it exited
    /// (issue #63) — the operator-facing stop verb, so nobody reaches
    /// for `pkill -INT`. Publishes a control message on `fq.control.down`;
    /// by default the daemon drains in-flight work to the next step boundary
    /// (bounded by `drain_deadline_ms`), then tears down its
    /// infrastructure, deregisters the worker, and exits. This command
    /// then waits — bounded — for the daemon's `fq.system.shutdown`
    /// event and reports the runtime that stopped, or a timeout error.
    /// Use `--now` (or `--no-drain`) to skip the drain.
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
    /// Dead-lettered triggers: list and requeue (#49/#169)
    DeadLetters {
        #[command(subcommand)]
        command: DeadLetterCommands,
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
pub(crate) enum WorkerCommands {
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
pub(crate) enum DeadLetterCommands {
    /// List dead-lettered triggers (from the event stream, newest first).
    /// Visibility is bounded by event-stream retention (30 days by default).
    List {
        /// Filter by agent
        #[arg(long)]
        agent: Option<String>,
        /// Maximum number of rows
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Re-publish a dead-lettered trigger as a fresh trigger.
    /// Payloads are recoverable only within trigger-stream retention (24 hours by default).
    /// NOT idempotent: requeueing twice triggers the agent twice.
    Requeue {
        /// Agent whose dead letter to requeue
        agent: String,
        /// Select by the original trigger's stream sequence
        /// (see `fq dead-letters list`); default: the most recent
        #[arg(long)]
        trigger_seq: Option<u64>,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum AgentCommands {
    /// List registered agent definitions
    List,
    /// Validate an agent definition file
    Validate {
        /// Path to agent definition
        path: PathBuf,
    },
}

#[derive(Subcommand)]
pub(crate) enum InvocationCommands {
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
pub(crate) enum TranscriptFormat {
    /// Human-readable text (default).
    Pretty,
    /// Machine-readable ordered JSON array (never truncated).
    Json,
}

#[derive(Subcommand)]
pub(crate) enum EventCommands {
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

pub(crate) async fn entry() -> ExitCode {
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
        Commands::DeadLetters { command } => match command {
            DeadLetterCommands::List { agent, limit, json } => {
                list_dead_letters(&cli.global, agent.as_deref(), limit, json).await?
            }
            DeadLetterCommands::Requeue {
                agent,
                trigger_seq,
                json,
            } => requeue_dead_letter(&cli.global, &agent, trigger_seq, json).await?,
        },
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
