use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use fq_runtime::agent::{definition::parse_agent, AgentRegistry};
use fq_runtime::Config;
use tracing::error;
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_CONFIG_PATH: &str = "fq.toml";

#[derive(Parser)]
#[command(name = "fq", about = "factor-q agent runtime")]
struct Cli {
    /// Path to the config file
    #[arg(long, global = true, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialise a new factor-q project
    Init,
    /// Run the runtime in the foreground
    Run,
    /// Trigger an agent manually
    Trigger {
        /// Agent name
        agent: String,
        /// Optional payload
        payload: Option<String>,
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
        /// Filter by time
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// List registered agent definitions
    List,
    /// Validate an agent definition file
    Validate {
        /// Path to agent definition
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum EventCommands {
    /// Tail the event stream in real time
    Tail {
        /// Subject filter
        #[arg(long)]
        subject: Option<String>,
    },
    /// Query the event history
    Query {
        /// Filter by agent
        #[arg(long)]
        agent: Option<String>,
        /// Filter by event type
        #[arg(long, name = "type")]
        event_type: Option<String>,
        /// Filter by time
        #[arg(long)]
        since: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Commands::Init => {
            println!("factor-q project initialisation not yet implemented");
        }
        Commands::Run => {
            let config = Config::load_or_default(&cli.config)?;
            println!("Loaded config: NATS at {}", config.nats.url);
            println!("Agent directory: {}", config.agents.directory.display());
            println!("(Runtime not yet implemented.)");
        }
        Commands::Trigger { agent, payload } => {
            println!("Triggering agent: {agent}, payload: {payload:?}");
        }
        Commands::Agent { command } => match command {
            AgentCommands::List => list_agents(&cli.config)?,
            AgentCommands::Validate { path } => validate_agent(&path)?,
        },
        Commands::Events { command } => match command {
            EventCommands::Tail { subject } => {
                println!("Tailing events, filter: {subject:?}");
            }
            EventCommands::Query {
                agent,
                event_type,
                since,
            } => {
                println!("Querying events: agent={agent:?}, type={event_type:?}, since={since:?}");
            }
        },
        Commands::Costs { agent, since } => {
            println!("Cost breakdown: agent={agent:?}, since={since:?}");
        }
    }
    Ok(())
}

fn list_agents(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load_or_default(config_path)?;
    let dir = &config.agents.directory;

    if !dir.exists() {
        println!("Agent directory {} does not exist.", dir.display());
        return Ok(());
    }

    let registry = AgentRegistry::load_from_directory(dir)?;

    if registry.is_empty() && registry.errors().is_empty() {
        println!("No agents found in {}", dir.display());
        return Ok(());
    }

    if !registry.is_empty() {
        println!("Loaded {} agent(s) from {}:", registry.len(), dir.display());
        let mut agents: Vec<_> = registry.iter().collect();
        agents.sort_by(|a, b| a.agent.id().as_str().cmp(b.agent.id().as_str()));
        for loaded in agents {
            println!(
                "  {:<30} model={} tools={} path={}",
                loaded.agent.id().as_str(),
                loaded.agent.model(),
                loaded.agent.tools().len(),
                loaded.path.display()
            );
        }
    }

    if !registry.errors().is_empty() {
        println!();
        println!("Errors ({}):", registry.errors().len());
        for err in registry.errors() {
            println!("  {err}");
        }
    }

    Ok(())
}

fn validate_agent(path: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)
        .map_err(|err| anyhow::anyhow!("failed to read {}: {err}", path.display()))?;

    match parse_agent(&content) {
        Ok(agent) => {
            println!("✓ {} is valid", path.display());
            println!("  id:      {}", agent.id().as_str());
            println!("  model:   {}", agent.model());
            println!("  tools:   {}", agent.tools().len());
            if let Some(budget) = agent.budget() {
                println!("  budget:  ${budget:.2}");
            }
            Ok(())
        }
        Err(err) => Err(anyhow::anyhow!("{} is invalid: {err}", path.display())),
    }
}
