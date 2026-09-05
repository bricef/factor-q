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
mod shared_servers;
mod status_report;
mod trigger_command;
mod version;
mod worker_registration;

pub use crate::control_commands::{DownMode, DownSignal, MachineryDeps};
pub use crate::operator_surface::{DaemonFacts, OperatorDeps, operator_registry};
/// The resume path's handle, out here because `OperatorDeps` carries
/// one: a caller that assembles the operator surface — this daemon, or
/// an integration test standing one up — has to be able to name it.
pub use crate::resume::ResumeControl;

use crate::cli::{FqdArgs, init_tracing};

/// The `fqd` entry point: the daemon and nothing else.
#[tokio::main]
pub async fn fqd_main() -> ExitCode {
    let args = FqdArgs::parse();
    init_tracing(args.global.log_format);
    // The daemon keeps SIGPIPE ignored (Rust's startup default): a
    // long-running process must not be killable by a closed stdout.
    // The client sets the opposite disposition, for the opposite
    // reason: `fq events tail | head` should end quietly.
    match run_daemon(&args.global).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

mod boot;
mod daemon;
mod hosted;
mod signals;

use crate::daemon::run_daemon;
