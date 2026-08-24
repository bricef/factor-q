//! The `fq`/`fqd` binaries' library half: the composition root.
//!
//! What lives here is the wiring nothing else can own — the two entry points,
//! the dispatch from a parsed `cli::Commands` to the verb that serves it,
//! the handful of primitives every verb shares (store paths, the read views),
//! and the daemon itself. Every verb group is a module: `status`, `doctor`,
//! `invocations`, `trigger`, `workers`, `events`, and so on (#189).
//!
//! `run_daemon` is the one large inhabitant that stays. Its startup recovery
//! and its control listeners are modules of their own (`recovery`,
//! `listeners`); what remains is the ordered wiring of the supervised task
//! set, the select that watches it, and the teardown that unwinds it — three
//! things that share a dozen live bindings and cannot be separated without
//! changing behaviour. Splitting those is the rest of #189.

use std::process::ExitCode;

use clap::Parser;

use crate::agents::{list_agents, validate_agent};
use crate::cli::{
    AgentCommands, Cli, Commands, DeadLetterCommands, EventCommands, InvocationCommands,
    OpsCommands, TokenCommands, WorkerCommands, init_tracing,
};
use crate::connections::{connect, ops_list, token_attenuate};
use crate::control::{down_daemon, reload_daemon};
use crate::costs::show_costs;
use crate::dead_letters::{list_dead_letters, requeue_dead_letter};
use crate::doctor::doctor;
use crate::events::{get_event, query_events, tail_events};
use crate::invocations::{
    invocation_drop, invocation_list, invocation_resume, invocation_show, invocation_transcript,
};
use crate::project::init_project;
use crate::status::show_status;
use crate::trigger::publish_trigger;
use crate::version::print_version;
use crate::workers::{workers_list, workers_show};

/// The `fq` entry point: the operator CLI (and, until the Phase-5
/// split completes, the daemon via `fq run`).
#[tokio::main]
pub async fn fq_main() -> ExitCode {
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
    // panics on). The daemon keeps the ignore disposition: a
    // long-running path must not be killable by a closed stdout, and
    // the exec tool's child processes inherit whatever disposition is
    // in effect at spawn time. `fq trigger` used to be on that list
    // because it ran the reducer in-process; it is a request now
    // (D-1), so it filters like every other command.
    //
    // `fq down` joins the daemon on that list, for a reason that only
    // appeared once it started speaking the edge: it is the one verb
    // whose job is to make its peer go away, so it is *expected* to be
    // writing to a socket the daemon has already closed — a tarpc
    // client shutting down, or the next liveness poll dialling a
    // process that has exited. Tokio's socket writes are plain
    // `write(2)`, which raises SIGPIPE on a closed peer, so under the
    // default disposition the stop verb is killed by the very success
    // it is waiting to confirm. Caught by `daemon_stops_now_on_fq_down_now`:
    // `--now` exits fast enough to win that race every time, while the
    // drain path merely usually lost it.
    #[cfg(unix)]
    if !matches!(cli.command, Commands::Down { .. }) {
        // SAFETY: changing a process signal disposition before any
        // output has been written; no handler is installed, only the
        // kernel default is restored.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Fatal CLI errors are an unconditional stderr contract: they must
            // neither pollute machine-readable stdout nor vanish under RUST_LOG=off.
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Commands::Init { force } => init_project(force)?,
        Commands::Reload => reload_daemon(&cli.global).await?,
        Commands::Down { now } => down_daemon(&cli.global, now).await?,
        Commands::Trigger {
            agent,
            payload,
            // Retired (D-1): the in-process runner is gone, so both
            // spellings mean the same thing — ask the daemon.
            via_nats: _,
        } => publish_trigger(&cli.global, &agent, payload.as_deref()).await?,
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
            AgentCommands::List => list_agents(&cli.global).await?,
            AgentCommands::Validate { path } => validate_agent(&path)?,
        },
        Commands::Events { command } => match command {
            EventCommands::Tail {
                agent,
                event_type,
                json,
            } => tail_events(&cli.global, agent, event_type, json).await?,
            EventCommands::Query {
                agent,
                event_type,
                since,
                limit,
                json,
            } => query_events(&cli.global, agent, event_type, since, limit, json).await?,
            EventCommands::Get { event_id, json } => {
                get_event(&cli.global, &event_id, json).await?
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
            InvocationCommands::Drop {
                id,
                reason,
                live,
                json,
            } => invocation_drop(&cli.global, &id, reason.as_deref(), live, json).await?,
            InvocationCommands::Resume { id, reason, json } => {
                invocation_resume(&cli.global, &id, reason.as_deref(), json).await?
            }
            InvocationCommands::Transcript {
                id,
                follow,
                json,
                format,
                full,
            } => invocation_transcript(&cli.global, &id, follow, json, format, full).await?,
        },
        Commands::Workers { command } => match command {
            WorkerCommands::List {
                stale_only,
                alive_only,
                json,
            } => workers_list(&cli.global, stale_only, alive_only, json).await?,
            WorkerCommands::Show { id, json } => workers_show(&cli.global, &id, json).await?,
        },
        Commands::Connect {
            addr,
            token,
            fingerprint,
        } => connect(&cli.global, addr, token, fingerprint).await?,
        Commands::Ops { command } => match command {
            OpsCommands::List { addr, json } => ops_list(&cli.global, addr, json).await?,
        },
        Commands::Token { command } => match command {
            TokenCommands::Attenuate { grant, token, addr } => {
                token_attenuate(&cli.global, &grant, token, addr)?
            }
        },
        Commands::Version { json } => print_version(json),
    }
    Ok(())
}

// The verb groups and the atoms behind them. One module per group, on the
// `workers.rs` precedent (#189): a subcommand's rendering, and the daemon-side
// declaration it rides, each belong in their own file.
mod agents;
mod cli;
mod config;
mod connections;
mod control;
mod costs;
mod dead_letters;
mod doctor;
mod edge_call;
mod events;
mod invocations;
mod project;
mod status;
mod trigger;
mod version;
mod workers;

/// Compact single-line JSON, truncated for terminal display.
pub(crate) fn truncate_json(value: &serde_json::Value, max: usize) -> String {
    let s = value.to_string();
    if s.len() > max {
        format!("{}…", &s[..s.floor_char_boundary(max)])
    } else {
        s
    }
}
