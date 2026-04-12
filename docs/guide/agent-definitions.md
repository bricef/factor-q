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
ships three built-in tools:

| Tool | What it does | Sandbox dimension |
|---|---|---|
| `file_read` | Read a file's contents | `fs_read` |
| `file_write` | Write/overwrite a file | `fs_write` |
| `shell` | Run a command (argv, no shell) | `exec_cwd` |

To grant a tool, list it in `tools:` and declare the corresponding
sandbox paths. **Nothing is available by default** — an agent with
no sandbox declaration cannot touch the filesystem or run commands.

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
  level. For stronger isolation see [ADR-0010](../adrs/draft/0010-agent-execution-isolation.md).

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
