//! The MCP servers one invocation runs with: the grant-bearing ones it
//! starts for itself, and the shared ones it needs to be up already.
//!
//! Split from `runner.rs` to keep that file inside its size budget — a
//! child module, so the runner's private config stays private while
//! this code can still reach it. The seam is a real one either way:
//! both halves answer "which servers does this agent need, and are
//! they there?", which is a different question from the step loop
//! below it.
//!
//! **Refusal is the point of the second half.** A shared server that
//! failed to start has no tools in the registry, and before #548 the
//! agent that declared it ran anyway — the model was simply never
//! offered the tools, so it improvised, and the run failed somewhere
//! downstream with a reason that had nothing to do with the server
//! being down. Refusing at dispatch turns that into one terminal event
//! naming the server and why it is unavailable.

use super::*;

/// The per-invocation MCP servers an agent's grants require, started.
pub(super) struct GrantServers {
    /// Owns the started servers for this invocation; the caller shuts
    /// it down on every exit path, including the pre-WAL ones (#116).
    pub(super) manager: McpClientManager,
    /// The base registry plus this invocation's server tools. `None`
    /// on the common no-grants path, where the shared registry is used
    /// as it stands — no clone, no per-invocation registry.
    pub(super) tools: Option<ToolRegistry>,
    /// The inbound request channels the runner services in its
    /// `select!`. `None` when no grant-bearing server started.
    pub(super) sampling: Option<SamplingChannel>,
}

impl<R: Reducer + Send + Sync> ReducerRunner<R> {
    /// Start the agent's grant-bearing servers (ADR-0018), layering
    /// their tools onto a clone of the base registry.
    ///
    /// Called only after the sandbox has been materialised, so the
    /// roots advertised to a server are the same bound paths the tools
    /// enforce. `sampling` seeds the channel map for a caller that
    /// already has one (the direct `run_with_server_requests` path).
    pub(super) async fn start_grant_servers(
        &self,
        agent: &Agent,
        sandbox: &ToolSandbox,
        tools: &ToolRegistry,
        sampling: Option<SamplingChannel>,
    ) -> GrantServers {
        let agent_id = agent.id().clone();
        let mut manager = self.config.mcp_manager();
        let grant_decls: Vec<_> = agent
            .mcp_servers()
            .iter()
            .filter(|decl| agent.grants_inbound_capability(&decl.server))
            .collect();
        // The common no-grants invocation keeps the shared registry —
        // no clone, no per-invocation registry (the pre-#179 fast
        // path). `Some` only when a grant server will layer tools on.
        let mut invocation_tools: Option<ToolRegistry> =
            (!grant_decls.is_empty()).then(|| tools.clone());
        let mut sampling = sampling.unwrap_or_default();
        for decl in grant_decls {
            let capabilities = AdvertisedCapabilities {
                sampling: agent
                    .sampling_grant()
                    .is_some_and(|g| g.permits(&decl.server)),
                elicitation: agent
                    .elicitation_grant()
                    .is_some_and(|g| g.permits(&decl.server)),
                roots: agent.roots_grant().is_some_and(|g| g.permits(&decl.server)),
            };
            let roots = advertised_roots_from_tool_sandbox(
                sandbox,
                agent.roots_grant(),
                &decl.server,
                &ValidatorChain::new(),
            );
            let config = McpServerConfig {
                name: decl.server.clone(),
                command: decl.command.clone().unwrap_or_default(),
                args: decl.args.clone(),
                env: decl.env.clone(),
                url: decl.url.clone(),
            };
            match manager
                .start_server_with_requests(config, roots, capabilities)
                .await
            {
                Ok((server_tools, rx, _)) => {
                    for tool in server_tools {
                        let registry = invocation_tools
                            .as_mut()
                            .expect("cloned above: grant_decls is non-empty on this path");
                        if let Err(error) = registry.register(tool) {
                            warn!(server = %decl.server, %error, "refusing per-invocation MCP tool registration");
                        }
                    }
                    sampling.insert(decl.server.clone(), rx);
                }
                Err(err) => {
                    warn!(agent_id = %agent_id, server = %decl.server, error = %err, "failed to start grant-bearing MCP server per-invocation; skipping it")
                }
            }
        }
        GrantServers {
            manager,
            tools: invocation_tools,
            sampling: (!sampling.is_empty()).then_some(sampling),
        }
    }

    /// Why this agent cannot run, if a shared MCP server it declares is
    /// unavailable — `None` when every one of them is up, which is
    /// every invocation on a healthy daemon.
    ///
    /// Only *shared* servers can answer here. A grant-bearing server
    /// runs per-invocation under the manager built above, so it has no
    /// standing state to consult and a failure to start it stays what
    /// it was: a warning against that one run.
    pub(super) fn unavailable_mcp_servers(&self, agent: &Agent) -> Option<String> {
        let now_ms = self.config.clock.unix_now_ms();
        let refused: Vec<String> = agent
            .mcp_servers()
            .iter()
            .filter(|decl| !agent.grants_inbound_capability(&decl.server))
            .filter_map(|decl| match self.config.mcp_states.state(&decl.server) {
                Some(crate::mcp::McpServerState::Unavailable {
                    reason,
                    attempts,
                    next_retry_at_ms,
                }) => Some(format!(
                    "'{}' ({reason}; {attempts} attempt(s), {})",
                    decl.server,
                    match next_retry_at_ms {
                        Some(at) => format!("next retry in {}s", (at - now_ms).max(0) / 1000),
                        None => "no further retries configured".to_string(),
                    }
                )),
                _ => None,
            })
            .collect();
        (!refused.is_empty()).then(|| {
            format!(
                "MCP server(s) this agent declares are unavailable, so the tools it needs are \
                 not registered: {}. The daemon retries them in the background; `fq doctor` \
                 reports their state.",
                refused.join(", ")
            )
        })
    }

    /// Refuse the invocation with a terminal `failed` event naming the
    /// server, rather than running it without the tools it declared.
    ///
    /// The event is the whole point: a silent skip left an operator
    /// with an agent that failed for an unrelated-looking reason, and a
    /// hang left them with nothing at all. `Setup` is the phase because
    /// nothing of the agent's work has begun — and nothing of it has,
    /// literally: this runs before the workspace is provisioned and
    /// before a single grant-bearing server is spawned, so a refused
    /// invocation costs one pair of events and no child processes.
    ///
    /// `triggered` is published first regardless. The failure has to
    /// hang off something: a `failed` with no chain root is an event
    /// the projection cannot attribute to an invocation.
    pub(super) async fn refuse_for_unavailable_mcp(
        &self,
        agent: &Agent,
        invocation_id: Uuid,
        trigger: &Trigger,
        message: String,
        totals: InvocationTotals,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let agent_id = agent.id().clone();
        let mut cursor: Option<Uuid> = None;
        self.publish_chained(&mut cursor, triggered_event(agent, invocation_id, trigger))
            .await?;
        let kind = FailureKind::RuntimeError;
        self.emit_failed(
            &agent_id,
            invocation_id,
            kind,
            message.clone(),
            FailurePhase::Setup,
            totals,
            &mut cursor,
        )
        .await?;
        Err(ExecutorError::InvocationFailed { kind, message })
    }
}

/// The `triggered` event that opens an invocation's chain.
///
/// Its own function because two paths publish it now — the ordinary
/// one, and the refusal above, which has to open a chain before it can
/// close one. Cloning what it carries rather than moving it is what
/// lets the caller keep the trigger; the ordinary path built its own
/// step-0 payload from the same fields anyway.
pub(super) fn triggered_event(agent: &Agent, invocation_id: Uuid, trigger: &Trigger) -> Event {
    Event::new(
        agent.id().clone(),
        invocation_id,
        EventPayload::Triggered(TriggeredPayload {
            trigger_id: Some(trigger.id),
            trigger_source: trigger.source,
            trigger_subject: trigger.subject.clone(),
            trigger_payload: trigger.payload.clone(),
            config_snapshot: agent.to_snapshot(),
        }),
    )
}
