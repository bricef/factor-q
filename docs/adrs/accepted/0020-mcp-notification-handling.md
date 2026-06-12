# ADR-0020: MCP server notifications — drained in the daemon, tools refresh between invocations

## Status
Accepted (2026-06-12)

## Context

Connected MCP servers push out-of-band notifications at the client:
`tools/list_changed`, `resources/updated` / `list_changed`,
`prompts/list_changed`, log records (`notifications/message`), and
progress. The MCP full-spec work (Step 7) built the receiving
machinery — a unified `ServerNotification` sink per server,
`refresh_tools`, `call_tool_cancellable` — but nothing in the running
daemon consumed it. Two consequences:

- **Unbounded growth.** Each server's notification channel is
  unbounded and never drained, so a chatty server grows the daemon's
  memory for the life of the process.
- **Built-but-dead mechanisms.** A `tools/list_changed` never refreshes
  the live `ToolRegistry`; the tool list the daemon discovered at boot
  is served until restart.

The obvious "full" fix — hot-swapping a server's tools into the
**in-flight** invocation — is structurally invasive and behaviorally
questionable:

- The per-invocation effective registry is built once at invocation
  start and borrowed immutably through the whole step loop; making it
  shared-mutable mid-loop touches every dispatch path.
- An agent that reasoned for N steps against one tool set and silently
  gains/loses tools at step N+1 is a consistency hazard — the model's
  earlier reasoning no longer matches its action space. The
  `ConfigSnapshot` already pins the principle: **an invocation runs
  with the configuration it started with.**
- Tool lists changing mid-invocation are rare in practice (the
  reference everything server emits `list_changed` once, at startup).

## Decision

1. **The daemon drains every shared server's notification channel** in
   a background task (receivers are extracted from the
   `McpClientManager` at boot; the manager keeps its `&mut` lifecycle —
   `shutdown()` — in `main`). No unbounded accumulation.
2. **Logs and progress fold into `tracing`.** (Log records are already
   traced at the handler; the drain consumes them. Bridging logs onto
   the event bus is the separate logs→bus step of the MCP-completion
   plan; richer operator surfacing is owned by the Observability
   backlog.)
3. **`tools/list_changed` refreshes the registry *between*
   invocations.** The drain re-discovers the server's tools via a
   cloneable refresher handle (per-server client `Arc`s — the
   `McpResourceReader` pattern), rebuilds the registry
   (built-ins + every shared server's current tools; the registry is
   register-only, so rebuild-from-scratch is the honest operation), and
   installs it into the shared `ReducerContext`. The **next**
   invocation picks it up; **in-flight invocations keep the registry
   they started with**, consistent with `ConfigSnapshot` semantics.
4. **Per-invocation (grant-bearing) servers need none of this** — they
   are started fresh per invocation (ADR-0018), so every invocation
   already sees their current tool list.

### Explicitly deferred

- **Mid-invocation hot-swap** of a server's tools into a running step
  loop. Revisit only if a real workload demonstrates servers that
  mutate their tool list mid-call *and* agents that must observe it
  within the same invocation.
- **A cancellation trigger.** `call_tool_cancellable` exists and is
  tested, but the daemon has no abort *source* (timeout / budget-abort
  / shutdown interlock) wired to it. That belongs to whichever feature
  first produces a real abort signal (e.g. stuck-invocation detection,
  backlog).

## Consequences

- `ReducerContext`'s tool registry becomes swappable
  (interior-mutable `Arc<ToolRegistry>`): invocations clone the `Arc`
  at start and stay consistent; the drain installs replacements.
- The drain task is where future notification→action loops hook in
  (logs→bus lands there next; `resources/updated` invalidation and
  operator progress surfacing later).
- A server that mutates its tool list is fully supported across
  invocations and intentionally not supported within one.
