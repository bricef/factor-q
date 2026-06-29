//! `fq-cas` — a command-line interface over the content-addressed store.
//!
//! A small standalone tool that exercises the M1a CAS: store files (or
//! stdin), read content back by id, and query presence and size. The store
//! lives under a root directory (`--root`, env `FQ_CAS_ROOT`, default
//! `./.fq-cas`).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::fs::FilesystemStore;
use crate::{Cid, ContentStore, Stats};

type CliResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    name = "fq-cas",
    about = "Content-addressed storage CLI (factor-q fq-store)"
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

fn run() -> CliResult<ExitCode> {
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
            let content = if path == "-" {
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;
                buf
            } else {
                tokio::fs::read(&path).await?
            };
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
            match output {
                Some(path) => tokio::fs::write(path, data).await?,
                None => std::io::stdout().write_all(&data)?,
            }
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
        let store = FilesystemStore::with_params(
            dir.path(),
            ChunkParams {
                min: 256,
                avg: 1024,
                max: 4096,
            },
        );
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
