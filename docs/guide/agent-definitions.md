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

The smallest valid definition has a `name`, a `model`, and a body:

```markdown
---
name: greeter
model: claude-haiku-4-5
---

You are a friendly assistant. Greet the user warmly.
```

This agent has no tools, no sandbox, and no budget. It can only
produce text responses — it cannot read files, run commands, or
call any external service.

Save this as `agents/greeter.md`, then:

```sh
fq agent validate agents/greeter.md
fq trigger greeter "Hello!"
```

## Adding tools

Tools give the agent capabilities beyond text generation. factor-q
ships four built-in tools:

| Tool | What it does | Sandbox dimension |
|---|---|---|
| `file_read` | Read a file's contents | `fs_read` |
| `file_write` | Write/overwrite a file | `fs_write` |
| `shell` | Run a command (argv, no shell) | `exec_cwd` |
| `self_inspect` | Ask the runtime about this invocation's own state — budget, iteration count, model, available tools. | none — host-fulfilled |

To grant a tool, list it in `tools:` and declare the corresponding
sandbox paths. **Nothing is available by default** — an agent with
no sandbox declaration cannot touch the filesystem or run commands.

`self_inspect` is special: its data is synthesised by the runtime
itself, not by an external process, so it has no sandbox dimension
to declare. Granting it just adds it to the `tools:` list. See the
[self-aware example](../../agents/examples/self-aware.md).

### File reader

```markdown
---
name: reader
model: claude-haiku-4-5
tools:
  - file_read
sandbox:
  fs_read:
    - /path/to/readable/directory
budget: 0.10
---

You are a research assistant. Use `file_read` to answer questions
about files in the readable directory.
```

### File writer

```markdown
---
name: writer
model: claude-haiku-4-5
tools:
  - file_read
  - file_write
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

### Shell runner

The shell tool takes an **argv array** (`["ls", "-la"]`), not a
shell string. No shell is invoked — there is no opportunity for
shell injection. Pipes, redirects, and glob expansion are not
supported.

```markdown
---
name: inspector
model: claude-haiku-4-5
tools:
  - shell
sandbox:
  exec_cwd:
    - /data/project
budget: 0.10
---

You can run commands in /data/project using the shell tool. Pass
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
  - file_read
  - file_write
  - shell
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

## The sandbox

Every tool call is checked against the agent's sandbox **before**
execution. A call that violates the sandbox is rejected with a
clear error message that the LLM sees and can adapt to.

### Dimensions

| Dimension | Controls | Used by |
|---|---|---|
| `fs_read` | Directories the agent can read from | `file_read` |
| `fs_write` | Directories the agent can write to | `file_write` |
| `exec_cwd` | Directories the agent can run commands in | `shell` |
| `env` | Environment variables visible to child processes | `shell` |
| `network` | Network access patterns (reserved for future use) | — |

### Path handling

- Paths are canonicalised (resolved to their real location) before
  comparison, so `..` traversal and symlink escapes are defeated.
- Paths in the agent definition can be absolute (`/data/project`)
  or relative (`./data`). Relative paths are resolved against the
  config file's directory.
- The sandbox is enforced at the **process level**, not the OS
  level. For stronger isolation see [ADR-0010](../adrs/accepted/0010-agent-execution-isolation.md).

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
the ones you want in `tools:` by their own names (MCP tool names are not
prefixed):

```yaml
tools:
  - read_file                   # a tool the filesystem server provides
mcp:
  - server: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

A server that exposes **resources** also gets two host-fulfilled tools,
`<server>__list_resources` and `<server>__read_resource` (e.g.
`filesystem__read_resource`), so the agent can browse and read them on
demand. Grant those by listing them in `tools:` too.

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
for phase 1, triggers are dispatched via `fq trigger` or
`fq trigger --via-nats`.

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
fetched at runtime start. If the model identifier is not in the
table, cost is reported as $0 with a warning.

## System prompt (the Markdown body)

Everything below the closing `---` of the frontmatter is the
system prompt. Write it as you would any LLM system prompt:

- State the agent's role and personality
- Describe what tools are available and when to use them
- Specify output format expectations
- Include any domain-specific instructions

The body supports full Markdown formatting. Use headers, lists,
code blocks — anything that helps the LLM understand its task.

## Validating and testing

```sh
# Check that the definition parses correctly
fq agent validate agents/my-agent.md

# List all agents in a directory
fq agent list

# Trigger it manually
fq trigger my-agent "Your prompt here."

# Watch events as they flow (in another terminal)
fq events tail --subject "fq.agent.my-agent.>"
```
