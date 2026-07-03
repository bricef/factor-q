//! `fq-cas` — a command-line interface over the content-addressed store.
//!
//! A small standalone tool that exercises the M1a CAS: store files (or
//! stdin), read content back by id, and query presence and size. The store
//! lives under a root directory (`--root`, env `FQ_CAS_ROOT`, default
//! `./.fq-cas`).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::fs::FilesystemStore;
use crate::{
    AuditReport, Cid, ContentStore, ReachabilityAuditor, Repository, SqliteNameIndex, Stats,
    StoreError,
};

type CliResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const AFTER_HELP: &str = "\
Content-addressed (by id):
  fq-cas put file.bin                       store a file; prints its content id
  echo hi | fq-cas put -                    store from stdin
  fq-cas get <cid> -o out.bin               read content back to a file
  fq-cas metrics                            dedup ratio, object/block counts, sizes
  fq-cas serve --bind 127.0.0.1:9000        serve the store over the network

Named objects (a name -> content mapping, with version history):
  fq-cas object put research.papers.doc1 paper.pdf   store and name a file
  fq-cas object get research.papers.doc1 -o out.pdf  read content by name
  fq-cas object ls research.papers                   list a namespace
  fq-cas object history research.papers.doc1         show version history
  fq-cas object rm research.papers.doc1              remove a name

Maintenance:
  fq-cas gc                                 reclaim unreferenced storage (safe on a live store)
  fq-cas gc --grace 60 --json               shorter grace, machine-readable report

The store lives under --root (env FQ_CAS_ROOT, default ./.fq-cas).";

#[derive(Parser)]
#[command(
    name = "fq-cas",
    about = "Content-addressed storage CLI (factor-q fq-store)",
    version,
    after_help = AFTER_HELP
)]
struct Cli {
    /// Store root directory (ignored when --server is set).
    #[arg(long, env = "FQ_CAS_ROOT", default_value = ".fq-cas", global = true)]
    root: PathBuf,
    /// Run the command against a remote `fq-cas serve` instead of local
    /// storage (e.g. 127.0.0.1:9000).
    #[arg(long, global = true)]
    server: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Store a file (or stdin with '-'); print its content id.
    Put {
        /// Path to a file, or '-' for stdin.
        path: String,
    },
    /// Read content by id to stdout (or a file).
    Get {
        /// The content id (hex).
        cid: String,
        /// Write to this file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Start reading at this byte offset.
        #[arg(long)]
        offset: Option<u64>,
        /// Read at most this many bytes.
        #[arg(long)]
        length: Option<u64>,
    },
    /// Print whether content for an id is present (exit 0 if so).
    Has {
        /// The content id (hex).
        cid: String,
    },
    /// Print the byte size of content for an id.
    Size {
        /// The content id (hex).
        cid: String,
    },
    /// Print storage metrics (object/block counts, sizes, dedup ratio).
    Metrics {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Serve the local store over the network (unauthenticated; localhost
    /// only until M2). Clients connect with `--server`.
    Serve {
        /// Address to bind (host:port).
        #[arg(long, default_value = "127.0.0.1:9000")]
        bind: String,
    },
    /// Object operations: store, read, list, and remove content by
    /// hierarchical name (the local name index over the content store).
    Object {
        #[command(subcommand)]
        command: ObjectCommand,
    },
    /// Reclaim unreferenced storage. Runs the reachability audit: reclaim dead
    /// objects and blocks, reap orphan files, reconcile leaked reservations, and
    /// alarm on the forbidden state. Safe to run on a live store, and never
    /// removes anything a live name still needs. Exits non-zero if it alarms.
    Gc {
        /// Reap/reconcile grace, in seconds: a file or reservation must have gone
        /// untouched at least this long to be eligible, so an in-flight write is
        /// never mistaken for garbage. Default 900 (15 minutes).
        #[arg(long, default_value_t = 900)]
        grace: u64,
        /// Emit a machine-readable JSON report instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// Operations on the name index — the named, versioned object store.
#[derive(Subcommand)]
enum ObjectCommand {
    /// Store a file (or stdin '-') and bind it to a name; prints the cid.
    Put {
        /// The dotted, hierarchical name (e.g. research.papers.doc1).
        name: String,
        /// Path to a file, or '-' for stdin.
        path: String,
    },
    /// Read content bound to a name to stdout (or a file).
    Get {
        /// The name to read.
        name: String,
        /// Write to this file instead of stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Start reading at this byte offset.
        #[arg(long)]
        offset: Option<u64>,
        /// Read at most this many bytes.
        #[arg(long)]
        length: Option<u64>,
    },
    /// List names within a namespace prefix (empty lists all).
    Ls {
        /// The namespace prefix (e.g. research.papers); default lists all.
        #[arg(default_value = "")]
        prefix: String,
    },
    /// Remove a name — its current binding and history.
    Rm {
        /// The name to remove.
        name: String,
    },
    /// Print the cid a name currently resolves to.
    Resolve {
        /// The name to resolve.
        name: String,
    },
    /// Print a name's version history (cids, newest first).
    History {
        /// The name whose history to print.
        name: String,
    },
    /// Bind a name to an already-stored cid (an alias).
    Bind {
        /// The name to bind.
        name: String,
        /// The content id (hex).
        cid: String,
    },
}

/// Entry point for the `fq-cas` binary.
pub fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fq-cas: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Initialize tracing output — off unless `RUST_LOG` is set (e.g.
/// `RUST_LOG=fq_store=debug`). Logs to stderr so stdout stays pipeable, and
/// span-close events surface per-operation timings.
fn init_tracing() {
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off")),
        )
        .with_span_events(FmtSpan::CLOSE)
        .try_init();
}

fn run() -> CliResult<ExitCode> {
    init_tracing();
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        match cli.command {
            Command::Serve { bind } => {
                let store = std::sync::Arc::new(FilesystemStore::new(&cli.root));
                eprintln!(
                    "fq-cas serving {} on {bind} (unauthenticated — localhost only until M2)",
                    cli.root.display()
                );
                crate::service::serve(&bind, store).await?;
                Ok(ExitCode::SUCCESS)
            }
            Command::Object { command } => {
                if cli.server.is_some() {
                    return Err("named operations run against the local store; \
                                --server addresses the CID-level CAS only \
                                (a remote name service is future)"
                        .into());
                }
                let repo = Repository::new(
                    FilesystemStore::new(&cli.root),
                    SqliteNameIndex::open(cli.root.join("index.db")).await?,
                );
                dispatch_object(&repo, command).await
            }
            Command::Gc { grace, json } => {
                if cli.server.is_some() {
                    return Err("gc runs against the local store; --server addresses \
                                the CID-level CAS only"
                        .into());
                }
                let repo = Repository::new(
                    FilesystemStore::new(&cli.root),
                    SqliteNameIndex::open(cli.root.join("index.db")).await?,
                );
                run_gc(&repo, grace, json).await
            }
            command => {
                if let Some(addr) = &cli.server {
                    let store = crate::service::RemoteStore::connect(addr).await?;
                    dispatch(&store, command).await
                } else {
                    let store = FilesystemStore::new(&cli.root);
                    dispatch(&store, command).await
                }
            }
        }
    })
}

async fn dispatch(store: &dyn ContentStore, command: Command) -> CliResult<ExitCode> {
    match command {
        Command::Put { path } => {
            let content = read_input(&path).await?;
            let cid = store.put(&content).await?;
            println!("{cid}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Get {
            cid,
            output,
            offset,
            length,
        } => {
            let cid = Cid::from_hex(&cid)?;
            let data = read(store, &cid, offset, length).await?;
            write_output(&data, output).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Has { cid } => {
            let cid = Cid::from_hex(&cid)?;
            let present = store.has(&cid).await?;
            println!("{present}");
            Ok(if present {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Size { cid } => {
            let cid = Cid::from_hex(&cid)?;
            println!("{}", store.size(&cid).await?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Metrics { json } => {
            let stats = store.stats().await?;
            if json {
                let value = serde_json::json!({
                    "objects": stats.objects,
                    "blocks": stats.blocks,
                    "logical_bytes": stats.logical_bytes,
                    "physical_bytes": stats.physical_bytes,
                    "block_refs": stats.block_refs,
                    "dedup_ratio": stats.dedup_ratio(),
                    "dedup_savings": stats.dedup_savings(),
                    "avg_block_sharing": stats.avg_block_sharing(),
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_metrics(&stats);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve { .. } => unreachable!("Serve is handled in run(), before dispatch"),
        Command::Object { .. } => unreachable!("Object is handled in run(), before dispatch"),
        Command::Gc { .. } => unreachable!("Gc is handled in run(), before dispatch"),
    }
}

/// Run the reachability audit as the `gc` command and report what it did. Exits
/// non-zero when it alarms — the forbidden state (a live object missing a block)
/// must never occur, so surfacing it to a script or operator is the point.
async fn run_gc(
    repo: &Repository<FilesystemStore, SqliteNameIndex>,
    grace_secs: u64,
    json: bool,
) -> CliResult<ExitCode> {
    let report = ReachabilityAuditor
        .audit(repo, Duration::from_secs(grace_secs))
        .await?;
    if json {
        let value = serde_json::json!({
            "reclaimed_objects": report.reclaimed.objects,
            "reclaimed_blocks": report.reclaimed.blocks,
            "orphan_blocks": report.orphan_blocks,
            "orphan_objects": report.orphan_objects,
            "reconciled": report.reconciled,
            "alarms": report.alarms.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print_gc_report(&report);
    }
    Ok(if report.alarms.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Human-readable GC report. Alarms go to stderr, loudly.
fn print_gc_report(report: &AuditReport) {
    println!("reclaimed objects     {}", report.reclaimed.objects);
    println!("reclaimed blocks      {}", report.reclaimed.blocks);
    println!(
        "orphan files reaped   {}",
        report.orphan_blocks + report.orphan_objects
    );
    println!("refcounts reconciled  {}", report.reconciled);
    if report.alarms.is_empty() {
        println!("alarms                none — every invariant holds");
    } else {
        eprintln!(
            "\nALARM: {} invariant violation(s) — this must never happen; investigate:",
            report.alarms.len()
        );
        for violation in &report.alarms {
            eprintln!("  {violation:?}");
        }
    }
}

/// Read full content, or a range when `offset`/`length` is given. An `offset`
/// without a `length` reads to the end.
async fn read(
    store: &dyn ContentStore,
    cid: &Cid,
    offset: Option<u64>,
    length: Option<u64>,
) -> crate::Result<Vec<u8>> {
    match (offset, length) {
        (None, None) => store.get(cid).await,
        (offset, length) => {
            let offset = offset.unwrap_or(0);
            let length = match length {
                Some(length) => length,
                None => store.size(cid).await?.saturating_sub(offset),
            };
            store.get_range(cid, offset, length).await
        }
    }
}

/// Read input from a file path, or stdin when `path` is "-".
async fn read_input(path: &str) -> CliResult<Vec<u8>> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        Ok(tokio::fs::read(path).await?)
    }
}

/// Write `data` to a file, or stdout when `output` is `None`.
async fn write_output(data: &[u8], output: Option<PathBuf>) -> CliResult<()> {
    match output {
        Some(path) => tokio::fs::write(path, data).await?,
        None => std::io::stdout().write_all(data)?,
    }
    Ok(())
}

/// Run a named-object command against the local repo.
async fn dispatch_object(
    repo: &Repository<FilesystemStore, SqliteNameIndex>,
    command: ObjectCommand,
) -> CliResult<ExitCode> {
    match command {
        ObjectCommand::Put { name, path } => {
            let content = read_input(&path).await?;
            let cid = repo.put(&name, &content).await?;
            println!("{cid}");
            Ok(ExitCode::SUCCESS)
        }
        ObjectCommand::Get {
            name,
            output,
            offset,
            length,
        } => {
            let cid = repo
                .resolve(&name)
                .await?
                .ok_or_else(|| StoreError::NameNotFound(name.clone()))?;
            let data = read(repo.content(), &cid, offset, length).await?;
            write_output(&data, output).await?;
            Ok(ExitCode::SUCCESS)
        }
        ObjectCommand::Ls { prefix } => {
            for name in repo.list(&prefix).await? {
                println!("{name}");
            }
            Ok(ExitCode::SUCCESS)
        }
        ObjectCommand::Rm { name } => {
            repo.unbind(&name).await?;
            Ok(ExitCode::SUCCESS)
        }
        ObjectCommand::Resolve { name } => match repo.resolve(&name).await? {
            Some(cid) => {
                println!("{cid}");
                Ok(ExitCode::SUCCESS)
            }
            None => Err(StoreError::NameNotFound(name).into()),
        },
        ObjectCommand::History { name } => {
            for cid in repo.history(&name).await? {
                println!("{cid}");
            }
            Ok(ExitCode::SUCCESS)
        }
        ObjectCommand::Bind { name, cid } => {
            let cid = Cid::from_hex(&cid)?;
            repo.bind(&name, &cid).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_metrics(stats: &Stats) {
    println!("objects:        {}", stats.objects);
    println!("blocks:         {}", stats.blocks);
    println!(
        "logical:        {} ({} bytes)",
        humanize(stats.logical_bytes),
        stats.logical_bytes
    );
    println!(
        "physical:       {} ({} bytes)",
        humanize(stats.physical_bytes),
        stats.physical_bytes
    );
    println!(
        "dedup ratio:    {:.2}x ({:.0}% saved)",
        stats.dedup_ratio(),
        stats.dedup_savings() * 100.0
    );
    println!("block refs:     {}", stats.block_refs);
    println!("block sharing:  {:.2}x", stats.avg_block_sharing());
}

/// Format a byte count as a short human-readable string (e.g. "3.1 MB").
fn humanize(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::ChunkParams;

    #[tokio::test]
    async fn read_full_and_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemStore::with_params(dir.path(), ChunkParams::small());
        let content: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let cid = store.put(&content).await.unwrap();

        assert_eq!(read(&store, &cid, None, None).await.unwrap(), content);
        assert_eq!(
            read(&store, &cid, Some(100), Some(50)).await.unwrap(),
            &content[100..150]
        );
        assert_eq!(
            read(&store, &cid, Some(4000), None).await.unwrap(),
            &content[4000..]
        );
        assert!(
            read(&store, &cid, Some(99999), None)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
