# Writing Agent Definitions

Agent definitions are Markdown files with YAML frontmatter. The
frontmatter is the structured configuration (model, tools, sandbox,
budget); the Markdown body is the system prompt. This guide walks
through writing one from scratch.

For the formal specification, see
[ADR-0005](../adrs/accepted/0005-agent-definition-format.md).
For ready-made examples, see
[`agents/examples/`](../../agents/examples/).

## Minimal agent

The smallest definition the daemon will load is a `name` and a body.
`model:` is shown here because it is what you will usually write, but it
is optional — see [Choosing the model](#choosing-the-model), which also
covers the trap that comes with leaving it out.

```markdown
---
name: greeter
model: claude-haiku-4-5
---

You are a friendly assistant. Greet the user warmly.
```

This agent has no tools, no sandbox, and no budget. It can only
produce text responses — it cannot read files, run commands, or
call any external service. It can still *end its run*: every invocation
is offered `report_outcome` whether or not the definition asks for it —
see [Ending the run](#ending-the-run).

Save this as `agents/greeter.md`. `fq agent validate` works offline, but
every other `fq` verb answers over the daemon's authenticated edge, so
the daemon has to be running and this client paired with it before any
of them do anything:

```sh
fq agent validate agents/greeter.md     # offline; needs no daemon

fqd                                     # (another terminal) start the daemon —
                                        # on its first run it prints a certificate
                                        # fingerprint and writes the admin token to
                                        # ~/.local/state/factor-q/edge/admin.token
fq connect 127.0.0.1:9472 --token "$(cat ~/.local/state/factor-q/edge/admin.token)"

fq trigger greeter "Hello!"
```

Skip the middle two and `fq trigger` stops at ``no daemon paired — run
`fq connect <addr> --token <token>` first``. The daemon reads `fqd.toml`
and the client reads `fq.toml`; `fq init` writes both, along with
`agents/` and a sample agent.

## The frontmatter is strict

Every **top-level** key in the frontmatter must be one the runtime
recognises. An unknown key is a **hard error** — the definition fails to
load, and the error names the offending key, lists the keys that were
expected, and gives the line and column:

```text
agents/greeter.md is invalid: invalid YAML: unknown field `budgett`,
expected one of `name`, `model`, `tools`, `sandbox`, `budget`,
`max_iterations`, `effort`, `trigger`, `mcp`, `static_resources`,
`sampling_budget`, `elicitation_budget` at line 3 column 1
```

That list is the whole recognised set — twelve keys, and the error
prints them in the order the runtime declares them.

Strictness is deliberate: a dropped key is silent, and silence here is
expensive. `budgett: 0.05` used to parse as a definition with *no* cost
cap, and `fq agent validate` called it valid — the only trace was a
missing line in its output (ADR-0004 is "cost controls from day one").
`sandboxx:` is the same shape with a security edge: the agent would run
with no grants rather than the ones its author wrote. Both are now
rejected outright.

An unknown key is fatal to the definition that carries it, and only to
that one: the daemon records the parse failure and carries on loading
the rest of the directory, so one bad file costs you one agent rather
than the registry. Run `fq agent validate` over a definition before
adding it.

### The nested blocks are strict too

The same check applies **inside** the `sandbox:` block and inside each
`mcp:` entry, and the error names the block it came from:

```text
invalid YAML: sandbox: unknown field `fs_writ`, expected one of
`fs_read`, `fs_write`, `network`, `env`, `exec_cwd` at line 4 column 3

invalid YAML: mcp[0]: unknown field `commandd`, expected one of
`server`, `command`, `url`, `args`, `env`, `sampling`, `elicitation`,
`roots` at line 5 column 5
```

`mcp[0]` is the index of the offending entry, which matters once a
definition declares several servers.

These are the typos worth catching, because the nested keys are the ones
an author actually edits, and every one of them defaults to *empty*: a
misspelled `fs_writ:` used to grant no write access while reporting a
valid definition, surfacing much later as a permission denial with
nothing to connect it back to the missing letter.

The check reaches one level further, into the table form of a
`sampling:` or `elicitation:` grant, where it guards a security default:
every key there is off unless set, so `redact_secretz: true` used to
parse as a grant with redaction **off** — the opposite of what the
author wrote. A typo inside a grant is now rejected, and the error names
the key, the grant it sits in, and the keys that were expected:

```text
invalid YAML: mcp[0].sampling: unknown field `redact_secretz`, expected
one of `redact_secrets`, `reject_sensitive_fields`, `input_validation`,
`output_validation` at line 7 column 7
```

A grant that is neither a bool nor a table (`sampling: 42`) is refused
the same way, with an error that says what a grant may be. The
recognised keys are listed under [Capability grants](#capability-grants).

## Choosing the model

`model:` names a model the deployment makes available. Different agents
can run on different models — expensive ones for hard reasoning, cheaper
ones for simple triage/classify steps (ADR-0003) — and non-Anthropic
models (via any OpenAI-compatible provider) are available by
configuration alone.

- **`model:` is optional.** When omitted, the agent inherits the
  deployment's `agents.default_model`. A definition with neither an
  explicit `model:` nor a configured default fails to load.
- **A model must be declared to be usable.** Every model an agent names
  must appear in some provider's `models = [...]` list in `fqd.toml`. An
  agent that names an undeclared model fails to load — a typo can't
  silently reach the wire.
- **Every declared model must be priced.** The daemon refuses to start
  unless each declared model resolves to a price (the LiteLLM table or a
  `[providers.<name>.pricing]` override). This guarantees cost controls
  (ADR-0004) can't be defeated by an unpriced model tracking as $0.

Declaring providers, the default model, and price overrides is a
deployment (`fqd.toml`) concern — see the generated config's `[providers]`
and `[agents]` sections (`fq init`).

> **⚠ `fq agent validate` cannot see any of this (issue #508).** The
> client reads the definition through the same parser the daemon's
> registry uses, but it runs offline, with no `fqd.toml` and no pricing
> table — so its verdict diverges from the daemon's in **both**
> directions:
>
> - **It rejects a definition the daemon accepts.** With no deployment
>   config to read, the client has no default model to substitute, so a
>   definition that omits `model:` — the supported shape described just
>   above — fails with `invalid agent: missing required field: model`
>   and exit code 1. That is the client's blind spot, not a problem with
>   your definition.
> - **It accepts definitions the daemon refuses.** Nothing on the client
>   side knows which models the deployment declares or prices, so a
>   typo'd or unpriced model passes validation and then takes the
>   *daemon* down at startup — the ADR-0004 coverage guarantee is
>   fail-fast, so one bad model stops every other agent loading too.
>
> Treat `fq agent validate` as a syntax and shape check, not as the
> deployment's verdict. The model-and-pricing half is answered only by
> starting the daemon.

## Adding tools

Tools give the agent capabilities beyond text generation. factor-q
ships seven built-in tools:

| Tool | What it does | Sandbox dimension |
|---|---|---|
| `builtin__file_read` | Read a file's contents | `fs_read` |
| `builtin__file_write` | Write/overwrite a file | `fs_write` |
| `builtin__file_list` | List files under the sandbox by relative glob | `fs_read` |
| `builtin__file_search` | Find text in sandboxed files | `fs_read` |
| `builtin__exec` | Run a single program (argv, no shell) | `exec_cwd` |
| `builtin__self_inspect` | Ask the runtime about this invocation's own state — budget, iteration count, model, available tools. | none — host-fulfilled |
| `builtin__report_outcome` | Declare how the task went and end the invocation. | none — never dispatched; see [Ending the run](#ending-the-run) |

Every tool name is namespaced: built-ins live under the reserved
`builtin__` prefix, MCP tools under their server's id
(`<server>__<tool>` — see [MCP servers](#mcp-servers)). The registry
rejects any other registration into `builtin__` and never replaces on
a name collision, so an MCP server cannot shadow a sandboxed built-in.
Legacy bare built-in names (`exec`, `file_read`, …) in `tools:` are
still accepted for one release and mapped to their canonical names
with a deprecation warning in the daemon log — update definitions to
the `builtin__` form now.

To grant a tool, list it in `tools:` and declare the corresponding
sandbox paths. **Nothing is available by default** — an agent with
no sandbox declaration cannot touch the filesystem or run commands.

`builtin__self_inspect` is special: its data is synthesised by the runtime
itself, not by an external process, so it has no sandbox dimension
to declare. Granting it just adds it to the `tools:` list. See the
[self-aware example](../../agents/examples/self-aware.md).

`builtin__report_outcome` is more special still: it is the one tool you
never need to grant, because granting it is not a decision an author gets
to make. See [Ending the run](#ending-the-run) below.

### File reader

```markdown
---
name: reader
model: claude-haiku-4-5
tools:
  - builtin__file_read
sandbox:
  fs_read:
    - /path/to/readable/directory
budget: 0.10
---

You are a research assistant. Use `builtin__file_read` to answer questions
about files in the readable directory.
```

### File writer

```markdown
---
name: writer
model: claude-haiku-4-5
tools:
  - builtin__file_read
  - builtin__file_write
sandbox:
  fs_read:
    - /data/project
  fs_write:
    - /data/project/output
budget: 0.20
---

You can read files anywhere under /data/project and write output
files to /data/project/output.
```

### Command runner

The `builtin__exec` tool takes an **argv array** (`["ls", "-la"]`), not a
shell string. No shell is invoked — there is no opportunity for
shell injection. Pipes, redirects, and glob expansion are not
supported.

Because there is no shell, there is no `| head` / `| tail` either —
pass `max_lines` (keep the first N lines) or `tail_lines` (keep the
last N) in the tool call to bound large output. Each stream is also
capped at a byte limit as a safety backstop, and the result says
when — and by how much — it truncated.

```markdown
---
name: inspector
model: claude-haiku-4-5
tools:
  - builtin__exec
sandbox:
  exec_cwd:
    - /data/project
budget: 0.10
---

You can run commands in /data/project using the `builtin__exec` tool. Pass
the command as an argv array, e.g. `["ls", "-la"]`.
```

Note that `exec_cwd` is a **separate sandbox dimension** from
`fs_read` and `fs_write`. An agent with read access does not
automatically get exec access, and vice versa.

### Combined

```markdown
---
name: full-toolkit
model: claude-haiku-4-5
tools:
  - builtin__file_read
  - builtin__file_write
  - builtin__exec
sandbox:
  fs_read:
    - /data/project
  fs_write:
    - /data/project/output
  exec_cwd:
    - /data/project
  env:
    - HOME
budget: 0.50
---

You have read access to the project, write access to the output
directory, and can run commands in the project root.
```

## Ending the run

An agent ends its invocation by calling `builtin__report_outcome` with a
`status` and a `summary`:

| `status` | Means |
|---|---|
| `success` | The goal was achieved. |
| `failed` | The goal was not achieved. |
| `blocked` | The agent could not proceed — the summary says what blocked it. |
| `partial` | Some of the goal was delivered. |

The status describes how the **task** went, independently of whether the
runtime worked. `summary` is one short paragraph a human can act on. Both
are required.

Three consequences worth knowing before you write a system prompt:

- **This is the only clean ending.** It is the sole path to a `completed`
  event. Nothing else the model can do ends an invocation successfully;
  the remaining exits are the iteration cap, the budget ceiling, and
  errors, all of which are failures.
- **Bare text does not finish anything.** A turn that returns prose and no
  tool calls is recorded, answered with a durable host notice asking the
  model to continue or report an outcome, and followed by another model
  turn. An agent whose prompt says "reply with your answer when done" will
  loop until it hits `max_iterations`.
- **Every invocation is offered it, declared or not.** The runtime appends
  `builtin__report_outcome` to the effective tool list of every agent,
  including one whose `tools:` list is empty — an agent that could not
  call it could not finish. It is not a capability to opt into: do not
  list it in `tools:`, and there is no sandbox dimension to grant.

What is worth writing in the definition is *when* to call it and what
belongs in the summary. Its availability is not something the definition
controls.

## The sandbox

Every tool call is checked against the agent's sandbox **before**
execution. A call that violates the sandbox is rejected with a
clear error message that the LLM sees and can adapt to.

### Dimensions

| Dimension | Controls | Used by |
|---|---|---|
| `fs_read` | Directories the agent can read from | `builtin__file_read` |
| `fs_write` | Directories the agent can write to | `builtin__file_write` |
| `exec_cwd` | Directories the agent can run commands in | `builtin__exec` |
| `env` | Environment variables visible to child processes | `builtin__exec` |
| `network` | Network egress patterns — **declared but not enforced** | — |

> **⚠ `network` is not a boundary yet (issue #35).** A definition may
> declare `sandbox.network`, and the runtime parses and carries it — but
> no tool consults it. The agent has **ambient network access** and can
> reach any host regardless of what it declares. Declaring it is
> accepted, so definitions can record intent ahead of enforcement, and
> logs a warning at load; `fq` also flags it when validating a
> definition. Until enforcement lands, **treat every agent as
> network-unrestricted no matter what its definition says.** Enforcement
> is tracked by #208 (a filtering proxy) and #209
> ([ADR-0010](../adrs/accepted/0010-agent-execution-isolation.md)'s
> container boundary).

### Path handling

- Paths are canonicalised (resolved to their real location) before
  comparison, so `..` traversal and symlink escapes are defeated.
- **Prefer absolute paths** (`/data/project`). A relative path is
  accepted, but it is stored verbatim and canonicalised at the moment of
  each tool call — so it resolves against the **daemon's working
  directory**, not the definition's directory and not the config's. Move
  the definition and it still means the same thing; start `fqd` from
  somewhere else and it does not.
- **A prefix that cannot be canonicalised is skipped in silence.** If a
  declared directory does not exist when the check runs — a typo, a
  relative path that missed, a volume not yet mounted — that prefix
  simply grants nothing. There is no error at load and no warning at use;
  the agent gets `resolved path … is outside every allowed prefix` on
  every call, as though it had been denied. When a grant appears to have
  no effect, check that the directory exists as the daemon sees it before
  suspecting the sandbox.
- The sandbox is enforced at the **process level**, not the OS
  level. For stronger isolation see [ADR-0010](../adrs/accepted/0010-agent-execution-isolation.md).

### Ambient identity variables

Beyond the `env` allowlist, the runtime injects three variables of its
own into every command the `builtin__exec` tool spawns:

| Variable | Value |
|---|---|
| `FQ_INVOCATION_ID` | The current invocation's id |
| `FQ_AGENT_ID` | The agent's id |
| `FQ_MODEL` | The model the agent runs on |

They need no `env` grant: they are facts the runtime owns about the
invocation, not host environment passed through, and they expose
nothing about the host. A same-named host variable never shadows them,
even if allowlisted. They persist across suspend/resume with the same
invocation id. The `self_inspect` tool mirrors these values in its `identity` section.

Use them for provenance on out-of-band work — for example, configuring
the git author so commits attribute to the agent rather than the
operator (a shell is needed for the expansion):

```yaml
# In the system prompt, an early step such as:
#   ["sh", "-c", "git config user.name \"$FQ_AGENT_ID (factor-q agent)\" \
#     && git config user.email \"$FQ_AGENT_ID+$FQ_INVOCATION_ID@agents.invalid\""]
```

### The `${workspace}` token

Instead of hardcoding an absolute workspace path, an agent can reference
its working directory as `${workspace}`. The runtime binds the token per
invocation, driven by `[workspace]` in `fqd.toml`:

```yaml
sandbox:
  fs_read:
    - ${workspace}
  fs_write:
    - ${workspace}
  exec_cwd:
    - ${workspace}
```

- With `per_invocation = false` (default), every invocation binds to the
  shared `path` directory — identical to naming the path directly.
- With `per_invocation = true`, each invocation gets a fresh **empty**
  directory, so concurrent invocations cannot touch each other's files.
  A suspended invocation keeps its directory across daemon restarts;
  terminal invocations are reclaimed.
- If no `[workspace]` is configured, an agent that uses the token fails
  loudly at invocation start rather than running with an unresolved
  grant.

**Where the token resolves.** Sandbox paths always bind. In tool calls,
substitution happens **only in declared path parameters** — properties
whose JSON schema carries `format: "path"`. There are five:

| Tool | Parameter |
|---|---|
| `builtin__file_read` | `path` |
| `builtin__file_write` | `path` |
| `builtin__exec` | `cwd` |
| `builtin__file_list` | `root` |
| `builtin__file_search` | `root` |

Everything else is passed through verbatim:
file *contents*, command *arguments*, and any other string reach the tool
exactly as the agent wrote them, so writing the literal text
`${workspace}` into a file works and nothing silently rewrites agent
output.

**How the agent knows the real path.** The runtime injects a step-0
environment preamble stating the workspace's absolute path and the token
convention, ahead of the trigger message. For anything outside a path
parameter (an argv element, a config file it generates), the agent uses
that real path — or paths relative to `cwd: ${workspace}`.

The runtime provisions *directories* and nothing more. Populating the
workspace is the agent's job through its granted tools — a code agent's
first step is typically cloning the upstream into `${workspace}`, which
also guarantees it starts from the latest upstream state.

## Budget

The `budget` field sets a hard ceiling in USD for a single
invocation. If the cumulative cost of LLM calls exceeds the
budget, the executor halts the invocation and emits a `Failed`
event with `error_kind: BudgetExceeded`.

```yaml
budget: 0.50   # half a dollar per invocation
```

Omit `budget` to run without a ceiling. This is not recommended
for unattended agents.

## Iteration cap and reasoning effort

Two further optional fields tune the invocation itself.

```yaml
max_iterations: 40      # per-agent cap on LLM turns
effort: high            # reasoning effort for each request
```

- **`max_iterations`** overrides the daemon's default cap on LLM turns in
  a single invocation. Omit it to inherit `fqd.toml`'s value. The cap is
  literal, including `0`: an agent with `max_iterations: 0` stops before
  its first model turn. Hitting the cap is a failure, not a completion —
  it is the exit an agent takes when it never called `report_outcome`.
- **`effort`** sets the model's reasoning effort per request: `minimal`,
  `low`, `medium`, `high`, or `xhigh`. Omit it to leave the provider's
  default. `minimal` exists for a real failure mode rather than for
  economy — on gpt-5-family models the default reasoning scales to fill
  `max_tokens` and can return empty content on short mechanical tasks.

Both are top-level keys, so a typo in either is refused at load rather
than ignored — see [The frontmatter is strict](#the-frontmatter-is-strict).

## MCP servers

[MCP](https://modelcontextprotocol.io) (Model Context Protocol)
servers extend an agent with external tools, resources, and prompts.
Declare them in the `mcp:` block — a list, one entry per server:

```yaml
mcp:
  - server: filesystem          # the name you refer to it by
    command: npx                # how to launch it (a stdio child process)
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
    env:                        # optional process environment
      LOG_LEVEL: info
```

Each server runs as a **stdio child process** (`command:`) or, with a
`url:` instead, a remote server reached over **Streamable HTTP** (the
2025-11-25 spec remote transport) — exactly one of `command` / `url`
per server:

```yaml
mcp:
  - server: remote-tools
    url: https://tools.internal/mcp   # Streamable HTTP; no command/args/env
```

The server's **tools** become available exactly like built-ins — list
the ones you want in `tools:` by their canonical namespaced names,
`<server>__<tool>`:

```yaml
tools:
  - filesystem__read_file       # a tool the filesystem server provides
mcp:
  - server: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

A server that exposes **resources** also gets three host-fulfilled tools —
`<server>__list_resources`, `<server>__read_resource`, and
`<server>__list_resource_templates` (e.g. `filesystem__read_resource`) — so
the agent can browse, read, and expand parameterised URIs on demand. Grant
those by listing them in `tools:` too.

### Pinning resources with `static_resources`

To guarantee a specific resource is in context from the first turn —
without the agent having to fetch it — pin it with `static_resources`,
a list of `mcp://<server>/<resource-uri>` URLs:

```yaml
static_resources:
  - "mcp://filesystem/file:///data/README.md"
```

Pinned resources are read once at invocation start and injected into
the opening prompt. Use this for context the agent always needs (a
schema, a style guide, project facts).

### Capability grants

Beyond providing tools, the MCP spec lets a **server ask things of the
agent** mid-call — the *server-initiated* capabilities:

| Capability | The server asks to… | How factor-q answers |
|---|---|---|
| `sampling` | run an LLM completion (using the agent's model + budget) | runs it, gated and cost-tracked |
| `elicitation` | get structured input matching a schema | answers from the agent's model |
| `roots` | learn the agent's workspace filesystem scope | the sandbox's fs paths |

factor-q resolves all three **autonomously** — there is never a human
in the loop (see
[ADR-0017](../adrs/accepted/0017-mcp-human-in-the-loop.md)). Because
they spend the agent's budget or expose its context, they are **off by
default** and granted **per server**:

```yaml
budget: 1.00
sampling_budget: 0.25           # aggregate ceiling for sampling spend
mcp:
  - server: research
    command: my-research-server
    sampling: true              # this server may request sampling
    elicitation: true           # …and structured input
    roots: true                 # …and may see the workspace roots
  - server: untrusted
    command: some-third-party-server
    # no flags → tools only; any sampling/elicitation/roots request is declined
```

- **Nothing by default.** A server with no flags gets tools only; any
  server-initiated request it makes is declined. Grants are per server,
  so you can trust one server with sampling and not another.
- **Sub-budgets.** `sampling_budget` and `elicitation_budget`
  (top-level, USD) cap the *aggregate* spend on each across the
  invocation, inside the overall `budget`. Omit them to bound only by
  `budget`. Once a sub-budget is reached, further requests are declined
  *without* a model call.
- **Roots are advisory.** They are derived from the sandbox's
  `fs_read`/`fs_write` paths (advertised ⊆ the sandbox boundary) and
  tell a cooperative server its intended scope — the sandbox itself is
  the actual wall.

The inbound request and outbound result of each granted sampling /
elicitation exchange can be **validated** — expand the boolean flag into
a table (still off by default):

```yaml
mcp:
  - server: research
    command: my-research-server
    sampling:
      redact_secrets: true            # strip secret-looking tokens from the result
      output_validation: [{ llm: claude-haiku-4-5 }, deny_all]
    elicitation:
      reject_sensitive_fields: true   # decline credential-shaped fields (api_key, password, …)
      input_validation: [approve_all]
```

- `redact_secrets` / `reject_sensitive_fields` — synchronous redaction /
  request-policy gates.
- `input_validation` / `output_validation` — ordered evaluator lists run
  with AND semantics (the first deny short-circuits; proceeds only if all
  approve). Each entry is `approve_all`, `deny_all`, or `llm` — a model
  judge, optionally on a cheaper model via `{ llm: <model-id> }`, that
  fails closed.

A server granted any capability runs as its **own process per
invocation** (so its requests attribute to the right invocation's
budget and grant); tool-only servers are shared. See the
[MCP guide](mcp.md) for the full model and worked examples, and
[ADR-0017](../adrs/accepted/0017-mcp-human-in-the-loop.md) /
[ADR-0018](../adrs/accepted/0018-mcp-server-initiated-execution.md) for
the rationale.

## Triggers

The `trigger` field (optional) declares the NATS subject this
agent responds to. This is a design-time declaration for graph
definitions (see
[ADR-0012](../adrs/accepted/0012-graph-definition-format.md));
for phase 1, triggers are dispatched via `fq trigger`, which asks the
daemon to queue the work.

```yaml
trigger: tasks.research.*
```

## Model selection

The `model` field takes a model identifier that the genai adapter
recognises. Examples:

| Model | Identifier |
|---|---|
| Claude Haiku 4.5 | `claude-haiku-4-5` |
| Claude Sonnet 4.5 | `claude-sonnet-4-5` |
| Claude Opus 4.6 | `claude-opus-4-6` |
| GPT-4o | `gpt-4o` |
| GPT-4o mini | `gpt-4o-mini` |

Cost is calculated from the
[LiteLLM pricing table](https://github.com/BerriAI/litellm),
fetched at daemon start and merged with any
`[providers.<name>.pricing]` overrides.

An identifier that resolves to no price is not tolerated at any stage —
that is the whole of ADR-0004's guarantee, since a model tracking as $0
would silently defeat every budget:

- **At startup**, the daemon refuses to run and names each offending
  model: `model "…" is declared but has no pricing — add
  [providers.<name>.pricing."…"] or ensure the LiteLLM table lists it`.
  The same check rejects an agent naming a model no provider declares.
- **At use**, a second backstop refuses the dispatch before any WAL write
  rather than proceeding at $0.

So an unpriced model does not produce a $0 invocation and a warning; it
produces a daemon that will not start.

## System prompt (the Markdown body)

Everything below the closing `---` of the frontmatter is the
system prompt. Write it as you would any LLM system prompt:

- State the agent's role and personality
- Describe what tools are available and when to use them
- Specify output format expectations
- Say when to call `report_outcome` and what belongs in the summary —
  see [Ending the run](#ending-the-run); a prompt that instead says
  "reply with your answer when you are done" produces an agent that
  never finishes
- Include any domain-specific instructions

The body supports full Markdown formatting. Use headers, lists,
code blocks — anything that helps the LLM understand its task.

## Validating and testing

```sh
# Check that the definition parses correctly (offline, no daemon needed).
# `budget` and `max_iterations` print whether or not they are set —
# `budget: not set (no cap)` rather than an omitted line, so an absence
# is something you can read rather than something you have to notice.
fq agent validate agents/my-agent.md

# List the agents the running daemon has loaded — its live registry,
# which `fq reload` swaps, not whatever is on this machine's disk
fq agent list

# Trigger it manually
fq trigger my-agent "Your prompt here."

# Watch events as they flow (in another terminal)
fq events tail --agent my-agent
```

Only the first of those runs offline. The rest reach the daemon over its
authenticated edge, so `fqd` must be running and this client paired with
`fq connect` — see [Minimal agent](#minimal-agent).

And know what `fq agent validate` is worth before you lean on it. It
checks syntax and shape: that the frontmatter parses, that the fields it
does know are well-formed, and that the definition builds. It catches a
misspelled top-level key but not one nested inside `sandbox:` or `mcp:`,
and it cannot reach the deployment's model or pricing config — so it
both rejects the valid model-less shape and passes models the daemon
will refuse (issue #508). The verdict that counts is the daemon starting
and `fq agent list` showing the agent in the live registry.
