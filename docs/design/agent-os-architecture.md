# factor-q as an Agent Operating System

## The insight

If you squint at factor-q's architecture — process isolation,
concurrency, scheduling, resource sharing, capability-based
security — it has the shape of an operating system. Instead of
processes and threads we're dealing with agents, but the
structural parallels are exact.

This is not just an analogy. With the decision to target WASM as
the isolation tier, it becomes *structural*: a WASM guest making
capability calls to the host **is** a process making syscalls to
a kernel. factor-q is not building something *like* an OS. It is
building an OS for AI agents.

Taking this seriously means we can draw on decades of operating
system research — capability-based security, per-process
namespaces, supervision trees, distributed transparency — rather
than reinventing these patterns from first principles.

## The mapping

| OS concept | factor-q equivalent |
|---|---|
| Process | Agent invocation |
| Syscall | Tool call / MCP capability request to the WASM host |
| Virtual memory | WASM linear memory (per-agent address space) |
| Process namespace | Agent sandbox (what the agent can "see") |
| Process descriptor | Agent definition (sandbox, tools, budget, trigger) |
| IPC / pipes | Event bus / NATS message passing |
| Scheduler | Agent concurrency management |
| Device drivers | Tool implementations / MCP servers |
| Shell | CLI / human-in-the-loop interface |
| Kernel | The WASM host runtime |
| Library OS | Agent harness code compiled to WASM |
| Hot code reload | Self-improvement loop (shadow → promote) |
| Supervision tree | Agent graph with error handling strategies |

## Lessons from OS research

Three bodies of work are directly applicable to factor-q's
design.

### 1. Capability-based security (seL4, KeyKOS, Fuchsia)

**The principle:** possession of a capability IS authorization.
You don't check who's asking; you check what token they're
presenting. If an agent has a handle to the Slack capability, it
can use it. If it doesn't, it can't even name it. There's nothing
to escalate, nothing to forge, no confused deputy problem.

**How it applies:** WASM naturally provides capability-based
security. A WASM guest can only call functions the host has
explicitly provided as imports. It cannot discover or access
capabilities it wasn't given. The agent definition's `tools:`
list is literally a capability list. The runtime (kernel)
provides the declared capabilities as WASM imports, and nothing
else.

**The seL4 lesson — keep the capability space small and formally
verifiable.** The fewer capability types the host provides, the
smaller the attack surface and the easier it is to reason about
what any agent can do. This argues for a minimal set of primitive
capabilities at the host/guest boundary:

- `read(path) → bytes` — read from the agent's namespace
- `write(path, bytes)` — write to the agent's namespace
- `net_call(endpoint, request) → response` — make a network
  request (mediated by the proxy)
- `tool_call(name, params) → result` — invoke a tool
- `spawn(agent_id, payload)` — start a sub-agent
- `send(subject, message)` — publish to the event bus
- `recv(subject) → message` — receive from the event bus
- `log(level, message)` — emit a log event

Higher-level capabilities (MCP tool calls, LLM requests, memory
operations) are composed from these primitives, not added as
new syscall-level imports. This keeps the kernel interface narrow
and auditable.

**Relevant systems to study:**
- **seL4** — formally verified microkernel, capability-based.
  The gold standard for reasoning about what a process can do.
- **KeyKOS / EROS / CapROS** — capability-based OS lineage with
  strong persistence and confinement properties.
- **Fuchsia (Zircon)** — Google's capability-based OS. Modern,
  practical, designed for real-world use cases including
  untrusted code.

### 2. Plan 9's per-process namespaces

**The principle:** each process has its own *namespace* — its own
view of what resources exist. You compose a namespace by
*mounting* resources into it. Two processes on the same machine
can see completely different filesystems, network endpoints, and
services.

**How it applies:** each agent's `sandbox:` block defines its
namespace — which filesystem paths exist, which network endpoints
are reachable, which tools are available. Different agents on the
same runtime see different worlds. An agent that declares
`fs_read: ["/project/docs"]` sees a filesystem containing only
that subtree. An agent that declares
`network: ["*.slack.com"]` can only reach Slack.

**The network transparency insight:** in Plan 9, a mounted
resource could be local or remote, and the process couldn't tell
the difference. The same file operation might read from local
disk or from a file server across the network.

This maps to factor-q's tool abstraction. An agent calls a tool,
and it doesn't know (or care) whether the implementation is:
- A local function in the WASM host
- A child-process MCP server on the same machine
- A remote MCP server on another node
- A recorded response replayed for shadow mode evaluation

**The distributed OS consequence:** if capabilities are
network-transparent, multi-node distribution falls out of the
architecture. An agent on node A calls a tool. The host routes
the call to an MCP server on node B. The agent doesn't know. You
get distribution without the agent code changing. The WASM model
guarantees this — the guest calls an import, the host decides
where to fulfil it.

**Relevant systems to study:**
- **Plan 9 from Bell Labs** — the definitive per-process
  namespace design. Everything is a file, files can be anywhere,
  the namespace is composable.
- **Inferno** — Plan 9's spiritual successor. Ran on a virtual
  machine (Dis), portable across architectures. Agents on WASM
  is remarkably similar to Inferno processes on Dis.
- **9P protocol** — Plan 9's resource-access protocol. Simple,
  message-based, network-transparent. A potential model for the
  host/guest capability protocol.

### 3. Erlang/OTP's actor model and supervision

**The principle:** lightweight processes (actors) communicate via
message passing, are supervised hierarchically, and can be
hot-reloaded without dropping messages or losing state.

Erlang's BEAM VM is essentially an OS for telecom applications,
and the parallels to factor-q are striking:

| Erlang/OTP | factor-q |
|---|---|
| Lightweight process | Agent invocation |
| Message passing | Event bus |
| Supervision tree | Agent graph with error strategies |
| Hot code reload | Self-improvement loop (shadow → promote) |
| OTP behaviour (gen_server, etc.) | Skill / workflow template |
| Location transparency | Capability dispatch (local or remote) |
| "Let it crash" | Agent failure → supervisor decides |

**The supervision tree pattern** is the most directly actionable
lesson. When an agent fails, what happens? Erlang's answer: the
supervisor decides, based on a declared strategy:

- **one_for_one** — restart only the failed child
- **one_for_all** — restart all children (some depend on the
  failed one)
- **rest_for_one** — restart the failed child and all children
  started after it

factor-q's agent graph should declare supervision strategies.
If a sub-agent fails, the parent agent's supervision policy
determines the response: retry, restart, escalate, or abort.
This is more robust than ad-hoc error handling inside each
agent.

**"Let it crash"** is a powerful philosophy for agents. Don't
try to handle every error inside the agent; let it fail cleanly,
and let the supervisor decide what to do. This is especially
valuable for LLM-driven agents where failures are often
non-deterministic — a retry with slightly different context may
succeed where defensive error handling would just mask the
problem.

**Hot code reloading** maps directly to the self-improvement
loop. Erlang can replace a module's code while the system is
running without dropping in-flight messages. factor-q's shadow
mode and promotion mechanism achieves the same: validate a new
workflow version, then swap it in while the system continues to
operate.

**Relevant systems to study:**
- **Erlang/OTP** — the actor model, supervision trees, hot code
  reload, distribution. The closest existing system to what
  factor-q is becoming.
- **Akka** (Scala/JVM) — actor model with supervision, inspired
  by Erlang. Useful for seeing the patterns translated to a
  different runtime.
- **Microsoft Orleans** — virtual actor model ("grains") with
  automatic lifecycle management. Relevant for the "agents don't
  manage their own lifecycle" pattern.

## Architectural implications

Taking the OS analogy as a design principle (not just a metaphor)
leads to specific architectural decisions:

### The WASM host is a microkernel

The host provides the minimal set of primitives:
- **Capability dispatch** — route tool/MCP calls to
  implementations
- **Message passing** — event bus send/receive
- **Scheduling** — agent concurrency, priority, resource limits
- **Memory isolation** — WASM linear memory per agent
- **Namespace composition** — mount the declared sandbox
  resources into the agent's view

Everything else is "user space." Tools, MCP servers, the network
proxy, the LLM client — these are all services the kernel routes
to, not part of the kernel itself.

This follows the **exokernel** idea (MIT): the kernel multiplexes
capabilities, and library code in the guest implements
higher-level patterns. The agent harness (compiled to WASM) is a
library OS that provides the agent's programming model on top of
the host's primitive capabilities.

### The agent definition is a process descriptor

It declares:
- The **namespace** (sandbox: filesystem, network, env)
- The **capabilities** (tools, MCP servers)
- The **resource limits** (budget, timeout, memory)
- The **entry point** (system prompt + trigger pattern)
- The **supervision policy** (future: restart strategy, escalation)

The kernel reads this descriptor and provisions the execution
environment. The agent code doesn't configure its own sandbox —
the kernel does, based on the descriptor. This is the
container/process analogy: a Dockerfile describes the
environment, the container runtime provisions it, the process
runs inside it.

### Distribution is a deployment concern, not an application concern

If the host/guest boundary is a capability interface, the host
can fulfil capabilities locally or remotely. The guest (agent)
never knows. This means:

- Single-node: all capabilities fulfilled locally
- Multi-node: some capabilities routed to other nodes
- The agent code is identical in both cases

This is Plan 9's network transparency applied to AI agents. It
means factor-q can start as a single-node system and grow to
a distributed cluster without changing agent definitions or
harness code.

### Supervision is a first-class runtime concern

Error handling belongs in the graph (supervisor), not in the agent
(process). The runtime should provide:
- Restart policies (retry count, backoff, escalation)
- Failure propagation (parent notified when child fails)
- Circuit breakers (stop retrying after N failures)
- Graceful degradation (continue with reduced capability)

These are configured in the agent graph definition, not coded
into individual agents.

## Open questions

- **Guest/host boundary design:** designed as a reducer.
  See [`wasm-boundary-design.md`](./wasm-boundary-design.md).
  Guest exports a single pure `step(StepInput) -> StepOutput`
  function; zero imports. The host drives the loop, persists
  state, executes `NextAction` variants (`call-model`,
  `call-tool`, `call-tools-parallel`, `complete`, `failed`).
  This collapses suspension, migration, fault recovery,
  shadow-mode replay, and audit logging into one mechanism
  at the harness level. Workspace state (filesystem the
  agent operates on) is handled separately; see
  [`tool-isolation-model.md`](./tool-isolation-model.md).
- **Security boundary relocated to tools.** Because the
  reducer harness is pure and trusted, isolation moves from
  agent-scoped to tool-scoped. Each tool declares its own
  isolation tier (in-process / subprocess / container /
  wasm / microvm) based on its threat profile. Details in
  [`tool-isolation-model.md`](./tool-isolation-model.md).
  Remaining open: workspace-snapshot mechanism, tool
  catalogue design, debugging tractability.
- **Scheduling model:** preemptive (the host can interrupt an
  agent mid-execution) or cooperative (agents yield at capability
  call boundaries)? WASM naturally supports cooperative scheduling
  at import call boundaries, but long-running pure computation
  in the guest could starve other agents.
- **State persistence:** Erlang processes are ephemeral but can
  checkpoint state. Should agents have a mechanism for
  checkpointing their working state so they can resume after a
  restart? Or is "restart from scratch with persistent memory"
  sufficient?
- **Capability delegation:** can an agent pass a capability to a
  sub-agent? In seL4, capabilities can be copied, minted
  (derived with reduced rights), or revoked. Should factor-q
  support delegation, or is the flat "declared in the agent
  definition" model sufficient?
- **Inter-agent communication:** the event bus is pub/sub. Should
  there also be direct agent-to-another channels (like Erlang's
  process mailboxes)? Or is pub/sub sufficient?

## Addendum: factor-q as a single deployable unit

A consequence of the WASM isolation model is that the entire
factor-q runtime — including all agent isolation — can ship as
a single container image or a single binary.

Traditional container-based agent systems put agents *inside*
containers, which means the host must manage container
orchestration, Docker-in-Docker, or privileged access to a
container runtime. With WASM, agent isolation is enforced
*within the process* by the WASM runtime's compiler. No kernel
namespaces, no cgroups, no seccomp, no mount operations. It's
all userspace.

This means:

- **Distribution is trivial.** `docker run factorq` (or just
  `./fq run`) and you have a running agent OS with full
  isolation. No special privileges, no nested containers, no
  runtime class configuration. It runs anywhere — CI systems,
  cheap VPS providers, PaaS platforms that don't support nested
  containers.

- **Multi-node is N copies of the same thing.** Each node runs
  the same image/binary. They connect via NATS. Agent invocations
  are dispatched across nodes via the event bus. Because
  capabilities are network-transparent, agents don't know which
  node they're on. Scaling is horizontal replication of a
  stateless service, not per-agent container orchestration.

- **The security story inverts.** The outer container (if used)
  protects the runtime from the host environment, not the other
  way around. The trust boundary you care about — agent isolation
  — is enforced by the WASM spec inside the process. The
  deployment packaging is a convenience, not a security
  mechanism.

- **The orchestration question simplifies.** ADR-0010 deferred
  the decision between self-managed containers and Kubernetes.
  If factor-q manages its own isolation internally, you don't
  need Kubernetes for isolation at all. You might still want it
  for horizontal scaling (N replicas, rolling updates, health
  checks), but that's a much simpler ask — commodity container
  orchestration, not per-agent lifecycle management.

- **The network proxy can be built-in.** Since the WASM host
  mediates all outbound capability calls, the proxy logic
  (recording, replay, allowlist enforcement, caching) can be a
  component of the runtime itself rather than a separate sidecar.
  Every outbound call already passes through the host — the host
  *is* the proxy. This eliminates a moving part from the
  deployment architecture.

The single-binary deployment model is particularly appealing for
the "personal tool" use case: download one binary, run it, point
it at your agents directory. No Docker, no Kubernetes, no
infrastructure. The same binary scales to a multi-node cluster
when the workload demands it — just run more copies behind NATS.
