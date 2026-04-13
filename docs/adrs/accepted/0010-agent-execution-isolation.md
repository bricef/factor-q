# ADR-0010: Agent Execution Isolation Model

## Status
Accepted

## Context
Each agent in factor-q runs in a sandboxed execution context where nothing is available by default — filesystem, tools, environment, network, and resource limits are all explicitly declared in the agent definition.

Phase 1 shipped process-level sandboxing: path canonicalisation for file tools, `exec_cwd` restrictions for the shell tool, and output caps. This is sufficient for single-tenant self-hosted use, but does not defend against a determined or adversarial agent (e.g., a compromised or untrusted model that exploits PATH-visible binaries, opens arbitrary network connections, or attempts kernel-level escapes).

With the introduction of MCP support (phase 2), agents now interact with external systems over the network via MCP servers. Even stdio-transport MCP servers make outbound network calls to the services they wrap (Slack, GitHub, vector databases, etc.). In a containerised deployment, **all** meaningful agent actions are network calls, which makes the network boundary the natural trust enforcement point.

Additionally, factor-q aims to support multiple LLM providers, including models of varying provenance and trust levels. The isolation model must account for the possibility that an untrusted model could actively attempt to escape its sandbox.

## Decision: Containers by default, with a pluggable upgrade path to microVMs

### Tier 1: Containers (default)

All agent invocations run inside OCI containers (Docker, Podman, or equivalent). The agent definition maps to container configuration:

- **Filesystem:** read-only root image plus explicit bind mounts from `sandbox.fs_read` / `sandbox.fs_write`
- **Network:** a network proxy enforces the `sandbox.network` allowlist patterns at the container boundary
- **Environment:** only declared `sandbox.env` variables are injected
- **Resources:** CPU and memory limits via cgroups, derived from agent configuration or global defaults
- **Tools:** MCP servers run as child processes inside the container; their outbound traffic is subject to the same network proxy

This provides battle-tested isolation (Linux namespaces, cgroups, seccomp) without requiring a custom sandbox implementation. Startup latency (~200ms) is acceptable for the target workloads.

### Tier 2: MicroVMs (upgrade path for high-security workloads)

For agents that handle sensitive credentials, operate on production infrastructure, or use untrusted models, the isolation backend can be upgraded to microVMs via Kata Containers with Firecracker.

Kata Containers present the standard OCI/container API — the orchestration layer does not need to change. The runtime substitutes a Firecracker microVM for the container, providing:

- **Separate kernel per agent** — a container escape (kernel exploit) does not breach the sandbox
- **Minimal attack surface** — Firecracker's VMM is purpose-built for multi-tenant isolation (~50k lines of Rust)
- **Fast boot** (~125ms) and low overhead (~5MB memory) — practical for per-invocation VMs
- **Unchanged networking model** — the network proxy enforcement works identically

This tier is not required for initial deployment. It becomes relevant when:
- Untrusted or unvetted models are used
- Agents have access to production credentials or high-value systems
- Compliance or audit requirements demand hypervisor-level isolation

### The network proxy as the trust enforcement point

Regardless of isolation tier, a network proxy sits between every agent and the outside world. This proxy is the single enforcement point for:

- **Network policy:** the agent definition's `sandbox.network` patterns are enforced here, not inside the container. An agent that lists `network: ["*.slack.com"]` can only reach Slack endpoints.
- **Shadow mode:** for workflow evaluation, the proxy can record live outbound traffic and replay it for shadow invocations, enabling safe comparison of workflow changes against real workloads without side effects.
- **Audit logging:** every outbound request/response is recorded for observability and forensic review.
- **Caching:** repeated identical requests (e.g., fetching the same resource) can be served from cache, reducing cost and latency.
- **Rate limiting:** prevents runaway agents from hammering external APIs.
- **Trust-based access control:** the proxy can enforce different allowlists based on the model's trust tier, independent of what the agent definition declares. An untrusted model might have its effective network allowlist intersected with a global restriction policy.

This means the security model is: **permissive inside the sandbox, controlled at the boundary.** Agents do not need to ask permission to use their declared tools or modify files within their workspace. The container (or microVM) provides the isolation floor, and the network proxy provides the policy enforcement ceiling.

### Isolation tier selection

The isolation tier is determined by deployment configuration, not by the agent definition. Agent definitions declare *what* they need (network access, filesystem mounts, tools). The deployment environment decides *how* to provide it:

| Context | Isolation | Rationale |
|---|---|---|
| Local development | Process-level (current) | Fast iteration, no container runtime needed |
| Single-node production | Container | Good isolation, simple operations |
| Untrusted models | Container + restrictive network policy | Network proxy limits blast radius |
| High-security / compliance | Kata + Firecracker microVM | Hypervisor-level isolation |
| Multi-tenant cluster | Kata + Firecracker | Hard tenant isolation required |

This keeps the agent definition portable across environments. The same agent YAML works on a developer laptop and in a locked-down production cluster — only the enforcement strength changes.

## Rationale

**Why containers first, not process-level sandboxing:**
Process-level sandboxing (seccomp, namespaces, chroot) requires building and maintaining a custom sandbox that replicates what containers already provide, with weaker guarantees and poor cross-platform portability. Containers are the industry-standard isolation primitive for this exact problem class.

**Why not containers only (without the microVM upgrade path):**
Container isolation shares the host kernel. Newer frontier models are demonstrably capable of sophisticated security research, including finding zero-day vulnerabilities. For untrusted models, shared-kernel isolation may not provide a sufficient trust boundary. Designing the interface to be isolation-backend-agnostic from the start avoids a painful migration later.

**Why the network proxy is architecturally fundamental:**
In a containerised deployment, all meaningful agent actions are network calls — even stdio MCP servers make outbound HTTP requests to the services they wrap. The network layer is where the real trust decisions happen. Process-level filesystem sandboxing is necessary but not sufficient; network policy enforcement is where the highest-value security controls live.

**Why Kata + Firecracker over other microVM options:**
Kata Containers preserve the OCI API, so the orchestration layer doesn't need to know whether it's running containers or microVMs. Firecracker is purpose-built for multi-tenant isolation, is written in Rust (minimising memory safety vulnerabilities in the VMM itself), boots in ~125ms, and is battle-tested at AWS Lambda/Fargate scale. This combination provides the strongest isolation with the least architectural disruption.

## Deferred decision: container orchestration

This ADR decides *what* isolation technology to use but not *who manages the container lifecycle*. The options are:

- **Self-managed:** Factor Q's runtime spawns and manages containers directly via the Docker/Podman API. Simpler operationally for single-node deployments, avoids a heavyweight dependency, but means building scheduling, health checks, resource management, and scaling ourselves.
- **Delegated to an external orchestrator:** Kubernetes, Nomad, or Docker Swarm manages the container lifecycle. Factor Q submits workloads (as Jobs, Tasks, or Services) and the orchestrator handles placement, resource limits, networking, restarts, and scaling. Kubernetes in particular provides network policy, service mesh integration, and Kata/Firecracker support out of the box.

Kubernetes is the strongest candidate for the delegated approach — it provides the most complete coverage of our requirements (network policy, resource quotas, pod security standards, Kata runtime class support, horizontal scaling) but carries significant operational complexity, especially for single-node or small-scale deployments.

This decision must be taken before container support ships, but does not need to be taken now. The key constraint is that the runtime's interface for launching agent workloads should be abstract enough to support either approach without rearchitecting.

## Consequences

- The current process-level sandboxing (phase 1) continues to work for local development and is not removed.
- Container support will be the next isolation milestone, requiring: a container image build pipeline, runtime integration to launch agents in containers, and a network proxy component.
- Agent definitions do not change — the `sandbox` block already declares the right primitives. The deployment layer maps these to container/network configuration.
- The network proxy becomes a required component for production deployments, even before microVM support is added.
- MicroVM support via Kata + Firecracker is deferred until the trust or compliance requirements demand it, but the architecture does not need to change when it arrives.
- The container orchestration question (self-managed vs Kubernetes/Nomad) is deferred but must be resolved before container support ships.
