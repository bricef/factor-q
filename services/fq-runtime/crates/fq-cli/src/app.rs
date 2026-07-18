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

/// Start one shared MCP server and register every tool it exposes.
async fn start_mcp_server(
    manager: &mut McpClientManager,
    tools: &mut ToolRegistry,
    decl: &fq_runtime::agent::McpServerDeclaration,
) -> anyhow::Result<()> {
    let config = McpServerConfig {
        name: decl.server.clone(),
        command: decl.command.clone().unwrap_or_default(),
        args: decl.args.clone(),
        env: decl.env.clone(),
        url: decl.url.clone(),
    };
    for tool in manager.start_server(config).await? {
        if let Err(error) = tools.register(tool) {
            tracing::warn!(server = %decl.server, %error, "refusing MCP tool registration");
        }
    }
    Ok(())
}

include!("cli/args.rs");
include!("cmd/core.rs");
include!("cmd/trigger.rs");
include!("cmd/status.rs");
include!("cmd/doctor.rs");
include!("daemon/mod.rs");
include!("cmd/events.rs");
include!("cmd/invocation.rs");
include!("cmd/workers.rs");
