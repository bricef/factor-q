//! The factor-q daemon.
//!
//! It owns the runtime, the three SQLite stores and every word spoken
//! to NATS, and serves the operation registry over the authenticated
//! edge. The client links none of it (ADR-0031): what a reader needs
//! is the wire types in `fq-ops` and the transport in `fq-edge`, not
//! this crate.

use std::process::ExitCode;

use clap::Parser;

mod active_report;
mod cli;
mod control_commands;
mod cost_report;
mod dead_letter_atom;
mod dead_letter_requeue;
mod doctor_report;
mod edge_identity;
mod event_atom;
mod operator_surface;
mod pricing;
mod recovery;
mod resume;
mod status_report;
mod trigger_command;
mod version;

pub use crate::control_commands::{DownSignal, MachineryDeps};
pub use crate::operator_surface::{DaemonFacts, OperatorDeps, operator_registry};
/// The resume path's handle, out here because `OperatorDeps` carries
/// one: a caller that assembles the operator surface — this daemon, or
/// an integration test standing one up — has to be able to name it.
pub use crate::resume::ResumeControl;

use crate::cli::{FqdArgs, init_tracing};

/// The `fqd` entry point: the daemon and nothing else. `fq run`
/// remains a compatibility alias until the Phase-5 split completes;
/// both drive the same `run_daemon` path, so the edge and every other
/// daemon behaviour land once, in shared code.
#[tokio::main]
pub async fn fqd_main() -> ExitCode {
    let args = FqdArgs::parse();
    init_tracing(args.global.log_format);
    // The daemon keeps SIGPIPE ignored (Rust's startup default): a
    // long-running process must not be killable by a closed stdout —
    // the same disposition `fq run` runs under.
    match run_daemon(&args.global).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Long-running foreground runtime. Connects to NATS, opens the
/// projection store, spawns two tokio tasks — the projection
/// consumer and the NATS trigger dispatcher — and waits for
/// either Ctrl-C or a premature task failure.
///
mod boot;
mod daemon;
mod hosted;
mod signals;

use crate::daemon::run_daemon;
