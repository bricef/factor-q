//! The daemon's argument surface.
//!
//! No subcommands: `fqd` starts, runs, and drains on signal. What it
//! takes is only what says where its state lives and what it talks to
//! — the client's flags describe things the daemon owns and the client
//! no longer does.

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use fq_runtime::Config;
use tracing_subscriber::EnvFilter;

/// The daemon's config file. Named for the binary that reads it: the
/// client has its own `fq.toml`, and one file per binary means neither
/// has to know the other's shape. A client reading the daemon's config
/// could not work against a remote daemon anyway — the operator on
/// another machine has no such file.
const DEFAULT_CONFIG_PATH: &str = "fqd.toml";
use tracing_subscriber::fmt;

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
