# MCP: Connecting Agents to External Capabilities

factor-q is a full [Model Context Protocol](https://modelcontextprotocol.io)
(MCP) **client** (spec revision 2025-11-25). An agent can connect to
MCP servers to gain capabilities the runtime doesn't ship itself —
not just tools, but resources, prompts, and the server-initiated
capabilities (sampling, elicitation, roots).

This guide covers the capability model and how to use each piece.
For the exact frontmatter syntax, see
[Writing Agent Definitions → MCP servers](agent-definitions.md#mcp-servers).

## What you get

An MCP server is reached either as a child process over **stdio**
(`command:`) or as a remote server over **Streamable HTTP** (`url:` —
the 2025-11-25 spec remote transport). Depending on what the server
advertises, an agent can:

| Capability | Direction | What it is |
|---|---|---|
| **Tools** | agent → server | Functions the agent calls (the familiar tool-call loop). |
| **Resources** | agent → server | Readable data (files, records) addressed by URI. |
| **Prompts** | agent → server | Reusable, parameterised message templates, with argument completion. |
| **Sampling** | server → agent | The server asks the agent to run an LLM completion on its behalf. |
| **Elicitation** | server → agent | The server asks for structured input matching a schema. |
| **Roots** | server → agent | The server asks which filesystem roots the agent's workspace covers. |

The first three are *agent-initiated* (the agent reaches out). The last
three are *server-initiated* — the server calls back into the agent
mid-tool-call. Those are the ones gated by capability grants.

## The governing principle: autonomous, nothing-by-default

MCP's server-initiated capabilities are designed around a human
("ask the user to confirm this sampling request", "elicit input from
the user"). factor-q runs agents unattended, so there is **no human in
the loop** — every server-initiated request is resolved autonomously or
declined ([ADR-0017](../adrs/accepted/0017-mcp-human-in-the-loop.md)):

- **The runtime is the only gate.** The agent's LLM never authorises a
  server-initiated request; the runtime decides, from the declared
  grant, whether to honour it.
- **Nothing by default.** A server gets *tools only* unless you grant
  it more. Sampling, elicitation, and roots are each off until granted,
  and granted **per server** — you can trust one server with sampling
  and not another.
- **Grants are a trust + cost boundary.** Sampling and elicitation
  spend the agent's budget on the server's behalf and can expose the
  agent's context to a third party, so they route through cost controls
  and a validation seam, always.

## Tools

The simplest use: an MCP server provides tools, and the agent calls
them. Declare the server, then list canonical namespaced tools (`<server>__<tool>`):

```yaml
tools:
  - filesystem__read_file
  - filesystem__write_file
mcp:
  - server: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

A tool-only server is shared across invocations.

**MCP tool execution is server-enforced, and trust is per server.** The
runtime's sandbox (`fs_read`, `fs_write`, `exec_cwd`, `env`) governs the
built-in tools only; it is not consulted for an MCP tool call. In the
example above, `/data` is whatever `server-filesystem` was told to serve
— no `fs_read` the agent declares narrows it, and declaring one would
not. A filesystem or shell server therefore acts with the daemon's
privileges over whatever its arguments name: declare one only for a
server you would run by hand with those arguments. Deriving a server's
arguments from the sandbox, or refusing a filesystem-shaped server whose
arguments fall outside it, is future work tracked with the isolation
epic ([#209](https://github.com/bricef/factor-q/issues/209)).

What the runtime *does* control is the process. A stdio server starts
with a constructed environment — a pinned `PATH` plus the directory its
`command` resolved to, then exactly the declared `env:` and nothing
inherited from the daemon — in a working directory of its own under the
daemon's state directory, with its stderr forwarded to the daemon log
under the server's name
([#541](https://github.com/bricef/factor-q/issues/541)). Each call is
still recorded in the invocation's event trail like any other tool
call; what the call *does* is the server's.

Naming rules — enforced at discovery, failing the server loudly rather
than offering a partial tool set:

- Server ids must match `[a-z0-9-]+` and be at most 48 characters. The
  charset excludes `_`, so the first `__` in a canonical name always
  splits server from tool; `builtin` is reserved for runtime tools.
- The combined `<server>__<tool>` name is checked for **length only**:
  at most 64 characters, the strictest provider limit (Anthropic's).
  The charset providers accept — `[a-zA-Z0-9_-]` — is *not* enforced on
  the tool half, which arrives from the remote server as it chose to
  name it. Names reach the LLM API verbatim, so a server exposing a tool
  with a character outside that set fails at the provider rather than at
  discovery. If a server's tools are rejected on the wire with nothing
  in the daemon log, look there first.
- Registrations never replace an existing name: a collision with a
  built-in or another server's tool is rejected and logged, not
  silently shadowed.

## Resources

A server that advertises resources gets three host-fulfilled tools so
the agent can discover and read them on demand:

- `<server>__list_resources` — list available resources (URI + name).
- `<server>__read_resource` — read a resource by URI.
- `<server>__list_resource_templates` — list parameterised resource templates.

Grant them by listing them in `tools:`. To guarantee a resource is in
context from the first turn without the agent fetching it, **pin** it
with `static_resources` (read once and injected into the opening
prompt):

```yaml
static_resources:
  - "mcp://filesystem/file:///data/schema.json"
```

The client also tracks `resources/updated` and `*_list_changed`
notifications so cached lists don't go stale.

## Prompts

factor-q's client implements the prompts + completion surface: it can
list a server's prompts, fetch one with arguments substituted, and ask
the server to complete a partially-typed argument. (Prompt content is
captured losslessly; rendering non-text prompt content is evolving —
text prompts work today.)

## Sampling — the server runs a completion on the agent's model

When a granted server requests `sampling/createMessage`, the runner —
the sole arbiter of LLM calls — runs the completion on the **agent's
model**, through the same WAL'd, evented, budgeted path as an agent
turn, then returns the result to the *server* (it does not enter the
agent's transcript). The request is gated first:

1. Is the server granted `sampling`? If not → declined, no model call.
2. Is there sampling budget left (`sampling_budget`, and the overall
   `budget`)? If not → declined, no model call.
3. Do the `input_validation` evaluators approve the request? A deny
   short-circuits before the sampling call is made. Note that an `llm`
   evaluator is itself a model call, charged to `sampling_cost` — so
   this step can spend even when it ends in a decline.
4. Otherwise run it, tagged `origin = sampling{server}` for cost
   attribution.
5. Pass the result through the outbound validation seam: the context
   validator chain, then the definition's declarative config
   (`redact_secrets`).
6. Do the `output_validation` evaluators approve the result? A deny
   declines the request rather than returning the completion.

```yaml
budget: 1.00
sampling_budget: 0.25
mcp:
  - server: research
    command: my-research-server
    sampling: true
```

## Elicitation — structured input from the agent's model

`elicitation/create` asks for input matching a JSON schema. factor-q
answers it as a **schema-constrained completion**: it asks the agent's
model for JSON matching the requested schema, validates it, retries a
bounded number of times, and returns `accept` with the value — or
`decline` if ungranted, over budget, or the model can't produce a
valid value. Gated and cost-tracked exactly like sampling.

```yaml
elicitation_budget: 0.10
mcp:
  - server: form-filler
    command: my-form-server
    elicitation: true
```

## Validation: redaction, request policy, evaluators

The inbound request and outbound result of every sampling / elicitation
exchange pass through a validation seam you configure **per server** (off
by default). Two layers:

- **Synchronous policies** — `redact_secrets` strips high-entropy,
  secret-looking tokens from a sampled result or elicited value;
  `reject_sensitive_fields` declines an elicitation that asks for a
  credential-shaped field (`api_key`, `password`, …).
- **Evaluator gates** — ordered `input_validation` / `output_validation`
  lists run with AND semantics (the first deny short-circuits; the
  exchange proceeds only if all approve). Each entry is `approve_all`,
  `deny_all`, or `llm` — a model judge, optionally on a cheaper model
  (`{ llm: claude-haiku-4-5 }`) — that returns an approve/deny verdict
  and fails closed.

```yaml
mcp:
  - server: research
    command: my-research-server
    sampling:
      redact_secrets: true
      output_validation: [{ llm: claude-haiku-4-5 }, deny_all]
```

See [Writing Agent Definitions → Capability grants](agent-definitions.md#capability-grants)
for the full syntax.

## Roots — advertising the workspace scope

`roots/list` asks which filesystem roots the agent's workspace covers.
factor-q derives the advertised roots from the agent's sandbox
`fs_read`/`fs_write` paths — **advertised roots ⊆ the sandbox boundary**
(you can never advertise a path the sandbox doesn't permit). Roots are
**advisory**: they tell a cooperative server its intended scope, and
nothing enforces them on the server's side. The sandbox walls the
built-in tools, not what a server does with its own arguments — see
[Tools](#tools).

```yaml
sandbox:
  fs_read: ["/data/project"]
mcp:
  - server: code-indexer
    command: my-indexer
    roots: true     # advertises file:///data/project
```

## Utilities

The client handles the cross-cutting machinery real servers rely on:
**logging** (`notifications/message` folded into tracing *and* bridged
onto the event bus as a daemon-scoped `mcp.log` event), **progress**
(`notifications/progress` for long-running tool calls), **cancellation**
(in-flight calls send `notifications/cancelled`), and **pagination**
(every `list` follows cursors to completion). The daemon **drains every
shared server's notification stream**; a `tools/list_changed`
re-discovers and installs a refreshed tool registry for the next
invocation ([ADR-0020](../adrs/accepted/0020-mcp-notification-handling.md)).

## Lifecycle

- **Tool-only servers** are started once and shared across invocations.
- **Grant-bearing servers** (sampling / elicitation / roots) run as
  their **own process per invocation**, so a server-initiated request
  attributes to the right invocation's budget, grant, and event chain,
  and request-scoped state can't leak across agents.

## Current limits

- **No mid-invocation tool hot-swap.** A `tools/list_changed` refreshes
  the registry for the *next* invocation; an in-flight invocation keeps
  the tool set it started with
  ([ADR-0020](../adrs/accepted/0020-mcp-notification-handling.md)).
- **`includeContext` is ignored** — the field is not read, and nothing
  reflects a value back to the server. Sampling does not yet inject the
  agent's own context into a server's prompt, so the effect matches
  `none`, but a server that asks for `thisServer` or `allServers` is not
  told it was refused: it simply gets a completion built from its own
  messages. The inbound redaction seam lands with context injection.
- **Roots advertise `file://` only**; audio prompt content awaits an
  upstream rmcp fix.

See [ADR-0017](../adrs/accepted/0017-mcp-human-in-the-loop.md) (policy),
[ADR-0018](../adrs/accepted/0018-mcp-server-initiated-execution.md)
(execution model), and
[ADR-0020](../adrs/accepted/0020-mcp-notification-handling.md)
(notification handling) for the design rationale.
