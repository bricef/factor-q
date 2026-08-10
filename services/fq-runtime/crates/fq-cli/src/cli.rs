//! The command-line surface itself: every clap type `fq` and `fqd` parse,
//! plus the tracing subscriber the entry points install once the flags are
//! known.
//!
//! Split out of `lib.rs` (#189). Declaration only — nothing here reaches back
//! into a verb module, which is what lets every other module depend on
//! [`GlobalArgs`] without a cycle. The dispatch that turns a parsed
//! [`Commands`] into a verb call stays at the crate root, where the
//! composition root belongs.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use fq_runtime::Config;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_CONFIG_PATH: &str = "fq.toml";

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

    /// Override the state directory from config — durable data that
    /// must survive a restart (the edge identity), as opposed to the
    /// regenerable cache directory
    #[arg(long, env = "FQ_STATE_DIR", global = true)]
    state_dir: Option<PathBuf>,

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
        if let Some(dir) = &self.state_dir {
            config.state.directory = dir.clone();
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
    /// Ask a running daemon to hot-reload its agent definitions from
    /// disk, without a restart. The daemon re-reads ITS agents
    /// directory and atomically swaps the registry the dispatcher
    /// reads, then answers — so a reload that could not read the
    /// directory is an error here, not a silence. The reload affects
    /// the NEXT trigger only — in-flight invocations keep the config
    /// they snapshotted at trigger time (ADR-0020
    /// refresh-between-invocations).
    Reload,
    /// Cleanly stop a running daemon and confirm it exited (issue #63)
    /// — the operator-facing stop verb, so nobody reaches for
    /// `pkill -INT`. By default the daemon drains in-flight work to the
    /// next step boundary (bounded by `drain_deadline_ms`), then tears
    /// down its infrastructure, deregisters the worker, and exits. This
    /// command then waits — bounded — for the daemon's edge to stop
    /// answering, and exits 0 only once it has, or with a timeout
    /// error. Use `--now` (or `--no-drain`) to skip the drain.
    Down {
        /// Skip the drain: clean infra teardown + worker deregister +
        /// immediate exit, accepting that in-flight invocations become
        /// recoverable-on-next-start (equivalent to today's SIGINT, but
        /// as a proper confirmable command). Alias: `--no-drain`.
        #[arg(long, visible_alias = "no-drain")]
        now: bool,
    },
    /// Trigger an agent manually: the daemon queues the work on its
    /// durable trigger stream and its dispatcher runs it.
    Trigger {
        /// Agent name
        agent: String,
        /// Optional payload (JSON or plain string)
        payload: Option<String>,
        /// Accepted and ignored. It used to select this mode over
        /// running the reducer in the CLI's own process; that second
        /// execution path is retired (decision D-1) and every trigger
        /// goes to the daemon, so the flag is kept only so existing
        /// scripts keep working.
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
        /// Filter by time: a date, a UTC date-time, or an RFC3339 instant
        #[arg(long, value_parser = fq_runtime::views::since::lower_bound)]
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
    /// Pair this client with a daemon's edge: pin the certificate
    /// fingerprint (trust-on-first-use, with confirmation) and store
    /// the capability token
    Connect {
        /// Edge address; defaults to `[edge] bind` from config
        addr: Option<String>,
        /// Capability token to present and store (the daemon printed
        /// the admin token at first run); defaults to the token
        /// already stored for this address
        #[arg(long)]
        token: Option<String>,
        /// Pin this certificate fingerprint (64 hex chars) instead of
        /// trusting the first connection
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// The daemon's registry-first operator surface
    Ops {
        #[command(subcommand)]
        command: OpsCommands,
    },
    /// Capability-token helpers
    Token {
        #[command(subcommand)]
        command: TokenCommands,
    },
    /// Print version and build information
    Version {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum OpsCommands {
    /// List the operations the daemon's registry serves — the surface
    /// describing itself, over the authenticated edge.
    List {
        /// Edge address; defaults to `[edge] bind` from config
        #[arg(long)]
        addr: Option<String>,
        /// Emit the raw describe payload as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum TokenCommands {
    /// Narrow a token offline — no daemon round-trip. The result
    /// authorises the intersection of the source token and the given
    /// grants; attenuation can never widen.
    Attenuate {
        /// A grant to narrow to, as `verb:domain` (`*` wildcard on
        /// either side, e.g. `read:*`). Repeatable; the grants union.
        #[arg(long, required = true)]
        grant: Vec<String>,
        /// Source token; defaults to the stored token for `--addr`.
        #[arg(long)]
        token: Option<String>,
        /// Which stored connection's token to attenuate; defaults to
        /// `[edge] bind` from config.
        #[arg(long)]
        addr: Option<String>,
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
        /// Maximum rows in one page, at most 500. A bigger ask is
        /// refused, not shortened — so fewer rows than you asked for
        /// means there are no more. For more, narrow with --agent.
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
    /// List the agent definitions the daemon has loaded (its live
    /// registry, as `fq reload` left it — not this machine's disk)
    List,
    /// Validate an agent definition file (offline; needs no daemon)
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
    /// worker-initiated ones. Works on any state EXCEPT one this
    /// daemon is actively driving — that needs `--live`, which halts
    /// it at its next step boundary first (#107). To reconcile unknown
    /// execution and preserve progress instead, use `resume`.
    Drop {
        /// Invocation id to drop.
        id: String,
        /// Free-form reason recorded on the event payload.
        #[arg(long)]
        reason: Option<String>,
        /// Explicitly halt the invocation if it is currently running.
        #[arg(long)]
        live: bool,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Recover an Ambiguous invocation by durably completing every stuck tool
    /// dispatch with an honest interrupted result, then re-driving normal
    /// SafeReplay recovery. See data-architecture.md §4.4. Use `drop` instead
    /// when progress should be abandoned.
    Resume {
        /// Invocation id to resume.
        id: String,
        /// Free-form reason recorded on the audit event.
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
        /// Emit machine-readable JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
        /// Deprecated output-format alias; use `--json`.
        #[arg(long, value_enum)]
        format: Option<TranscriptFormat>,
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
        /// Follow one agent's events (defaults to every agent)
        #[arg(long)]
        agent: Option<String>,
        /// Follow one event type (triggered, llm_request, llm_response,
        /// llm_failure, tool_call, tool_result, completed, failed)
        #[arg(long, name = "type")]
        event_type: Option<String>,
        /// Emit one JSON event per line.
        #[arg(long)]
        json: bool,
    },
    /// Query the event history from the SQLite projection
    Query {
        /// Filter by agent
        #[arg(long)]
        agent: Option<String>,
        /// Filter by event type (triggered, llm_request, llm_response,
        /// llm_failure, tool_call, tool_result, cost, completed,
        /// failed)
        #[arg(long, name = "type")]
        event_type: Option<String>,
        /// Events at or after this time: a date, a UTC date-time, or RFC3339
        #[arg(long, value_parser = fq_runtime::views::since::lower_bound)]
        since: Option<String>,
        /// Maximum rows in one page, at most 2000. A bigger ask is
        /// refused, not shortened — so fewer rows than you asked for
        /// means there are no more. For more, narrow the query or tail.
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Read one whole event back by its identity — the `event-id`
    /// `fq events query` prints in its last column, passed through
    /// unchanged. Query answers from the projection's index and
    /// carries no payloads; this reads the event itself out of the
    /// log, payload included, for as long as the log still holds it.
    ///
    /// Three answers mean the event is not readable, and they are
    /// different facts about this system: `not found` (no such event
    /// here), `unlocatable` (the event is indexed and where its
    /// payload sits was never recorded) and `gone` (the log has aged
    /// past it — routine for an old cost-bearing row, which the index
    /// keeps indefinitely while the log keeps thirty days).
    Get {
        /// The event's identity, exactly as `fq events query` prints
        /// it. A whole UUID: there is no prefix matching, so a
        /// shortened id is refused rather than guessed at.
        event_id: String,
        /// Emit the event as JSON instead of human-readable output.
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
///
/// Logs go to stderr in both modes: stdout is reserved for machine
/// output (issue #190), and query-style commands log incidental INFO
/// (e.g. the NATS connect) before their result is known.
pub(crate) fn init_tracing(format: LogFormat) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Text => fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init(),
        LogFormat::Json => fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .json()
            .init(),
    }
}

/// `fqd` takes the global connection/config flags and no subcommands.
#[derive(clap::Parser)]
#[command(name = "fqd", about = "The factor-q daemon", version)]
pub(crate) struct FqdArgs {
    #[command(flatten)]
    pub(crate) global: GlobalArgs,
}

#[cfg(test)]
mod tests;
