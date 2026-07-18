
/// Initialise the global tracing subscriber. Both branches share the
/// same `EnvFilter` wiring — `RUST_LOG` (or `info` by default) governs
/// levels identically — and differ only in how each event is rendered:
///
/// - [`LogFormat::Text`] keeps the human-readable ANSI output (the
///   default, so existing behaviour is unchanged).
/// - [`LogFormat::Json`] emits one JSON object per log line so a log
///   aggregator (ELK, Loki, Datadog) can query the structured fields
///   directly instead of regex-scraping (issue #36).
fn init_tracing(format: LogFormat) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Text => fmt().with_env_filter(env_filter).init(),
        LogFormat::Json => fmt().with_env_filter(env_filter).json().init(),
    }
}

#[tokio::main]
pub(crate) async fn entry() -> ExitCode {
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
    // panics on). The daemon and the in-process trigger keep the
    // ignore disposition: long-running paths must not be killable by a
    // closed stdout, and the exec tool's child processes inherit
    // whatever disposition is in effect at spawn time.
    #[cfg(unix)]
    if !matches!(cli.command, Commands::Run | Commands::Trigger { .. }) {
        // SAFETY: changing a process signal disposition before any
        // output has been written; no handler is installed, only the
        // kernel default is restored.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }

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
        Commands::Init { force } => init_project(force)?,
        Commands::Run => run_daemon(&cli.global).await?,
        Commands::Reload => reload_daemon(&cli.global).await?,
        Commands::Down { now } => down_daemon(&cli.global, now).await?,
        Commands::Trigger {
            agent,
            payload,
            via_nats,
        } => {
            if via_nats {
                publish_trigger(&cli.global, &agent, payload.as_deref()).await?
            } else {
                trigger_agent(&cli.global, &agent, payload.as_deref()).await?
            }
        }
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
            AgentCommands::List => list_agents(&cli.global)?,
            AgentCommands::Validate { path } => validate_agent(&path)?,
        },
        Commands::Events { command } => match command {
            EventCommands::Tail { subject } => tail_events(&cli.global, &subject).await?,
            EventCommands::Query {
                agent,
                event_type,
                since,
                limit,
                json,
            } => {
                query_events(
                    &cli.global,
                    agent.as_deref(),
                    event_type.as_deref(),
                    since.as_deref(),
                    limit,
                    json,
                )
                .await?
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
            InvocationCommands::Drop { id, reason, json } => {
                invocation_drop(&cli.global, &id, reason.as_deref(), json).await?
            }
            InvocationCommands::Transcript {
                id,
                follow,
                format,
                full,
            } => invocation_transcript(&cli.global, &id, follow, format, full).await?,
        },
        Commands::Workers { command } => match command {
            WorkerCommands::List {
                stale_only,
                alive_only,
                json,
            } => workers_list(&cli.global, stale_only, alive_only, json).await?,
            WorkerCommands::Show { id, json } => workers_show(&cli.global, &id, json).await?,
            WorkerCommands::Prune { dry_run } => workers_prune(&cli.global, dry_run).await?,
        },
        Commands::Version { json } => print_version(json),
    }
    Ok(())
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

/// Print version + build information: semver, commit, build date, target.
fn print_version(json: bool) {
    let build_date = FQ_BUILD_EPOCH
        .parse::<i64>()
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string());

    if json {
        let info = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": FQ_GIT_SHA,
            "build_date": build_date,
            "target": FQ_TARGET,
        });
        println!("{}", serde_json::to_string_pretty(&info).unwrap());
    } else {
        println!("fq {}", env!("CARGO_PKG_VERSION"));
        println!("  commit:      {FQ_GIT_SHA}");
        println!("  build date:  {build_date}");
        println!("  target:      {FQ_TARGET}");
    }
}

/// Template files embedded in the binary. Each entry is `(destination,
/// contents)` and is written verbatim when `fq init` runs.
const FQ_TOML_TEMPLATE: &str = include_str!("../templates/fq.toml");

/// Build the `${workspace}` provider from `[workspace]` (parallel-workers
/// Phase 0): with `per_invocation = true` each invocation gets a fresh
/// empty directory under `path`; otherwise every invocation binds to
/// `path` itself. No `path` configured → no binding, and agents that use
/// the token fail loudly at invocation start. Pure filesystem either way
/// — what goes into a workspace is the agent's business.
fn workspace_provider(
    config: &fq_runtime::Config,
) -> Option<std::sync::Arc<dyn fq_runtime::worker::workspace::WorkspaceProvider>> {
    use fq_runtime::worker::workspace::{PerInvocationWorkspace, StaticWorkspace};
    let ws = &config.workspace;
    let path = ws.path.clone()?;
    if ws.per_invocation {
        Some(std::sync::Arc::new(PerInvocationWorkspace::new(path)))
    } else {
        Some(std::sync::Arc::new(StaticWorkspace::new(path)))
    }
}
const README_TEMPLATE: &str = include_str!("../templates/README.md");
const SAMPLE_AGENT_TEMPLATE: &str = include_str!("../templates/sample-agent.md");
const DOCKER_COMPOSE_TEMPLATE: &str = include_str!("../templates/docker-compose.yml");

/// Initialise a new factor-q project in the current working directory.
///
/// Writes four files (plus an `agents/` directory):
/// - `fq.toml`
/// - `README.md`
/// - `docker-compose.yml` (NATS with JetStream)
/// - `agents/sample-agent.md`
///
/// Errors and exits if any of the target files already exist, unless
/// `--force` is set.
fn init_project(force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let fq_toml = cwd.join("fq.toml");
    let readme = cwd.join("README.md");
    let agents_dir = cwd.join("agents");
    let sample_agent = agents_dir.join("sample-agent.md");
    let docker_compose = cwd.join("docker-compose.yml");

    // Detect conflicts up front so the user sees all of them at once
    // rather than fixing them one by one.
    if !force {
        let mut conflicts: Vec<&Path> = Vec::new();
        if fq_toml.exists() {
            conflicts.push(&fq_toml);
        }
        if readme.exists() {
            conflicts.push(&readme);
        }
        if sample_agent.exists() {
            conflicts.push(&sample_agent);
        }
        if docker_compose.exists() {
            conflicts.push(&docker_compose);
        }
        if !conflicts.is_empty() {
            let listing = conflicts
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "the following files already exist:\n{listing}\n\n\
                 Use `fq init --force` to overwrite them."
            );
        }
    }

    std::fs::create_dir_all(&agents_dir)
        .with_context(|| format!("failed to create {}", agents_dir.display()))?;
    write_file(&fq_toml, FQ_TOML_TEMPLATE)?;
    write_file(&readme, README_TEMPLATE)?;
    write_file(&docker_compose, DOCKER_COMPOSE_TEMPLATE)?;
    write_file(&sample_agent, SAMPLE_AGENT_TEMPLATE)?;

    println!("Initialised factor-q project in {}", cwd.display());
    println!();
    println!("Created:");
    println!("  fq.toml");
    println!("  README.md");
    println!("  docker-compose.yml");
    println!("  agents/");
    println!("  agents/sample-agent.md");
    println!();
    println!("Next steps:");
    println!("  1. Start NATS (JetStream) in the background:");
    println!("     docker compose up -d");
    println!("  2. Export your LLM provider API key, e.g.:");
    println!("     export ANTHROPIC_API_KEY='sk-ant-...'");
    println!("  3. Trigger the sample agent:");
    println!("     fq trigger sample-agent \"Say hello in one sentence.\"");
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn list_agents(global: &GlobalArgs) -> anyhow::Result<()> {
    let config = global.resolve_config()?;
    let dir = &config.agents.directory;

    if !dir.exists() {
        let absolute = if dir.is_absolute() {
            dir.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| normalise(&cwd.join(dir)))
                .unwrap_or_else(|_| dir.clone())
        };
        println!(
            "Agent directory {} does not exist (resolved: {}).",
            dir.display(),
            absolute.display()
        );
        return Ok(());
    }

    let registry = AgentRegistry::load_from_directory(dir, config.agents.default_model.as_deref())?;

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
            // #35: valid, but do not let "✓ is valid" imply the declared
            // network boundary holds — nothing enforces it yet.
            if let Some(declared) = agent.sandbox().unenforced_network() {
                println!();
                println!("  ⚠ sandbox.network is declared but NOT enforced (#35)");
                println!("    declared: {}", declared.join(", "));
                println!("    This agent has ambient network access — it can reach any");
                println!("    host regardless. Enforcement: #208 (proxy), #209 (ADR-0010).");
            }
            Ok(())
        }
        Err(err) => Err(anyhow::anyhow!("{} is invalid: {err}", path.display())),
    }
}

