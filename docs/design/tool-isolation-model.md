# Tool Isolation Model

## Status

Draft. Consolidates the architectural shift that emerges from
the reducer-model harness design: isolation moves from
agent-scoped to tool-scoped, and workspace state is introduced
as a first-class concern.

## Context

Phase 1 shipped process-level sandboxing: path canonicalisation,
argv-only shell invocation, mandatory timeouts, output caps.
ADR-0010 accepted "containers by default" as the next
isolation tier, with the entire agent execution running inside
a container.

The reducer-model harness design
(see [`wasm-boundary-design.md`](./wasm-boundary-design.md))
changes the picture. The harness is now a pure function; it
cannot execute anything. A compromised or adversarial harness
can at worst return malformed tool-call requests, which the
host filters against the agent's declared tool list. The
harness is no longer a meaningful attack surface.

This means the security boundary belongs around *individual
tools*, not around the agent as a whole. Each tool has its own
threat profile, its own sandbox requirements, and its own
state-management needs. Sandboxing the harness wholesale is
overkill for tools that need no isolation (pure computation)
and insufficient for tools that need heavy isolation
(executing untrusted code).

This document describes the resulting architecture.

## The reframing

### Trust boundaries

What's trusted under the new model:
- The agent harness code (first-party, pure function,
  auditable)
- The host runtime (first-party, audited)
- Tool-call arguments *as data* (they're model output the
  host inspects, not code it executes)

What's untrusted:
- Tool *implementations* that execute arbitrary behaviour on
  behalf of the model (`shell`, code runners, external API
  clients)
- Tool-call arguments *interpreted as commands* — e.g., a
  path argument could be a traversal attempt; a shell
  argument could be an injection payload
- MCP servers (third-party code by default)
- User-authored tools

The rule: **trust the orchestrator, isolate each tool based on
what it actually does**.

### How the reducer model enables this

Because the harness is a pure function returning only a
`NextAction` data structure, the host has total visibility
and control over what actually executes. Every side-effectful
action goes through:

1. The harness decides (returns
   `NextAction::CallTool(name, args)`).
2. The host validates (is this tool in the agent's declared
   list?).
3. The host dispatches through the isolation tier declared
   for that tool.
4. The host records the outcome and feeds it back as
   `last-result`.

Steps 2–3 are where security enforcement happens, and they're
per-tool. The harness can't sneak past them because it has no
execution capability of its own.

## Tool isolation tiers

Each tool declares its isolation tier. The runtime provides
implementations of each tier and selects based on the
declaration.

| Tier | Characteristics | Suitable for |
|---|---|---|
| **In-process** | Native function call, no sandbox, no overhead | Pure computation: regex, jq, base64, date parsing, JSON schema validation |
| **Subprocess** | Child process with restricted cwd, argv-only args, timeout, output cap, minimal env | Bounded native commands where the argv itself is trusted: the phase-1 `shell`, `git` with validated subcommands |
| **Container per call** | Full OCI container, short-lived, filesystem and network policy enforced, resource limits | Arbitrary shell commands, language runtimes (python/node), MCP servers of moderate trust |
| **WASM instance** | Wasmtime sandbox with hermetic execution, no syscalls by default, bounded memory and fuel | Hermetic tool implementations: jq-in-wasm, busybox-in-wasm, pure-computation tools that need resource bounds. See [`wasm-posix-sandbox.md`](./wasm-posix-sandbox.md) for the specific direction on WASM-native shell and file tools. |
| **microVM** | Kata+Firecracker hypervisor isolation, separate kernel, strong tenant separation | Third-party MCP servers with no trust basis, user-submitted tools, tools that must run untrusted code (e.g., executing arbitrary model-generated scripts) |

Most tools don't need the top tier. A `jq` tool is pure
computation; a subprocess sandbox is already stronger than it
needs. A third-party MCP server that wraps an entire shell
environment deserves microVM isolation. The tier is chosen
per tool, not per agent.

### Pooling and amortisation

For tiers with non-trivial startup cost (container, microVM),
the runtime may pool warm instances. An invocation that calls
`shell` fifty times need not spin up fifty containers — a
warm pool can service requests in milliseconds rather than
hundreds of milliseconds.

Pool sizing and lifecycle (per-agent pools? global pools?
priming on agent declaration?) is an operational concern
orthogonal to this document.

## Tool definition schema

Tool definitions gain several declarations beyond name and
argument schema:

```
tool:
  name: shell
  description: Execute a shell command...
  parameters: { ... }           # JSON schema, as today

  isolation:
    tier: container              # in-process | subprocess | container | wasm | microvm
    image: alpine:latest         # for container/microvm tiers
    resources:
      cpu: 1                     # vCPUs
      memory: 512MB
      time: 30s                  # wall-clock limit

    network:
      policy: deny               # default-deny
      allow:                     # exceptions enforced by network proxy
        - "*.github.com"

    filesystem:
      read:                      # paths readable inside the sandbox
        - "$WORKSPACE"
        - "$AGENT_DATA:ro"
      write:                     # paths writable inside the sandbox
        - "$WORKSPACE"

    env:
      pass: [HOME, LANG]         # env vars passed through
      credentials:               # host-managed creds injected at invocation
        - ANTHROPIC_API_KEY

  pricing:                       # for cost tracking
    per_call: 0.0                # USD per invocation
    per_second: 0.0              # USD per second of execution
```

Not every field is required — defaults apply based on tier.
The schema grows as more tools need specific declarations,
but the shape is extensible.

## Workspace state: the third leg of the stool

The reducer model handles harness state. Tool isolation handles
untrusted code. Neither addresses the state the agent's work
accumulates in its environment — primarily the filesystem.

### Three tiers of state

| Tier | Scope | Persistence mechanism |
|---|---|---|
| **Harness state** | The invocation's logic progression | Opaque bytes in `StepOutput.state`, persisted by host |
| **Workspace state** | The filesystem (and other mutable environment) the agent's tools operate on | **This section** |
| **External state** | Systems outside our runtime (databases, remote APIs, third-party services) | Not persisted by factor-q. Tool design must handle via idempotency |

Harness state is solved by the reducer design. External state
is fundamentally out of scope — you cannot un-send a Slack
message. Workspace state is the unsolved middle ground: it
belongs to the invocation, must persist across suspensions,
but lives outside the harness's view.

### Why the workspace matters

Any useful agent mutates a filesystem:
- Reads files to gather context
- Writes files to produce output
- Modifies files iteratively
- Creates directories, builds artefacts, runs scripts that
  produce files

If we suspend an invocation and lose its filesystem state, we
lose the work. The agent may have spent 20 steps building up
a codebase; resuming must restore it.

The full invocation state is therefore:

```
(config, trigger, harness_state, workspace_state, last_result)
```

Not just `(config, trigger, harness_state, last_result)`.

### The workspace strategy

**Each agent has a persistent workspace** — a directory that
is the agent's working filesystem. Tools that interact with
a filesystem operate on this directory. Absolute paths
outside the workspace are rejected at the sandbox boundary.

**The workspace is snapshotable.** Using overlay filesystems
(OverlayFS) or container-based volume snapshots, the host
can take point-in-time snapshots. Suspension captures a
snapshot; resume restores it; migration transfers the snapshot
to another node.

**The workspace has a declared base layer.** An agent
definition can declare a read-only base filesystem — a
container image, a tarball, a git repository ref — that's
mounted as the lower layer of the overlay. Invocation-specific
mutations happen in the upper writable layer. When the
invocation ends, the upper layer is discarded (or preserved
as a new base, for long-running agents).

### Why pre-loaded workspaces are architecturally significant

A pre-loaded base layer contains whatever context the agent
needs to work: reference documentation, code samples, data
files, templates, prior artefacts, schemas.

This has a powerful consequence: **many things that would
otherwise need dedicated tools become filesystem operations**.

Without pre-loading, an agent working with the Stripe API
might need:
- A `fetch_stripe_docs(topic)` tool
- A `get_stripe_example(pattern)` tool
- A `lookup_stripe_error(code)` tool

With pre-loading, the agent's workspace has `/docs/stripe/`
populated with the reference material, and the agent uses
`file_read`, `grep`, `glob` — tools it already has. The
agent's experience matches how a human developer works: the
project on disk contains the context; the shell is how you
navigate it.

The tool catalogue shrinks because the filesystem becomes a
universal data interface. Instead of N custom tools for N
context domains, one filesystem-operations toolkit handles
all of them. This is a significant reduction in tooling
complexity at the cost of more upfront work preparing
workspace images — generally a good trade.

It also aligns with how models are trained. LLMs have seen
enormous amounts of code where the pattern is "navigate a
directory tree, read files, understand context". Agents that
operate on a rich workspace match this pattern; agents that
rely on many custom domain-specific tools force the model to
work against its training.

### Implementation options for workspaces

- **Overlay filesystem (OverlayFS on Linux).** Base layer is
  read-only; upper layer is per-invocation writable.
  Snapshotting the upper layer captures invocation state.
  Migration transfers the upper-layer delta. Well-supported
  in the kernel. Natural fit with container images as base
  layers.
- **Container-per-invocation with CRIU.** The workspace is
  the container filesystem; checkpoint/restore captures
  everything including processes. Heavyweight but complete.
  Brings back per-invocation containers — justified on state
  grounds even though security doesn't require it.
- **Git-backed workspace.** Every step auto-commits; state
  is a commit hash. Clean for code/text-heavy agents;
  awkward for binary artefacts.
- **Stable workspace paths, no migration.** Simplest: each
  invocation owns `/workspaces/<invocation-id>`; state
  persists as long as the directory exists. Cannot migrate.
  Acceptable for single-node deployments as a starting point.

Phase 2 will almost certainly start at stable-paths for
simplicity. Overlay filesystems are the most likely target
for phase 3 once migration requirements are concrete. The
choice isn't load-bearing at this stage — the architecture
accommodates any of them.

### Tool design implications

Tools should be designed to minimise state that lives outside
the workspace:

- **Prefer file-based persistence over in-memory state.** A
  tool that caches data should cache to files, not process
  memory.
- **Avoid long-lived resources.** Don't open a DB connection
  on first use and hold it; open, use, close. Reopen on
  subsequent calls.
- **Avoid background processes.** A tool that spawns a
  long-running daemon creates state the workspace snapshot
  won't capture. Prefer per-call invocations.
- **Declare paths relative to the workspace.** Absolute
  paths that escape the workspace are rejected.

These are discipline, not mechanism. Enforceable by
convention during tool review and by the sandbox's filesystem
policy.

## Load-bearing components

The new model concentrates security responsibility in three
places.

### 1. The declared-tools list

Each agent definition declares which tools it can invoke.
This is the meta-policy. An agent that doesn't declare
`shell` cannot run a shell command, regardless of what the
model hallucinates. The host validates every `CallTool` in
a `NextAction` against this list before dispatching.

This is the primary agent-level access control — narrower
than "what the agent might be able to do in a big sandbox"
and more explicit (visible in the agent definition file).

### 2. The network proxy

Every tool that makes network calls passes through the
network proxy. The proxy enforces:

- Per-tool network allowlists (from the tool's
  `isolation.network.allow` declaration)
- Aggregate per-agent policy (from the agent definition)
- Credential injection (tools don't see API keys directly;
  the proxy attaches them at request time based on declared
  credential requirements)
- Rate limiting (against external APIs)
- Audit logging (every outbound request recorded)
- Shadow-mode recording/replay (the proxy is where recording
  naturally happens)

The proxy becomes the most security-critical piece of
infrastructure in the runtime. It must be correct, it must be
performant, it must be auditable.

### 3. The tool registry

Resolves tool names to implementations, applies the declared
isolation tier, manages sandbox lifecycle, handles result
serialisation. In the phase-1 model this was a dispatch
table; in the new model it is the policy enforcement point
for most of the security story.

The registry's tool lookup is where:
- Tool name → implementation binding happens
- Isolation tier is applied (spawn container, create WASM
  instance, call native function)
- Sandbox lifecycle is managed (warm pool acquisition,
  teardown on completion)
- Result serialisation and event emission happen

Both correctness and performance concentrate here.

## External state: what we do not own

Some tool actions cause effects outside our runtime that we
cannot suspend, rewind, or retry:

- A row inserted in an external database
- A commit pushed to a remote git repository
- A message sent via Slack, email, SMS
- A payment processed
- An issue filed on GitHub

These are not recoverable from our side. If an invocation
fails mid-flight after such an action, the action has
happened.

Discipline for this:

- **Idempotency keys.** Tools that perform side-effectful
  actions should use idempotency tokens so retries produce
  the same result rather than duplicating the effect.
- **Explicit declaration in tool definitions.** Tools that
  cause irreversible external effects should be marked as
  such — visible when reviewing an agent's declared tools
  ("this tool has permanent external effects").
- **Accept non-recoverability.** Runtime does not attempt to
  undo external actions; agents and humans reason about
  them at the task level.

This is the same boundary every system that interacts with
the outside world has to draw. Our version is just more
explicit about it.

## Impact on ADR-0010

ADR-0010 accepted "containers by default" for agent
execution. The reducer-model shift and this
per-tool-isolation architecture require revisiting it.

Core decisions still hold:
- Containers are a key isolation primitive
- Kata+Firecracker is the high-security upgrade path
- WASM is a first-class future investigation
- Network proxy is load-bearing

What changes:
- The *unit of isolation* is the tool invocation (or the
  workspace, for state), not the agent invocation
- Per-agent containers may still appear, but for workspace
  encapsulation rather than security
- The tier table is now about tool-level concerns

Action: amend ADR-0010 with an addendum, or write ADR-0011
to supersede the parts that have shifted. Not urgent —
both documents can coexist during the transition, and the
phase-1 sandbox keeps working in the meantime.

## Decisions

Confirming the directions the architecture has settled into.

### Per-tool isolation tier declaration
Each tool declares its tier. The runtime dispatches through
the corresponding sandbox implementation. Overhead is paid
only for tools that need it.

### Per-agent workspace directory
Each agent has a persistent workspace. Filesystem operations
in tools act on this directory. The workspace is snapshotable
for suspension, migration, and fault recovery. Overlay
filesystems and container snapshotting are the likely target
mechanisms; stable-path workspaces are the starting point.

### Pre-loaded workspace base layers
Agent definitions can declare a read-only base layer
(container image, tarball, git ref) mounted as the lower
layer of the workspace overlay. This enables context-rich
agents without proliferating tool definitions — the
filesystem becomes the universal data interface.

### Network proxy is load-bearing
Every network-touching tool passes through the proxy. The
proxy enforces allowlists, injects credentials, logs
traffic, and supports shadow mode. Its correctness matters
disproportionately to the overall security posture.

### External state is out of scope
Runtime does not provide undo semantics for external
effects. Tool design uses idempotency where needed; agents
and humans reason about external effects at the task level.

## Open questions

### Workspace-snapshot mechanism

OverlayFS vs CRIU vs git vs stable-paths. The choice affects
migration story, storage overhead, and complexity. Needs a
dedicated design pass before phase 3. Starting position:
stable paths for phase 2.

### Pool sizing and lifecycle

How warm sandbox pools are managed. Per-agent? Global? How
many? Priming on agent declaration or on-demand? Operational
concern, deferred until performance data from the prototype
is available.

### Tool-definition schema formalisation

The schema sketched above is illustrative. Settling it
formally (full JSON schema, versioning story, migration path
for existing phase-1 tool definitions) is follow-up work.

### Credential-injection protocol

How the proxy knows which credentials to inject for which
tool. Declarative binding in the tool definition is the
shape, but details (credential store backend, rotation,
audit) are their own design concern.

### Agent-level container scoping

Given per-tool isolation, is there still value in an outer
container scoping the agent invocation? Likely yes — for
workspace encapsulation and aggregate resource limits — but
the justification is no longer security isolation per se.
Decide as workspace strategy solidifies.

## Next steps

1. Write a workspace-state design doc, settling the snapshot
   mechanism and the base-layer declaration format.
2. Extend the tool-definition schema in the phase-1 code to
   include isolation-tier declarations (default: subprocess
   for current tools).
3. Build the network proxy as a first-class component.
4. Amend ADR-0010 or write ADR-0011 to reflect the
   per-tool-isolation framing.
5. Add an agent tool catalogue design to the backlog (it can
   now reference the isolation schema).

## Appendix: the solution space is complex

This design settles several things but leaves others open.
The system has to balance:

- **Isolation strength** vs **startup overhead**
- **Configuration expressiveness** vs **operational surface**
- **Migration completeness** vs **storage cost**
- **Tool proliferation** vs **filesystem-as-interface generality**
- **Determinism and replayability** vs **natural-feeling
  tool behaviour**

No single choice is right across all these axes. The
architecture aims to be expressive enough to accommodate
different choices per tool and per deployment, without
forcing uniformity where it isn't warranted.
