# ADR-0001: Agent Execution Isolation Model

## Status
Open

## Context
Each agent in factor-q runs in a sandboxed execution context where nothing is available by default — filesystem, tools, environment, network, and resource limits are all explicitly declared in the agent definition.

This sandbox model is architecturally close to how containers work, which raises the question: should agents run as containers, or should isolation be implemented at the process level?

The answer has significant implications for the orchestrator's complexity, the user's operational burden, and the system's performance characteristics.

## Options

### Option A: Containerised agents (all agents run in containers)

Every agent invocation runs inside a container (Docker, Podman, or similar). The agent definition maps directly to container configuration — filesystem mounts, network policy, resource limits, installed tools.

**Advantages:**
- Hard isolation via a battle-tested, well-understood security model
- Filesystem, network, environment, and resource limits are native container primitives — no custom sandbox to build
- Reproducible environments — agent dependencies (system tools, language runtimes) are pinned in the image
- Clean resource accounting via cgroups
- Future-proofs remote and distributed execution (local Docker → Kubernetes is a smaller step)

**Disadvantages:**
- Startup latency — container spin-up adds seconds per agent invocation; mitigations (pre-warmed pools, keep-alive) add complexity
- Hard dependency on a container runtime for every user — heavier installation and operational burden
- Development friction — tweaking a prompt and re-running now involves image layers, mounts, and container networking
- Inter-agent communication needs explicit network wiring and volume mounts rather than simple IPC
- Overkill for lightweight agents (call an LLM, write a file) where isolation overhead is pure tax
- Image management — who builds images, how are they cached, where are they stored

### Option B: Process-level sandboxing (all agents run as processes)

Agents run as OS processes with isolation enforced through OS-level mechanisms (seccomp, namespaces, cgroups, chroot/bind mounts, network namespaces).

**Advantages:**
- Fast startup — process creation is near-instant
- No external runtime dependency
- Simpler development loop — change config, re-run
- Inter-agent communication via the event bus is straightforward (shared process space, no network wiring)

**Disadvantages:**
- Must build and maintain a custom sandbox — replicating what containers already provide
- OS-level sandboxing varies across platforms (Linux namespaces vs macOS sandbox profiles) — portability burden
- Weaker isolation guarantees than containers unless significant effort is invested
- System-level tool dependencies (agent needs `terraform`, `kubectl`, etc.) pollute the host or require per-agent management

### Option C: Pluggable isolation backend (per-agent choice)

The agent executor supports multiple isolation strategies. Each agent definition declares its isolation level:
- **Process-level** — lightweight, fast startup, basic OS sandboxing. Default for trusted agents and rapid iteration.
- **Container-level** — full container isolation for agents that need it (infrastructure operations, untrusted code, specific system dependencies).

**Advantages:**
- Fast path for simple/trusted agents, hard isolation when stakes are high
- Agent definition is explicit about its requirements
- Avoids forcing container overhead on every use case

**Disadvantages:**
- Two execution paths to build, test, and maintain
- Behavioural differences between backends may cause subtle bugs (works in process mode, breaks in container mode)
- Users must understand when to choose which — adds a decision to agent authoring

## Decision
Not yet taken.

## Considerations for the decision
- What is the expected ratio of lightweight agents (LLM + simple tools) to heavyweight agents (infrastructure operations, untrusted code)?
- How important is sub-second agent startup for the target use cases?
- Is cross-platform support (Linux, macOS) a requirement, or is Linux-only acceptable?
- Does the phase 1 single-agent executor need full isolation, or is it acceptable to start with process-level and add container support later?
