# Backlog

Deferred work that isn't committed to a specific phase yet. Items
accumulate here when they're flagged during design or implementation
but aren't important enough to block a phase closing.

Ordering within a section is **priority-ish** — the things at the
top are more likely to be pulled into the next phase. Nothing in
here should be assumed scheduled.

## Deferred from phase 1

### Hot-reload of agent definitions
**Source:** phase 1 plan (`docs/plans/closed/2026-04-02-phase-1-foundation.md`), stretch goals section.

The runtime currently loads agent definitions once at `fq run`
start. Editing a `.md` file does not affect a running daemon.

Work needed:
- File watcher on the configured agents directory (`notify` crate
  is the usual choice)
- Debounce burst changes (common when an editor writes via a
  temp file and rename)
- Parse the new definition; reject invalid changes with a clear
  log line and keep the old one
- Swap the definition in the registry atomically
- Decide what happens to in-flight invocations using the old
  definition — simplest is "they finish with the version they
  started with" (ConfigSnapshot already supports this)

Not urgent: operators can `kill` and restart the daemon. NATS
durability means no triggers are lost. But iterative development
is noticeably faster with hot-reload.

### Second LLM provider wired end-to-end
**Source:** phase 1 plan, stretch goals section.

`LlmClient` is a trait and `genai` supports Anthropic, OpenAI,
Gemini, Ollama, xAI, and others. Structurally we can target any
of them. In practice, only Anthropic is exercised end-to-end, and
the pricing lookup only has examples tested against Anthropic
model IDs.

Work needed:
- Verify the genai adapter handles provider-specific quirks for at
  least one non-Anthropic target (OpenAI is the obvious choice)
- Update the sample agents with a second-provider variant
- Add a smoke test that exercises the second provider behind an
  opt-in env var (e.g. `OPENAI_API_KEY`)
- Document the provider-pinning story in the agent definition docs

Mostly mechanical. Real value is small until we need per-call
model routing in a multi-agent system.

## Known gaps flagged during phase 1 design

### Scheduled refresh of pricing data (internal job scheduler)
**Source:** `docs/plans/closed/2026-04-02-phase-1-foundation.md` deferred-work section, and `docs/design/storage-and-scaling.md`.

Pricing is fetched once at `fq run` start from the LiteLLM JSON.
Continuously running deployments will keep stale pricing
indefinitely, which is fine operationally but produces drift over
weeks.

The right place to fix this is a general internal job scheduler
that can run both pricing refreshes and user-facing scheduled
agent triggers. See the phase 1 plan's "Scheduled refresh of
pricing data" section for full rationale.

Related future work: scheduled agent triggers themselves are in
the design space but not yet scoped.

### Container-level sandboxing (ADR-0010)
**Source:** [`docs/adrs/draft/0010-agent-execution-isolation.md`](../adrs/draft/0010-agent-execution-isolation.md) and the shell tool known-gaps section.

The shell, file_read, and file_write tools enforce sandboxing at
the **process level**: path canonicalisation for the file tools,
`exec_cwd` plus argv-only invocation plus output caps for the
shell tool. This defeats path traversal, symlink escape, and
shell injection — but cannot defeat:

- PATH-visible binaries (an agent with `shell` can call `curl`)
- Network connections the child process opens itself
- CPU/memory consumption above what cgroups can limit
- Kernel-level escapes requiring seccomp

Closing these requires container or VM-level isolation. ADR-0010
is the open decision about which approach (per-invocation
containers, rootless containers, firejail/bwrap, etc.) factor-q
adopts.

Not blocking for single-tenant self-hosted phase 1. Becomes
urgent when:
- Agents handle untrusted input
- Multi-tenant or multi-user operation becomes a concern
- Compliance or audit requirements appear

### Continuous learning
**Source:** `ARCHITECTURE.md` subsystems section.

Agent performance should be reviewed and instructions, prompts,
and workflows updated based on outcomes. The event bus provides
the raw material — full traces of actions and results — but the
learning loop itself does not exist. This was identified as a
core subsystem during the vision/architecture phase but is
entirely future work.

This belongs in a later phase once we have stable multi-agent
operation and real usage data to learn from.

## Known gaps flagged during phase 1 implementation

### Agent env allowlist plumbed through ToolSandbox
**Source:** inline comment in `fq-tools/src/builtin/shell.rs`,
`allowed_env_vars` function.

The agent definition can declare an env var allowlist via
`sandbox.env: [HOME, PATH]`. The declaration is parsed, round
trips through the Agent builder, and is included in the Triggered
event's ConfigSnapshot. But it is **not** currently plumbed
through `ToolSandbox` into the shell tool's child process env.
The shell tool's child env is hardcoded to a minimal baseline
(just PATH).

Work needed:
- Extend `ToolSandbox` with an `env_allowlist: Vec<String>` field
- Update `agent::Sandbox::to_tool_sandbox` to populate it
- Update `ShellTool::execute` to layer the allowlisted vars on top
  of the default baseline at child spawn time
- Add a test that an agent declaring `env: [HOME]` actually sees
  `HOME` in `env` tool output

Small, straightforward. Moved to backlog because the shell tool
works fine for the current use cases without it.

### Multi-runtime / horizontal scaling of dispatchers
**Source:** `fq-runtime/src/bus.rs`, trigger stream design comment.

The trigger stream uses `Limits` retention (not `WorkQueue`)
because NATS disallows overlapping consumer filters on work-queue
streams, which breaks parallel test isolation. For phase 1
single-runtime this is fine — triggers age out within 24h and
the single dispatcher consumes them within seconds.

For multi-runtime horizontal scaling (multiple `fq run` instances
sharing a NATS), we need:
- Some form of work distribution that guarantees each trigger is
  handled by exactly one instance
- Either per-instance consumer names (each sees every trigger,
  each decides independently whether to handle it — risky), or
  a queue-group pattern on top of Limits, or revisiting the
  workqueue decision with a test-isolation story

Not needed until we have multi-runtime deployments to worry about.

## Process and documentation gaps

### ADRs still in draft
The following ADRs are still in `docs/adrs/draft/` and should
either be accepted (with a resolution documented) or deliberately
deprecated:
- ADR-0006 API design
- ADR-0007 Inter-agent communication
- ADR-0008 Extension model
- ADR-0010 Agent execution isolation

Phase 2 (MCP, memory, skills) will likely force ADR-0008 to
resolution. ADR-0007 and ADR-0010 can wait for multi-agent and
security-sensitive deployments respectively.
