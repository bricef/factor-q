use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser)]
#[command(name = "fq", about = "factor-q agent runtime")]
struct Cli {
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
        path: String,
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
async fn main() {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => {
            println!("factor-q project initialisation not yet implemented");
        }
        Commands::Run => {
            println!("factor-q runtime not yet implemented");
        }
        Commands::Trigger { agent, payload } => {
            println!("Triggering agent: {agent}, payload: {payload:?}");
        }
        Commands::Agent { command } => match command {
            AgentCommands::List => {
                println!("Agent listing not yet implemented");
            }
            AgentCommands::Validate { path } => {
                println!("Validating agent definition: {path}");
            }
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
}
