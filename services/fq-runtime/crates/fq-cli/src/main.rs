#![allow(unused_imports)]
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
#[path = "cli/args.rs"]
mod args;
#[path = "cli/core.rs"]
mod core;
#[path = "cli/daemon.rs"]
mod daemon;
#[path = "cli/doctor.rs"]
mod doctor;
#[path = "cli/events.rs"]
mod events;
#[path = "cli/invocation.rs"]
mod invocation;
#[path = "cli/workers.rs"]
mod workers;

use args::*;
use core::*;
use daemon::*;
use doctor::*;
use events::*;
use invocation::*;
use workers::*;

#[tokio::main]
async fn main() -> ExitCode {
    entry().await
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
