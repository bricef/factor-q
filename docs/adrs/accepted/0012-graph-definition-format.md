# ADR-0012: Execution Graph Definition Format

## Status
Accepted

## Context
Agent definitions (ADR-0005) specify what an individual agent is. A separate concern is how agents are wired together — what events flow between them, where work enters and exits the system, and what structural patterns (fan-out, fan-in, approval gates) govern the topology.

The graph definition must be:
- **Machine-readable** — parsed, validated (no dangling references, compatible edges), and executed by the runtime
- **Human-readable** — understandable without specialist tooling
- **Diffable** — produces clean diffs in version control, since the learning loop will produce new versions
- **LLM-readable** — a meta-agent reviewing or modifying the graph needs to work with the format directly
- **Visually authorable** (future) — humans think about graphs visually; a UI layer over the format is the expected primary authoring experience long-term

## Decision: YAML graph definitions with JSON Schema validation

Execution graphs are defined as YAML files with a published JSON Schema. The YAML file is the source of truth. Visual authoring tools are a UI layer on top, not a replacement.

### Structure

- **Triggers** — external events that enter the graph, routed to an entry agent
- **Agents** — references to agent definition files (Markdown, per ADR-0005), plus graph-specific configuration: subscriptions, publish targets, capabilities (spawn, exec), and approval gates
- **Edges** — explicit wiring between agents, annotated with structural patterns (fan-out, fan-in, pipeline)
- **Version** — graph definitions are versioned; the running graph is always a specific version

### Example

```yaml
name: software-development
version: 3
triggers:
  - subject: "tasks.new.software"
    entry: planner

agents:
  planner:
    definition: agents/planner.md
    publishes:
      - tasks.research.*
      - tasks.implement.*

  researcher:
    definition: agents/researcher.md
    subscribes:
      - tasks.research.*
    publishes:
      - results.research.*
    capabilities: [spawn]

  implementer:
    definition: agents/implementer.md
    subscribes:
      - tasks.implement.*
    publishes:
      - results.implement.*
    capabilities: [spawn, exec]

  reviewer:
    definition: agents/reviewer.md
    subscribes:
      - results.implement.*
    approval_gate: true

edges:
  - from: planner
    to: [researcher, implementer]
    pattern: fan-out

  - from: [researcher, implementer]
    to: reviewer
    pattern: fan-in
```

### Separation of concerns

- **Agent definition** (Markdown) — what an agent is: model, prompt, tools, sandbox, budget
- **Graph definition** (YAML) — how agents relate: event routing, capabilities, structural patterns, approval gates
- **Spawn/exec** — dynamic, per-agent capabilities granted in the graph definition. An agent with spawn permission can create child agents at runtime within the dataflow; this is runtime dynamism, not graph mutation.
- **Graph versioning** — the learning loop can author new versions of the graph, but a running graph is always a specific version. Changes are versioned replacements, not live mutations.

### Incremental path

1. Manually authored YAML graph definitions (phase 1+)
2. CLI tooling for validation, visualisation, and dry-run
3. Visual authoring UI that reads and writes the YAML files
4. LLM-assisted graph authoring and optimisation

### Rationale

YAML is widely understood, diffs cleanly, and is natively validateable against JSON Schema. The format is simple enough to author by hand for small graphs and structured enough for tooling to parse and render. A visual UI is the expected long-term authoring experience but is not required to start — manually authored YAML is a viable starting point.

### Tradeoffs accepted

- **YAML limitations** — complex graphs may become verbose. Mitigation: graph composition (importing sub-graphs) can be added later.
- **No visual tooling at launch** — text-only authoring for graphs is less intuitive than a node-and-edge editor. Acceptable for power users in early phases.
- **Validation at load time, not author time** — until editor plugins or a UI exist, invalid graphs are caught when the runtime loads them. JSON Schema enables editor-side validation for users with schema-aware editors.

## Consequences
- Graph definitions are stored as `.yaml` files alongside agent definition directories
- A JSON Schema is published for graph definitions, enabling editor validation
- The runtime validates graph definitions at load time (no dangling agent references, compatible pub/sub wiring)
- Spawn and exec are capabilities granted per-agent in the graph, not global permissions
- Graph versions are tracked; the learning loop produces new versions rather than mutating the running graph
