//! The daemon's argument surface.
//!
//! No subcommands: `fqd` starts, runs, and drains on signal. What it
//! takes is only what says where its state lives and what it talks to
//! — the client's flags describe things the daemon owns and the client
//! no longer does.

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use fq_runtime::Config;
use tracing::Level;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// The daemon's config file. Named for the binary that reads it: the
/// client has its own `fq.toml`, and one file per binary means neither
/// has to know the other's shape. A client reading the daemon's config
/// could not work against a remote daemon anyway — the operator on
/// another machine has no such file.
const DEFAULT_CONFIG_PATH: &str = "fqd.toml";

/// The `tracing` target prefix of the NATS client crate — `async_nats`,
/// `async_nats::connection`, `async_nats::connector`, and so on.
const NATS_CLIENT_TARGET: &str = "async_nats";

/// How the tracing subscriber renders log lines. `Text` is the
/// human-readable ANSI default; `Json` emits one structured JSON
/// object per line for machine parsing (issue #36).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum LogFormat {
    Text,
    Json,
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
///
/// One ceiling sits above whatever `RUST_LOG` asks for: the NATS client
/// never logs at `trace`. `async-nats` 0.50 traces every protocol
/// operation it writes, `CONNECT` included, and `CONNECT` carries the
/// broker token in clear (`connection.rs`, `trace!(?connect_info, …)`);
/// 0.38 traced no operations at all, which is why the bump would
/// otherwise undo #540. `[nats] token_env` keeps the credential out of
/// `Config` and the URL; this keeps it out of the log, so
/// `RUST_LOG=trace` on a host costs wire-level NATS debugging and never
/// the credential. The client's `debug` and above are unaffected.
pub(crate) fn init_tracing(format: LogFormat) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let client_ceiling = filter_fn(|meta| {
        !(*meta.level() == Level::TRACE && meta.target().starts_with(NATS_CLIENT_TARGET))
    });
    let registry = tracing_subscriber::registry().with(env_filter);
    match format {
        LogFormat::Text => registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_filter(client_ceiling),
            )
            .init(),
        LogFormat::Json => registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .json()
                    .with_filter(client_ceiling),
            )
            .init(),
    }
}

/// The daemon's arguments. `fqd` has no subcommands — it starts, runs
/// and stops — so these are simply its flags. Each has a corresponding
/// environment variable, and together they override values loaded from
/// the config file.
///
/// Precedence: CLI flag > env var > config file > default.
#[derive(Args, Clone)]
pub(crate) struct GlobalArgs {
    /// Path to the daemon's config file.
    ///
    /// The variable names the binary, because the two binaries' configs
    /// are different files with different shapes. A shared `FQ_CONFIG`
    /// pointed both at one file, which now breaks in two different
    /// ways: this daemon rejects unknown keys outright and would refuse
    /// to start on the client's tables, while `fq` still ignores keys
    /// it does not know and would silently skip this file's.
    #[arg(long, env = "FQ_DAEMON_CONFIG", default_value = DEFAULT_CONFIG_PATH, global = true)]
    config: PathBuf,

    /// Override the agents directory from config
    #[arg(long, env = "FQ_AGENTS_DIR", global = true)]
    agents_dir: Option<PathBuf>,

    /// Override the NATS URL from config
    #[arg(long, env = "FQ_NATS_URL", global = true, hide_env_values = true)]
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
        // An override can carry the same mistake the file can: an
        // `FQ_NATS_URL` with userinfo is refused here, after the merge,
        // with the message that names `[nats] token_env` (#540).
        config.validate()?;
        Ok(config)
    }
}

/// `fqd` takes the global connection/config flags and no subcommands.
///
/// `long_version` carries the commit, matching `fq`. A bare `version`
/// printed `fqd 0.1.0` and nothing else, so the one binary whose
/// deployed build actually matters could only be identified over a live
/// authenticated edge or by reading its startup banner — and the
/// deploy's bundle-coherence check, which compares the SHAs the other
/// binaries report, had to skip the daemon.
#[derive(clap::Parser)]
#[command(
    name = "fqd",
    about = "The factor-q daemon",
    version,
    long_version = env!("FQ_VERSION_LONG")
)]
pub(crate) struct FqdArgs {
    #[command(flatten)]
    pub(crate) global: GlobalArgs,
}
