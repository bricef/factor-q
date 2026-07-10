# Quickstart

Get from a fresh clone of factor-q to a running agent with full event-trail inspection in under ten minutes.

This is the user-facing path: install, configure, run, observe. If you want to read the architecture or contribute changes, see [`README.md`](README.md), [`ARCHITECTURE.md`](ARCHITECTURE.md), and [`CONTRIBUTING.md`](CONTRIBUTING.md) instead.

## Prerequisites

| Tool | Why | Install |
|---|---|---|
| **Rust toolchain** (edition 2024) | Build the runtime. | [rustup.rs](https://rustup.rs) |
| **Docker** + **Docker Compose** | Run NATS+JetStream locally. | Distribution-specific. |
| **`just`** | Run the project's task recipes. | `cargo install just` or your package manager. |
| **An LLM API key** | Today: `ANTHROPIC_API_KEY`. | [console.anthropic.com](https://console.anthropic.com). |

That's the whole list. No global Rust libraries, no language runtimes besides Rust itself, no manual NATS install.

## 1. Clone and bring up the system

```sh
git clone https://github.com/bricef/factor-q.git
cd factor-q

# Starts NATS+JetStream in Docker and builds the fq binary.
just up
```

`just up` is two things: `just infra-up` (Docker compose for NATS) and `just build` (cargo build of the workspace). When it finishes you have a NATS server on `nats://localhost:4222` — bound to loopback, token-authenticated (the token lives in `infrastructure/nats/nats.conf` and rides in the default `FQ_NATS_URL` as URL userinfo) — and a usable `fq` CLI behind `just fq`.

Verify it:

```sh
just fq --help
just fq status
```

`fq status` connects to NATS, lists the streams it manages, and reports projection health. If it complains, NATS isn't reachable — re-run `just infra-up` and check `just infra-status`.

## 2. Initialise a project

factor-q runs out of a directory containing a config file (`fq.toml`), an agents directory (`agents/`), and a cache (`./cache/` or `$XDG_CACHE_HOME/factor-q`). `fq init` writes those for you.

```sh
mkdir my-fq-project && cd my-fq-project
just fq init
```

This produces:

| File | What it does |
|---|---|
| `fq.toml` | Runtime configuration — NATS URL, agents directory, provider env vars. |
| `agents/sample-agent.md` | A starter agent with `file_read` and `exec` tools and a `$0.10` budget. |
| `README.md` | A pointer back to factor-q docs. |

Open `agents/sample-agent.md` to see the format: YAML frontmatter declaring model, tools, sandbox, and budget; Markdown body containing the system prompt. Full reference in the [agent authoring guide](docs/guide/agent-definitions.md).

## 3. Set your API key

```sh
export ANTHROPIC_API_KEY='sk-ant-...'
```

The runtime reads provider keys from environment variables (named in `fq.toml` under `[providers.*]`). They're never written back to disk.

## 4. Run an agent

```sh
just fq trigger sample-agent "List the files in this directory."
```

You'll see:
- A "Loaded agent..." line.
- A "Running agent..." line.
- A short result printed to your terminal.
- A cost figure ("Completed in NNNms (cost: $0.000NNN)").

Behind the scenes the runtime emitted ~5–10 events to NATS — one per LLM call, one per tool call, plus the lifecycle events (`triggered`, `completed`).

## 5. Watch events live (in another terminal)

```sh
just fq events tail
```

Then run `just fq trigger sample-agent ...` again. You'll see each event scroll past as it happens: `triggered`, `llm.request`, `llm.response`, `tool.call`, `tool.result`, `cost`, `completed`. Every decision the agent made is on the bus.

`fq events tail --subject fq.agent.sample-agent.>` narrows to one agent. `--subject fq.agent.>.tool.call` narrows to all tool calls across all agents. Subjects compose; see [`docs/design/committed/event-schema.md`](docs/design/committed/event-schema.md) for the full hierarchy.

## 6. Query history and costs

The runtime also materialises every event into a SQLite projection so you can query historical runs without replaying NATS.

```sh
# Last 20 events, all agents
just fq events query --limit 20

# All tool.result events for sample-agent
just fq events query --agent sample-agent --type tool_result

# Per-agent cost totals
just fq costs

# Costs for one agent, or since a given time (combine freely)
just fq costs --agent sample-agent
just fq costs --since 2026-04-25
```

The projection is rebuildable from NATS at any time — NATS is the source of truth.

## 7. Try `self_inspect` (optional)

The `self_inspect` built-in lets an agent ask the runtime about its own invocation state — budget remaining, iterations used, the configured model — instead of guessing. Try it via the bundled `self-aware` example:

```sh
just fq --agents-dir agents/examples trigger self-aware \
  "What model are you running and how much budget do you have left?"
```

The agent calls `self_inspect`, the runtime synthesises the answer from its own bookkeeping, and the agent reports back with authoritative numbers (e.g. *"Claude Haiku 4.5, $0.049 remaining of $0.05"*). See the [host-fulfilled tools section](docs/guide/reducer-harness.md#host-fulfilled-tools) of the reducer guide for the implementation pattern.

## 8. Run the daemon (optional)

So far each `fq trigger` runs in-process and exits when the agent finishes. The daemon mode keeps the runtime alive: it consumes triggers from NATS, runs agents asynchronously, and keeps the projection up to date.

```sh
just run
```

In another terminal:

```sh
# Publishes a trigger over NATS instead of running in-process.
just fq trigger sample-agent "Hello." --via-nats
```

The daemon picks up the trigger, runs the agent, and emits events. The CLI returns immediately because dispatch is asynchronous. Stop the daemon with Ctrl-C — it shuts down cleanly and emits a `system.shutdown` event.

## What to do next

| Goal | Where to look |
|---|---|
| Write your own agent | [Agent authoring guide](docs/guide/agent-definitions.md) |
| Understand the event model | [Event schema](docs/design/committed/event-schema.md) |
| Run agents on the suspend/resume-capable path | [Reducer harness guide](docs/guide/reducer-harness.md) |
| Understand the architecture | [`ARCHITECTURE.md`](ARCHITECTURE.md) |
| Contribute changes | [`CONTRIBUTING.md`](CONTRIBUTING.md) |
| Read the design rationale | [`docs/design/`](docs/design/) |

## Common problems

| Symptom | Likely cause | Fix |
|---|---|---|
| `fq status` reports NATS unreachable. | NATS isn't running. | `just infra-up`; check `just infra-status`. |
| `fq trigger` says "LLM authentication failed". | `ANTHROPIC_API_KEY` is unset or invalid. | Re-export the variable in the same shell that runs `fq`. |
| `fq trigger` exits with "agent not found". | You're not in the project directory, or the agents directory in `fq.toml` is wrong. | `cd` into the project, or pass `--agents-dir`. |
| Tools fail with "path not in sandbox". | The agent's `sandbox.fs_read` / `fs_write` / `exec_cwd` doesn't include the path the model tried to use. | Edit the agent definition's sandbox section; nothing is granted by default. |
| `just up` complains about a port. | Something else is using port 4222 (NATS) or 8222. | Stop the conflicting service or edit `infrastructure/docker-compose.yml`. |

For more detail on any of these, see [`CONTRIBUTING.md`](CONTRIBUTING.md) (developer setup) and [`services/fq-runtime/README.md`](services/fq-runtime/README.md) (runtime configuration and deployment).
