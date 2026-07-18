use crate::*;

/// Trigger an agent by name. Loads the registry, resolves the agent,
/// connects to NATS, loads the pricing table, then drives the
/// reducer runner against a real LLM client.
pub(crate) async fn trigger_agent(
    global: &GlobalArgs,
    agent_name: &str,
    payload: Option<&str>,
) -> anyhow::Result<()> {
    let config = global.resolve_config()?;

    // Resolve and load the registry.
    let registry = AgentRegistry::load_from_directory(
        &config.agents.directory,
        config.agents.default_model.as_deref(),
    )
    .context("failed to load agent registry")?;
    let agent_id =
        AgentId::new(agent_name).with_context(|| format!("invalid agent name '{agent_name}'"))?;
    let loaded = registry.get_loaded(&agent_id).ok_or_else(|| {
        let available: Vec<String> = registry
            .iter()
            .map(|l| l.agent.id().as_str().to_string())
            .collect();
        anyhow::anyhow!(
            "agent '{agent_name}' not found in {}. Available: {}",
            config.agents.directory.display(),
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        )
    })?;
    println!(
        "Loaded agent '{}' from {}",
        loaded.agent.id(),
        loaded.path.display()
    );

    // Connect to NATS.
    println!("Connecting to NATS at {}...", config.nats.url);
    let bus = EventBus::connect(&config.nats.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", config.nats.url))?;

    // Load pricing (cached path on disk, fetches on startup), merge
    // config overrides, and enforce the coverage guarantee (ADR-0004).
    let cache_path = config.cache.directory.join("pricing.json");
    let pricing =
        build_validated_pricing(&config, &registry, PricingTable::load(&cache_path).await)?;
    let pricing = Arc::new(pricing);
    println!("Loaded {} pricing entries", pricing.len());

    // Real LLM client — genai resolves API keys from provider-specific
    // environment variables (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc).
    // Routes each model to the [providers.<name>] that declares it,
    // honouring per-provider base_url/api_key_env (ADR-0003).
    let llm = GenAiClient::from_providers(&config.providers);
    // Retry transient LLM errors (rate limits, transport failures) with
    // backoff instead of failing the whole invocation (issue #10).
    let llm = fq_runtime::llm::RetryingLlmClient::new(llm, config.worker.llm_retry.clone());

    // Parse trigger payload: try JSON first, fall back to string literal.
    let trigger_payload: Value = match payload {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())),
        None => Value::Null,
    };

    // Build tool registry: built-ins + MCP servers declared by this agent.
    let mut tools = ToolRegistry::with_builtins_exec(config.tools.exec.to_exec_config());
    let mut mcp_manager = McpClientManager::new();
    start_agent_mcp_servers(&loaded.agent, &mut tools, &mut mcp_manager, false).await;
    if !loaded.agent.mcp_servers().is_empty() {
        println!(
            "  MCP tools:        {} (from {} server(s))",
            tools.len() - fq_runtime::tools::BUILTIN_TOOL_COUNT,
            loaded.agent.mcp_servers().len()
        );
    }

    let tools = Arc::new(tools);
    println!("Running agent...");
    // Tool/LLM dispatches are persisted through the worker
    // WAL. The store opens against the same worker.db the
    // daemon would use; if `fq run` is also active the same
    // file is shared (locks at the SQLite layer).
    let db_paths = ensure_split_dbs(&config).await?;
    let worker_store = Arc::new(
        // allow-direct-store-open: `fq trigger` is a one-shot worker — it writes the WAL.
        fq_runtime::WorkerStore::open(&db_paths.worker)
            .await
            .with_context(|| {
                format!(
                    "failed to open worker store at {}",
                    db_paths.worker.display()
                )
            })?,
    );
    // Each ad-hoc `fq trigger` is a one-shot worker instance.
    // The runtime-id-shaped worker_id matches the daemon's
    // naming so any archive hand-off the runner performs
    // routes the ack subject
    // (`fq.worker.{id}.invocation.archive_acked`) consistently.
    let cli_worker_id = fq_runtime::worker::WorkerId::new(uuid::Uuid::now_v7().to_string())
        .expect("uuid is a valid worker id");
    let runner = fq_runtime::ReducerRunner::new(
        Arc::new(
            fq_runtime::ReducerContext::builder()
                .tools(tools)
                .resources(mcp_manager.resource_reader())
                .build(),
        ),
        Arc::new(
            fq_runtime::RunnerConfig::builder()
                .bus(bus)
                .pricing(pricing)
                .store(worker_store)
                .worker_id(cli_worker_id)
                .max_iterations(config.max_iterations)
                .enforce_pricing(true)
                .workspace(workspace_provider(&config))
                .build(),
        ),
        fq_runtime::Harness::new(),
    );
    let outcome_result = runner
        .run(
            &loaded.agent,
            &llm,
            TriggerSource::Manual,
            None,
            trigger_payload,
        )
        .await;
    let outcome = match outcome_result {
        Ok(outcome) => outcome,
        Err(fq_runtime::ExecutorError::Llm(fq_runtime::LlmError::Auth(msg))) => {
            mcp_manager.shutdown().await;
            anyhow::bail!(
                "LLM authentication failed.\n\
                 \n\
                 This usually means the provider-specific API key environment\n\
                 variable is not set. For Anthropic, export ANTHROPIC_API_KEY\n\
                 before running `fq trigger`.\n\
                 \n\
                 Underlying error: {msg}"
            );
        }
        Err(err) => {
            mcp_manager.shutdown().await;
            return Err(err.into());
        }
    };

    mcp_manager.shutdown().await;

    println!();
    match outcome {
        InvocationOutcome::Completed {
            invocation_id,
            response,
            cost,
            duration_ms,
        } => {
            println!("✓ Completed in {duration_ms}ms (cost: ${cost:.6})");
            println!("  invocation: {invocation_id}");
            if let Some(content) = response.content {
                println!();
                println!("Response:");
                for line in content.lines() {
                    println!("  {line}");
                }
            }
        }
        InvocationOutcome::BudgetExceeded {
            invocation_id,
            cost,
        } => {
            println!("✗ Budget exceeded: cost ${cost:.6}");
            println!("  invocation: {invocation_id}");
        }
        InvocationOutcome::Suspended { invocation_id } => {
            // A drain suspended the run at a step boundary; the row
            // stays in-flight for recovery to resume under the next
            // binary. An in-process `fq trigger` has no drain source, so
            // this is effectively unreachable here — but the match is
            // total.
            println!("⏸ Suspended at a step boundary (drained); recovery will resume it");
            println!("  invocation: {invocation_id}");
        }
    }

    Ok(())
}
